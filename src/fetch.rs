//! HTTPS fetch of the MiniMax quota endpoint + parse into Window shape.
//! Reqwest with rustls-tls (pure Rust TLS, no OpenSSL/GnuTLS).
//!
//! The HTTP call is blocking (reqwest blocking) and dispatched on a
//! background thread so the GLib main loop never stalls. The result is
//! delivered back to the main thread via `glib::idle_add_once`.

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

/// Synchronous fetch — blocks the calling thread. Use with `dispatch` to
/// keep it off the GLib main thread.
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

/// Run a blocking fetch on a background thread, then call `on_done` on the
/// GLib main thread. The on_done closure runs in the main loop's idle slot.
///
/// `on_done` does NOT need to be Send — it's called from the main thread
/// via `glib::idle_add_once`. The background thread captures only the HTTP
/// inputs (all Send) and hands the result off.
pub fn dispatch<F>(
    client: Client,
    endpoint: String,
    api_key: String,
    on_done: F,
) where
    F: FnOnce(Result<(Window, Window)>) + Send + 'static,
{
    std::thread::Builder::new()
        .name("minimax-fetch".into())
        .spawn(move || {
            let result = fetch_windows_blocking(&client, &endpoint, &api_key);
            glib::idle_add_once(move || on_done(result));
        })
        .expect("spawn fetch thread");
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The fetch layer is mostly the parse layer + an HTTP client builder;
    /// full end-to-end coverage lives in tests/integration.rs (against a
    /// local httpmock). Here we exercise the cheap paths.

    #[test]
    fn build_client_succeeds() {
        assert!(build_client().is_ok());
    }

    #[test]
    fn parse_error_propagates_from_blocking_fetch_signature() {
        // We can't easily run the blocking fetch without a server, but we
        // can verify the parse layer is what would surface as the error.
        let v: Value = serde_json::from_str("{}").unwrap();
        assert!(parse_coding_plan(&v).is_err());
    }

    #[test]
    fn empty_payload_returns_parse_error_not_panic() {
        let v = json!({"model_remains": []});
        assert!(parse_coding_plan(&v).is_err());
    }
}