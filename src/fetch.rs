//! HTTPS fetch of the MiniMax quota endpoint + parse into Window shape.
//! Reqwest with rustls-tls (pure Rust TLS, no OpenSSL/GnuTLS).
//!
//! The HTTP call is blocking (reqwest blocking) and intended to be wrapped
//! in `tokio::task::spawn_blocking` from the polling loop.

use anyhow::{Context, Result};
use reqwest::blocking::Client;
use serde_json::Value;

use crate::burn::Window;
use crate::parse::parse_coding_plan;

// Re-export so callers can refer to `fetch::Client` without importing reqwest.
pub use reqwest::blocking::Client as HttpClient;

const USER_AGENT: &str = concat!(
    "minimax-quota-tray/",
    env!("CARGO_PKG_VERSION"),
);

/// Build a shared blocking reqwest client. TLS via rustls.
pub fn build_client() -> Result<Client> {
    Client::builder()
        .user_agent(USER_AGENT)
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
) -> Result<(Window, Window)> {
    let resp = client
        .get(endpoint)
        .bearer_auth(api_key)
        .send()
        .context("HTTP request")?;

    let status = resp.status();
    if !status.is_success() {
        return Err(anyhow::anyhow!("HTTP {status}"));
    }

    let body: Value = resp.json().context("decoding JSON body")?;
    parse_coding_plan(&body).context("parsing payload")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn build_client_succeeds() {
        assert!(build_client().is_ok());
    }

    #[test]
    fn parse_error_propagates() {
        let v: Value = serde_json::from_str("{}").unwrap();
        assert!(parse_coding_plan(&v).is_err());
    }

    #[test]
    fn empty_model_remains_returns_parse_error_not_panic() {
        let v = json!({"model_remains": []});
        assert!(parse_coding_plan(&v).is_err());
    }
}