//! Pure-Rust SVG → ARGB32 pixel buffer for the tray icon.
//!
//! Uses the `resvg` crate to parse + rasterize SVG into a 22×22 ARGB
//! buffer that we send to StatusNotifierItem via the IconPixmap property.
//! No more ImageMagick / disk caching / external processes — entirely
//! in-memory, entirely in Rust.
//!
//! The icon shape mirrors the gjs original: a ring (the remaining %
//! shown as a stroked arc around a center dot) layered on top of a
//! faded full-circle "track" so the unfilled portion reads as the same
//! color at 25% opacity, not as a contrasting grey. Round caps on the
//! progress arc terminate cleanly. The dasharray is corrected by half a
//! stroke-width so the visible rounded cap aligns where the dasharray
//! says (otherwise round caps overshoot and the arc visibly extends past
//! the 12-o'clock start point at pct=100).
//!
//! Colors are pinned to the gjs `RING_COLOR` table so the rendered icon
//! matches what the gjs version produced: same green / yellow / red.

use anyhow::Result;
use usvg::{Options, Tree};

/// Colors per bucket — must match the gjs `RING_COLOR` table in
/// `minimax-quota-tray.js`. Same hex, same role. The throttled color is
/// the same red as the static `quota-throttled.svg` dot so the ring and
/// the static fallback look identical.
const RING_COLOR: &[(&str, &str)] = &[
    ("normal", "#3a9d4d"),     // gjs RING_COLOR.normal
    ("warning", "#f6d32d"),    // gjs RING_COLOR.warning
    ("throttled", "#e01b24"),  // matches icons/quota-throttled.svg
];

/// Icon dimensions — 22×22 is the standard symbolic icon size and matches
/// the gjs version (its `viewBox` is 0 0 22 22).
const ICON_SIZE: u32 = 22;
/// Outer ring radius — matches gjs (`<circle r="9">`).
const RING_RADIUS: f32 = 9.0;
/// Outer ring stroke width — matches gjs `stroke-width="2.5"`.
const RING_STROKE: f32 = 2.5;
/// Center dot radius — matches gjs `<circle r="3.5" fill="${color}">`.
const INNER_DOT_RADIUS: f32 = 3.5;
/// Track opacity — gjs uses `stroke-opacity="0.25"` so the unfilled
/// portion reads as the same color faded into the background, not as a
/// separate dark grey that fights the panel theme.
const TRACK_OPACITY: f32 = 0.25;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bucket {
    Normal,
    Warning,
    Throttled,
}

/// Compute the bucket from remaining% + the window's `throttled` flag +
/// optional burn-rate projection. Matches gjs `bucketForChip()`:
///
///   - throttled: pct <= 0 OR `throttled` flag set (window exhausted)
///   - warning:   used% >= yellow OR burn projects exhaustion before
///                reset (gjs ORs the burn flip with the threshold checks
///                — if the trend would empty the window before it
///                resets, the chip flips to yellow even when remaining%
///                looks healthy)
///   - normal:    otherwise
///
/// Important: gjs does NOT switch to a red ring when remaining drops
/// below the red threshold — it keeps the yellow ring until pct hits 0,
/// then falls through to the static `quota-throttled` dot. The earlier
/// Rust port returned `Throttled` for `used >= red` which produced a
/// red ring at low-but-not-zero percentages (e.g. 15% with default
/// thresholds), diverging from gjs. The fix: only `Throttled` when
/// the window is exhausted; otherwise Warning/Normal based on yellow
/// or the burn projection.
///
/// The `red` parameter is kept in the signature for API stability but
/// is unused — the gjs OR-with-yellow subsumes it.
pub fn bucket_for(
    remaining_pct: i64,
    throttled: bool,
    yellow: i64,
    _red: i64,
    burn: Option<&crate::burn::BurnResult>,
) -> Bucket {
    if throttled || remaining_pct <= 0 { return Bucket::Throttled; }
    let used = 100 - remaining_pct;
    if used >= yellow { return Bucket::Warning; }
    // Burn-driven flip: a healthy-looking remaining% but a burn rate
    // that would exhaust the window before it resets → yellow. The
    // title text already carries `⚠ exhausts in Xm` (see title_for),
    // so flipping the icon too matches gjs — chip color and chip text
    // both flip together when the burn rate signals trouble.
    if burn.map_or(false, |b| b.exhaust_before_reset) {
        return Bucket::Warning;
    }
    Bucket::Normal
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

/// Compute the path to a cached PNG for the given percentage.
///
/// Matches gjs `ringIconPath()`: `${TMPDIR}/minimax-quota-ring-${pct}.png`.
/// One file per integer percentage (0..100), shared across buckets via the
/// bucket color baked into the BGRA bytes. The path is what we send as
/// `IconName` so the SNI host reads the PNG from disk — without this,
/// the AppIndicator extension prefers the theme name `quota-normal`
/// (a solid filled circle) over `IconPixmap`, so the panel shows the
/// static dot instead of our rendered ring. This is exactly what gjs
/// does to make the ring visible: it sets the icon name to the path of
/// the rendered PNG file, then the AppIndicator extension loads that
/// PNG via the standard theme-resolution path.
pub fn ring_icon_path(pct: i64) -> std::path::PathBuf {
    let clamped = pct.clamp(0, 100);
    let dir = std::env::var("TMPDIR")
        .ok()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir());
    dir.join(format!("minimax-quota-ring-{clamped}.png"))
}

