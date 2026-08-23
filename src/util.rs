//! Display / formatting helpers shared across the menu and
//! scheduler layers.
//!
//! Pure functions — no I/O, no logger, no state. Every public
//! function here renders user-visible text (chip, menu rows,
//! tooltip strings, log-free status output) from typed inputs.
//! Kept in one module so the visual language stays consistent:
//! every "5m", "1h 5m", "$1.23", "20/h" the user sees in the UI
//! flows through one of these functions and can be tweaked in one
//! place.
//!
//! Grouped by what they format:
//!
//! - **Durations** — `fmt_duration()`, `fmt_age()` — used for
//!   reset countdowns on window labels and "last refresh Xm ago"
//!   stale annotations. Both round at the minute boundary; the
//!   rounding direction differs by context (ceil for "resets in"
//!   to never display "0m", floor for "X ago" to never display
//!   the next minute prematurely).
//! - **Rates** — `fmt_rate()` — e.g. "20/h" for burn projections.
//! - **Currency** — `fmt_cost()` — uses the ISO 4217 code from
//!   config (`"USD"`, `"EUR"`, `"JPY"`, …) with sensible
//!   per-currency decimal rules. Falls back to `$` for unknown /
//!   empty codes so the UI never renders blank.
//! - **Progress bars** — `bar_markup()` — Pango markup for the
//!   menu's "burn projected vs budget" progress bar. Two-segment
//!   (filled + empty) using a Unicode block character so it
//!   renders in any GTK theme.
//! - **Menu rows** — `burn_row_label()`, `window_label()` —
//!   composed strings that combine the above primitives into the
//!   per-window labels the menu tree displays. The single source
//!   of truth for "what does a window row look like".
//!
//! gjs parity: most of these correspond to `fmt*()` helpers in
//! `llm-quota-tray.js`. See [`docs/gjs-parity.md`](../../docs/gjs-parity.md)
//! for the "must not change" decisions each helper encodes.

