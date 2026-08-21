//! Burn-rate projection: same logic as the gjs `computeBurn()` + `decideBurnRow()`,
//! ported 1:1 so the test fixtures in the gjs suite can be ported verbatim.
//!
//! A "window" is one quota epoch (5h, weekly, anything). A "sample" is a snapshot
//! at poll time. The math: max of a least-squares slope over `lookback_ms` and
//! (optionally) the whole-epoch average. Mode is 'token' when count fields move,
//! 'pct' when only the integer-percent field moves (Coding Plan), 'idle' otherwise.
//!
//! `compute_burn()` returns `Some(BurnResult)` once enough history has accumulated
//! (`min_history_ms`), or `None` otherwise. A result with `rate_per_hour == 0.0`
//! means "idle, but computed" — call `decide_burn_row()` to apply the
//! pct-only suppression rule (Coding Plan weekly + idle 5h → no row;
//! token plans keep the row even at rate=0).

use serde::{Deserialize, Serialize};

/// Per-sample observation recorded at each successful poll.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    /// ms since unix epoch.
    pub t: i64,
    /// Tokens consumed at this poll (0 for pct-only providers).
    pub used: i64,
    /// Total tokens for the epoch (0 for pct-only providers).
    pub total: i64,
    /// Integer 0-100 from `*_remaining_percent`.
    pub remaining_pct: i64,
    /// Epoch start, ms since unix epoch.
    pub start_at: i64,
    /// Epoch reset, ms since unix epoch.
    pub reset_at: i64,
}

/// Per-window quota state from the most recent successful fetch.
///
/// `id` is owned (`String`, not `&'static str`) so each instance's
/// config.json can define windows dynamically — the parser reads the
/// id from `WindowShape::id` and produces a `Window` with the same
/// owned string. The burn-rate history key in `AppState` is also
/// `String` for the same reason.
///
/// `count_unit` and `currency` mirror the corresponding `WindowShape`
/// fields verbatim. They're carried on `Window` (rather than looked
/// up from config at render time) so `build_menu_state` has
/// everything it needs without a side-table. Default `None` for both
/// preserves legacy behavior: tok/h label, no currency symbol.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    /// Stable label, e.g. "5h", "weekly", "daily". Used as the
    /// menu row label and as the burn-rate history key.
    pub id: String,
    pub total: i64,
    pub used: i64,
    pub remaining_pct: i64,
    pub start_at: i64,
    pub reset_at: i64,
    /// Copy of `WindowShape::count_unit`. `None` ⇒ treat as `"tokens"`.
    /// `Some("cents")` ⇒ the `used` field is in smallest currency
    /// units (e.g. cents); the burn row renders as `$/h`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count_unit: Option<String>,
    /// Copy of `WindowShape::currency`. `None` or `""` ⇒ no currency
    /// symbol in the burn row. `Some("USD")` ⇒ prefix the rate with
    /// `$` (currently we render `$` regardless of code; v1 prototype).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    /// Model id from the API response entry (set by the parser when
    /// `WindowShape::pricing_model_path` is configured). Used by
    /// `build_menu_state` to look up per-token pricing in the
    /// cached price table and append a `· $X/h` cost fragment.
    ///
    /// `None` means "the API didn't tag this entry with a model",
    /// which is the common case for quota-only endpoints (Coding
    /// Plan, OpenRouter auth/key, Together credit, etc.) and
    /// preserves legacy behavior with no cost fragment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Outcome of a burn-rate projection.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BurnResult {
    pub rate_per_hour: f64,
    /// "token" | "pct" | "idle".
    pub mode: &'static str,
    /// "token" | "pct". Carried so labels stay right even when rate=0.
    pub unit: &'static str,
    /// ms until projected 0% at the current rate; +∞ if rate=0.
    pub exhaust_ms: f64,
    /// ms until reset (always > 0 for valid inputs).
    pub remaining_ms: i64,
    pub exhaust_before_reset: bool,
    /// What remaining_pct will be at reset if the current rate holds.
    pub projected_pct_left: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BurnConfig {
    pub enabled: bool,
    pub min_history_ms: i64,
    pub lookback_ms: i64,
    pub use_epoch_average: bool,
}

