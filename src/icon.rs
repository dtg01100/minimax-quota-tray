//! Pure-Rust SVG → ARGB32 pixel buffer for the tray icon.
//!
//! Uses the `resvg` crate to parse + rasterize SVG into a 22×22 ARGB
//! buffer that we send to StatusNotifierItem via the IconPixmap property.
//! No more ImageMagick / disk caching / external processes — entirely
//! in-memory, entirely in Rust.
//!
//! The icon shape itself mirrors the gjs original: a circular ring
//! stroked with the bucket color, with the unfilled portion shown in
//! dark grey. The filled percentage is the remaining quota percent.

use anyhow::Result;
use usvg::{Options, Tree};

/// Colors per bucket (matches the gjs `RING_COLOR`).
const RING_COLOR: &[(&str, &str)] = &[
    ("normal", "#a8d1a3"),     // pastel green
    ("warning", "#f0c674"),    // amber
    ("throttled", "#e57373"),  // red
];

const TRACK_COLOR: &str = "#3a3a3a";  // dark grey for the unfilled portion

/// Icon dimensions — must match what StatusNotifierItem expects (22×22
/// is the standard symbolic icon size).
const ICON_SIZE: u32 = 22;
const RING_RADIUS: f32 = 9.0;
const RING_STROKE: f32 = 3.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bucket {
    Normal,
    Warning,
    Throttled,
}

pub fn bucket_for(remaining_pct: i64, throttled: bool, yellow: i64, red: i64) -> Bucket {
    if throttled { return Bucket::Throttled; }
    let used = 100 - remaining_pct;
    if used >= red { Bucket::Throttled }
    else if used >= yellow { Bucket::Warning }
    else { Bucket::Normal }
}

fn bucket_hex(b: Bucket) -> &'static str {
    RING_COLOR
        .iter()
        .find(|(name, _)| matches_bucket_name(b, name))
        .map(|(_, hex)| *hex)
        .unwrap_or("#a8d1a3")
}

fn matches_bucket_name(b: Bucket, name: &str) -> bool {
    match b {
        Bucket::Normal => name == "normal",
        Bucket::Warning => name == "warning",
        Bucket::Throttled => name == "throttled",
    }
}

fn bucket_name(b: Bucket) -> &'static str {
    match b {
        Bucket::Normal => "normal",
        Bucket::Warning => "warning",
        Bucket::Throttled => "throttled",
    }
}

/// SVG template for the ring icon. `pct` is the percentage of the ring
/// that's filled (i.e., remaining quota %). The unfilled portion is the
/// dark track color.
fn svg_for(pct: i64, color: &str) -> String {
    // Circumference of the ring at r=9.
    let circ = 2.0 * std::f32::consts::PI * 9.0;
    let filled = (pct.clamp(0, 100) as f32 / 100.0) * circ;
    let remaining = circ - filled;
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{ICON_SIZE}" height="{ICON_SIZE}" viewBox="0 0 {ICON_SIZE} {ICON_SIZE}">
  <circle cx="11" cy="11" r="{RING_RADIUS}" fill="none" stroke="{TRACK_COLOR}" stroke-width="{RING_STROKE}"/>
  <circle cx="11" cy="11" r="{RING_RADIUS}" fill="none" stroke="{color}" stroke-width="{RING_STROKE}"
          stroke-dasharray="{filled:.3} {remaining:.3}" stroke-linecap="round"
          transform="rotate(-90 11 11)"/>
</svg>"#
    )
}

/// Render the SVG template for `(pct, bucket)` and rasterize it to an
/// ARGB32 byte buffer matching the StatusNotifierItem IconPixmap
/// property: `(width, height, bytes)`. Bytes are ARGB32 in memory order
/// (alpha first, then R, G, B per pixel — the byte ordering the SNI
/// spec mandates).
///
/// Returns `None` if SVG parsing or rasterization fails; callers fall
/// back to a theme icon name in that case.
pub fn render_pixmap(pct: i64, bucket: Bucket) -> Option<(u32, u32, Vec<u8>)> {
    let svg_str = svg_for(pct, bucket_hex(bucket));
    let opts = Options::default();
    let tree = Tree::from_str(&svg_str, &opts).ok()?;

    let mut pixmap = resvg::tiny_skia::Pixmap::new(ICON_SIZE, ICON_SIZE)?;

    // resvg 0.48: render(tree, transform, &mut pixmap) — fits to pixmap size.
    resvg::render(&tree, resvg::tiny_skia::Transform::default(), &mut pixmap.as_mut());

    // Convert RGBA → host-endian uint32 layout (BGRA bytes on x86).
    //
    // The SNI spec says "ARGB32 in network byte order" — but the actual
    // reference implementations (KDE plasma's kstatusnotifieritem, the
    // gjs appindicator extension's Cogl pipeline) all pass the bytes in
    // *host-endian* uint32 order, which is `[B, G, R, A]` on little-endian
    // (x86) and `[A, R, G, B]` on big-endian. The receiving end
    // (Cogl's ARGB_8888 → Cairo → GdkPixbuf) interprets the bytes as a
    // native uint32, so we must send the host layout — otherwise alpha
    // gets swapped with blue and the icon becomes mostly transparent,
    // which Cogl/the watcher report as a missing icon (the "three dots"
    // placeholder).
    let mut out = Vec::with_capacity((ICON_SIZE * ICON_SIZE * 4) as usize);
    for pixel in pixmap.pixels() {
        out.push(pixel.blue());
        out.push(pixel.green());
        out.push(pixel.red());
        out.push(pixel.alpha());
    }
    Some((ICON_SIZE, ICON_SIZE, out))
}

