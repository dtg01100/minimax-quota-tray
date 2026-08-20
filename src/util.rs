//! Formatting helpers shared across modules.

/// Format a duration in ms as e.g. "5m", "1h 5m", "2d 3h". Used in the
/// "stale data" indicator and any other place we display an age.
pub fn fmt_age(ms: i64) -> String {
    let mins = ms / 60_000;
    if mins < 60 {
        return format!("{mins}m");
    }
    let h = mins / 60;
    let m = mins % 60;
    if m == 0 {
        format!("{h}h")
    } else {
        format!("{h}h {m}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_age_units() {
        assert_eq!(fmt_age(0), "0m");
        assert_eq!(fmt_age(60_000), "1m");
        assert_eq!(fmt_age(3_600_000), "1h");
        assert_eq!(fmt_age(3_900_000), "1h 5m");
        assert_eq!(fmt_age(86_400_000), "24h");
    }
}