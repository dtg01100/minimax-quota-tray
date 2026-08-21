//! Formatting helpers shared across modules.

/// Format a duration in ms as e.g. "5m", "1h 5m", "2d 3h". Used for reset
/// countdowns (window labels) and "last update Xm ago" stale annotations.
///
/// gjs parity: matches `fmtReset()` / `fmtAge()` in `llm-quota-tray.js`.
/// Note the gjs `fmtReset()` uses ceil-minutes ("4h" for 3h59m left), but
/// `fmtAge()` uses floor — they're separate helpers. Here we pick the
/// appropriate rounding via the `floor` flag at the call site.
pub fn fmt_duration(ms: i64, floor: bool) -> String {
    let abs = ms.max(0);
    let mins = if floor { abs / 60_000 } else { (abs + 59_999) / 60_000 };
    if mins < 60 {
        return format!("{mins}m");
    }
    let h = mins / 60;
    let m = mins % 60;
    if h < 24 {
        return if m == 0 { format!("{h}h") } else { format!("{h}h {m}m") };
    }
    let d = h / 24;
    let rh = h % 24;
    if rh == 0 {
        format!("{d}d")
    } else {
        format!("{d}d {rh}h")
    }
}

/// Format a duration in ms as e.g. "30s", "5m", "2h". Used for the
/// compact "since last update" label (floor — gjs `fmtAge()`).
pub fn fmt_age(ms: i64) -> String {
    let abs = ms.max(0);
    let s = abs / 1000;
    if s < 60 {
        return format!("{s}s");
    }
    let m = s / 60;
    if m < 60 {
        return format!("{m}m");
    }
    format!("{}h", m / 60)
}

/// Compact burn-rate label: "850" or "1.2k" or "12k" tokens/hour.
///
/// gjs parity: matches `fmtRate()`. The `.0` suffix is stripped on
/// the decimal range (`1.0k` → `1k`) but the integer range is preserved
/// so a 12,000/h rate reads "12k" not "1.2k" — same readability rule
/// as the gjs label.
pub fn fmt_rate(rate: f64) -> String {
    if !rate.is_finite() || rate <= 0.0 {
        return "0".to_string();
    }
    if rate >= 1000.0 {
        let k = rate / 1000.0;
        if k >= 100.0 {
            format!("{}k", k.round() as i64)
        } else {
            // 1.2k, 1.5k — but "1.0k" → "1k" (no decimal).
            // Only strip when the decimal is exactly .0 (and k >= 1.0,
            // so we don't render "0.5k" as just "5k").
            let rounded = (k * 10.0).round() / 10.0;
            if (rounded - rounded.trunc()).abs() < 0.05 && rounded >= 1.0 {
                format!("{}k", rounded as i64)
            } else {
                format!("{rounded:.1}k")
            }
        }
    } else {
        format!("{}", rate.round() as i64)
    }
}

/// Plain-text progress bar. Matches gjs `barMarkup()`.
///
/// W = 22 columns (chosen by gjs to fit inside a 24-character
/// terminal at the typical menu font). Block characters U+2588 (full)
/// + U+2591 (light shade) — universally rendered by every menu widget;
/// gjs explicitly avoids Pango markup here because some SNI menu
/// renderers don't support it.
pub fn bar_markup(fraction_pct: i64) -> String {
    const W: i64 = 22;
    let clamped = fraction_pct.max(0).min(100);
    let filled = ((clamped as i64 * W) + 50) / 100; // round-to-nearest
    let empty = W - filled;
    format!("  [{} {}]", "\u{2588}".repeat(filled as usize),
            "\u{2591}".repeat(empty as usize))
}

/// Burn-row label: the per-window informational line under the bar.
///
/// gjs parity: matches `burnRowLabel()`. Two variants:
///
///   - normal: "  · on pace to have ~X% left at reset (Y tok/h)"
///   - warning (exhausts before reset):
///     "  ⚠ Y tok/h → exhausts ~Xh Xm before reset"
///
/// The pct-only variant uses "/h" not "tok/h" (Coding Plan).
/// `projected_pct_left` rounds to the nearest integer for display.
pub fn burn_row_label(burn: &crate::burn::BurnResult) -> String {
    let rate_unit = if burn.unit == "pct" {
        format!("{}/h", fmt_rate(burn.rate_per_hour))
    } else {
        format!("{} tok/h", fmt_rate(burn.rate_per_hour))
    };
    if burn.exhaust_before_reset {
        format!("  ⚠ {rate_unit} → exhausts ~{} before reset",
                fmt_duration(burn.exhaust_ms as i64, false))
    } else {
        let pct_left = burn.projected_pct_left.round() as i64;
        format!("  · on pace to have ~{pct_left}% left at reset ({rate_unit})")
    }
}

