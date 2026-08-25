//! Single-instance PID lock at `$XDG_RUNTIME_DIR/<basename>.pid`,
//! where `<basename>` is the per-instance name from
//! `crate::instance` (e.g. `quota-tray` for the default instance,
//! `quota-tray-codex` for `--instance=codex`).
//!
//! Two concurrent instances have different lock paths so they don't
//! conflict — each has its own `pid` file in its own slot.
//!
//! Uses `flock(2)` with `LOCK_EX | LOCK_NB` for atomic exclusive
//! locking — no TOCTOU race between check-and-write. The PID is
//! written to the file so stale locks can be identified by readers,
//! but the kernel flock is the source of truth for ownership.
//!
//! On platforms without `flock` (non-Unix), the lock is best-effort:
//! we'd rather run two instances than refuse to start.

use anyhow::Result;
use std::fs::File;
use std::path::PathBuf;

/// Acquired lock state; holds the file handle so the flock is released
/// on drop, and the path for cleanup.
pub struct Lock {
    path: PathBuf,
    file: File,
}

impl Lock {
    /// Try to acquire the lock. Returns `Ok(Some(Lock))` on success,
    /// `Ok(None)` if another live instance holds it, `Err` if the
    /// lock file cannot be created or opened.
    pub fn acquire() -> Result<Option<Self>> {
        let path = crate::instance::lock_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        // Open (creating if needed) — the file must exist for flock.
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;

        // Try to acquire an exclusive, non-blocking lock. This is
        // atomic: either we get the lock or we don't — no race.
        if !try_lock_exclusive(&file)? {
            return Ok(None);
        }

        // Write our PID for diagnostics / stale identification.
        let pid_str = std::process::id().to_string();
        use std::io::Write;
        let _ = file.set_len(0);
        let _ = file.write_all(pid_str.as_bytes());

        Ok(Some(Self { path, file }))
    }

    /// Release the lock (best-effort). Idempotent.
    pub fn release(&self) {
        let _ = unlock_file(&self.file);
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        self.release();
    }
}

/// Try to acquire an exclusive non-blocking lock on `file`.
/// Returns `Ok(true)` if the lock was acquired, `Ok(false)` if
/// another process holds it, or `Err` on a system error.
#[cfg(unix)]
fn try_lock_exclusive(file: &File) -> Result<bool> {
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    // LOCK_EX | LOCK_NB: exclusive, non-blocking. Returns 0 on
    // success, -1 with EWOULDBLOCK if already locked.
    let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if ret == 0 {
        Ok(true)
    } else {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            Ok(false)
        } else {
            Err(err.into())
        }
    }
}

/// Release a flock on `file`. Best-effort.
#[cfg(unix)]
fn unlock_file(file: &File) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    let ret = unsafe { libc::flock(fd, libc::LOCK_UN) };
    if ret == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

/// Non-Unix fallback: no locking. Always succeeds.
#[cfg(not(unix))]
fn try_lock_exclusive(_file: &File) -> Result<bool> {
    Ok(true)
}

#[cfg(not(unix))]
fn unlock_file(_file: &File) -> Result<()> {
    Ok(())
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
        // a very high number that's definitely dead.
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
        // RAII safety net: Lock::Drop must release the flock so
        // a panic in the middle of a session does not leave a stale
        // lock that the next launch would have to take over.
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        isolated_lock_path();
        let path = crate::instance::lock_path();
        {
            let _lock = Lock::acquire().expect("acquire").expect("not held");
            assert!(path.exists(), "lockfile should exist while held");
        } // _lock drops here
        // After drop, a new acquire should succeed (flock released).
        let second = Lock::acquire().expect("acquire-after-drop");
        assert!(
            second.is_some(),
            "lock should be releasable after Drop"
        );
        second.unwrap().release();
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
        // should not deadlock the next launch. With flock, the
        // previous holder has already exited (no live flock), so
        // we can take over.
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
    }
}
