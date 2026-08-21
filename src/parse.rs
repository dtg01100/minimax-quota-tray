//! Parse a provider's JSON payload into the internal `Window` shape
//! used by the burn-rate and menu code.
//!
//! All provider-specific details (JSON field names, unit conversions,
//! error envelopes) live in `crate::provider` — this module is purely
//! the data-driven reader. To port to a different API, edit
//! `src/provider.rs`, not this file.
//!
//! The parser returns `Vec<Window>` with one entry per window the
//! `PlanShape` defines. There's no fixed pair — `main.rs` consumes
//! whatever windows the parser produces and renders one menu row per
//! window; the first window drives the chip percentage.
//!
//! For the MiniMax live shape (verified 2026-08-18) see the doc on
//! `provider::MINIMAX_REMAINS`. The Coding Plan and Token Plan use
//! the same shape, just different endpoints.

use anyhow::Result;
use serde_json::Value;

use crate::burn::Window;
use crate::provider::{PlanShape, WindowShape};

fn num(v: &Value, key: &str) -> i64 {
    v.get(key)
        .and_then(|x| x.as_i64().or_else(|| x.as_f64().map(|f| f as i64)))
        .unwrap_or(0)
}

fn pct(v: &Value, key: &str) -> i64 {
    num(v, key).clamp(0, 100)
}

/// Parse one window's worth of fields out of the API JSON entry,
/// using the `WindowShape` data to find the right field names and
/// apply the right unit conversions.
///
/// `now_ms` is the current epoch ms — needed when the provider
/// returns the reset as a DURATION (time-until-reset, default), in
/// which case the parser computes `reset_at = now_ms + raw_reset`.
/// Providers that return an absolute epoch instead set
/// `reset_is_absolute_epoch: true` and the parser uses the raw value
/// (after `reset_unit_ms` scaling).
fn parse_one_window(entry: &Value, w: &WindowShape, now_ms: i64) -> Window {
    let total = num(entry, &format!("{}_total_count", w.field_prefix));
    let used = num(entry, &format!("{}_usage_count", w.field_prefix));
    let remaining_pct = pct(entry, &format!("{}_remaining_percent", w.field_prefix));
    let start_field = w.start_field.unwrap_or("start_time");
    let reset_field = w.reset_field.unwrap_or("remains_time");
    let start_at = num(entry, start_field) * w.start_unit_ms;
    let raw_reset = num(entry, reset_field) * w.reset_unit_ms;
    let reset_at = if w.reset_is_absolute_epoch {
        raw_reset
    } else {
        now_ms + raw_reset
    };
    Window { id: w.id, total, used, remaining_pct, start_at, reset_at }
}

/// Look up the first entry in `payload` according to `shape`. For an
/// array-typed `entries_path` (the MiniMax case: `/model_remains`),
/// returns the element at index 0. For `/` (single-object response),
/// returns the payload itself.
fn first_entry<'a>(payload: &'a Value, shape: &PlanShape) -> Option<&'a Value> {
    if shape.entries_path == "/" {
        Some(payload)
    } else {
        payload
            .pointer(shape.entries_path)
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
    }
}

