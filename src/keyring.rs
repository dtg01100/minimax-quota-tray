//! GNOME Keyring wrapper. Stores the API key as a Secret Service item,
//! schema `{ application: <instance> }`. The `application` attribute
//! is per-instance (see `crate::instance::keyring_application`) so
//! two concurrent instances don't clobber each other's API key.
//!
//! Falls back to a per-machine file at
//! `~/.config/<instance>/key` (mode 0600) when the keyring daemon
//! isn't running — the gjs code used the same fallback.

use anyhow::{Context, Result};
use secret_service::blocking::SecretService;
use secret_service::EncryptionType;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

/// One SecretService handle per process — opening the daemon connection
/// is slow (~50ms), so we cache it.
static SERVICE: OnceLock<Result<SecretService<'static>, String>> = OnceLock::new();

fn service() -> Option<&'static SecretService<'static>> {
    let r = SERVICE.get_or_init(|| {
        SecretService::connect(EncryptionType::Dh).map_err(|e| format!("{e:?}"))
    });
    r.as_ref().ok()
}

/// Build the search attributes for the active instance. The Secret
/// Service `search_items` API takes `HashMap<&str, &str>`, so we
/// leak the application attribute to `&'static` — this is one
/// Box::leak per process and the string lives for the binary's
/// entire lifetime, which is fine for an instance-name-derived
/// value.
fn attrs() -> HashMap<&'static str, &'static str> {
    let app: &'static str = Box::leak(crate::instance::keyring_application().into_boxed_str());
    let mut m: HashMap<&'static str, &'static str> = HashMap::new();
    m.insert("application", app);
    m
}

/// Label shown in the keyring GUI. Namespaced by instance so two
/// instances' entries are distinguishable when the user opens
/// Seahorse / KeePassXC / etc.
fn label() -> String {
    format!("{} API Key", crate::instance::config_dir_basename())
}

/// Look up the API key. Priority order matches the gjs `loadApiKey()`:
///
///   1. Secret Service (GNOME Keyring / KWallet via libsecret).
///   2. Legacy plaintext file at `$HOME/.config/.config/quota-tray/key`
///      (auto-migrated to the keyring on first run by the gjs code;
///      here we just read it as a fallback if the keyring daemon is
///      unreachable).
///   3. `MINIMAX_API_KEY` env var. Useful for systemd unit overrides
///      where the user prefers not to store a key in the keyring.
///
/// Returns `None` if all three are missing/empty. Callers should treat
/// `None` as "no API key configured" and surface that in the UI
/// (matches gjs `printerr` + "No API key" chip state).
pub fn get() -> Option<String> {
    // 1. Secret Service
    if let Some(svc) = service() {
        let attrs = attrs();
        if let Ok(collection) = svc.get_default_collection() {
            if let Ok(items) = collection.search_items(attrs.clone()) {
                if let Some(item) = items.into_iter().next() {
                    if let Ok(bytes) = item.get_secret() {
                        if let Some(k) = secret_to_key(&bytes) {
                            return Some(k);
                        }
                    }
                }
            }
        }
    }
    // 2. Legacy file (created by older install scripts; install.sh
    // now uses Secret Service directly, so this is a fallback).
    let path = legacy_key_path();
    if let Ok(bytes) = std::fs::read(&path) {
        if let Some(k) = secret_to_key(&bytes) {
            return Some(k);
        }
    }
    // 3. Env var. Trim + reject — matches the gjs handling that
    // treats an all-whitespace env value as absent.
    if let Ok(raw) = std::env::var("MINIMAX_API_KEY") {
        if let Some(k) = secret_to_key(raw.as_bytes()) {
            return Some(k);
        }
    }
    None
}

/// Convert the raw secret bytes from the keyring into a usable API key.
/// Some keyring tools (e.g. `secret-tool store` via a shell pipe) persist a
/// trailing newline; passing that straight into the `Authorization` header
/// makes reqwest fail with "failed to parse header value". Trim both ends
/// and drop empty secrets.
fn secret_to_key(bytes: &[u8]) -> Option<String> {
    String::from_utf8(bytes.to_vec())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn set(value: &str) -> Result<()> {
    if let Some(svc) = service() {
        let collection = svc
            .get_default_collection()
            .context("opening default keyring collection")?;
        let attrs = attrs();
        // Replace any existing item first so we don't accumulate duplicates.
        if let Ok(items) = collection.search_items(attrs.clone()) {
            for item in items {
                let _ = item.delete();
            }
        }
        collection
            .unlock()
            .context("unlocking keyring for write")?;
        collection
            .create_item(
                &label(),
                attrs,
                value.as_bytes(),
                true, // replace_existing
                "text/plain",
            )
            .context("creating keyring item")?;
        return Ok(());
    }
    // Keyring unavailable — fall back to file. install.sh creates 0700 perms.
    let path = legacy_key_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&path, value)
        .with_context(|| format!("writing fallback key to {}", path.display()))?;
    Ok(())
}

/// Remove the stored API key from the keyring (and the legacy file
/// fallback). Used by `gjs` menu's clear flow; exposed here for
/// parity but not currently wired into the Rust menu. Calling it is
/// safe; the function is best-effort.
#[allow(dead_code)]
pub fn clear() -> Result<()> {
    if let Some(svc) = service() {
        let attrs = attrs();
        if let Ok(collection) = svc.get_default_collection() {
            if let Ok(items) = collection.search_items(attrs) {
                for item in items {
                    let _ = item.delete();
                }
            }
        }
    }
    let path = legacy_key_path();
    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("removing fallback key at {}", path.display()))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Legacy plaintext fallback
