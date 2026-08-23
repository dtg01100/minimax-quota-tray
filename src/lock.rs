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
                    "create lock file at {}: {e}",
                    path.display()
                ));
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
        let tmp = std::env::temp_dir().join("llm-quota-lock-test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("XDG_RUNTIME_DIR", &tmp);
        let _ = std::fs::create_dir_all(&tmp);
    }

    #[test]
    fn acquire_and_release() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        isolated_lock_path();
        let lock = Lock::acquire().expect("acquire").expect("not held");
        // Verify the lockfile actually contains our PID (not just
        // that acquire returned Some). Guards against a regression
        // where Lock::acquire's success path forgets to write the
        // pid bytes.
        let path = crate::instance::lock_path();
        let contents = std::fs::read_to_string(&path).expect("read lockfile");
        let my_pid = std::process::id();
        let expected = my_pid.to_string();
        assert_eq!(
            contents, expected,
            "lockfile should contain current PID after acquire"
        );
        // Second acquire should refuse.
        let second = Lock::acquire().expect("acquire2");
        assert!(
            second.is_none(),
            "second acquire should fail while first holds"
        );
        lock.release();
        // After release the lockfile should be gone (Lock::Drop
        // also releases — explicit release() is idempotent).
        assert!(
            !path.exists(),
            "lockfile should be removed after release(); still exists"
        );
        // Now third acquire should succeed.
        let third = Lock::acquire().expect("acquire3").expect("free");
        third.release();
    }

    #[test]
    fn take_over_stale_lock() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        isolated_lock_path();
        // Write a fake stale PID. 1 is init (usually alive), so use
        // a very high number that's definitely dead - /proc/9999999
        // does not exist on any sane system.
        let path = crate::instance::lock_path();
        std::fs::write(&path, "9999999").unwrap();
        let lock = Lock::acquire().expect("acquire").expect("stale takeover");
        // Verify the stale PID was actually replaced with our PID
        // (otherwise this test would pass even if the takeover
        // branch were a no-op).
        let contents = std::fs::read_to_string(&path).expect("read lockfile");
        let my_pid = std::process::id();
        let expected = my_pid.to_string();
        assert_eq!(
            contents, expected,
            "stale PID 9999999 should be replaced with current PID"
        );
        lock.release();
    }

    #[test]
    fn drop_releases_lock() {
        // RAII safety net: Lock::Drop must remove the lockfile so
        // a panic in the middle of a session does not leave a stale
        // lock that the next launch would have to take over.
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        isolated_lock_path();
        let path = crate::instance::lock_path();
        {
            let _lock = Lock::acquire().expect("acquire").expect("not held");
            assert!(path.exists(), "lockfile should exist while held");
        } // _lock drops here
        assert!(
            !path.exists(),
            "lockfile should be removed by Lock::Drop"
        );
    }

    #[test]
    fn release_is_idempotent() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        isolated_lock_path();
        let lock = Lock::acquire().expect("acquire").expect("not held");
        lock.release();
        // Second release must not error (file is already gone).
        lock.release();
        // Third acquire must succeed - if release had somehow
        // re-created the file with garbage, this would fail.
        let third = Lock::acquire().expect("acquire-after-double-release").expect("free");
        third.release();
    }

    #[test]
    fn malformed_lockfile_is_treated_as_stale() {
        // A lockfile containing non-numeric garbage (e.g. truncated
        // write, or a process that wrote a name instead of a PID)
        // should not deadlock the next launch. The spec says
        // /proc/<pid> liveness is the gate; non-numeric values parse
        // as 0 which fails the > 0 check, so the takeover branch
        // runs.
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        isolated_lock_path();
        let path = crate::instance::lock_path();
        std::fs::write(&path, "not-a-pid\n").unwrap();
        let lock = Lock::acquire()
            .expect("acquire despite garbage lockfile")
            .expect("malformed lockfile must be treated as stale");
        // Verify our PID is now in the lockfile.
        let contents = std::fs::read_to_string(&path).expect("read lockfile");
        let my_pid = std::process::id();
        assert_eq!(contents, my_pid.to_string());
        lock.release();
    }}
