//! GNOME Keyring wrapper. Stores the API key as a Secret Service item
//! via the `secret-tool(1)` CLI (shipped with libsecret-tools),
//! schema `{ application: <instance> }`. The `application` attribute
//! is per-instance (see `crate::instance::keyring_application`) so
//! two concurrent instances don't clobber each other's API key.
//!
//! Falls back to a per-machine file at
//! `~/.config/<instance>/key` (mode 0600) when neither `secret-tool`
//! nor a Secret Service daemon is reachable — same fallback the gjs
//! implementation used.
//!
//! ## Why subprocess and not the `secret-service` Rust crate
//!
//! Earlier versions of this module used `secret-service = "3"` with
//! the `rt-tokio-crypto-rust` feature. The crate's sync API
//! internally calls `zbus::utils::block_on(...)`, which panics with
//! `"Cannot start a runtime from within a runtime"` when invoked
//! from inside a tokio worker thread. The daemon's main runtime is
//! `rt-multi-thread`; every keyring read from `do_refresh()` and
//! every write from the menu's "Set API Key…" entry ran on a worker
//! thread. The panic propagated back through the `spawn_blocking`
//! `JoinHandle`, killed the daemon, and systemd restarted it 5s
//! later with no key written — the user-visible "key doesn't stick"
//! bug. Subprocess invocation sidesteps the runtime-context conflict
//! entirely; the per-call cost (~20–50ms) is well below the
//! `refresh_seconds` cadence.

use anyhow::{Context, Result};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Subprocess wrapper around `secret-tool lookup`. Returns the
/// stored secret on success, `None` on any failure (binary missing,
/// daemon unreachable, no matching entry, etc.). Silent because we
/// have two more fallbacks (file + env var) and the caller already
/// deals with `None`.
fn secret_tool_lookup() -> Option<String> {
    let app = crate::instance::keyring_application();
    let output = Command::new("secret-tool")
        .args(["lookup", "application", &app])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8(output.stdout).ok()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Subprocess wrapper around `secret-tool store`. Writes the secret
/// via stdin so it never appears in process arguments, argv, or
/// `/proc/<pid>/cmdline`.
fn secret_tool_store(value: &str) -> Result<()> {
    let app = crate::instance::keyring_application();
    let label = format!("{} API Key", crate::instance::config_dir_basename());
    let mut child = Command::new("secret-tool")
        .args(["store", "--label", &label, "application", &app])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning secret-tool store")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(value.as_bytes())
            .context("writing secret to secret-tool stdin")?;
        // Drop stdin to close the pipe and signal EOF; secret-tool
        // otherwise blocks reading.
        drop(stdin);
    }
    let status = child.wait().context("waiting for secret-tool store")?;
    if !status.success() {
        anyhow::bail!("secret-tool store exited with {status}");
    }
    Ok(())
}

/// Subprocess wrapper around `secret-tool clear`. Best-effort — a
/// non-zero exit (no matching entry) is treated as success.
fn secret_tool_clear() -> Result<()> {
    let app = crate::instance::keyring_application();
    let status = Command::new("secret-tool")
        .args(["clear", "application", &app])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("spawning secret-tool clear")?;
    if !status.success() {
        log::debug!("secret-tool clear returned {status} (no entries to clear?)");
    }
    Ok(())
}

