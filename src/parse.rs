//! Parse the Coding Plan / Token Plan JSON payload into the internal
//! window shape used by the burn-rate and menu code.
//!
//! Live shape (verified 2026-08-18):
//!   {
//!     "model_remains": [{
//!       "model_name": "general",
//!       "current_interval_total_count":    <i64>,   // 0 on Coding Plan
//!       "current_interval_usage_count":    <i64>,   // 0 on Coding Plan
//!       "current_interval_remaining_percent": <i64>,
//!       "start_time":    <seconds-since-epoch>,
//!       "remains_time":  <seconds-since-epoch>,   // absolute reset time
//!       "current_interval_status": <i64>,
//!       "current_weekly_total_count":    <i64>,   // 0 on Coding Plan
//!       "current_weekly_usage_count":    <i64>,   // 0 on Coding Plan
//!       "current_weekly_remaining_percent": <i64>,
//!       "weekly_start_time":   <seconds-since-epoch>,
//!       "weekly_remains_time": <seconds-since-epoch>,
//!       "current_weekly_status": <i64>
//!     }]
//!   }
//!
//! Token Plan may use a different shape — fields are read defensively
//! via `Option::or_default` so missing keys degrade to zeros.

use anyhow::Result;
use serde_json::Value;

use crate::burn::Window;

fn num(v: &Value, key: &str) -> i64 {
    v.get(key)
        .and_then(|x| x.as_i64().or_else(|| x.as_f64().map(|f| f as i64)))
        .unwrap_or(0)
}

fn pct(v: &Value, key: &str) -> i64 {
    num(v, key).clamp(0, 100)
}

/// Parse one window's worth of fields out of the API JSON entry.
///
/// `now_ms` is the current epoch ms — needed because the API's
/// `remains_time` is a **duration** in ms (time until reset), not an
/// absolute epoch. We add `now_ms + remains_time_ms` to produce the
/// `reset_at` field, matching the gjs `parseWindow()` which does
/// `resetAt: nowFn() + resetMs`. Treating `remains_time` as
/// seconds-since-epoch and multiplying by 1000 (the previous Rust
/// behavior) gave absurd reset_at values ~144 days from epoch for
/// a 5h window — the "resets in 4h" line in the menu became
/// "resets in 144d 5h", which is why the reset countdown was broken.
///
/// The `start_time` field IS in seconds-since-epoch, so we multiply
/// by 1000 (a one-time conversion that the gjs version skips because
/// it doesn't need cross-window consistency for the burn projection).
pub fn parse_interval_v2(v: &Value, prefix: &str, id: &'static str, now_ms: i64) -> Window {
    let total = num(v, &format!("{prefix}_total_count"));
    let used = num(v, &format!("{prefix}_usage_count"));
    let remaining_pct = pct(v, &format!("{prefix}_remaining_percent"));
    let start_at = num(v, &start_key(prefix)) * 1000; // sec → ms
    let remains_ms = num(v, &reset_key(prefix));       // already ms
    let reset_at = now_ms + remains_ms;
    Window { id, total, used, remaining_pct, start_at, reset_at }
}

fn start_key(prefix: &str) -> String {
    if prefix == "current_weekly" { "weekly_start_time".to_string() }
    else { "start_time".to_string() }
}

fn reset_key(prefix: &str) -> String {
    if prefix == "current_weekly" { "weekly_remains_time".to_string() }
    else { "remains_time".to_string() }
}