/// Format a duration in ms as e.g. "5m", "1h 5m", "2d 3h". Used for reset
/// countdowns (window labels) and "last update Xm ago" stale annotations.
///
/// gjs parity: matches `fmtReset()` / `fmtAge()` in `llm-quota-tray.js`.
/// Note the gjs `fmtReset()` uses ceil-minutes ("4h" for 3h59m left), but
/// `fmtAge()` uses floor — they're separate helpers. Here we pick the
/// appropriate rounding via the `floor` flag at the call site.
///
/// # Examples
///
/// ```text
/// use crate::util::fmt_duration;
///
/// // "resets in" label — ceil so a 3h59m40s countdown shows "4h", not "3h".
/// assert_eq!(fmt_duration(3 * 3_600_000 + 59 * 60_000 + 40_000, false), "4h");
/// assert_eq!(fmt_duration(90 * 60_000, false), "2h");        // ceil 1h30m → 2h
///
/// // "X ago" label — floor so 1m29s reads as "1m", not "2m".
/// assert_eq!(fmt_duration(90 * 60_000, true), "1h");
/// assert_eq!(fmt_duration(45 * 60_000, true), "45m");
/// ```
pub fn fmt_duration(ms: i64, floor: bool) -> String {
    let abs = ms.max(0);
    let mins = if floor {
        abs / 60_000
    } else {
        (abs + 59_999) / 60_000
    };
    if mins < 60 {
        return format!("{mins}m");
    }
    let h = mins / 60;
    let m = mins % 60;
    if h < 24 {
        return if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h {m}m")
        };
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
///
/// # Examples
///
/// ```text
/// use crate::util::fmt_age;
///
/// assert_eq!(fmt_age(45 * 1000), "45s");
/// assert_eq!(fmt_age(2 * 60_000), "2m");
/// assert_eq!(fmt_age(3 * 3_600_000 + 5 * 60_000), "3h");
/// ```
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
///
/// # Examples
///
/// ```text
/// use crate::util::fmt_rate;
///
/// assert_eq!(fmt_rate(0.0), "0");
/// assert_eq!(fmt_rate(850.0), "850");
/// assert_eq!(fmt_rate(1_200.0), "1.2k");
/// assert_eq!(fmt_rate(12_000.0), "12k"); // integer range, NOT "1.2k"
/// ```
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
/// `W = 22` columns (chosen by gjs to fit inside a 24-character
/// terminal at the typical menu font), drawn with the universally-
/// supported block characters U+2588 (full) + U+2591 (light shade);
/// gjs explicitly avoids Pango markup because some SNI menu
/// renderers don't support it.
///
/// # Examples
///
/// ```text
/// use crate::util::bar_markup;
///
/// assert_eq!(bar_markup(0),   "  [\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}]");
/// assert_eq!(bar_markup(50),  "  [\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}]");
/// assert_eq!(bar_markup(100), "  [\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}]");
///
/// // Out-of-range values are clamped, not errored.
/// assert_eq!(bar_markup(-10), bar_markup(0));
/// assert_eq!(bar_markup(150), bar_markup(100));
/// ```
pub fn bar_markup(fraction_pct: i64) -> String {
    const W: i64 = 22;
    let clamped = fraction_pct.clamp(0, 100);
    let filled = ((clamped * W) + 50) / 100; // round-to-nearest
    let empty = W - filled;
    format!(
        "  [{} {}]",
        "\u{2588}".repeat(filled as usize),
        "\u{2591}".repeat(empty as usize)
    )
}

/// Compact cost-per-hour label for currency-denominated windows
/// (`WindowShape::count_unit == "cents"` or `"milliunits"`).
///
/// The `rate_per_hour` arriving here is in the same units as the
/// window's `used` integer — so when the upstream adapter scales a
/// USD float to integer cents (e.g. `$25.00 → 2500`), the rate is
/// also cents-per-hour. We convert cents→dollars for display here.
///
/// `currency` is an ISO-4217 code (`"usd"`, `"eur"`, `"cny"`) or a
/// display symbol (`"USD"`, `"¥"`). The currency symbol prefixed to
/// the output is derived from this value; unrecognized or empty
/// values fall back to `$`. This keeps single-currency configs
/// working while letting multi-currency setups (e.g. a CNY-denominated
/// DeepSeek window alongside a USD OpenRouter one) render distinct
/// symbols.
///
/// Format rules (mirroring `fmt_rate`'s tier system):
/// - < $0.01/h → `"$0.0042"` (4 decimals — typical for tiny OpenRouter
///   mini-models)
/// - < $1/h   → `"$0.40"`     (3 decimals)
/// - < $100/h → `"$5.23"`     (2 decimals — most common range)
/// - ≥ $100/h → `"$123"`      (integer dollars)
///
/// Trailing zeros are stripped at every tier (mirroring `fmt_rate`'s
/// `"$1.0k"` → `"$1k"` rule) so the row reads cleanly: `$0.4`, `$5`,
/// `$0.005` — never `$0.400` or `$5.00`.
///
/// Non-finite or negative rates return `"$0"` (or the equivalent in
/// the configured currency).
///
/// # Examples
///
/// ```text
/// use crate::util::fmt_cost;
///
/// // 0.4 cents/h = $0.004 → "$0.004" (4 decimals tier)
/// assert_eq!(fmt_cost(0.4, Some("USD")), "$0.004");
///
/// // 40 cents/h = $0.40 → "$0.40" (3 decimals, trailing 0 stripped
/// // by display logic — see implementation for the exact rule)
/// assert_eq!(fmt_cost(40.0, Some("USD")), "$0.4");
///
/// // 523 cents/h = $5.23 → "$5.23" (2 decimals tier)
/// assert_eq!(fmt_cost(523.0, Some("USD")), "$5.23");
///
/// // 12300 cents/h = $123 → "$123" (integer tier)
/// assert_eq!(fmt_cost(12_300.0, Some("USD")), "$123");
///
/// // Unknown / empty currency → "$" fallback.
/// assert_eq!(fmt_cost(523.0, Some("FOO")), "$5.23");
/// assert_eq!(fmt_cost(523.0, None),       "$5.23");
/// assert_eq!(fmt_cost(-1.0, Some("USD")), "$0");
/// ```
pub fn fmt_cost(cents_per_hour: f64, currency: Option<&str>) -> String {
    let symbol = currency_symbol(currency);
    let zero = format!("{symbol}0");
    if !cents_per_hour.is_finite() || cents_per_hour <= 0.0 {
        return zero;
    }
    let usd = cents_per_hour / 100.0;
    let trim = |s: String| -> String {
        if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            s
        }
    };
    if usd < 0.01 {
        trim(format!("{symbol}{usd:.4}"))
    } else if usd < 1.0 {
        trim(format!("{symbol}{usd:.3}"))
    } else if usd < 100.0 {
        trim(format!("{symbol}{usd:.2}"))
    } else {
        format!("{symbol}{}", usd.round() as i64)
    }
}