/// Parse one payload against a `PlanShape` description, returning all
/// windows the shape defines.
///
/// Returns an error if the entry can't be located (missing array,
/// empty array, etc.). Field-level missing values degrade to zeros
/// (the parser is defensive — providers sometimes drop optional
/// fields).
pub fn parse_plan(payload: &Value, shape: &PlanShape, now_ms: i64) -> Result<Vec<Window>> {
    let entry = first_entry(payload, shape)
        .ok_or_else(|| anyhow::anyhow!(
            "payload missing entry at {}", shape.entries_path))?;
    Ok(shape.windows.iter()
        .map(|w| parse_one_window(entry, w, now_ms))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::MINIMAX_REMAINS;
    use serde_json::json;

    /// Convenience: parse the standard fixture and destructure the
    /// resulting Vec into the canonical 5h/weekly pair (MiniMax
    /// always produces exactly 2 windows for the `remains` shape).
    fn parse_minimax(payload: &Value, now_ms: i64) -> (crate::burn::Window, crate::burn::Window) {
        let windows = parse_plan(payload, &MINIMAX_REMAINS, now_ms)
            .expect("parse_plan(MINIMAX_REMAINS) should succeed on the fixture");
        assert_eq!(windows.len(), 2,
                   "MINIMAX_REMAINS shape should produce exactly 2 windows");
        (windows[0], windows[1])
    }

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
        let (five_h, weekly) = parse_minimax(&fixture(), TEST_NOW_MS);
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
        let (five_h, weekly) = parse_minimax(&fixture(), TEST_NOW_MS);
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
        let (five_h, weekly) = parse_minimax(&v, TEST_NOW_MS);
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
        let (five_h, weekly) = parse_minimax(&v, TEST_NOW_MS);
        assert_eq!(five_h.total, 0);
        assert_eq!(five_h.used, 0);
        assert_eq!(five_h.remaining_pct, 80);
        assert_eq!(weekly.total, 0);
        assert_eq!(weekly.remaining_pct, 90);
    }

    #[test]
    fn missing_model_remains_returns_error() {
        let v = json!({});
        assert!(parse_plan(&v, &MINIMAX_REMAINS, 0).is_err());
    }

    #[test]
    fn empty_model_remains_returns_error() {
        let v = json!({"model_remains": []});
        assert!(parse_plan(&v, &MINIMAX_REMAINS, 0).is_err());
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
        let (five_h, weekly) = parse_minimax(&v, TEST_NOW_MS);
        assert_eq!(five_h.start_at, 1_632_000_000 * 1000);
        // `remains_time` is in ms (the float is truncated to integer ms),
        // so reset_at = now + 16320000 (the rounded ms value).
        assert_eq!(five_h.reset_at, TEST_NOW_MS + 16_320_000);
        assert_eq!(weekly.reset_at, TEST_NOW_MS + 561_600_000);
    }

    #[test]
    fn single_window_shape_works() {
        // Prove the parser is generic over N windows: define a
        // shape with one window and verify the Vec has one entry.
        // The entry path is `/` (the root object IS the entry —
        // `first_entry` wraps it into a one-element synthetic array).
        use crate::provider::{PlanShape, WindowShape};
        let shape = PlanShape {
            entries_path: "/",
            windows: &[WindowShape {
                id: "daily",
                field_prefix: "daily",
                start_field: None,
                reset_field: None,
                start_unit_ms: 1000,
                reset_unit_ms: 1,
                reset_is_absolute_epoch: false,
            }],
            error_envelope: None,
        };
        let v = json!({
            "daily_total_count": 1000,
            "daily_usage_count": 200,
            "daily_remaining_percent": 80,
            "start_time": 1700000000,
            "remains_time": 7200000,
        });
        let windows = parse_plan(&v, &shape, 1_700_000_000_000).unwrap();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].id, "daily");
        assert_eq!(windows[0].remaining_pct, 80);
        assert_eq!(windows[0].reset_at, 1_700_000_000_000 + 7_200_000);
    }

    #[test]
    fn three_window_shape_works() {
        // And the N>2 case — three windows from one shape, all sharing
        // the same entry object.
        use crate::provider::{PlanShape, WindowShape};
        let shape = PlanShape {
            entries_path: "/",
            windows: &[
                WindowShape {
                    id: "m5", field_prefix: "m5",
                    start_field: None, reset_field: None,
                    start_unit_ms: 1, reset_unit_ms: 1, reset_is_absolute_epoch: false,
                },
                WindowShape {
                    id: "h1", field_prefix: "h1",
                    start_field: None, reset_field: None,
                    start_unit_ms: 1, reset_unit_ms: 1, reset_is_absolute_epoch: false,
                },
                WindowShape {
                    id: "d1", field_prefix: "d1",
                    start_field: None, reset_field: None,
                    start_unit_ms: 1, reset_unit_ms: 1, reset_is_absolute_epoch: false,
                },
            ],
            error_envelope: None,
        };
        let v = json!({
            "m5_total_count": 0, "m5_usage_count": 0, "m5_remaining_percent": 80,
            "start_time": 0, "remains_time": 100,
            "h1_total_count": 0, "h1_usage_count": 0, "h1_remaining_percent": 50,
            "start_time": 0, "remains_time": 200,
            "d1_total_count": 0, "d1_usage_count": 0, "d1_remaining_percent": 25,
            "start_time": 0, "remains_time": 300,
        });
        let windows = parse_plan(&v, &shape, 1000).unwrap();
        assert_eq!(windows.len(), 3);
        assert_eq!(windows[0].id, "m5");
        assert_eq!(windows[0].remaining_pct, 80);
        assert_eq!(windows[1].id, "h1");
        assert_eq!(windows[1].remaining_pct, 50);
        assert_eq!(windows[2].id, "d1");
        assert_eq!(windows[2].remaining_pct, 25);
    }
}