/// Parse the Coding Plan payload (5h + weekly on the same entry).
/// `now_ms` is the current epoch ms; see `parse_interval_v2` for why.
pub fn parse_coding_plan(payload: &Value, now_ms: i64) -> Result<(Window, Window)> {
    let entry = payload
        .get("model_remains")
        .and_then(|m| m.as_array())
        .and_then(|a| a.first())
        .ok_or_else(|| anyhow::anyhow!("payload missing model_remains[0]"))?;
    let five_h = parse_interval_v2(entry, "current_interval", "5h", now_ms);
    let weekly = parse_interval_v2(entry, "current_weekly", "weekly", now_ms);
    Ok((five_h, weekly))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Fixture uses realistic values: `start_time` in epoch seconds,
    /// `remains_time` in ms (a 4.5-hour reset for the 5h window and a
    /// 6.5-day reset for the weekly window — matches what the live API
    /// returns). The Rust port originally treated `remains_time` as
    /// epoch seconds (multiplying by 1000), which gave 5h windows a
    /// reset countdown of "144 days" instead of "4h 32m". This
    /// fixture reflects the corrected semantics.
    fn fixture() -> Value {
        json!({
            "model_remains": [{
                "model_name": "general",
                "current_interval_total_count": 1000,
                "current_interval_usage_count": 200,
                "current_interval_remaining_percent": 80,
                "start_time": 1632000000,
                "remains_time": 16320000,
                "current_interval_status": 1,
                "current_weekly_total_count": 5000,
                "current_weekly_usage_count": 1000,
                "current_weekly_remaining_percent": 80,
                "weekly_start_time": 1631000000,
                "weekly_remains_time": 561600000,
                "current_weekly_status": 1
            }]
        })
    }

    /// Anchor time for tests. Picked so the 5h window math checks out
    /// exactly: `start_time = 1_632_000_000`, `remains_time = 16_320_000`
    /// (4.533h), `window_length = 18_000_000` (5h), so
    /// `TEST_NOW_MS = start + 5h − remains = 1_632_018_000_000 − 16_320_000`.
    /// Matches gjs's parseWindow: `resetAt: nowFn() + resetMs`.
    const TEST_NOW_MS: i64 = 1_632_001_680_000;

    #[test]
    fn parses_full_coding_plan() {
        let (five_h, weekly) = parse_coding_plan(&fixture(), TEST_NOW_MS).unwrap();
        assert_eq!(five_h.id, "5h");
        assert_eq!(five_h.total, 1000);
        assert_eq!(five_h.used, 200);
        assert_eq!(five_h.remaining_pct, 80);
        // start_time is in seconds-since-epoch → multiply by 1000.
        assert_eq!(five_h.start_at, 1_632_000_000 * 1000);
        // remains_time is a duration in ms → add to now directly.
        assert_eq!(five_h.reset_at, TEST_NOW_MS + 16_320_000);

        assert_eq!(weekly.id, "weekly");
        assert_eq!(weekly.total, 5000);
        assert_eq!(weekly.used, 1000);
        assert_eq!(weekly.remaining_pct, 80);
        assert_eq!(weekly.start_at, 1_631_000_000 * 1000);
        assert_eq!(weekly.reset_at, TEST_NOW_MS + 561_600_000);
    }

    /// Sanity check: `reset_at - now` is the remaining time. This is
    /// what the menu's "resets in X" countdown is derived from —
    /// gjs parity requires this math to be correct.
    #[test]
    fn reset_at_minus_now_is_time_remaining() {
        let (five_h, weekly) = parse_coding_plan(&fixture(), TEST_NOW_MS).unwrap();
        // Time until reset equals `remains_time` (the raw duration
        // in ms, no conversion — the bug we just fixed).
        assert_eq!(five_h.reset_at - TEST_NOW_MS, 16_320_000); // 4.53h
        assert_eq!(weekly.reset_at - TEST_NOW_MS, 561_600_000); // 6.5d
        // 5h window length is exactly 5h = 18_000_000 ms (since
        // TEST_NOW_MS is picked to make this true).
        assert_eq!(five_h.reset_at - five_h.start_at, 18_000_000);
    }

    #[test]
    fn clamps_pct_to_0_100() {
        let v = json!({
            "model_remains": [{
                "model_name": "x",
                "current_interval_total_count": 0,
                "current_interval_usage_count": 0,
                "current_interval_remaining_percent": 150,
                "start_time": 0, "remains_time": 1,
                "current_interval_status": 1,
                "current_weekly_total_count": 0,
                "current_weekly_usage_count": 0,
                "current_weekly_remaining_percent": -50,
                "weekly_start_time": 0, "weekly_remains_time": 1,
                "current_weekly_status": 1
            }]
        });
        let (five_h, weekly) = parse_coding_plan(&v, TEST_NOW_MS).unwrap();
        assert_eq!(five_h.remaining_pct, 100);
        assert_eq!(weekly.remaining_pct, 0);
    }

    #[test]
    fn live_coding_plan_shape_zero_counts() {
        // 0/0 on count fields — this is the live Coding Plan shape.
        let v = json!({
            "model_remains": [{
                "model_name": "general",
                "current_interval_total_count": 0,
                "current_interval_usage_count": 0,
                "current_interval_remaining_percent": 80,
                "start_time": 1700000000,
                "remains_time": 1700018000,
                "current_interval_status": 1,
                "current_weekly_total_count": 0,
                "current_weekly_usage_count": 0,
                "current_weekly_remaining_percent": 90,
                "weekly_start_time": 1699900000,
                "weekly_remains_time": 1700504000,
                "current_weekly_status": 1
            }]
        });
        let (five_h, weekly) = parse_coding_plan(&v, TEST_NOW_MS).unwrap();
        assert_eq!(five_h.total, 0);
        assert_eq!(five_h.used, 0);
        assert_eq!(five_h.remaining_pct, 80);
        assert_eq!(weekly.total, 0);
        assert_eq!(weekly.remaining_pct, 90);
    }

    #[test]
    fn missing_model_remains_returns_error() {
        let v = json!({});
        assert!(parse_coding_plan(&v, 0).is_err());
    }

    #[test]
    fn empty_model_remains_returns_error() {
        let v = json!({"model_remains": []});
        assert!(parse_coding_plan(&v, 0).is_err());
    }

    #[test]
    fn float_seconds_coerced() {
        // Some APIs return fractional seconds (e.g. for `start_time`).
        // The Rust parser should truncate to integer seconds before
        // multiplying by 1000.
        let v = json!({
            "model_remains": [{
                "model_name": "x",
                "current_interval_total_count": 0,
                "current_interval_usage_count": 0,
                "current_interval_remaining_percent": 80,
                "start_time": 1632000000.7,
                "remains_time": 16320000.2,
                "current_interval_status": 1,
                "current_weekly_total_count": 0,
                "current_weekly_usage_count": 0,
                "current_weekly_remaining_percent": 90,
                "weekly_start_time": 1631000000.0,
                "weekly_remains_time": 561600000.9,
                "current_weekly_status": 1
            }]
        });
        let (five_h, weekly) = parse_coding_plan(&v, TEST_NOW_MS).unwrap();
        assert_eq!(five_h.start_at, 1_632_000_000 * 1000);
        // `remains_time` is in ms (the float is truncated to integer ms),
        // so reset_at = now + 16320000 (the rounded ms value).
        assert_eq!(five_h.reset_at, TEST_NOW_MS + 16_320_000);
        assert_eq!(weekly.reset_at, TEST_NOW_MS + 561_600_000);
    }
}