//! HTTPS fetch of the quota endpoint + parse into Window shape.
//! Reqwest with rustls-tls (pure Rust TLS, no OpenSSL/GnuTLS).
//!
//! Provider-specific bits (auth header, User-Agent, error envelope)
//! live in `crate::provider`; this module is the data-driven HTTP
//! driver. To port to a different API, edit `src/provider.rs`.
//!
//! The HTTP call is blocking (reqwest blocking) and intended to be
//! wrapped in `tokio::task::spawn_blocking` from the polling loop.
//!
//! `fetch_windows_blocking` returns `Vec<Window>` — one entry per
//! window the plan's `PlanShape` defines. There's no fixed pair;
//! `main.rs` consumes whatever the parser produces.

use anyhow::{Context, Result};
use reqwest::blocking::Client;
use serde_json::Value;

use crate::burn::Window;
use crate::parse::parse_plan;
use crate::provider::{PlanShape, Provider};

// Re-export so callers can refer to `fetch::Client` without importing reqwest.
pub use reqwest::blocking::Client as HttpClient;

/// Build a shared blocking reqwest client. TLS via rustls.
pub fn build_client(provider: &Provider) -> Result<Client> {
    let user_agent = format!("{}/{}",
        provider.user_agent_prefix, env!("CARGO_PKG_VERSION"));
    Client::builder()
        .user_agent(user_agent)
        .timeout(std::time::Duration::from_secs(15))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .context("building reqwest client")
}

/// Synchronous fetch — blocks the calling thread. Callers should run this
/// on a blocking thread (`tokio::task::spawn_blocking`) so the runtime
/// isn't blocked.
pub fn fetch_windows_blocking(
    client: &Client,
    endpoint: &str,
    api_key: &str,
    provider: &Provider,
    shape: &PlanShape,
) -> Result<Vec<Window>> {
    let (auth_name, auth_value) = (provider.auth_header)(api_key);
    let resp = client
        .get(endpoint)
        .header(auth_name, auth_value)
        .send()
        .context("HTTP request")?;

    let status = resp.status();
    if !status.is_success() {
        // gjs parity — read the body even on error so the menu can
        // show a useful diagnostic. Some providers echo the bearer
        // token in error bodies (especially cloudflare/edge); truncate
        // hard and redact anything that looks like an Authorization
        // header before this lands in the menu / journald.
        let raw = resp.text().unwrap_or_default();
        let snippet = sanitize_error_snippet(&raw);
        return Err(anyhow::anyhow!("HTTP {status}: {snippet}"));
    }

    let body: Value = resp.json().context("decoding JSON body")?;

    if let Some(msg) = envelope_error(&body, shape.error_envelope.as_ref()) {
        return Err(anyhow::anyhow!("{msg}"));
    }

    // Pass current epoch ms to parse_plan so it can compute
    // `reset_at = now_ms + remains_time_ms` (when the API's
    // `remains_time` is a duration in ms, not an epoch timestamp).
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    parse_plan(&body, shape, now_ms).context("parsing payload")
}

/// Strip anything that looks like a credential from an error snippet
/// before it's surfaced in the menu's error row. The first 80 bytes
/// are kept (gjs's `slice(0, 80)`); `Bearer ...`, `Authorization ...`,
/// and `api[-_]key ...` patterns are replaced with `[redacted]`.
pub fn sanitize_error_snippet(raw: &str) -> String {
    let truncated: String = raw.chars().take(80).collect();
    // Case-insensitive match: Bearer/Authorization/api[-_]?key, optional
    // separator (`:`, `=`, whitespace), then the value (alphanumeric,
    // dot, dash, plus, slash, equals — base64 + url-safe). gjs uses
    // /(?:Bearer|Authorization|api[-_]?key)\s*[:=]?\s*[A-Za-z0-9._\-+/=]+/gi.
    let re = regex_lite_redact(&truncated);
    re
}

