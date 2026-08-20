//! Dynamic tray icon: SVG circular progress ring → PNG via ImageMagick,
//! cached on disk per (bucket, remaining_pct) so we only fork `magick` once
//! per (bucket, %) pair.
//!
//! This is a placeholder — the user wants to rethink the icon approach
//! (static icon + title text, or pure-Rust rendering, etc.). The current
//! shape mirrors the gjs implementation so the menu mockup matches.

use anyhow::{Context, Result};
use std::path::PathBuf;

/// Bucket thresholds — see config::Thresholds. Used to pick the ring color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bucket {
    Normal,
    Warning,
    Throttled,
}

pub fn bucket_for(remaining_pct: i64, throttled: bool, yellow: i64, red: i64) -> Bucket {
    if throttled {
        return Bucket::Throttled;
    }
    let used = 100 - remaining_pct;
    if used >= red {
        Bucket::Throttled
    } else if used >= yellow {
        Bucket::Warning
    } else {
        Bucket::Normal
    }
}

const RING_COLOR: &[(&str, &str)] = &[
    ("normal", "#a8d1a3"),
    ("warning", "#f0c674"),
    ("throttled", "#e57373"),
];
const RING_CIRC: f64 = 2.0 * std::f64::consts::PI * 9.0;

/// Build the cache directory (~/.cache/minimax-quota-tray/icons/).
fn cache_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let dir = PathBuf::from(home).join(".cache/minimax-quota-tray/icons");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn bucket_name(b: Bucket) -> &'static str {
    match b {
        Bucket::Normal => "normal",
        Bucket::Warning => "warning",
        Bucket::Throttled => "throttled",
    }
}

fn bucket_color(b: Bucket) -> &'static str {
    RING_COLOR
        .iter()
        .find(|(name, _)| *name == bucket_name(b))
        .map(|(_, c)| *c)
        .unwrap_or("#a8d1a3")
}

fn svg_for(pct: i64, color: &str) -> String {
    let dash = (pct as f64 / 100.0) * RING_CIRC;
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"22\" height=\"22\" viewBox=\"0 0 22 22\">\n\
         <circle cx=\"11\" cy=\"11\" r=\"9\" fill=\"none\" stroke=\"#3a3a3a\" stroke-width=\"3\"/>\n\
         <circle cx=\"11\" cy=\"11\" r=\"9\" fill=\"none\" stroke=\"{color}\" stroke-width=\"3\"\n\
                 stroke-dasharray=\"{dash:.2} {RING_CIRC:.2}\"\n\
                 stroke-linecap=\"round\" transform=\"rotate(-90 11 11)\"/>\n\
         </svg>"
    )
}

/// Get the cached PNG path for a (bucket, pct) pair, rendering it via
/// `magick` if not already cached. Returns None if `magick` isn't available
/// (caller can fall back to a static icon).
pub fn ring_icon_path(pct: i64, bucket: Bucket) -> Option<PathBuf> {
    let dir = cache_dir();
    let path = dir.join(format!("ring-{}-{}.png", bucket_name(bucket), pct));

    if path.exists() {
        return Some(path);
    }

    let svg = svg_for(pct, bucket_color(bucket));
    let svg_path = dir.join(format!("ring-{}-{}.svg", bucket_name(bucket), pct));
    if std::fs::write(&svg_path, &svg).is_err() {
        return None;
    }

    let status = std::process::Command::new("magick")
        .args([
            "-background",
            "none",
            svg_path.to_str().unwrap_or(""),
            path.to_str().unwrap_or(""),
        ])
        .status();

    match status {
        Ok(s) if s.success() && path.exists() => Some(path),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_thresholds() {
        // 0 remaining (100 used) → throttled
        assert_eq!(bucket_for(0, false, 60, 85), Bucket::Throttled);
        // 50 remaining (50 used) → normal
        assert_eq!(bucket_for(50, false, 60, 85), Bucket::Normal);
        // 35 remaining (65 used) → warning (≥ yellow 60)
        assert_eq!(bucket_for(35, false, 60, 85), Bucket::Warning);
        // 14 remaining (86 used) → throttled (≥ red 85)
        assert_eq!(bucket_for(14, false, 60, 85), Bucket::Throttled);
        // Explicit throttled flag wins regardless of pct.
        assert_eq!(bucket_for(80, true, 60, 85), Bucket::Throttled);
    }

    #[test]
    fn svg_contains_expected_strokes() {
        let s = svg_for(80, "#a8d1a3");
        assert!(s.contains("<svg"));
        assert!(s.contains("stroke=\"#a8d1a3\""));
        // 80% of 2π·9 = 0.8 * 56.549 ≈ 45.24
        assert!(s.contains("45.24"));
    }

    #[test]
    fn cache_dir_creates() {
        // Use a fresh subdir under the existing HOME rather than mutating
        // HOME globally — that races with config tests' HOME_LOCK.
        let dir = cache_dir();
        assert!(dir.exists());
    }
}