impl Default for BurnConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_history_ms: 10 * 60 * 1000,
            lookback_ms: 60 * 60 * 1000,
            use_epoch_average: true,
        }
    }
}

/// Least-squares slope of `key` over `samples`, per hour. Returns None if
/// fewer than 2 samples or zero variance in time.
pub fn slope_per_hour(samples: &[Sample], key: &str) -> Option<f64> {
    if samples.len() < 2 {
        return None;
    }
    let t0 = samples[0].t;
    let field = |s: &Sample| -> f64 {
        match key {
            "used" => s.used as f64,
            "remaining_pct" => s.remaining_pct as f64,
            _ => panic!("unknown key: {key}"),
        }
    };
    let n = samples.len() as f64;
    let sx: f64 = samples.iter().map(|s| (s.t - t0) as f64).sum();
    let sy: f64 = samples.iter().map(field).sum();
    let mx = sx / n;
    let my = sy / n;
    let mut num = 0.0;
    let mut den = 0.0;
    for s in samples {
        let dx = (s.t - t0) as f64 - mx;
        num += dx * (field(s) - my);
        den += dx * dx;
    }
    if den <= 0.0 {
        return None;
    }
    Some((num / den) * 3.6e6) // per ms → per hour
}

/// Compute burn projection for a single window. None until enough history
/// has accumulated (`min_history_ms`), or when the feature is disabled,
/// or when the window is already past its reset.
pub fn compute_burn(
    window: Option<&Window>,
    history: &[Sample],
    now: i64,
    config: &BurnConfig,
) -> Option<BurnResult> {
    let window = window?;
    if history.is_empty() {
        return None;
    }
    if !config.enabled {
        return None;
    }
    if history.len() < 2 {
        return None;
    }
    if now - history[0].t < config.min_history_ms {
        return None;
    }

    // Recent samples within the lookback window, oldest first.
    let mut recent: Vec<Sample> = Vec::with_capacity(history.len());
    for s in history.iter().rev() {
        if now - s.t > config.lookback_ms {
            break;
        }
        recent.push(*s);
    }
    recent.reverse();
    if recent.len() < 2 {
        return None;
    }

    // Token-based rate (used > 0 in recent samples, OR epoch-average floor).
    let mut token_rate: Option<f64> = None;
    if recent.iter().any(|s| s.used > 0) {
        if let Some(slope) = slope_per_hour(&recent, "used") {
            if slope > 0.0 {
                token_rate = Some(slope);
            }
        }
    }
    if config.use_epoch_average && window.start_at > 0 {
        let elapsed_ms = now - window.start_at;
        if elapsed_ms > 0 {
            let avg = (window.used as f64 / elapsed_ms as f64) * 3.6e6;
            if avg > 0.0 && token_rate.map_or(true, |t| avg > t) {
                token_rate = Some(avg);
            }
        }
    }

    // Pct-based rate. remaining_pct drops, slope is negative; store as positive.
    let pct_rate = slope_per_hour(&recent, "remaining_pct")
        .and_then(|s| if s < 0.0 { Some(-s) } else { None });

    // Mode selection: count_provider → token if available; pct otherwise;
    // pct-only idle rows keep unit='pct' so labels say "%/h" not "tok/h".
    let count_provider = window.total > 0;
    let (mode, rate, unit) = if count_provider && token_rate.is_some() {
        ("token", token_rate.unwrap(), "token")
    } else if let Some(p) = pct_rate {
        ("pct", p, "pct")
    } else if !count_provider && recent.iter().any(|s| s.remaining_pct >= 0) {
        // Idle pct-only user — preserve the unit.
        ("idle", 0.0, "pct")
    } else {
        ("idle", 0.0, "token")
    };

    let remaining_ms = std::cmp::max(0, window.reset_at - now);
    if remaining_ms <= 0 {
        return None;
    }

    let mut projected_pct_left = window.remaining_pct as f64;
    let mut exhaust_ms = f64::INFINITY;
    if mode == "pct" && rate > 0.0 {
        let hours_to_zero = window.remaining_pct as f64 / rate;
        exhaust_ms = hours_to_zero * 3.6e6;
        projected_pct_left = (window.remaining_pct as f64 - (rate * remaining_ms as f64) / 3.6e6).max(0.0);
    } else if mode == "token" && rate > 0.0 && window.total > 0 {
        let used_at_reset = window.used as f64 + (rate * remaining_ms as f64) / 3.6e6;
        projected_pct_left =
            (100.0 * (window.total as f64 - used_at_reset) / window.total as f64).max(0.0);
        // Time to exhaust: how long until used reaches total at the current rate.
        let hours_to_zero = (window.total as f64 - window.used as f64) / rate;
        exhaust_ms = hours_to_zero * 3.6e6;
    }

    Some(BurnResult {
        rate_per_hour: rate,
        mode,
        unit,
        exhaust_ms,
        remaining_ms,
        exhaust_before_reset: exhaust_ms < remaining_ms as f64,
        projected_pct_left,
    })
}

