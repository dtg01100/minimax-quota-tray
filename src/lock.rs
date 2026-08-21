//! Single-instance PID lock at `$XDG_RUNTIME_DIR/<basename>.pid`,
//! where `<basename>` is the per-instance name from
//! `crate::instance` (e.g. `quota-tray` for the default instance,
//! `quota-tray-codex` for `--instance=codex`).
//!
//! Two concurrent instances have different lock paths so they don't
//! conflict — each has its own `pid` file in its own slot.
//!
//! gjs parity: matches `acquireSingleInstanceLock()` /
//! `releaseSingleInstanceLock()`. Uses O_EXCL semantics for
//! first-time creation (atomic check-then-write) and takes over
//! stale locks whose owner process has died.
//!
//! On platforms without `/proc` (rare — gjs handles it gracefully
//! too), the lock is best-effort: we'd rather run two instances
//! than refuse to start.

use anyhow::{Context, Result};
use std::path::PathBuf;

/// Acquired lock state; holds the path so we can release on exit.
pub struct Lock {
    path: PathBuf,
}

impl Lock {
    /// Try to acquire the lock. Returns `Ok(Some(Lock))` on success,
    /// `Ok(None)` if another live instance holds it, `Err` if the
    /// lock file is malformed (e.g. non-numeric contents).
    pub fn acquire() -> Result<Option<Self>> {
        let path = crate::instance::lock_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let pid_str = std::process::id().to_string();

        // Try O_EXCL create first.
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                use std::io::Write;
                let _ = f.write_all(pid_str.as_bytes());
                return Ok(Some(Self { path }));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Lock exists — check whether the holder is alive.
            }
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "create lock file at {}: {e}", path.display()));
            }
        }

        // Read the existing PID; if its /proc/<pid> is gone, take
        // over.
        let existing_pid: i64 = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);

        if existing_pid > 0 && process_alive(existing_pid) {
            // Live holder — refuse to start.
            return Ok(None);
        }

        // Stale lock: replace.
        std::fs::write(&path, &pid_str)
            .with_context(|| format!("replacing stale lock at {}", path.display()))?;
        Ok(Some(Self { path }))
    }

    /// Release the lock (best-effort). Idempotent.
    pub fn release(&self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        self.release();
    }
}

/// True iff `/proc/<pid>` exists — the cheap-and-cheerful
/// liveness signal on Linux. Returns false on platforms without
/// `/proc` (caller treats as "stale" → takeover).
fn process_alive(pid: i64) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests in this module mutate the global XDG_RUNTIME_DIR env
    /// var. Serialize them so `cargo test`'s parallel runner doesn't
    /// have two lock tests stomp on each other's path.
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Override HOME/XDG_RUNTIME_DIR to a temp dir so the lock
    /// doesn't stomp on a real one during tests.
    fn isolated_lock_path() {
        let tmp = std::env::temp_dir().join("minimax-lock-test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("XDG_RUNTIME_DIR", &tmp);
        let _ = std::fs::create_dir_all(&tmp);
    }

    #[test]
    fn acquire_and_release() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        isolated_lock_path();
        let lock = Lock::acquire().expect("acquire").expect("not held");
        // Second acquire should refuse.
        let second = Lock::acquire().expect("acquire2");
        assert!(second.is_none(), "second acquire should fail while first holds");
        lock.release();
        // Now third acquire should succeed.
        let third = Lock::acquire().expect("acquire3").expect("free");
        third.release();
    }

    #[test]
    fn take_over_stale_lock() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        isolated_lock_path();
        // Write a fake stale PID (1 is init — usually alive, so use
        // a very high number that's definitely dead).
        let path = crate::instance::lock_path();
        std::fs::write(&path, "9999999").unwrap();
        let lock = Lock::acquire().expect("acquire").expect("stale takeover");
        lock.release();
    }
}