/// Inline lightweight regex substitute — we don't pull the `regex`
/// crate for one pattern. Walks the input looking for matches of
/// `(?:Bearer|Authorization|api[-_]?key)` followed by optional `:`,
/// `=`, or whitespace, then a credential-shaped run. Replaces each
/// match with `[redacted]`.
fn regex_lite_redact(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let lower = s.to_ascii_lowercase();
    while i < bytes.len() {
        if let Some((rel, end)) = find_match(&lower, i) {
            // Copy bytes before the match.
            out.push_str(&s[i..i + rel]);
            out.push_str("[redacted]");
            i = end;
        } else {
            // No more matches from `i` onwards — push rest and stop.
            out.push_str(&s[i..]);
            break;
        }
    }
    out
}

/// Find a `(?:Bearer|Authorization|api[-_]?key)\s*[:=]?\s*[A-Za-z0-9._\-+/=]+`
/// match starting at or after position `start`. Returns the match
/// start (offset from `start`) and the position after the match.
fn find_match(lower: &str, start: usize) -> Option<(usize, usize)> {
    let bytes = lower.as_bytes();
    if start >= bytes.len() {
        return None;
    }
    let patterns: &[&str] = &["bearer", "authorization", "api-key", "api_key"];
    for idx in start..bytes.len() {
        for pat in patterns {
            if lower[idx..].starts_with(pat) {
                let after_pat = idx + pat.len();
                let mut k = after_pat;
                // Optional separator.
                if k < bytes.len() && (bytes[k] == b':' || bytes[k] == b'=') {
                    k += 1;
                }
                // Skip whitespace.
                while k < bytes.len() && (bytes[k] as char).is_ascii_whitespace() {
                    k += 1;
                }
                // Credential run.
                let cred_start = k;
                while k < bytes.len() && is_cred_char(bytes[k]) {
                    k += 1;
                }
                if k > cred_start {
                    return Some((idx, k));
                }
                // No credential after the keyword — not a match.
                break;
            }
        }
    }
    None
}

fn is_cred_char(b: u8) -> bool {
    matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'_' | b'+' | b'/' | b'=')
}