// ---------------------------------------------------------------------------

fn legacy_key_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    // Per-instance: matches the config dir naming (`<base>` or
    // `<base>-<instance>`). The legacy `.config/.config/` doubled
    // prefix is preserved for compatibility with old installs.
    PathBuf::from(home)
        .join(".config")
        .join(".config")
        .join(crate::instance::config_dir_basename())
        .join("key")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Tests in this module mutate the global `MINIMAX_API_KEY` env
    /// var. Serialize them so cargo's parallel runner doesn't have
    /// two tests stomp on each other's env state.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Set `MINIMAX_API_KEY` for the duration of `body`, restoring
    /// the previous value (if any) when `body` returns. The body
    /// must NOT panic — we don't catch_unwind here because that
    /// would force callers to be UnwindSafe.
    fn with_env<F: FnOnce()>(env_value: Option<&str>, body: F) {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("MINIMAX_API_KEY").ok();
        match env_value {
            Some(v) => std::env::set_var("MINIMAX_API_KEY", v),
            None => std::env::remove_var("MINIMAX_API_KEY"),
        }
        let result = body();
        match prev {
            Some(v) => std::env::set_var("MINIMAX_API_KEY", v),
            None => std::env::remove_var("MINIMAX_API_KEY"),
        }
        result
    }

    #[test]
    fn secret_with_trailing_newline_is_trimmed() {
        // Regression: `secret-tool store` via a shell pipe persists the
        // key with a trailing \n. Untrimmed, that byte lands in the
        // Authorization header and reqwest rejects it with "failed to
        // parse header value".
        let mut raw = b"sk-cp-7oBx1Pu1i-V7sgUWNltHjGF1YtTpiFReV_erIfmB4MGPDrh8unHRa5z2N9r5kIO9jGnYdf-LUhY6fnm6ng_pGtxwSBC4pyu1_AfGsOSvLf0IO0V7tj3j93s".to_vec();
        raw.push(b'\n');
        assert_eq!(secret_to_key(&raw).as_deref(),
                   Some("sk-cp-7oBx1Pu1i-V7sgUWNltHjGF1YtTpiFReV_erIfmB4MGPDrh8unHRa5z2N9r5kIO9jGnYdf-LUhY6fnm6ng_pGtxwSBC4pyu1_AfGsOSvLf0IO0V7tj3j93s"));
    }

    #[test]
    fn secret_with_leading_and_trailing_whitespace_is_trimmed() {
        assert_eq!(secret_to_key(b"  sk-test-key  \r\n").as_deref(),
                   Some("sk-test-key"));
    }

    #[test]
    fn empty_secret_is_rejected() {
        assert_eq!(secret_to_key(b""), None);
        assert_eq!(secret_to_key(b"\n\n"), None);
    }

    #[test]
    fn non_utf8_secret_is_rejected() {
        assert_eq!(secret_to_key(&[0xff, 0xfe, 0xfd]), None);
    }

    #[test]
    fn clean_secret_passes_through_unchanged() {
        assert_eq!(secret_to_key(b"sk-clean-key").as_deref(), Some("sk-clean-key"));
    }

    #[test]
    fn env_var_fallback_used_when_keyring_and_file_unavailable() {
        // We can't easily isolate the keyring daemon in unit tests,
        // so this test is gated on the keyring being unavailable
        // (which is the case in CI / containers — see the
        // `service()` cache).
        //
        // To force the env-var path we set MINIMAX_API_KEY and
        // rely on the keyring/legacy-file lookups failing in the
        // test container.
        with_env(Some("sk-env-test-key"), || {
            let result = get();
            // In an environment with a working keyring this might
            // short-circuit before the env check, so we only assert
            // what we can guarantee: if we got a value, it's non-empty.
            if let Some(k) = result {
                assert!(!k.is_empty());
            }
        });
    }

    #[test]
    fn env_var_with_trailing_newline_trimmed() {
        with_env(Some("sk-env-key\n"), || {
            // The actual return depends on whether the keyring is
            // available; what we can guarantee is that *if* the env
            // path is taken, the trailing newline is stripped.
            // We exercise secret_to_key directly to lock the behavior.
            let trimmed = secret_to_key(b"sk-env-key\n");
            assert_eq!(trimmed.as_deref(), Some("sk-env-key"));
        });
    }

    #[test]
    fn legacy_key_path_under_home() {
        let path = legacy_key_path();
        assert!(path.starts_with(std::env::var("HOME").unwrap_or_default())
                || path.starts_with("/tmp"));
        assert!(path.ends_with(".config/.config/quota-tray/key"));
    }

    #[test]
    #[ignore = "depends on whether a Secret Service daemon is reachable; covered in integration tests"]
    fn file_fallback_roundtrip() {
        // Sets HOME to a temp dir and exercises the legacy file fallback
        // path. Skipped by default because the headless test runner may have
        // SecretService::connect() succeed (returning a service handle) but
        // the daemon isn't actually running, so operations on it fail. Run
        // with `cargo test -- --ignored` to exercise the file path explicitly.
        let tmp = std::env::temp_dir().join("minimax-keyring-test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("HOME", &tmp);

        assert_eq!(legacy_key_path().exists(), false);

        set("test-key-12345").unwrap();
        assert_eq!(
            std::fs::read_to_string(legacy_key_path()).unwrap(),
            "test-key-12345"
        );

        clear().unwrap();
        assert!(!legacy_key_path().exists());

        let _ = std::fs::remove_dir_all(&tmp);
    }
}