/// Map a `currency` config value to a display symbol. Recognizes common
/// ISO-4217 codes (case-insensitive) and well-known symbols; anything
/// else falls back to `$`.
fn currency_symbol(currency: Option<&str>) -> &'static str {
    match currency {
        None => "$",
        Some(c) => match c.trim() {
            "" => "$",
            "usd" | "USD" => "$",
            "eur" | "EUR" => "€",
            "gbp" | "GBP" => "£",
            "jpy" | "JPY" | "cny" | "CNY" | "rmb" | "RMB" => "¥",
            "krw" | "KRW" => "₩",
            "inr" | "INR" => "₹",
            // Unrecognized value — fall back to `$` rather than echoing
            // a likely-typo back to the user.
            _ => "$",
        },
    }
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
///
/// Currency-aware: when the window's `count_unit` is `"cents"` (or
/// `"milliunits"`), the rate is rendered with `fmt_cost` instead of
/// the token formatter — so the row reads `(40 $/h)` for Together /
/// DeepSeek / OpenRouter-style windows. The `currency` field selects
/// the symbol used by `fmt_cost` (e.g. `$`, `€`, `¥`); see
/// `fmt_cost` for the recognized codes.
///
/// Pricing fragment: when `cost_fragment` is `Some(s)`, it's appended
/// to the rate portion of the label with a ` · ` separator — e.g.
/// `(40 tok/h · $0.4/h)`. The caller (build_menu_state) is
/// responsible for computing the fragment from the cached price
/// table; this function is a pure formatter. `None` preserves the
/// legacy one-rate label.
pub fn burn_row_label(
    burn: &crate::burn::BurnResult,
    count_unit: Option<&str>,
    currency: Option<&str>,
    cost_fragment: Option<&str>,
) -> String {
    let is_currency = matches!(count_unit, Some("cents") | Some("milliunits"));
    let rate_unit = if burn.unit == "pct" {
        format!("{}/h", fmt_rate(burn.rate_per_hour))
    } else if is_currency {
        format!("{}/h", fmt_cost(burn.rate_per_hour, currency))
    } else {
        format!("{} tok/h", fmt_rate(burn.rate_per_hour))
    };
    let rate_unit = match cost_fragment {
        Some(extra) => format!("{rate_unit} · {extra}"),
        None => rate_unit,
    };
    if burn.exhaust_before_reset {
        format!(
            "  ⚠ {rate_unit} → exhausts ~{} before reset",
            fmt_duration(burn.exhaust_ms as i64, false)
        )
    } else {
        let pct_left = burn.projected_pct_left.round() as i64;
        format!("  · on pace to have ~{pct_left}% left at reset ({rate_unit})")
    }
}