/// Generic provider-error-envelope reader. Reads the integer status
/// code at `envelope.code_path`; if it's not in `envelope.success_codes`,
/// returns the message at `envelope.message_path` formatted as
/// `"API error {code}: {msg}"`.
///
/// Some providers (MiniMax) return HTTP 200 with an error envelope in
/// the body (`{"base_resp": {"status_code": 1004, "status_msg":
/// "login fail: ..."}}`). Surfacing the real message beats the
/// generic "payload missing model_remains[0]" parse error the tray
/// would otherwise show — e.g. a placeholder key in the keyring now
/// reports the actual API rejection instead of a confusing parse
/// failure. Providers that use standard HTTP status codes only should
/// set `error_envelope: None` in their `PlanShape`.
fn envelope_error(body: &Value, envelope: Option<&crate::provider::ErrorEnvelope>) -> Option<String> {
    let env = envelope?;
    let code = body.pointer(env.code_path).and_then(|c| c.as_i64())?;
    if env.success_codes.contains(&code) {
        return None;
    }
    let msg = body
        .pointer(env.message_path)
        .and_then(|m| m.as_str())
        .unwrap_or("unknown error");
    Some(format!("API error {code}: {msg}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// gjs: `raw.replace(/(?:Bearer|Authorization|api[-_]?key)\s*[:=]?\s*[A-Za-z0-9._\-+/=]+/gi, '[redacted]')`.
    #[test]
    fn redacts_bearer_token() {
        let s = "Server said: Bearer abcDEF123-_./+= denied";
        let out = sanitize_error_snippet(s);
        assert!(!out.contains("abcDEF123"),
                "Bearer value must be redacted; got {out}");
        assert!(out.contains("[redacted]"));
    }

    #[test]
    fn redacts_authorization_header() {
        let s = "{\"Authorization: sk-cp-tok1234\", err}";
        let out = sanitize_error_snippet(s);
        assert!(!out.contains("sk-cp-tok1234"));
        assert!(out.contains("[redacted]"));
    }

    #[test]
    fn redacts_api_key_variants() {
        for variant in &["api_key", "api-key", "API_KEY", "Api-Key"] {
            let s = format!("{variant}=sk-cp-abcdef0123456789");
            let out = sanitize_error_snippet(&s);
            assert!(!out.contains("abcdef0123456789"),
                    "{variant} value must be redacted; got {out}");
        }
    }

    #[test]
    fn plain_text_passes_through() {
        let s = "Internal Server Error";
        assert_eq!(sanitize_error_snippet(s), s);
    }

    #[test]
    fn truncates_to_80_chars() {
        let s = "x".repeat(200);
        let out = sanitize_error_snippet(&s);
        assert_eq!(out.chars().count(), 80);
    }

    #[test]
    fn no_match_leaves_input_intact() {
        // No credential keyword — leave alone.
        let s = "Internal Server Error: please retry";
        let out = sanitize_error_snippet(s);
        assert_eq!(out, s);
    }

    #[test]
    fn keyword_followed_by_word_is_redacted_too() {
        // gjs's regex DOES match "Authorization header" — the cred
        // run doesn't require a separator, just the keyword followed
        // by a credential-shaped token. We mirror that behavior.
        let s = "Authorization header missing";
        let out = sanitize_error_snippet(s);
        assert!(out.contains("[redacted]"),
                "Authorization + word IS redacted (gjs regex behavior)");
    }

    #[test]
    fn build_client_succeeds() {
        assert!(build_client(&crate::provider::MINIMAX).is_ok());
    }

    #[test]
    fn parse_error_propagates() {
        let v: Value = serde_json::from_str("{}").unwrap();
        let shape = crate::provider::MINIMAX.shape("remains").unwrap();
        assert!(parse_plan(&v, shape, 0).is_err());
    }

    #[test]
    fn empty_model_remains_returns_parse_error_not_panic() {
        let v = json!({"model_remains": []});
        let shape = crate::provider::MINIMAX.shape("remains").unwrap();
        assert!(parse_plan(&v, shape, 0).is_err());
    }

    fn minimax_envelope() -> &'static crate::provider::ErrorEnvelope {
        crate::provider::MINIMAX.shape("remains")
            .and_then(|s| s.error_envelope.as_ref())
            .expect("MINIMAX_REMAINS should have an error envelope")
    }

    #[test]
    fn base_resp_error_envelope_is_reported_not_parse_error() {
        // MiniMax returns HTTP 200 with this envelope on auth failures.
        let v = json!({
            "base_resp": {"status_code": 1004, "status_msg": "login fail: bad key"}
        });
        let err = super::envelope_error(&v, Some(minimax_envelope()));
        assert_eq!(err.as_deref(), Some("API error 1004: login fail: bad key"));
    }

    #[test]
    fn base_resp_zero_code_is_not_an_error() {
        let v = json!({"base_resp": {"status_code": 0, "status_msg": "ok"}});
        let err = super::envelope_error(&v, Some(minimax_envelope()));
        assert_eq!(err, None);
    }

    #[test]
    fn missing_base_resp_is_not_an_error() {
        let v = json!({"model_remains": []});
        let err = super::envelope_error(&v, Some(minimax_envelope()));
        assert_eq!(err, None);
    }

    #[test]
    fn no_envelope_returns_none_for_any_body() {
        // Providers without an error envelope (set error_envelope: None)
        // should never trigger envelope_error — HTTP status is the only
        // signal. Sanity: pass None envelope with a base_resp body.
        let v = json!({"base_resp": {"status_code": 1004, "status_msg": "x"}});
        let err = super::envelope_error(&v, None);
        assert_eq!(err, None);
    }
}