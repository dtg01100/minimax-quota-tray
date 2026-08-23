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
    let user_agent = format!("{}/{}", user_agent_prefix, env!("CARGO_PKG_VERSION"));
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
/// are kept (gjs's `slice(0, 80)`); **any line** containing one of
/// the trigger words (`bearer`, `authorization`, `api-key`,
/// `api_key`, `token`) is replaced with `[redacted line]`, so the
/// pattern doesn't need to predict the separator (`:`, `=`, `-`,
/// etc.) that follows the trigger.
///
/// Whole-line redaction is intentional and conservative: a partial
/// redact like `Bearer ***` would still leak the *shape* of the
/// credential and miss keys that share the prefix
/// (e.g. `X-Bearer-Auth: foo`). Redacting the whole line tells the
/// user "a credential was here" without exposing the value or its
/// structure.
pub fn sanitize_error_snippet(raw: &str) -> String {
    let truncated: String = raw.chars().take(80).collect();
    redact_credential_lines(&truncated)
}

/// Patterns that, when found in a line, trigger whole-line redaction.
/// Lowercase comparison — `Authorization` and `authorization` both
/// match. `token` is included so providers that echo a `token=...`
/// in error bodies (some OAuth flows, e.g. Anthropic's
/// `x-auth-token` family) are caught.
const TRIGGER_PATTERNS: &[&str] = &["bearer", "authorization", "api-key", "api_key", "token"];