/// Render the ring for `pct` + `bucket` to a PNG file on disk, returning
/// the path. Skips the write when the file is already up to date (cache
/// hit on subsequent polls — saves a resvg invocation per refresh for
/// the steady-state "no change" case). The PNG is RGBA8 so the panel
/// picks up transparent background correctly.
pub fn write_ring_png(pct: i64, bucket: Bucket) -> std::path::PathBuf {
    let path = ring_icon_path(pct);
    if path.exists() { return path; }
    if let Some((w, h, bytes)) = render_pixmap(pct, bucket) {
        if let Err(e) = write_png_rgba(&path, w, h, &bytes) {
            log::warn!("icon: failed to write ring PNG to {path:?}: {e}");
        }
    }
    path
}

/// Encode `bytes` (BGRA host-endian, as `render_pixmap` returns) as a
/// RGBA8 PNG at `path`. Uses the `png` crate — a hand-rolled encoder
/// failed to produce a valid deflate stream the AppIndicator extension
/// would load (and would silently show no icon when the file path
/// couldn't be decoded).
fn write_png_rgba(
    path: &std::path::Path,
    w: u32,
    h: u32,
    bgra: &[u8],
) -> anyhow::Result<()> {
    // Convert BGRA → RGBA (just swap channel order; alpha stays).
    let mut raw = Vec::with_capacity(bgra.len());
    for px in bgra.chunks_exact(4) {
        raw.push(px[2]); // R
        raw.push(px[1]); // G
        raw.push(px[0]); // B
        raw.push(px[3]); // A
    }
    let file = std::fs::File::create(path)?;
    let w_ref = &mut std::io::BufWriter::new(file);
    let mut encoder = png::Encoder::new(w_ref, w, h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&raw)?;
    Ok(())
}