/// Should the burn row for `window` show? Same rule the renderer runs:
/// pct-only windows with no signal are suppressed (Coding Plan weekly +
/// idle 5h); token-counting windows always get a row when there's data.
pub fn decide_burn_row(
    window: Option<&Window>,
    history: &[Sample],
    now: i64,
    config: &BurnConfig,
) -> Option<BurnResult> {
    let burn = compute_burn(window, history, now, config)?;
    if burn.unit == "pct" && burn.rate_per_hour == 0.0 {
        return None; // pct idle → row suppressed
    }
    Some(burn)
}

// ---------------------------------------------------------------------------
// Tests — mirror tests/scheduler.test.js T11-T21
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000_000;
    const _TEST_CONFIG: BurnConfig = BurnConfig {
        enabled: true,
        min_history_ms: 1, // short span for unit tests; gate has its own test
        lookback_ms: 60 * 60 * 1000,
        use_epoch_average: true,
    };

    fn sample(t_offset_ms: i64, used: i64, remaining_pct: i64,
              start_at: i64, reset_at: i64) -> Sample {
        Sample {
            t: NOW + t_offset_ms,
            used,
            total: if remaining_pct >= 0 { 10_000 } else { 0 },
            remaining_pct,
            start_at,
            reset_at,
        }
    }

    fn pct_samples(pcts: &[i64], start_at: i64, reset_at: i64) -> Vec<Sample> {
        pcts.iter().enumerate().map(|(i, &pct)| {
            sample(-((pcts.len() - 1 - i) as i64) * 2 * 60_000,
                   (i as i64) * 10, pct, start_at, reset_at)
        }).collect()
    }

    fn burn(window: Option<&Window>, samples: &[Sample]) -> Option<BurnResult> {
        compute_burn(window, samples, NOW, &_TEST_CONFIG)
    }

    fn row(window: Option<&Window>, samples: &[Sample]) -> Option<BurnResult> {
        decide_burn_row(window, samples, NOW, &_TEST_CONFIG)
    }

    // ---- slope_per_hour ----

    #[test]
    fn slope_returns_none_for_too_few_samples() {
        assert!(slope_per_hour(&[], "used").is_none());
        let s = [Sample { t: 0, used: 0, total: 0, remaining_pct: 0,
                          start_at: 0, reset_at: 0 }];
        assert!(slope_per_hour(&s, "used").is_none());
    }

    #[test]
    fn slope_linear_ramp_per_hour() {
        let s = [
            Sample { t: 0,        used: 0,   total: 1000, remaining_pct: 100,
                     start_at: 0, reset_at: 0 },
            Sample { t: 500,      used: 50,  total: 1000, remaining_pct: 95,
                     start_at: 0, reset_at: 0 },
            Sample { t: 1000,     used: 100, total: 1000, remaining_pct: 90,
                     start_at: 0, reset_at: 0 },
        ];
        let slope = slope_per_hour(&s, "used").unwrap();
        assert!((slope - 360_000.0).abs() < 1e-6,
                "expected 360_000/h, got {slope}");
    }

    // ---- gating ----

    #[test]
    fn returns_none_when_disabled() {
        let w = Window { id: "5h".to_string(), total: 1000, used: 500,
                         remaining_pct: 50, start_at: NOW - 3_600_000,
                         count_unit: None,
                         currency: None,
                         model: None,
                         reset_at: NOW + 3_600_000 };
        let cfg = BurnConfig { enabled: false, ..BurnConfig::default() };
        let s = pct_samples(&[50, 49], w.start_at, w.reset_at);
        assert!(compute_burn(Some(&w), &s, NOW, &cfg).is_none());
    }

    #[test]
    fn returns_none_with_one_sample() {
        let w = Window { id: "5h".to_string(), total: 1000, used: 500,
                         remaining_pct: 50, start_at: NOW - 3_600_000,
                         count_unit: None,
                         currency: None,
                         model: None,
                         reset_at: NOW + 3_600_000 };
        let s = vec![sample(-60_000, 500, 50, w.start_at, w.reset_at)];
        assert!(burn(Some(&w), &s).is_none());
    }

    #[test]
    fn returns_none_until_min_history_ms_elapses() {
        let w = Window { id: "5h".to_string(), total: 1000, used: 500, remaining_pct: 50,
                         count_unit: None,
                         currency: None,
                         model: None,
                         start_at: NOW - 30_000, reset_at: NOW + 3_600_000 };
        let s = [
            sample(-30_000, 500, 50, w.start_at, w.reset_at),
            sample(0,       600, 40, w.start_at, w.reset_at),
        ];
        let cfg = BurnConfig { min_history_ms: 60_000, ..BurnConfig::default() };
        assert!(compute_burn(Some(&w), &s, NOW, &cfg).is_none());
    }

    #[test]
    fn returns_none_when_window_already_past_reset() {
        let w = Window { id: "5h".to_string(), total: 1000, used: 500, remaining_pct: 50,
                         count_unit: None,
                         currency: None,
                         model: None,
                         start_at: NOW - 7_200_000, reset_at: NOW - 1000 };
        let s = pct_samples(&[50, 40], w.start_at, w.reset_at);
        assert!(burn(Some(&w), &s).is_none());
    }

    // ---- token mode ----

    #[test]
    fn steep_token_burn_warns_when_exhaust_before_reset() {
        // 200 tokens / 2 min → 6000/h on 1000 total → exhausts in ~10 min, 1h left
        let w = Window { id: "5h".to_string(), total: 1000, used: 0, remaining_pct: 100,
                         count_unit: None,
                         currency: None,
                         model: None,
                         start_at: NOW - 4 * 3_600_000, reset_at: NOW + 3_600_000 };
        let s: Vec<Sample> = (0..6).map(|i| sample(
            -((5 - i) as i64) * 2 * 60_000,
            (i as i64) * 200,
            100 - (i as i64) * 20,
            w.start_at, w.reset_at)).collect();
        let result = burn(Some(&w), &s).unwrap();
        assert_eq!(result.mode, "token");
        assert_eq!(result.unit, "token");
        assert!(result.exhaust_before_reset);
        assert!((result.rate_per_hour - 6000.0).abs() < 5.0,
                "expected ~6000/h, got {}", result.rate_per_hour);
    }

    #[test]
    fn low_token_burn_does_not_warn() {
        // 2 tok / 2 min → 60/h on 10_000 total → won't exhaust in 1h
        let w = Window { id: "5h".to_string(), total: 10_000, used: 0, remaining_pct: 100,
                         count_unit: None,
                         currency: None,
                         model: None,
                         start_at: NOW - 4 * 3_600_000, reset_at: NOW + 3_600_000 };
        let s: Vec<Sample> = (0..6).map(|i| sample(
            -((5 - i) as i64) * 2 * 60_000,
            (i as i64) * 2, 100, w.start_at, w.reset_at)).collect();
        let result = burn(Some(&w), &s).unwrap();
        assert_eq!(result.mode, "token");
        assert!(!result.exhaust_before_reset);
    }

    #[test]
    fn idle_token_plan_still_gets_row_with_zero_rate() {
        let w = Window { id: "5h".to_string(), total: 10_000, used: 1000, remaining_pct: 90,
                         count_unit: None,
                         currency: None,
                         model: None,
                         start_at: 0, reset_at: NOW + 3 * 3_600_000 };
        let s: Vec<Sample> = (0..3).map(|i| sample(
            -((2 - i) as i64) * 60_000, 1000, 90, w.start_at, w.reset_at)).collect();
        let result = row(Some(&w), &s).unwrap();
        assert_eq!(result.unit, "token");
        assert_eq!(result.rate_per_hour, 0.0);
    }

    #[test]
    fn use_epoch_average_false_drops_the_floor() {
        let w = Window { id: "5h".to_string(), total: 10_000, used: 2000, remaining_pct: 80,
                         count_unit: None,
                         currency: None,
                         model: None,
                         start_at: NOW - 7_200_000, reset_at: NOW + 3_600_000 };
        let s: Vec<Sample> = (0..3).map(|i| sample(
            -((2 - i) as i64) * 60_000, 2000, 80, w.start_at, w.reset_at)).collect();
        // Default config: epoch-average kicks in (recent slope = 0, but used=2000
        // over a 2h epoch gives 1000/h via epoch-average floor).
        let with_floor = burn(Some(&w), &s).unwrap();
        // Same config but use_epoch_average: false → no floor → rate = 0.
        let cfg = BurnConfig { use_epoch_average: false, .._TEST_CONFIG };
        let no_floor = compute_burn(Some(&w), &s, NOW, &cfg).unwrap();
        assert!(with_floor.rate_per_hour > 0.0,
                "epoch-average should give positive rate, got {}", with_floor.rate_per_hour);
        assert_eq!(no_floor.rate_per_hour, 0.0,
                   "no-floor config should give rate=0, got {}", no_floor.rate_per_hour);
    }

    // ---- pct mode ----

    #[test]
    fn live_coding_plan_shape_pct_drops_each_poll() {
        let w = Window { id: "5h".to_string(), total: 0, used: 0, remaining_pct: 76,
                         count_unit: None,
                         currency: None,
                         model: None,
                         start_at: NOW - 30 * 60_000, reset_at: NOW + 4 * 3_600_000 };
        let s = pct_samples(&[80, 79, 78, 77, 76], w.start_at, w.reset_at);
        let result = burn(Some(&w), &s).unwrap();
        assert_eq!(result.mode, "pct");
        assert_eq!(result.unit, "pct");
        assert!((result.rate_per_hour - 30.0).abs() < 1.0,
                "expected ~30%/h, got {}", result.rate_per_hour);
    }

    #[test]
    fn idle_coding_plan_unit_is_pct_not_token() {
        let w = Window { id: "5h".to_string(), total: 0, used: 0, remaining_pct: 100,
                         count_unit: None,
                         currency: None,
                         model: None,
                         start_at: 0, reset_at: NOW + 5 * 3_600_000 };
        let s: Vec<Sample> = (0..3).map(|i| sample(
            -((2 - i) as i64) * 60_000, 0, 100, w.start_at, w.reset_at)).collect();
        let result = burn(Some(&w), &s).unwrap();
        assert_eq!(result.unit, "pct");
        assert_eq!(result.rate_per_hour, 0.0);
    }

    // ---- rollover ----

    #[test]
    fn new_epoch_uses_fresh_history() {
        let w = Window { id: "5h".to_string(), total: 1000, used: 0, remaining_pct: 100,
                         count_unit: None,
                         currency: None,
                         model: None,
                         start_at: NOW - 60_000, reset_at: NOW + 5 * 3_600_000 };
        let s = [
            sample(-60_000, 0,   100, w.start_at, w.reset_at),
            sample(0,       10,  99,  w.start_at, w.reset_at),
        ];
        let result = burn(Some(&w), &s).unwrap();
        // Epoch average: 10 tokens / 60 sec → 600/h
        assert!((result.rate_per_hour - 600.0).abs() < 5.0);
    }

    // ---- suppression ----

    #[test]
    fn pct_only_weekly_with_flat_data_is_suppressed() {
        let w = Window { id: "weekly".to_string(), total: 0, used: 0, remaining_pct: 90,
                         start_at: NOW - 2 * 86_400_000,
                         count_unit: None,
                         currency: None,
                         model: None,
                         reset_at: NOW + 5 * 86_400_000 };
        let s: Vec<Sample> = (0..5).map(|i| sample(
            -((4 - i) as i64) * 2 * 60_000, 0, 90, w.start_at, w.reset_at)).collect();
        assert!(burn(Some(&w), &s).is_some(),  // data exists
                "compute_burn should have data");
        assert!(row(Some(&w), &s).is_none(),   // suppressed
                "decide_burn_row should suppress pct-only idle weekly");
    }

    #[test]
    fn pct_only_5h_active_ticks_is_shown() {
        let w = Window { id: "5h".to_string(), total: 0, used: 0, remaining_pct: 76,
                         count_unit: None,
                         currency: None,
                         model: None,
                         start_at: NOW - 30 * 60_000, reset_at: NOW + 4 * 3_600_000 };
        let s = pct_samples(&[80, 79, 78, 77, 76], w.start_at, w.reset_at);
        let result = row(Some(&w), &s).unwrap();
        assert_eq!(result.mode, "pct");
        assert!(result.rate_per_hour > 0.0);
    }

    #[test]
    fn pct_only_5h_idle_is_suppressed() {
        let w = Window { id: "5h".to_string(), total: 0, used: 0, remaining_pct: 100,
                         count_unit: None,
                         currency: None,
                         model: None,
                         start_at: NOW - 30 * 60_000, reset_at: NOW + 5 * 3_600_000 };
        let s: Vec<Sample> = (0..3).map(|i| sample(
            -((2 - i) as i64) * 60_000, 0, 100, w.start_at, w.reset_at)).collect();
        assert!(row(Some(&w), &s).is_none());
    }

    #[test]
    fn token_plan_idle_still_shows_row() {
        let w = Window { id: "5h".to_string(), total: 10_000, used: 1000, remaining_pct: 90,
                         count_unit: None,
                         currency: None,
                         model: None,
                         start_at: NOW - 3_600_000, reset_at: NOW + 3 * 3_600_000 };
        let s: Vec<Sample> = (0..3).map(|i| sample(
            -((2 - i) as i64) * 60_000, 1000, 90, w.start_at, w.reset_at)).collect();
        assert!(row(Some(&w), &s).is_some());
    }

    // ---- independence ----

    #[test]
    fn rate_is_independent_of_unrelated_history() {
        let w = Window { id: "5h".to_string(), total: 0, used: 0, remaining_pct: 80,
                         count_unit: None,
                         currency: None,
                         model: None,
                         start_at: NOW - 30 * 60_000, reset_at: NOW + 4 * 3_600_000 };
        let five_h = pct_samples(&[80, 79, 78, 77, 76], w.start_at, w.reset_at);
        let weekly = pct_samples(&[90, 90, 90, 90, 90],
                                 NOW - 2 * 86_400_000, NOW + 5 * 86_400_000);
        assert!(burn(Some(&w), &five_h).unwrap().rate_per_hour > 0.0);
        assert_eq!(burn(Some(&w), &weekly).unwrap().rate_per_hour, 0.0,
                   "wrong history should give idle");
    }
}