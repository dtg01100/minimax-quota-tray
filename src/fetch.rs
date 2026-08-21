//! HTTPS fetch of the quota endpoint + parse into `Vec<Window>`.
//! Reqwest with rustls-tls (pure Rust TLS, no OpenSSL/GnuTLS).
//!
//! Provider-specific bits (auth style, User-Agent, JSON shape) come
//! from the active instance's `Config`. This module is the
//! data-driven HTTP driver — to port to a different API, change
//! config.json (or add a new instance), not this file.
//!
//! The HTTP call is blocking (reqwest blocking) and intended to be
//! wrapped in `tokio::task::spawn_blocking` from the polling loop.
//!
//! `fetch_windows_blocking` returns `Vec<Window>` — one entry per
//! window the per-instance `PlanShape` defines. There's no fixed
//! pair; `main.rs` consumes whatever the parser produces.

use anyhow::{Context, Result};
use reqwest::blocking::Client;
use serde_json::Value;

use crate::burn::Window;
use crate::parse::parse_plan;
use crate::provider::{AuthConfig, PlanShape};

// Re-export so callers can refer to `fetch::Client` without importing reqwest.
pub use reqwest::blocking::Client as HttpClient;

/// Build a shared blocking reqwest client. TLS via rustls.
pub fn build_client(user_agent_prefix: &str) -> Result<Client> {
    let user_agent = format!("{}/{}",
        user_agent_prefix, env!("CARGO_PKG_VERSION"));
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
///
/// `auth` dispatches to one of the auth styles in `AuthConfig`:
/// `Bearer` and `Header`/`Custom` set an HTTP header,
/// `QueryParam` appends `?<param>=<key>` to the URL (the header
/// values are empty in that case).
pub fn fetch_windows_blocking(
    client: &Client,
    endpoint: &str,
    api_key: &str,
    auth: &AuthConfig,
    shape: &PlanShape,
) -> Result<Vec<Window>> {
    // Apply query-param auth to the URL first (if applicable) so the
    // rest of the request goes out with the key in the URL.
    let endpoint = auth.apply_to_endpoint(endpoint, api_key);
    let (auth_name, auth_value) = auth.build(api_key);

    let mut req = client.get(&endpoint);
    if !auth_name.is_empty() {
        req = req.header(&auth_name, &auth_value);
    }
    let resp = req.send().context("HTTP request")?;

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
    regex_lite_redact(&truncated)
}

/// Inline lightweight regex substitute — we don't pull the `regex`
/// crate for one pattern.
fn regex_lite_redact(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let lower = s.to_ascii_lowercase();
    while i < bytes.len() {
        if let Some((rel, end)) = find_match(&lower, i) {
            out.push_str(&s[i..i + rel]);
            out.push_str("[redacted]");
            i = end;
        } else {
            out.push_str(&s[i..]);
            break;
        }
    }
    out
}

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
                if k < bytes.len() && (bytes[k] == b':' || bytes[k] == b'=') {
                    k += 1;
                }
                while k < bytes.len() && (bytes[k] as char).is_ascii_whitespace() {
                    k += 1;
                }
                let cred_start = k;
                while k < bytes.len() && is_cred_char(bytes[k]) {
                    k += 1;
                }
                if k > cred_start {
                    return Some((idx, k));
                }
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
/// "login fail: ..."}}`). Providers that use standard HTTP status
/// codes only should set `error_envelope: None` in their `PlanShape`.
fn envelope_error(body: &Value, envelope: Option<&crate::provider::ErrorEnvelope>) -> Option<String> {
    let env = envelope?;
    let code = body.pointer(&env.code_path).and_then(|c| c.as_i64())?;
    if env.success_codes.contains(&code) {
        return None;
    }
    let msg = body
        .pointer(&env.message_path)
        .and_then(|m| m.as_str())
        .unwrap_or("unknown error");
    Some(format!("API error {code}: {msg}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ErrorEnvelope, PlanShape, WindowShape};
    use serde_json::json;

    /// Build a shape with the `base_resp` error envelope so we can
    /// exercise the envelope reader without baking MiniMax into the
    /// test.
    fn shape_with_envelope(env: ErrorEnvelope) -> PlanShape {
        PlanShape {
            entries_path: "/".to_string(),
            windows: vec![WindowShape {
                id: "primary".to_string(),
                field_prefix: "primary".to_string(),
                start_field: None,
                reset_field: None,
                start_unit_ms: 1,
                reset_unit_ms: 1,
                reset_is_absolute_epoch: false,
            }],
            error_envelope: Some(env),
        }
    }

    #[test]
    fn build_client_succeeds() {
        assert!(build_client("llm-quota-tray").is_ok());
    }

    #[test]
    fn parse_error_propagates() {
        // Build a shape pointing at a non-`/` entries path so
        // missing/empty entry objects produce errors (rather than
        // the parser silently reading zeros from the root).
        let mut shape = shape_with_envelope(ErrorEnvelope {
            code_path: "/base_resp/status_code".to_string(),
            message_path: "/base_resp/status_msg".to_string(),
            success_codes: vec![0],
        });
        shape.entries_path = "/model_remains".to_string();
        let v: Value = serde_json::from_str("{}").unwrap();
        assert!(parse_plan(&v, &shape, 0).is_err());
    }

    #[test]
    fn empty_entry_returns_parse_error_not_panic() {
        let v = json!({"model_remains": []});
        let mut shape = shape_with_envelope(ErrorEnvelope {
            code_path: "/x".to_string(),
            message_path: "/y".to_string(),
            success_codes: vec![0],
        });
        shape.entries_path = "/model_remains".to_string();
        assert!(parse_plan(&v, &shape, 0).is_err());
    }

    #[test]
    fn envelope_reports_non_success_code() {
        // MiniMax-style envelope: code != success_codes → error.
        let v = json!({
            "base_resp": {"status_code": 1004, "status_msg": "login fail: bad key"}
        });
        let env = ErrorEnvelope {
            code_path: "/base_resp/status_code".to_string(),
            message_path: "/base_resp/status_msg".to_string(),
            success_codes: vec![0],
        };
        let err = super::envelope_error(&v, Some(&env));
        assert_eq!(err.as_deref(), Some("API error 1004: login fail: bad key"));
    }

    #[test]
    fn envelope_zero_code_is_not_an_error() {
        let v = json!({"base_resp": {"status_code": 0, "status_msg": "ok"}});
        let env = ErrorEnvelope {
            code_path: "/base_resp/status_code".to_string(),
            message_path: "/base_resp/status_msg".to_string(),
            success_codes: vec![0],
        };
        let err = super::envelope_error(&v, Some(&env));
        assert_eq!(err, None);
    }

    #[test]
    fn missing_envelope_path_returns_none() {
        // Body has no /base_resp — envelope_error treats it as success.
        let v = json!({"model_remains": []});
        let env = ErrorEnvelope {
            code_path: "/base_resp/status_code".to_string(),
            message_path: "/base_resp/status_msg".to_string(),
            success_codes: vec![0],
        };
        let err = super::envelope_error(&v, Some(&env));
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

    // ---- AuthConfig dispatch tests ----

    #[test]
    fn auth_bearer() {
        let cfg = AuthConfig::Bearer;
        let (name, value) = cfg.build("sk-test");
        assert_eq!(name, "Authorization");
        assert_eq!(value, "Bearer sk-test");
    }

    #[test]
    fn auth_header_custom_name() {
        let cfg = AuthConfig::Header { name: "x-api-key".to_string() };
        let (name, value) = cfg.build("sk-test");
        assert_eq!(name, "x-api-key");
        assert_eq!(value, "sk-test");
    }

    #[test]
    fn auth_custom_format() {
        let cfg = AuthConfig::Custom {
            name: "Authorization".to_string(),
            format: "Token {key}".to_string(),
        };
        let (name, value) = cfg.build("sk-test");
        assert_eq!(name, "Authorization");
        assert_eq!(value, "Token sk-test");
    }

    #[test]
    fn auth_query_param_appends_to_endpoint() {
        let cfg = AuthConfig::QueryParam { name: "key".to_string() };
        let (name, _) = cfg.build("sk-test");
        assert_eq!(name, "");  // no header
        let modified = cfg.apply_to_endpoint("https://api.example.com/v1/usage", "sk-test");
        assert_eq!(modified, "https://api.example.com/v1/usage?key=sk-test");
    }

    #[test]
    fn auth_query_param_preserves_existing_query() {
        let cfg = AuthConfig::QueryParam { name: "key".to_string() };
        let modified = cfg.apply_to_endpoint("https://api.example.com/v1/usage?foo=bar", "sk-test");
        assert_eq!(modified, "https://api.example.com/v1/usage?foo=bar&key=sk-test");
    }

    #[test]
    fn auth_default_is_bearer() {
        assert!(matches!(AuthConfig::default(), AuthConfig::Bearer));
    }
}
