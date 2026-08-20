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

/// Convert seconds-since-epoch from the API into ms-since-epoch for our
/// internal Window shape.
fn secs_to_ms(s: i64) -> i64 {
    s * 1000
}

/// Parse one window's worth of fields out of the API JSON entry.
pub fn parse_interval_v2(v: &Value, prefix: &str, id: &'static str) -> Window {
    let total = num(v, &format!("{prefix}_total_count"));
    let used = num(v, &format!("{prefix}_usage_count"));
    let remaining_pct = pct(v, &format!("{prefix}_remaining_percent"));
    let start_at = secs_to_ms(num(v, &start_key(prefix)));
    let reset_at = secs_to_ms(num(v, &reset_key(prefix)));
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
pub fn parse_coding_plan(payload: &Value) -> Result<(Window, Window)> {
    let entry = payload
        .get("model_remains")
        .and_then(|m| m.as_array())
        .and_then(|a| a.first())
        .ok_or_else(|| anyhow::anyhow!("payload missing model_remains[0]"))?;
    let five_h = parse_interval_v2(entry, "current_interval", "5h");
    let weekly = parse_interval_v2(entry, "current_weekly", "weekly");
    Ok((five_h, weekly))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> Value {
        json!({
            "model_remains": [{
                "model_name": "general",
                "current_interval_total_count": 1000,
                "current_interval_usage_count": 200,
                "current_interval_remaining_percent": 80,
                "start_time": 1700000000,
                "remains_time": 1700018000,
                "current_interval_status": 1,
                "current_weekly_total_count": 5000,
                "current_weekly_usage_count": 1000,
                "current_weekly_remaining_percent": 80,
                "weekly_start_time": 1699900000,
                "weekly_remains_time": 1700504000,
                "current_weekly_status": 1
            }]
        })
    }

    #[test]
    fn parses_full_coding_plan() {
        let (five_h, weekly) = parse_coding_plan(&fixture()).unwrap();
        assert_eq!(five_h.id, "5h");
        assert_eq!(five_h.total, 1000);
        assert_eq!(five_h.used, 200);
        assert_eq!(five_h.remaining_pct, 80);
        assert_eq!(five_h.start_at, 1700000000 * 1000);
        assert_eq!(five_h.reset_at, 1700018000 * 1000);

        assert_eq!(weekly.id, "weekly");
        assert_eq!(weekly.total, 5000);
        assert_eq!(weekly.used, 1000);
        assert_eq!(weekly.remaining_pct, 80);
        assert_eq!(weekly.start_at, 1699900000 * 1000);
        assert_eq!(weekly.reset_at, 1700504000 * 1000);
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
        let (five_h, weekly) = parse_coding_plan(&v).unwrap();
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
        let (five_h, weekly) = parse_coding_plan(&v).unwrap();
        assert_eq!(five_h.total, 0);
        assert_eq!(five_h.used, 0);
        assert_eq!(five_h.remaining_pct, 80);
        assert_eq!(weekly.total, 0);
        assert_eq!(weekly.remaining_pct, 90);
    }

    #[test]
    fn missing_model_remains_returns_error() {
        let v = json!({});
        assert!(parse_coding_plan(&v).is_err());
    }

    #[test]
    fn empty_model_remains_returns_error() {
        let v = json!({"model_remains": []});
        assert!(parse_coding_plan(&v).is_err());
    }

    #[test]
    fn float_seconds_coerced() {
        // Some APIs return fractional seconds.
        let v = json!({
            "model_remains": [{
                "model_name": "x",
                "current_interval_total_count": 0,
                "current_interval_usage_count": 0,
                "current_interval_remaining_percent": 80,
                "start_time": 1700000000.7,
                "remains_time": 1700018000.2,
                "current_interval_status": 1,
                "current_weekly_total_count": 0,
                "current_weekly_usage_count": 0,
                "current_weekly_remaining_percent": 90,
                "weekly_start_time": 1699900000.0,
                "weekly_remains_time": 1700504000.9,
                "current_weekly_status": 1
            }]
        });
        let (five_h, weekly) = parse_coding_plan(&v).unwrap();
        assert_eq!(five_h.start_at, 1700000000 * 1000);
        assert_eq!(five_h.reset_at, 1700018000 * 1000);
        assert_eq!(weekly.reset_at, 1700504000 * 1000);
    }
}