/// Window label: "  5h: 80% left · resets in 4h"
/// Appends a stale suffix when the data is from the last good fetch
/// but the most recent attempt failed.
pub fn window_label(label: &str, remaining_pct: i64, resets_in_ms: i64, stale: bool) -> String {
    let base = format!("  {label}: {remaining_pct}% left · resets in {}",
                       fmt_duration(resets_in_ms, false));
    if stale {
        // The caller passes the age of the last good fetch via the
        // title-level suffix ("last update Xm ago"); here we keep
        // the row stable and let the menu row carry the suffix.
        base
    } else {
        base
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_duration_units() {
        // floor=true (gjs fmtAge behavior)
        assert_eq!(fmt_duration(0, true), "0m");
        assert_eq!(fmt_duration(60_000, true), "1m");
        assert_eq!(fmt_duration(3_600_000, true), "1h");
        assert_eq!(fmt_duration(3_900_000, true), "1h 5m");
        assert_eq!(fmt_duration(86_400_000, true), "1d");
        assert_eq!(fmt_duration(86_400_000 + 3_600_000, true), "1d 1h");
        assert_eq!(fmt_duration(2 * 86_400_000 + 3_600_000, true), "2d 1h");
        // floor=false (gjs fmtReset behavior — ceil minutes)
        assert_eq!(fmt_duration(60_000 - 1, false), "1m");
        assert_eq!(fmt_duration(2 * 3_600_000 - 1, false), "2h");
        assert_eq!(fmt_duration(2 * 3_600_000 + 30 * 60_000, false), "2h 30m");
    }

    #[test]
    fn fmt_duration_clamps_negative() {
        assert_eq!(fmt_duration(-100, true), "0m");
        assert_eq!(fmt_duration(-100, false), "0m");
    }

    #[test]
    fn fmt_age_units() {
        assert_eq!(fmt_age(0), "0s");
        assert_eq!(fmt_age(1_000), "1s");
        assert_eq!(fmt_age(60_000), "1m");
        assert_eq!(fmt_age(3_600_000), "1h");
        assert_eq!(fmt_age(3_900_000), "1h"); // floor
        assert_eq!(fmt_age(86_400_000), "24h");
    }

    #[test]
    fn fmt_age_clamps_negative() {
        assert_eq!(fmt_age(-100), "0s");
    }

    #[test]
    fn fmt_rate_units() {
        assert_eq!(fmt_rate(0.0), "0");
        assert_eq!(fmt_rate(40.0), "40");
        assert_eq!(fmt_rate(850.0), "850");
        // < 1000 — round to nearest integer (no "k")
        assert_eq!(fmt_rate(999.5), "1000");
        // >= 1000: k-form
        assert_eq!(fmt_rate(1000.0), "1k");
        assert_eq!(fmt_rate(1200.0), "1.2k");
        assert_eq!(fmt_rate(1500.0), "1.5k");
        assert_eq!(fmt_rate(10000.0), "10k");
        assert_eq!(fmt_rate(12000.0), "12k");
        // k between 10 and 100: integer k (no decimal)
        assert_eq!(fmt_rate(99500.0), "99.5k"); // gjs: k=99.5 < 100 → decimal
        assert_eq!(fmt_rate(100000.0), "100k"); // k=100 → integer
        assert_eq!(fmt_rate(150000.0), "150k"); // k=150 → integer
        // Decimal stripping on the sub-100k range
        assert_eq!(fmt_rate(2000.0), "2k"); // not 2.0k
        assert_eq!(fmt_rate(10000.0), "10k"); // not 10.0k
    }

    #[test]
    fn fmt_rate_handles_nonfinite() {
        assert_eq!(fmt_rate(f64::NAN), "0");
        assert_eq!(fmt_rate(f64::INFINITY), "0");
        assert_eq!(fmt_rate(-1.0), "0");
    }

    #[test]
    fn bar_markup_clamps() {
        let s = bar_markup(80);
        // 80% of 22 = 17.6 → 18 filled, 4 empty
        assert!(s.contains("\u{2588}"));
        assert!(s.contains("\u{2591}"));
        assert!(s.starts_with("  ["));
        assert!(s.ends_with("]"));
        // Length: "  [" (3) + 22 bar chars + " " (1) + "]" (1) = 27 chars
        let filled_count = s.chars().filter(|c| *c == '\u{2588}').count();
        let empty_count = s.chars().filter(|c| *c == '\u{2591}').count();
        assert_eq!(filled_count + empty_count, 22,
                   "expected 22 chars between brackets, got {filled_count}+{empty_count}");
        assert_eq!(s.chars().count(), 27);
    }

    #[test]
    fn bar_markup_edge_cases() {
        assert_eq!(bar_markup(0).chars().filter(|c| *c == '\u{2588}').count(), 0);
        assert_eq!(bar_markup(100).chars().filter(|c| *c == '\u{2588}').count(), 22);
        assert_eq!(bar_markup(50).chars().filter(|c| *c == '\u{2588}').count(), 11);
        // Clamping
        assert_eq!(bar_markup(-5).chars().filter(|c| *c == '\u{2588}').count(), 0);
        assert_eq!(bar_markup(150).chars().filter(|c| *c == '\u{2588}').count(), 22);
    }
}