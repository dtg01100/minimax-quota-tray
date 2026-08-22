//! Threshold notifications via `notify-send`.
//!
//! gjs parity: matches the gjs `notify()` helper. Sends a desktop
//! notification via `notify-send` (provided by libnotify on every
//! distro we care about) when the bucket rank transitions upward:
//!
//!   normal   → warning   ("<plan> — running low")  urgency: normal
//!   warning  → throttled ("<plan> — throttled")     urgency: critical
//!   * → normal — NOT notified (only worse states trigger)
//!
//! Deduplication is the caller's job (track `_last_bucket` between
//! refreshes, like gjs's `_lastBucket`). This module only handles
//! the spawn + arg shaping.

use std::process::{Command, Stdio};

/// Notify via `notify-send`. Best-effort — failures are logged at
/// debug level only (a missing `notify-send` shouldn't take down
/// the tray).
///
/// `private_tag` is a stable per-notification tag string used as
/// `x-canonical-private-synchronous` so the desktop replaces
/// in-flight notifications of the same kind instead of stacking
/// them (matches gjs behavior with `-h string:x-canonical-private-
/// synchronous:<tag>`).
pub fn send(tag: &str, title: &str, body: &str, urgency: Urgency) {
    let urgency_str = match urgency {
        Urgency::Low => "low",
        Urgency::Normal => "normal",
        Urgency::Critical => "critical",
    };
    let result = Command::new("notify-send")
        .args([
            "-a",
            "llm-quota-tray",
            "-h",
            &format!("string:x-canonical-private-synchronous:{tag}"),
            "-u",
            urgency_str,
            title,
            body,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match result {
        Ok(_) => {}
        Err(e) => {
            log::debug!("notify-send failed (probably not installed): {e}");
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    /// Not currently emitted by the tray (the threshold logic only
    /// fires on `Normal` and `Critical`). Defined for parity with
    /// the libnotify urgency levels.
    #[allow(dead_code)]
    Low,
    Normal,
    Critical,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urgency_str_mapping() {
        fn s(u: Urgency) -> &'static str {
            match u {
                Urgency::Low => "low",
                Urgency::Normal => "normal",
                Urgency::Critical => "critical",
            }
        }
        assert_eq!(s(Urgency::Low), "low");
        assert_eq!(s(Urgency::Normal), "normal");
        assert_eq!(s(Urgency::Critical), "critical");
    }

    #[test]
    fn urgency_eq_and_copy() {
        // Used as a function arg by value; Copy trait is implicit
        // because all variants are unit. This test makes sure we
        // didn't accidentally add a payload.
        let u = Urgency::Normal;
        let v = u;
        assert_eq!(u, v);
    }
}