/// O(n) per-line scan. The truncated snippet is at most 80 chars
/// and rarely contains more than one or two newlines, so this
/// avoids the `regex` crate's footprint.
fn redact_credential_lines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let lower = s.to_ascii_lowercase();
    let mut line_start = 0usize;
    for (i, ch) in s.char_indices() {
        // Newline ends a line -- flush the accumulated line, push
        // the newline so multi-line error bodies (JSON, stack
        // traces) stay readable, then reset for the next one.
        // char_indices advances past multi-byte chars safely.
        if ch == '\n' {
            flush_line(&mut out, &s[line_start..i], &lower[line_start..i]);
            out.push('\n');
            line_start = i + ch.len_utf8();
        }
    }
    // Tail after the last newline (no trailing newline).
    flush_line(&mut out, &s[line_start..], &lower[line_start..]);
    out
}
fn flush_line(out: &mut String, line: &str, lower: &str) {
    if lower.contains(TRIGGER_PATTERNS[0])
        || lower.contains(TRIGGER_PATTERNS[1])
        || lower.contains(TRIGGER_PATTERNS[2])
        || lower.contains(TRIGGER_PATTERNS[3])
        || lower.contains(TRIGGER_PATTERNS[4])
    {
        out.push_str("[redacted line]");
    } else {
        out.push_str(line);
    }
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
fn envelope_error(
    body: &Value,
    envelope: Option<&crate::provider::ErrorEnvelope>,
) -> Option<String> {
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
                count_unit: None,
                currency: None,
                pricing_model_path: None,
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
        let cfg = AuthConfig::Header {
            name: "x-api-key".to_string(),
        };
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
        let cfg = AuthConfig::QueryParam {
            name: "key".to_string(),
        };
        let (name, _) = cfg.build("sk-test");
        assert_eq!(name, ""); // no header
        let modified = cfg.apply_to_endpoint("https://api.example.com/v1/usage", "sk-test");
        assert_eq!(modified, "https://api.example.com/v1/usage?key=sk-test");
    }

    #[test]
    fn auth_query_param_preserves_existing_query() {
        let cfg = AuthConfig::QueryParam {
            name: "key".to_string(),
        };
        let modified = cfg.apply_to_endpoint("https://api.example.com/v1/usage?foo=bar", "sk-test");
        assert_eq!(
            modified,
            "https://api.example.com/v1/usage?foo=bar&key=sk-test"
        );
    }

    #[test]
    fn auth_query_param_encodes_special_chars_in_key() {
        let cfg = AuthConfig::QueryParam {
            name: "key".to_string(),
        };
        // A key with `&` and `=` must be encoded so it doesn't corrupt
        // the URL query string.
        let modified = cfg.apply_to_endpoint(
            "https://api.example.com/v1/usage",
            "sk-abc&def=ghi",
        );
        assert_eq!(
            modified,
            "https://api.example.com/v1/usage?key=sk-abc%26def%3Dghi"
        );
    }

    #[test]
    fn auth_default_is_bearer() {
        assert!(matches!(AuthConfig::default(), AuthConfig::Bearer));
    }

    // ---- sanitize_error_snippet ----
    //
    // This is the last line of defense for credential redaction --
    // it runs on the body of every HTTP error before the error
    // reaches the menu's error row or journald. A regression that
    // let a credential slip through would surface in user-visible
    // UI, not in a test failure.

    #[test]
    fn sanitize_truncates_long_bodies() {
        // 200-char body → truncated to 80 chars
        let body = "x".repeat(200);
        let sanitized = sanitize_error_snippet(&body);
        assert_eq!(
            sanitized.chars().count(),
            80,
            "snippet must be exactly 80 chars"
        );
    }

    #[test]
    fn sanitize_passes_through_safe_text() {
        // No credential patterns → unchanged (modulo truncation)
        let body = "HTTP 404: endpoint not found";
        assert_eq!(sanitize_error_snippet(body), body);
    }

    #[test]
    fn sanitize_redacts_bearer_lines() {
        // The trigger word `bearer` (case-insensitive) redacts the
        // entire line. We don't test the exact replacement text --
        // we test that the credential-bearing substring is gone.
        let body = "Authorization: Bearer sk-abc123-secret";
        let sanitized = sanitize_error_snippet(body);
        assert!(
            !sanitized.contains("sk-abc123"),
            "credential value must not appear in sanitized output; got {sanitized:?}"
        );
        assert!(
            sanitized.contains("[redacted line]"),
            "redacted-line marker must replace the credential line; got {sanitized:?}"
        );
    }

    #[test]
    fn sanitize_redacts_authorization_lines() {
        let body = "X-Auth: authorization: sk-test";
        let sanitized = sanitize_error_snippet(body);
        assert!(!sanitized.contains("sk-test"));
        assert!(sanitized.contains("[redacted line]"));
    }

    #[test]
    fn sanitize_redacts_api_key_lines() {
        // Both spellings: "api-key" (hyphen) and "api_key" (underscore)
        let body1 = "api-key: abc-def-ghi";
        let body2 = "x-api_key=secret123";
        for body in [body1, body2] {
            let sanitized = sanitize_error_snippet(body);
            assert!(
                !sanitized.contains("abc-def") && !sanitized.contains("secret123"),
                "api-key/api_key lines must be redacted; got {sanitized:?}"
            );
        }
    }

    #[test]
    fn sanitize_redacts_token_lines() {
        let body = "x-auth-token: tok_abc_def_ghi";
        let sanitized = sanitize_error_snippet(body);
        assert!(!sanitized.contains("tok_abc"));
        assert!(sanitized.contains("[redacted line]"));
    }

    #[test]
    fn sanitize_redaction_is_case_insensitive() {
        // The trigger words match in any case. The previous
        // implementation only matched lowercase -- a regression
        // would surface as a leaked "Bearer" line in the menu.
        let body = "BEARER sk-upper";
        let sanitized = sanitize_error_snippet(body);
        assert!(!sanitized.contains("sk-upper"));
    }

    #[test]
    fn sanitize_preserves_lines_without_triggers() {
        // Multi-line body: lines without trigger words must pass
        // through unchanged. A regression that over-redacted (e.g.
        // redacting any line containing a colon) would break this.
        let body = "HTTP 503\nService Unavailable\nPlease retry later";
        let sanitized = sanitize_error_snippet(body);
        assert_eq!(sanitized, body);
    }

    #[test]
    fn sanitize_handles_mixed_lines() {
        // Mixed: one credential line + two safe lines. Only the
        // credential line should be redacted.
        let body = "HTTP 401\nAuthorization: Bearer sk-abc\nPlease authenticate";
        let sanitized = sanitize_error_snippet(body);
        assert!(!sanitized.contains("sk-abc"));
        assert!(sanitized.contains("HTTP 401"));
        assert!(sanitized.contains("Please authenticate"));
    }

    #[test]
    fn sanitize_handles_empty_body() {
        // Empty body must not panic (some providers return empty
        // error bodies on network failures).
        let sanitized = sanitize_error_snippet("");
        assert_eq!(sanitized, "");
    }

    #[test]
    fn sanitize_preserves_unicode_in_safe_text() {
        // Unicode must survive truncation -- byte-length 80 !=
        // char-length 80. A regression that used byte slicing
        // would split multi-byte chars and produce invalid UTF-8.
        let body = "é".repeat(80); // 240 bytes, 80 chars
        let sanitized = sanitize_error_snippet(&body);
        assert_eq!(sanitized.chars().count(), 80);
        assert!(sanitized.chars().all(|c| c == 'é'));
    }

    #[test]
    fn sanitize_redacts_partial_word_matches() {
        // The trigger words must match as substrings, not whole
        // words. "x-bearer-foo: ..." should still be redacted
        // because "bearer" appears in the line. (The gjs parity
        // decision: substring match catches more variants than
        // whole-word.)
        let body = "x-bearer-token: sk-test";
        let sanitized = sanitize_error_snippet(body);
        assert!(!sanitized.contains("sk-test"));
    }
}