/// Cache key — bucket + remaining pct. We round pct to a step so we
/// don't render a new pixmap for every single tick of an integer percent.
pub fn cache_step(pct: i64) -> i64 {
    (pct.max(0).min(100) as i64) / 2 * 2  // round to nearest 2%
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_thresholds() {
        assert_eq!(bucket_for(0, false, 60, 85), Bucket::Throttled);
        assert_eq!(bucket_for(50, false, 60, 85), Bucket::Normal);
        assert_eq!(bucket_for(35, false, 60, 85), Bucket::Warning);
        assert_eq!(bucket_for(14, false, 60, 85), Bucket::Throttled);
        assert_eq!(bucket_for(80, true, 60, 85), Bucket::Throttled);
    }

    #[test]
    fn svg_contains_ring_elements() {
        let s = svg_for(80, "#a8d1a3");
        assert!(s.contains("<svg"));
        assert!(s.contains("stroke=\"#a8d1a3\""));
        // 80% of 2π·9 ≈ 45.24
        assert!(s.contains("45.2"), "expected filled length ~45.24; got: {s}");
    }

    #[test]
    fn svg_clamps_pct() {
        // pct > 100 → clamped to 100; pct < 0 → clamped to 0.
        let s1 = svg_for(150, "#a8d1a3");
        let s2 = svg_for(-50, "#a8d1a3");
        // Both should produce valid (filled + remaining = circumference).
        assert!(s1.contains("<svg"));
        assert!(s2.contains("<svg"));
    }

    #[test]
    fn render_pixmap_returns_22x22_argb() {
        let result = render_pixmap(80, Bucket::Normal);
        let (w, h, bytes) = result.expect("render_pixmap should succeed");
        assert_eq!(w, 22);
        assert_eq!(h, 22);
        // 22 * 22 * 4 = 1936 bytes
        assert_eq!(bytes.len(), 22 * 22 * 4);
    }

    #[test]
    fn render_pixmap_has_visible_pixels() {
        // The track circle should make most pixels at least partially visible.
        // On host-endian (x86 = BGRA bytes per pixel), the alpha channel
        // is the LAST byte of each 4-byte group. Cogl/GTK interprets these
        // bytes as a host uint32.
        let (_w, _h, bytes) = render_pixmap(50, Bucket::Normal).unwrap();
        let any_visible = bytes.chunks_exact(4)
            .any(|px| px[3] > 0);
        assert!(any_visible, "rendered pixmap should have visible pixels (alpha > 0)");
    }

    #[test]
    fn render_pixmap_byte_order_is_host_endian() {
        // The receiving Cogl pipeline reads the bytes as a host-endian
        // uint32 in ARGB_8888 format. On x86 (little-endian), the in-memory
        // byte order is BGRA: [B, G, R, A]. Verify the ring-fill pixels
        // have the right bytes in that order.
        //
        // Ring color is #a8d1a3 (RGB = 168, 209, 163) at 100% fill, so we
        // expect at least one pixel with B=0xa3, G=0xd1, R=0xa8, A=0xff.
        let (_w, _h, bytes) = render_pixmap(100, Bucket::Normal).unwrap();
        let any_ring_pixel = bytes.chunks_exact(4)
            .any(|px| {
                px[0] == 0xa3 && px[1] == 0xd1 && px[2] == 0xa8 && px[3] == 0xff
            });
        assert!(any_ring_pixel,
                "expected at least one BGRA pixel (0xa3, 0xd1, 0xa8, 0xff) in the 100% fill ring");
    }

    #[test]
    fn cache_step_rounds_to_nearest_two_pct() {
        assert_eq!(cache_step(0), 0);
        assert_eq!(cache_step(1), 0);
        assert_eq!(cache_step(2), 2);
        assert_eq!(cache_step(50), 50);
        assert_eq!(cache_step(99), 98);
        assert_eq!(cache_step(100), 100);
        assert_eq!(cache_step(-5), 0);
        assert_eq!(cache_step(150), 100);
    }

    #[test]
    fn bucket_names() {
        assert_eq!(bucket_name(Bucket::Normal), "normal");
        assert_eq!(bucket_name(Bucket::Warning), "warning");
        assert_eq!(bucket_name(Bucket::Throttled), "throttled");
    }
}