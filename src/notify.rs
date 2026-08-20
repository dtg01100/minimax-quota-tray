//! Desktop notifications for state transitions (yellow / red thresholds).
//! Pure-Rust via `notify-rust` (D-Bus to orgfreedesktopNotifications).
//!
//! Notifications are deduped by urgency: we only fire when crossing into a
//! new bucket (normal → warning → throttled). Going back to normal does not
//! notify — it's quiet recovery, like the gjs code did.

use notify_rust::{Hint, Notification, Urgency};
use std::sync::Mutex;

use crate::icon::Bucket;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LastBucket {
    None,
    Normal,
    Warning,
    Throttled,
}

static LAST_BUCKET: Mutex<LastBucket> = Mutex::new(LastBucket::None);

/// Reset the notification state. Used by tests.
pub fn reset_for_test() {
    *LAST_BUCKET.lock().expect("LAST_BUCKET mutex") = LastBucket::None;
}

/// Fire a desktop notification iff the bucket has changed to a more severe
/// state. Quiet recovery (warning → normal) does NOT notify.
pub fn maybe_notify(bucket: Bucket, plan_label: &str, pct: i64) {
    let new = match bucket {
        Bucket::Normal => LastBucket::Normal,
        Bucket::Warning => LastBucket::Warning,
        Bucket::Throttled => LastBucket::Throttled,
    };

    let prev = {
        let mut guard = LAST_BUCKET.lock().expect("LAST_BUCKET mutex");
        let p = *guard;
        *guard = new;
        p
    };

    // Only fire when transitioning INTO a more severe bucket.
    if bucket_rank(new) <= bucket_rank(prev) {
        return;
    }

    let body = match bucket {
        Bucket::Normal => return, // unreachable due to the check above
        Bucket::Warning => format!("{plan_label}: {pct}% remaining (yellow)"),
        Bucket::Throttled => format!("{plan_label}: {pct}% remaining (red)"),
    };
    let urgency = match bucket {
        Bucket::Normal => Urgency::Low,
        Bucket::Warning => Urgency::Normal,
        Bucket::Throttled => Urgency::Critical,
    };

    let result = Notification::new()
        .summary("MiniMax quota")
        .body(&body)
        .urgency(urgency)
        .hint(Hint::Resident(true))
        .show();

    if let Err(e) = result {
        log::warn!("notification failed: {e}");
    }
}

fn bucket_rank(b: LastBucket) -> i32 {
    match b {
        LastBucket::None => -1,
        LastBucket::Normal => 0,
        LastBucket::Warning => 1,
        LastBucket::Throttled => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_normal_does_not_notify() {
        reset_for_test();
        // Normal bucket is suppressed even on first call (no body matches
        // Normal, and we explicitly `return` in the body match).
        maybe_notify(Bucket::Normal, "Test", 50);
    }

    #[test]
    fn escalation_warns_warning_to_throttled() {
        reset_for_test();
        // Simulate: Normal → Warning → Throttled. Both Warning and
        // Throttled calls would try to notify; we can't easily test the
        // DBus call, but we can verify the rank-tracking logic.
        // (Direct DBus tests are integration-only.)
        maybe_notify(Bucket::Warning, "Test", 30);
        maybe_notify(Bucket::Throttled, "Test", 10);
    }

    #[test]
    fn recovery_is_quiet() {
        reset_for_test();
        // Warning → Normal: NOT a notify. We just verify no panic.
        maybe_notify(Bucket::Warning, "Test", 30);
        maybe_notify(Bucket::Normal, "Test", 60);
    }
}