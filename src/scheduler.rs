//! Adaptive polling scheduler. Mirrors the gjs `nextIntervalSeconds()`
//! function: `refresh_seconds` baseline, halves when remaining quota
//! is below yellow, quarters when below red, and applies exponential
//! backoff up to `refresh_max_backoff_seconds` after consecutive errors.
//!
//! Floor is configurable via `min_seconds` (gjs reads this from
//! `config.refresh_min_seconds`, default 15). Caps at
//! `max_backoff_seconds` (gjs's `refresh_max_backoff_seconds`,
//! default 600).

/// Compute the next interval based on the current remaining_pct and the
/// error streak. Mirrors gjs `nextIntervalSeconds()`.
///
///   `base`           — baseline (gjs `refresh_seconds`)
///   `min_seconds`    — floor (gjs `refresh_min_seconds`, default 15)
///   `max_backoff`    — cap on exponential backoff (gjs
///                      `refresh_max_backoff_seconds`, default 600)
///   `remaining_pct`  — current quota (drives the yellow/red adaptive cut)
///   `yellow`, `red`  — threshold used% for the adaptive cut
///   `fail_streak`    — consecutive errors; 2^fail_streak caps at min(base,
///                      max_backoff) after the min_seconds floor
pub fn next_interval(
    base: u64,
    min_seconds: u64,
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
    .max(min_seconds); // never below min_seconds — avoid hammer-the-API

    // Exponential backoff on errors. The `fail_streak == 0` branch
    // returns `adaptive` unchanged — `2u64.saturating_pow(0) == 1`
    // would also work, but the explicit branch reads more clearly.
    if fail_streak == 0 {
        adaptive
    } else {
        adaptive
            .saturating_mul(2u64.saturating_pow(fail_streak.min(8)))
            .min(max_backoff)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_full_quota_uses_base_interval() {
        let s = next_interval(120, 15, 600, 100, 60, 85, 0);
        assert_eq!(s, 120);
    }

    #[test]
    fn yellow_cuts_in_half() {
        let s = next_interval(120, 15, 600, 40, 60, 85, 0); // 60% used
        assert_eq!(s, 60);
    }

    #[test]
    fn red_quarters() {
        let s = next_interval(120, 15, 600, 14, 60, 85, 0); // 86% used
        assert_eq!(s, 30);
    }

    #[test]
    fn backoff_doubles_per_failure() {
        assert_eq!(next_interval(120, 15, 600, 100, 60, 85, 1), 240);
        assert_eq!(next_interval(120, 15, 600, 100, 60, 85, 2), 480);
        assert_eq!(next_interval(120, 15, 600, 100, 60, 85, 3), 600); // capped at max_backoff
    }

    #[test]
    fn backoff_respects_max() {
        let s = next_interval(120, 15, 600, 100, 60, 85, 10);
        assert_eq!(s, 600);
    }

    #[test]
    fn adaptive_floor_at_min_seconds() {
        // With base=30 and red threshold crossed, /4 would give 7.5 —
        // floor at min_seconds (default 15).
        let s = next_interval(30, 15, 600, 0, 60, 85, 0);
        assert_eq!(s, 15);
    }

    #[test]
    fn min_seconds_is_configurable() {
        // Same adaptive math with min_seconds=30 → floor is 30, not 15.
        let s = next_interval(30, 30, 600, 0, 60, 85, 0);
        assert_eq!(s, 30);
        // min_seconds=60 caps the urgent (red) floor higher than gjs default.
        let s = next_interval(120, 60, 600, 14, 60, 85, 0);
        assert_eq!(s, 60, "red /4 must respect min_seconds floor");
    }

    #[test]
    fn min_seconds_zero_disables_floor() {
        // Edge case: operator can set min_seconds=0 to disable the
        // 15s floor (not recommended, but gjs allows it).
        let s = next_interval(30, 0, 600, 0, 60, 85, 0);
        assert_eq!(s, 7, "no floor: base/4 = 7.5 truncates to 7");
    }
}
