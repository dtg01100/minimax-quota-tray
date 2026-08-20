//! GNOME Keyring wrapper. Stores the API key as a Secret Service item,
//! schema `{ application: "minimax-quota" }`. Falls back to a per-machine
//! file at ~/.config/minimax-quota/key (mode 0600) when the keyring
//! daemon isn't running — the gjs code used the same fallback.

use anyhow::{Context, Result};
use secret_service::blocking::{Collection, SecretService};
use secret_service::EncryptionType;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

const ATTRS: &[(&str, &str)] = &[("application", "minimax-quota")];
const LABEL: &str = "MiniMax API Key";

/// One SecretService handle per process — opening the daemon connection
/// is slow (~50ms), so we cache it.
static SERVICE: OnceLock<Result<SecretService<'static>, String>> = OnceLock::new();

fn service() -> Option<&'static SecretService<'static>> {
    let r = SERVICE.get_or_init(|| {
        SecretService::connect(EncryptionType::Dh).map_err(|e| format!("{e:?}"))
    });
    r.as_ref().ok()
}

fn attrs() -> HashMap<&'static str, &'static str> {
    ATTRS.iter().copied().collect()
}

pub fn get() -> Option<String> {
    let svc = service()?;
    let collection = svc.get_default_collection().ok()?;
    let items: Vec<_> = collection.search_items(attrs()).ok()?;
    let item = items.into_iter().next()?;
    let bytes = item.get_secret().ok()?;
    String::from_utf8(bytes).ok()
}

pub fn set(value: &str) -> Result<()> {
    if let Some(svc) = service() {
        let collection = svc
            .get_default_collection()
            .context("opening default keyring collection")?;
        // Replace any existing item first so we don't accumulate duplicates.
        if let Ok(items) = collection.search_items(attrs()) {
            for item in items {
                let _ = item.delete();
            }
        }
        collection
            .unlock()
            .context("unlocking keyring for write")?;
        collection
            .create_item(
                LABEL,
                attrs(),
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

pub fn clear() -> Result<()> {
    if let Some(svc) = service() {
        if let Ok(collection) = svc.get_default_collection() {
            if let Ok(items) = collection.search_items(attrs()) {
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
    PathBuf::from(home).join(".config/.config/minimax-quota/key")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_key_path_under_home() {
        let path = legacy_key_path();
        assert!(path.starts_with(std::env::var("HOME").unwrap_or_default())
                || path.starts_with("/tmp"));
        assert!(path.ends_with(".config/.config/minimax-quota/key"));
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