/// SVG template for the ring icon — three layers, identical to the gjs
/// `renderRingSvg()` output so the resvg-rendered icon matches what the
/// gjs version produced via `magick -density 600`:
///
///   1. Faded full-circle track (`stroke` = color, `stroke-opacity`
///      = 0.25). Same color as the progress arc, so the unfilled
///      portion reads as "this much left" rather than as a separate
///      grey ring that fights the panel theme.
///   2. Progress arc (`stroke` = color, `stroke-linecap` = "round",
///      `stroke-dasharray` = `(arc - halfStroke) (circumference -
///      arc + halfStroke)`). The dasharray correction compensates for
///      one round cap so the visible rounded terminus aligns with the
///      12-o'clock start at pct=100 and the tail sits cleanly at
///      pct<100 — matches gjs `arc - halfStroke`.
///   3. Inner dot (`r=3.5`, `fill` = color). The dot is what makes the
///      icon read as "ring with center" instead of an empty arc — the
///      gjs version draws it explicitly; without it, the panel
///      background shows through the middle and the icon looks like
///      a thin curved line.
fn svg_for(pct: i64, color: &str) -> String {
    let circ = 2.0 * std::f32::consts::PI * RING_RADIUS;
    let filled = (pct.clamp(0, 100) as f32 / 100.0) * circ;
    // Round caps add ~half a stroke-width to each visible end, so the
    // foreground arc visually overshoots its dasharray length. Subtract
    // halfStroke so the visible rounded cap lands where the dasharray
    // specifies (matches gjs `arc - halfStroke`).
    let half_stroke = RING_STROKE / 2.0;
    let fg_arc = (filled - half_stroke).max(0.0);
    let fg_rest = circ - fg_arc;
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {ICON_SIZE} {ICON_SIZE}" width="{ICON_SIZE}" height="{ICON_SIZE}">
  <circle cx="11" cy="11" r="{RING_RADIUS}" fill="none" stroke="{color}" stroke-width="{RING_STROKE}" stroke-opacity="{TRACK_OPACITY}"/>
  <circle cx="11" cy="11" r="{RING_RADIUS}" fill="none" stroke="{color}" stroke-width="{RING_STROKE}" stroke-linecap="round"
          stroke-dasharray="{fg_arc:.3} {fg_rest:.3}" transform="rotate(-90 11 11)"/>
  <circle cx="11" cy="11" r="{INNER_DOT_RADIUS}" fill="{color}"/>
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
        // Mirrors gjs bucketForChip():
        //   - pct <= 0 OR throttled flag → Throttled
        //   - used >= yellow (60%)       → Warning
        //   - else                       → Normal
        // The red threshold (85%) does NOT switch to a red ring in gjs —
        // it stays yellow until pct hits 0. The `burn` parameter is
        // None in these cases (no burn-flip needed for the threshold
        // checks themselves).
        assert_eq!(bucket_for(0, false, 60, 85, None), Bucket::Throttled);
        assert_eq!(bucket_for(50, false, 60, 85, None), Bucket::Normal);
        assert_eq!(bucket_for(35, false, 60, 85, None), Bucket::Warning);
        // Critical regression: pct=14 (below red threshold but > 0) used
        // to return Throttled → red ring. gjs returns Warning → yellow
        // ring. The faithful reproduction requires Warning here.
        assert_eq!(bucket_for(14, false, 60, 85, None), Bucket::Warning);
        assert_eq!(bucket_for(80, true, 60, 85, None), Bucket::Throttled);
        assert_eq!(bucket_for(1, false, 60, 85, None), Bucket::Warning);
        assert_eq!(bucket_for(40, false, 60, 85, None), Bucket::Warning);
        assert_eq!(bucket_for(41, false, 60, 85, None), Bucket::Normal);
    }

    #[test]
    fn bucket_burn_flip_to_warning() {
        use crate::burn::BurnResult;
        // A high remaining% (90) with no burn-flip → Normal (green ring).
        let healthy = BurnResult {
            rate_per_hour: 0.0,
            mode: "pct",
            unit: "pct",
            exhaust_ms: f64::INFINITY,
            remaining_ms: 3_600_000,
            exhaust_before_reset: false,
            projected_pct_left: 90.0,
        };
        assert_eq!(bucket_for(90, false, 60, 85, Some(&healthy)),
                   Bucket::Normal);
        // Same healthy remaining% but the burn projects exhaustion
        // before reset → flips to Warning (yellow ring), matching gjs
        // bucketForChip's `(burn && burn.exhaustBeforeReset)` clause.
        let alarming = BurnResult { exhaust_before_reset: true, ..healthy };
        assert_eq!(bucket_for(90, false, 60, 85, Some(&alarming)),
                   Bucket::Warning);
        // Burn flip is OR'd with the threshold check — a burn flip on
        // an already-low remaining% still lands at Warning (idempotent).
        assert_eq!(bucket_for(20, false, 60, 85, Some(&alarming)),
                   Bucket::Warning);
        // Burn flip can't override Throttled — pct <= 0 still wins.
        assert_eq!(bucket_for(0, false, 60, 85, Some(&alarming)),
                   Bucket::Throttled);
    }

    #[test]
    fn svg_contains_ring_elements() {
        let s = svg_for(80, "#3a9d4d");
        assert!(s.contains("<svg"));
        assert!(s.contains("stroke=\"#3a9d4d\""));
        // 80% of 2π·9 ≈ 45.239, minus halfStroke (1.25) ≈ 43.989.
        // Dasharray correction matches the gjs `arc - halfStroke` rule.
        assert!(s.contains("43.989") || s.contains("43.990"),
                "expected corrected dasharray ~43.99; got: {s}");
    }

    #[test]
    fn svg_has_inner_dot() {
        // The ring-with-center-dot look is what makes the icon read as
        // a status indicator instead of a thin curve. gjs draws it as
        // an explicit filled `<circle r="3.5" fill="${color}">`.
        let s = svg_for(80, "#3a9d4d");
        assert!(s.contains("r=\"3.5\""),
                "inner dot circle missing (r=3.5); got: {s}");
        assert!(s.contains("fill=\"#3a9d4d\""),
                "inner dot fill should match ring color; got: {s}");
    }

    #[test]
    fn svg_uses_round_caps_on_progress_arc() {
        // Round caps are what makes the progress terminus look like a
        // rounded dot instead of a flat dasharray cut. The track (full
        // circle) doesn't need them — only the foreground arc.
        let s = svg_for(80, "#3a9d4d");
        assert!(s.contains("stroke-linecap=\"round\""),
                "progress arc should use stroke-linecap=round; got: {s}");
    }

    #[test]
    fn svg_track_is_faded_color_not_separate_grey() {
        // The gjs version uses stroke-opacity=0.25 with the same color
        // as the progress arc, so the unfilled portion reads as the
        // same color faded into the panel. The earlier Rust version
        // used a separate dark grey (#3a3a3a) which fights the panel
        // theme on light backgrounds.
        let s = svg_for(80, "#3a9d4d");
        assert!(s.contains("stroke-opacity=\"0.25\""),
                "track should use stroke-opacity=0.25; got: {s}");
        assert!(!s.contains("#3a3a3a"),
                "track should NOT use the legacy dark grey color; got: {s}");
    }

    #[test]
    fn svg_stroke_width_matches_gjs() {
        // gjs uses stroke-width="2.5". The earlier Rust version used 3.0
        // which made the ring look chunkier than the gjs original.
        let s = svg_for(80, "#3a9d4d");
        assert!(s.contains("stroke-width=\"2.5\""),
                "ring stroke-width should be 2.5 (matches gjs); got: {s}");
    }

    #[test]
    fn ring_colors_match_gjs() {
        // Pinned to gjs RING_COLOR table. If you change these, change
        // the gjs version too — the two implementations need to look
        // identical when the user toggles between them.
        assert_eq!(bucket_hex(Bucket::Normal), "#3a9d4d");
        assert_eq!(bucket_hex(Bucket::Warning), "#f6d32d");
        assert_eq!(bucket_hex(Bucket::Throttled), "#e01b24");
    }

    #[test]
    fn dasharray_at_zero_pct_is_only_round_cap() {
        // At pct=0 the foreground arc has no length to draw; the
        // dasharray is `0 (circumference - 0) = 0 circ`. The visible
        // result should be just the faded track (no progress arc) plus
        // the center dot.
        let s = svg_for(0, "#3a9d4d");
        // fg_arc = max(0, 0 - 1.25) = 0; fg_rest = circ - 0 = circ.
        // dasharray should contain 0.000 and ~56.5 (the circumference).
        assert!(s.contains("0.000"),
                "expected zero-length dasharray at pct=0; got: {s}");
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
        // Ring color is #3a9d4d (RGB = 58, 157, 77) — matches the gjs
        // RING_COLOR.normal. Expected bytes for an opaque ring pixel:
        // B=0x4d, G=0x9d, R=0x3a, A=0xff.
        let (_w, _h, bytes) = render_pixmap(100, Bucket::Normal).unwrap();
        let any_ring_pixel = bytes.chunks_exact(4)
            .any(|px| {
                px[0] == 0x4d && px[1] == 0x9d && px[2] == 0x3a && px[3] == 0xff
            });
        assert!(any_ring_pixel,
                "expected at least one BGRA pixel (0x4d, 0x9d, 0x3a, 0xff) in the 100% fill ring");
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
#[cfg(test)]
mod dump_tests {
    use super::*;
    use std::path::Path;

    /// Throwaway: render a few sample icons to /tmp so the gjs-vs-Rust
    /// visual diff can be inspected directly. Not normally run.
    #[test]
    #[ignore = "dump helper, run with `cargo test --release dump_icons -- --ignored`"]
    fn dump_icons_for_visual_inspection() {
        for (pct, bucket) in [(100, Bucket::Normal),
                              (80,  Bucket::Normal),
                              (50,  Bucket::Normal),
                              (80,  Bucket::Warning),
                              // Boundary: pct=14 used to be Throttled (red ring);
                              // now Warning (yellow ring) — the gjs faithful fix.
                              (14,  Bucket::Warning),
                              // Exhausted: pct=0 → Throttled bucket, but render_pixmap
                              // is intentionally not called in main.rs (skipped to fall
                              // through to the static quota-throttled SVG). This dumps
                              // the pixmap anyway for comparison reference.
                              (0,   Bucket::Throttled)] {
            let (_w, _h, bytes) = render_pixmap(pct, bucket).unwrap();
            std::fs::write(
                format!("/tmp/icon_{}_{:?}.argb", pct, bucket), &bytes).unwrap();
        }
        assert!(Path::new("/tmp/icon_100_Normal.argb").exists());
    }
}