/// Window label: "  5h: 80% left · resets in 4h"
/// Appends a stale suffix when the data is from the last good fetch
/// but the most recent attempt failed.
pub fn window_label(label: &str, remaining_pct: i64, resets_in_ms: i64, stale: bool) -> String {
    let base = format!(
        "  {label}: {remaining_pct}% left · resets in {}",
        fmt_duration(resets_in_ms, false)
    );
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

    // ---- fmt_cost (OpenRouter prototype: $/h from cents/h) ----

    /// Helper for the burn_row_label tests below. Builds a
    /// token-mode BurnResult with a fixed rate + pct.
    fn burn_for_test(
        rate_per_hour: f64,
        projected_pct_left: f64,
        exhaust_before_reset: bool,
        unit: &'static str,
    ) -> crate::burn::BurnResult {
        crate::burn::BurnResult {
            rate_per_hour,
            mode: unit,
            unit,
            exhaust_ms: if exhaust_before_reset {
                60.0 * 60_000.0
            } else {
                f64::INFINITY
            },
            remaining_ms: 5 * 60 * 60_000,
            exhaust_before_reset,
            projected_pct_left,
        }
    }

    #[test]
    fn fmt_cost_units() {
        // Cents/h → $/h, with tier boundaries matching fmt_rate's
        // style. Trailing zeros are stripped at every tier (mirrors
        // `fmt_rate`'s gjs parity rule: `"$1.0k"` → `"$1k"`).
        assert_eq!(fmt_cost(0.0, None), "$0");
        assert_eq!(fmt_cost(0.5, None), "$0.005"); // < $0.01 → 4dp
        assert_eq!(fmt_cost(1.0, None), "$0.01"); // exactly $0.01
        assert_eq!(fmt_cost(50.0, None), "$0.5"); // < $1 → 3dp
        assert_eq!(fmt_cost(99.0, None), "$0.99");
        assert_eq!(fmt_cost(100.0, None), "$1"); // ≥ $1 → 2dp, both zeros stripped
        assert_eq!(fmt_cost(523.0, None), "$5.23"); // typical PAYG burn
        assert_eq!(fmt_cost(9999.0, None), "$99.99");
        assert_eq!(fmt_cost(10_000.0, None), "$100"); // ≥ $100 → integer
        assert_eq!(fmt_cost(12_345.0, None), "$123");
        assert_eq!(fmt_cost(999_500.0, None), "$9995");
    }

    #[test]
    fn fmt_cost_handles_nonfinite() {
        assert_eq!(fmt_cost(f64::NAN, None), "$0");
        assert_eq!(fmt_cost(f64::INFINITY, None), "$0");
        assert_eq!(fmt_cost(-1.0, None), "$0");
        assert_eq!(fmt_cost(f64::NEG_INFINITY, None), "$0");
    }

    #[test]
    fn fmt_cost_currency_usd() {
        assert_eq!(fmt_cost(100.0, Some("USD")), "$1");
        assert_eq!(fmt_cost(100.0, Some("usd")), "$1");
    }

    #[test]
    fn fmt_cost_currency_eur() {
        assert_eq!(fmt_cost(100.0, Some("EUR")), "€1");
    }

    #[test]
    fn fmt_cost_currency_jpy() {
        assert_eq!(fmt_cost(100.0, Some("JPY")), "¥1");
        assert_eq!(fmt_cost(100.0, Some("cny")), "¥1");
    }

    #[test]
    fn fmt_cost_currency_empty_falls_back_to_dollar() {
        assert_eq!(fmt_cost(100.0, Some("")), "$1");
        assert_eq!(fmt_cost(100.0, None), "$1");
    }

    #[test]
    fn fmt_cost_currency_unknown_falls_back_to_dollar() {
        // Unrecognized codes fall back to `$` rather than echoing a
        // likely-typo back to the user.
        assert_eq!(fmt_cost(100.0, Some("XYZ")), "$1");
    }

    // ---- burn_row_label (count_unit / currency / cost_fragment) ----

    #[test]
    fn burn_row_label_default_uses_tok_h() {
        // Legacy: no count_unit → "tok/h" suffix (matches gjs parity).
        let b = burn_for_test(40.0, 48.0, false, "token");
        assert_eq!(
            burn_row_label(&b, None, None, None),
            "  · on pace to have ~48% left at reset (40 tok/h)"
        );
    }

    #[test]
    fn burn_row_label_cents_uses_dollar_h() {
        // OpenRouter / Together / DeepSeek prototype: count_unit="cents".
        // The same rate (40/h) now reads as "$0.4/h".
        let b = burn_for_test(40.0, 48.0, false, "token");
        assert_eq!(
            burn_row_label(&b, Some("cents"), Some("USD"), None),
            "  · on pace to have ~48% left at reset ($0.4/h)"
        );
    }

    #[test]
    fn burn_row_label_milliunits_also_uses_dollar_h() {
        // OpenAI-billed-units style (rare; included for completeness).
        // 1000 milliunits/h with our fmt_cost (which treats input as
        // cents/h) renders "$10/h" after trailing-zero strip.
        let b = burn_for_test(1000.0, 50.0, false, "token");
        assert_eq!(
            burn_row_label(&b, Some("milliunits"), None, None),
            "  · on pace to have ~50% left at reset ($10/h)"
        );
    }

    #[test]
    fn burn_row_label_cents_warn_variant() {
        let b = burn_for_test(500.0, 0.0, true, "token");
        assert_eq!(
            burn_row_label(&b, Some("cents"), Some("USD"), None),
            "  ⚠ $5/h → exhausts ~1h before reset"
        );
    }

    #[test]
    fn burn_row_label_cents_zero_rate_still_renders() {
        // Idle (rate=0) should not emit garbage — fmt_cost returns "$0".
        let b = burn_for_test(0.0, 100.0, false, "token");
        assert_eq!(
            burn_row_label(&b, Some("cents"), Some("USD"), None),
            "  · on pace to have ~100% left at reset ($0/h)"
        );
    }

    #[test]
    fn burn_row_label_pct_mode_unchanged_by_count_unit() {
        // pct-mode (Coding Plan) ignores count_unit — the rate is always
        // in %/h, never $/h. Lock this in so the OpenRouter change
        // doesn't accidentally promote a pct window to currency.
        let b = burn_for_test(15.0, 62.0, false, "pct");
        assert_eq!(
            burn_row_label(&b, Some("cents"), Some("USD"), None),
            "  · on pace to have ~62% left at reset (15/h)"
        );
    }

    #[test]
    fn burn_row_label_appends_cost_fragment() {
        // When cost_fragment is Some, append it to the rate portion
        // with a " · " separator. Sanity-checks the OpenRouter
        // pricing-lookup wiring end-to-end through this function.
        let b = burn_for_test(100_000.0, 50.0, false, "token");
        assert_eq!(
            burn_row_label(&b, None, None, Some("$0.4/h")),
            "  · on pace to have ~50% left at reset (100k tok/h · $0.4/h)"
        );
    }

    #[test]
    fn burn_row_label_cost_fragment_works_in_warn_variant() {
        let b = burn_for_test(500_000.0, 0.0, true, "token");
        assert_eq!(
            burn_row_label(&b, None, None, Some("$5/h")),
            "  ⚠ 500k tok/h · $5/h → exhausts ~1h before reset"
        );
    }

    #[test]
    fn burn_row_label_cost_fragment_works_in_currency_mode() {
        // Currency-mode (Together/DeepSeek): row is already in $/h,
        // but caller may still pass a cost_fragment (e.g. price-table
        // lookup found the model). Verify the " · " append is
        // harmless rather than erroring.
        let b = burn_for_test(40.0, 48.0, false, "token");
        assert_eq!(
            burn_row_label(&b, Some("cents"), Some("USD"), Some("$0.0001/h")),
            "  · on pace to have ~48% left at reset ($0.4/h · $0.0001/h)"
        );
    }

    #[test]
    fn burn_row_label_pct_mode_with_cost_fragment() {
        // pct-mode is opaque to count_unit; cost_fragment is currently
        // appended regardless of mode (callers should gate upstream).
        // Lock in current behavior so any future change is intentional.
        let b = burn_for_test(15.0, 62.0, false, "pct");
        assert_eq!(
            burn_row_label(&b, None, None, Some("$0.4/h")),
            "  · on pace to have ~62% left at reset (15/h · $0.4/h)"
        );
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
        assert_eq!(
            filled_count + empty_count,
            22,
            "expected 22 chars between brackets, got {filled_count}+{empty_count}"
        );
        assert_eq!(s.chars().count(), 27);
    }

    #[test]
    fn bar_markup_edge_cases() {
        assert_eq!(
            bar_markup(0).chars().filter(|c| *c == '\u{2588}').count(),
            0
        );
        assert_eq!(
            bar_markup(100).chars().filter(|c| *c == '\u{2588}').count(),
            22
        );
        assert_eq!(
            bar_markup(50).chars().filter(|c| *c == '\u{2588}').count(),
            11
        );
        // Clamping
        assert_eq!(
            bar_markup(-5).chars().filter(|c| *c == '\u{2588}').count(),
            0
        );
        assert_eq!(
            bar_markup(150).chars().filter(|c| *c == '\u{2588}').count(),
            22
        );
    }
}