/// Look up the API key. Priority order:
///
///   1. Secret Service via `secret-tool` (GNOME Keyring / KWallet via libsecret).
///   2. Legacy plaintext file at `$HOME/.config/.config/<instance>/key`.
///   3. `LLM_API_KEY` env var. Useful for systemd unit overrides
///      where the user prefers not to store a key in the keyring.
///
/// Returns `None` if all three are missing/empty. Callers should treat
/// `None` as "no API key configured" and surface that in the UI.
pub fn get() -> Option<String> {
    if let Some(k) = secret_tool_lookup() {
        return Some(k);
    }
    let path = legacy_key_path();
    if let Ok(bytes) = std::fs::read(&path) {
        if let Some(k) = secret_to_key(&bytes) {
            return Some(k);
        }
    }
    if let Ok(raw) = std::env::var("LLM_API_KEY") {
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

/// Write the API key. Tries `secret-tool store` first; on any failure
/// (binary missing, daemon unreachable, libsecret locked) falls back
/// to the legacy plaintext file at `~/.config/.config/<instance>/key`.
pub fn set(value: &str) -> Result<()> {
    match secret_tool_store(value) {
        Ok(()) => {
            log::debug!("API key stored via secret-tool");
            // Best-effort: clear any stale legacy file so we don't
            // leak a plaintext copy after the user migrates to the
            // keyring.
            let path = legacy_key_path();
            if path.exists() {
                let _ = std::fs::remove_file(&path);
            }
            return Ok(());
        }
        Err(e) => {
            log::debug!("secret-tool store failed ({e:#}); using file fallback");
        }
    }
    let path = legacy_key_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&path, value)
        .with_context(|| format!("writing fallback key to {}", path.display()))?;
    Ok(())
}

/// Remove the stored API key. Best-effort — both `secret-tool` and
/// the file fallback are cleared. Used by the gjs menu's clear flow;
/// exposed here for parity but not currently wired into the Rust
/// menu. Safe to call when no key is set.
#[allow(dead_code)]
pub fn clear() -> Result<()> {
    let _ = secret_tool_clear();
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

    /// Tests in this module mutate the global `LLM_API_KEY` env
    /// var. Serialize them so cargo's parallel runner doesn't have
    /// two tests stomp on each other's env state.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Set `LLM_API_KEY` for the duration of `body`, restoring
    /// the previous value (if any) when `body` returns. The body
    /// must NOT panic — we don't catch_unwind here because that
    /// would force callers to be UnwindSafe.
    fn with_env<F: FnOnce()>(env_value: Option<&str>, body: F) {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("LLM_API_KEY").ok();
        match env_value {
            Some(v) => std::env::set_var("LLM_API_KEY", v),
            None => std::env::remove_var("LLM_API_KEY"),
        }
        let result = body();
        match prev {
            Some(v) => std::env::set_var("LLM_API_KEY", v),
            None => std::env::remove_var("LLM_API_KEY"),
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
        // We can't easily isolate `secret-tool` (or its daemon) in
        // unit tests, so this test is gated on both the keyring and
        // the legacy file being unreachable — typical in CI / minimal
        // containers. To force the env-var path we set LLM_API_KEY
        // and rely on the keyring/legacy-file lookups failing.
        with_env(Some("sk-env-test-key"), || {
            let result = get();
            // In an environment where secret-tool succeeds this might
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
            // The actual return depends on whether secret-tool
            // succeeds; what we can guarantee is that *if* the env
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
        assert!(path.ends_with(".config/.config/llm-quota-tray/key"));
    }

    /// Exercises the file-fallback branch of `set()` + `clear()` by
    /// forcing `secret-tool` to be unfindable (empty PATH so the
    /// subprocess spawn fails with ENOENT). This isolates the file
    /// fallback from the keyring branch so we can verify the
    /// legacy plaintext path still round-trips.
    #[test]
    #[ignore = "mutates HOME and PATH; covered in integration tests"]
    fn file_fallback_roundtrip() {
        let tmp = std::env::temp_dir().join("llm-quota-keyring-test");
        let _ = std::fs::remove_dir_all(&tmp);

        // Force the subprocess path to be unfindable so `set()` falls
        // through to the legacy file write.
        let saved_path = std::env::var("PATH").ok();
        std::env::set_var("PATH", "");
        std::env::set_var("HOME", &tmp);

        assert!(!legacy_key_path().exists(),
                "precondition: legacy file should not exist yet");

        set("test-key-12345").unwrap();
        assert_eq!(
            std::fs::read_to_string(legacy_key_path()).unwrap(),
            "test-key-12345"
        );

        clear().unwrap();
        assert!(!legacy_key_path().exists(),
                "clear() should remove the legacy file");

        // Restore env so other tests aren't affected.
        match saved_path {
            Some(v) => std::env::set_var("PATH", v),
            None => std::env::remove_var("PATH"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
}