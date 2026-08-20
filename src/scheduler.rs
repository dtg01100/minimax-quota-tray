//! Adaptive polling scheduler. Maintains a GLib timer that fires at
//! `refresh_seconds` under normal conditions, halves when remaining quota
//! is below yellow, quarters when below red, and applies exponential
//! backoff up to `refresh_max_backoff_seconds` after consecutive errors.

// (intentionally no glib use here — pure-function tests only)

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Idle,
    Forced,
}

/// Compute the next interval based on the current remaining_pct and the
/// error streak. Mirrors the gjs `nextIntervalSeconds()`.
pub fn next_interval(
    base: u64,
    max_backoff: u64,
    remaining_pct: i64,
    yellow: i64,
    red: i64,
    fail_streak: u32,
) -> u64 {
    // Adaptive: faster polling when remaining% is low.
    let used = 100 - remaining_pct;
    let adaptive = if used >= red {
        base / 4
    } else if used >= yellow {
        base / 2
    } else {
        base
    }
    .max(15); // never below 15s — avoid hammer-the-API

    // Exponential backoff on errors.
    let backoff = if fail_streak == 0 {
        adaptive
    } else {
        adaptive
            .saturating_mul(2u64.saturating_pow(fail_streak.min(8)))
            .min(max_backoff)
    };

    backoff
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_full_quota_uses_base_interval() {
        let s = next_interval(120, 600, 100, 60, 85, 0);
        assert_eq!(s, 120);
    }

    #[test]
    fn yellow_cuts_in_half() {
        let s = next_interval(120, 600, 40, 60, 85, 0); // 60% used
        assert_eq!(s, 60);
    }

    #[test]
    fn red_quarters() {
        let s = next_interval(120, 600, 14, 60, 85, 0); // 86% used
        assert_eq!(s, 30);
    }

    #[test]
    fn backoff_doubles_per_failure() {
        assert_eq!(next_interval(120, 600, 100, 60, 85, 1), 240);
        assert_eq!(next_interval(120, 600, 100, 60, 85, 2), 480);
        assert_eq!(next_interval(120, 600, 100, 60, 85, 3), 600); // capped at max_backoff
    }

    #[test]
    fn backoff_respects_max() {
        let s = next_interval(120, 600, 100, 60, 85, 10);
        assert_eq!(s, 600);
    }

    #[test]
    fn adaptive_floor_at_15s() {
        // With base=30 and red threshold crossed, /4 would give 7.5 — floor at 15.
        let s = next_interval(30, 600, 0, 60, 85, 0);
        assert_eq!(s, 15);
    }
}