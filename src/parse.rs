//! Parse a per-instance JSON payload into the internal `Window`
//! shape used by the burn-rate and menu code.
//!
//! All provider-specific details (JSON field names, unit conversions,
//! error envelopes) come from the `PlanShape` in the instance's
//! `config.json`. This module is the data-driven reader — no provider
//! constants live here.
//!
//! The parser returns `Vec<Window>` with one entry per window the
//! `PlanShape` defines. `main.rs` consumes whatever windows the
//! parser produces and renders one menu row per window; the first
//! window drives the chip percentage.

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

/// Read an optional string field at a JSON-pointer-relative path
/// from the entry. Used by `pricing_model_path` to pluck a model
/// id out of the API response. Returns `None` for missing /
/// non-string values (defensive: the parser never errors on
/// optional fields).
///
/// Path is a relative JSON pointer (e.g. `"/model_name"` or
/// `"/data/0/model"`). Unlike the existing `start_field` /
/// `reset_field` overrides (which take a plain key), model paths
/// can traverse into nested objects/arrays because providers
/// nest their model tags inconsistently.
fn model_id_at(entry: &Value, path: &str) -> Option<String> {
    entry.pointer(path)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
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
    let start_field = w.start_field.as_deref().unwrap_or("start_time");
    let reset_field = w.reset_field.as_deref().unwrap_or("remains_time");
    let start_at = num(entry, start_field) * w.start_unit_ms;
    let raw_reset = num(entry, reset_field) * w.reset_unit_ms;
    let reset_at = if w.reset_is_absolute_epoch {
        raw_reset
    } else {
        now_ms + raw_reset
    };
    // Read the model id from the entry if `pricing_model_path` is
    // configured. None when unset or when the field is missing
    // (defensive — never errors).
    let model = w.pricing_model_path.as_deref()
        .and_then(|p| model_id_at(entry, p));
    Window {
        id: w.id.clone(),
        total,
        used,
        remaining_pct,
        start_at,
        reset_at,
        // Prototype fields: carried on Window so the renderer doesn't
        // need a side-table lookup of `WindowShape`. Both default to
        // None, which means "tokens" / "no currency symbol" — same
        // legacy behavior as before this change.
        count_unit: w.count_unit.clone(),
        currency: w.currency.clone(),
        model,
    }
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
            .pointer(&shape.entries_path)
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
    use serde_json::json;

    /// The standard MiniMax shape — a 2-window shape with the
    /// `model_remains` entry array, `current_interval_*` / `current_weekly_*`
    /// field-name prefixes, and the `base_resp` error envelope. Tests
    /// that need a real-world shape build this via JSON so we don't
    /// need a compile-time MiniMax-specific constant.
    fn minimax_shape() -> PlanShape {
        serde_json::from_value(json!({
            "entries_path": "/model_remains",
            "windows": [
                {
                    "id": "5h",
                    "field_prefix": "current_interval",
                    "start_unit_ms": 1000,
                    "reset_unit_ms": 1,
                    "reset_is_absolute_epoch": false
                },
                {
                    "id": "weekly",
                    "field_prefix": "current_weekly",
                    "start_field": "weekly_start_time",
                    "reset_field": "weekly_remains_time",
                    "start_unit_ms": 1000,
                    "reset_unit_ms": 1,
                    "reset_is_absolute_epoch": false
                }
            ],
            "error_envelope": {
                "code_path": "/base_resp/status_code",
                "message_path": "/base_resp/status_msg",
                "success_codes": [0]
            }
        })).expect("test fixture shape should deserialize")
    }

    /// Convenience: parse the standard fixture and destructure the
    /// resulting Vec into the canonical 5h/weekly pair (MiniMax
    /// always produces exactly 2 windows for the `remains` shape).
    fn parse_minimax(payload: &Value, now_ms: i64) -> (crate::burn::Window, crate::burn::Window) {
        let shape = minimax_shape();
        let mut windows = parse_plan(payload, &shape, now_ms)
            .expect("parse_plan(MiniMax shape) should succeed on the fixture");
        assert_eq!(windows.len(), 2,
                   "MiniMax shape should produce exactly 2 windows");
        // Vec::remove swaps the tail in, so this is O(1) — but
        // for a 2-element test Vec it doesn't matter.
        let weekly = windows.pop().expect("len checked");
        let five_h = windows.pop().expect("len checked");
        (five_h, weekly)
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

    /// OpenRouter prototype: confirm the new optional WindowShape
    /// fields (`count_unit`, `currency`) deserialize and round-trip
    /// through the parser into Window. Locks in that an OpenRouter
    /// config can drop `"count_unit": "cents"` in and have it land
    /// on the Window the renderer reads.
    #[test]
    fn parses_count_unit_and_currency() {
        let shape: PlanShape = serde_json::from_value(json!({
            "entries_path": "/data",
            "windows": [{
                "id": "credits",
                "field_prefix": "credit",
                "start_unit_ms": 1000,
                "reset_unit_ms": 1,
                "reset_is_absolute_epoch": false,
                "count_unit": "cents",
                "currency": "USD"
            }],
            "error_envelope": null
        })).expect("shape with count_unit + currency should deserialize");

        // Now feed it a payload that looks like OpenRouter's auth/key
        // response after an adapter has scaled USD floats to cents.
        // We don't need real auth/key semantics here — we just need to
        // verify the two new fields survive the parse.
        let v = json!({
            "data": [{
                "credit_total_count": 1000,    // $10.00 cap
                "credit_usage_count": 250,     // $2.50 used
                "credit_remaining_percent": 75,
                "credit_start_time": 1700000000,
                "credit_remains_time": 2592000000_i64  // 30d
            }]
        });
        let mut windows = parse_plan(&v, &shape, 1_700_001_000_000).unwrap();
        assert_eq!(windows.len(), 1);
        let w = windows.pop().unwrap();
        assert_eq!(w.id, "credits");
        assert_eq!(w.used, 250);
        assert_eq!(w.total, 1000);
        assert_eq!(w.remaining_pct, 75);
        // The new fields made it through parse_one_window intact.
        assert_eq!(w.count_unit.as_deref(), Some("cents"));
        assert_eq!(w.currency.as_deref(), Some("USD"));
    }

    /// OpenRouter inference prototype: the parser reads the model id
    /// via `pricing_model_path` and lands it on `Window.model` for
    /// the renderer. This is what lets the burn row show a cost
    /// fragment when the cached price table knows the model.
    #[test]
    fn parses_model_id_through_pricing_model_path() {
        // OpenRouter's inference response wraps usage in `usage` and
        // names the model at the top level. Two windows read the
        // same entry but with different model paths (some providers
        // nest the model id per-window — same shape works).
        let shape: PlanShape = serde_json::from_value(json!({
            "entries_path": "/",
            "windows": [{
                "id": "session",
                "field_prefix": "session",
                "start_unit_ms": 1000,
                "reset_unit_ms": 1,
                "reset_is_absolute_epoch": false,
                "pricing_model_path": "/model"
            }]
        })).unwrap();

        let v = json!({
            "model": "openai/gpt-4o",
            "session_total_count": 0,
            "session_usage_count": 1234,
            "session_remaining_percent": 95,
            "start_time": 0,
            "remains_time": 86_400_000
        });
        let mut windows = parse_plan(&v, &shape, 0).unwrap();
        let w = windows.pop().unwrap();
        assert_eq!(w.model.as_deref(), Some("openai/gpt-4o"));
        assert_eq!(w.used, 1234);
    }

    /// Deep nested model path (JSON pointer) — e.g. providers that
    /// return `data[0].model`.
    #[test]
    fn parses_model_id_via_nested_pointer() {
        let shape: PlanShape = serde_json::from_value(json!({
            "entries_path": "/data",
            "windows": [{
                "id": "day",
                "field_prefix": "day",
                "start_unit_ms": 1000,
                "reset_unit_ms": 1,
                "reset_is_absolute_epoch": false,
                "pricing_model_path": "/model"
            }]
        })).unwrap();
        let v = json!({
            "data": [{
                "model": "deepseek/deepseek-v4-flash",
                "day_total_count": 0,
                "day_usage_count": 500,
                "day_remaining_percent": 99,
                "start_time": 0,
                "remains_time": 86_400_000
            }]
        });
        let mut windows = parse_plan(&v, &shape, 0).unwrap();
        let w = windows.pop().unwrap();
        assert_eq!(w.model.as_deref(), Some("deepseek/deepseek-v4-flash"));
    }

    /// Missing model field is fine — the parser doesn't error, the
    /// Window just has `model = None` and the renderer omits the
    /// cost fragment.
    #[test]
    fn missing_model_field_yields_none() {
        let shape: PlanShape = serde_json::from_value(json!({
            "entries_path": "/",
            "windows": [{
                "id": "x",
                "field_prefix": "x",
                "start_unit_ms": 1,
                "reset_unit_ms": 1,
                "reset_is_absolute_epoch": false,
                "pricing_model_path": "/model"
            }]
        })).unwrap();
        let v = json!({
            // No `model` key
            "x_total_count": 0,
            "x_usage_count": 100,
            "x_remaining_percent": 90,
            "start_time": 0,
            "remains_time": 1
        });
        let mut windows = parse_plan(&v, &shape, 0).unwrap();
        let w = windows.pop().unwrap();
        assert!(w.model.is_none());
    }

    /// Backward-compat: a legacy config without count_unit / currency
    /// still parses, and the resulting Window has both fields as None.
    #[test]
    fn legacy_shape_omits_count_unit() {
        let shape: PlanShape = serde_json::from_value(json!({
            "entries_path": "/model_remains",
            "windows": [{
                "id": "5h",
                "field_prefix": "current_interval",
                "start_unit_ms": 1000,
                "reset_unit_ms": 1,
                "reset_is_absolute_epoch": false
            }]
        })).unwrap();
        let v = json!({
            "model_remains": [{
                "current_interval_total_count": 1000,
                "current_interval_usage_count": 200,
                "current_interval_remaining_percent": 80,
                "start_time": 0,
                "remains_time": 1000
            }]
        });
        let mut windows = parse_plan(&v, &shape, 0).unwrap();
        let w = windows.pop().unwrap();
        assert!(w.count_unit.is_none(), "legacy config ⇒ None");
        assert!(w.currency.is_none(), "legacy config ⇒ None");
    }

    #[test]
    fn missing_model_remains_returns_error() {
        let v = json!({});
        let shape = minimax_shape();
        assert!(parse_plan(&v, &shape, 0).is_err());
    }

    #[test]
    fn empty_model_remains_returns_error() {
        let v = json!({"model_remains": []});
        let shape = minimax_shape();
        assert!(parse_plan(&v, &shape, 0).is_err());
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
        let shape: PlanShape = serde_json::from_value(json!({
            "entries_path": "/",
            "windows": [{
                "id": "daily",
                "field_prefix": "daily",
                "start_unit_ms": 1000,
                "reset_unit_ms": 1,
                "reset_is_absolute_epoch": false
            }]
        })).unwrap();
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
        let shape: PlanShape = serde_json::from_value(json!({
            "entries_path": "/",
            "windows": [
                { "id": "m5", "field_prefix": "m5",
                  "start_unit_ms": 1, "reset_unit_ms": 1, "reset_is_absolute_epoch": false },
                { "id": "h1", "field_prefix": "h1",
                  "start_unit_ms": 1, "reset_unit_ms": 1, "reset_is_absolute_epoch": false },
                { "id": "d1", "field_prefix": "d1",
                  "start_unit_ms": 1, "reset_unit_ms": 1, "reset_is_absolute_epoch": false }
            ]
        })).unwrap();
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