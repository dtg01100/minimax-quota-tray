//! Pure-Rust 2D rasterizer for the tray icon.
//!
//! Renders directly to a 22×22 ARGB32 pixel buffer via `tiny-skia`
//! (a Skia subset ported to Rust). Three primitives are drawn:
//!
//!   1. A faded full-circle track (`stroke`, opacity 0.25) — what makes
//!      the unfilled portion read as "this much left" rather than as a
//!      separate grey ring that fights the panel theme.
//!   2. A progress arc (`stroke`, `stroke-linecap=round`,
//!      `stroke-dasharray` = `(arc − halfStroke) (circumference −
//!      arc + halfStroke)`) — the rounded terminus aligns with the
//!      12-o'clock start at pct=100, matches gjs `arc − halfStroke`.
//!   3. A center dot (`fill`) — without it the icon reads as a thin
//!      curved line.
//!
//! No SVG parser, no external process, no resvg/usvg. Dropping those
//! removed `usvg`, `svgtypes`, `winnow`, `pico-args`, `rgb`, `bytemuck`
//! from the build (`resvg` itself brings them all in). The output is
//! sent to StatusNotifierItem as `IconPixmap` (universal fallback) and
//! the same shape is also serialized to `${TMPDIR}/*.svg` for hosts
//! that can render SVG natively (see `write_static_svgs` /
//! `write_ring_svg`).
//!
//! Colors are pinned to the gjs `RING_COLOR` table so the rendered icon
//! matches what the gjs version produced: same green / yellow / red.

use tiny_skia::{Color, FillRule, LineCap, Paint, PathBuilder, Pixmap, Stroke, StrokeDash, Transform};

use crate::provider::RingColors;

/// Colors for the offline + error chip states. Independent of
/// provider — these aren't bucket states, they're tray lifecycle
/// states, so they're the same regardless of which provider the
/// tray is running against.
///
/// `error` shares the throttled red (the legacy gjs chip used the
/// same red dot for both — see icons/quota-error.svg).
/// `offline` is a neutral gray so the panel doesn't show a red
/// chip just because the network is down.
const OFFLINE_COLOR: &str = "#9a9996";

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
    // title text already carries `⚠ exhausts in Xm` historically
    // (gjs's chip showed no title at all; the rust port followed
    // that and so the flip is purely visual now — the menu row
    // carries the message),
    // so flipping the icon too matches gjs — chip color and chip text
    // both flip together when the burn rate signals trouble.
    if burn.map_or(false, |b| b.exhaust_before_reset) {
        return Bucket::Warning;
    }
    Bucket::Normal
}

/// Look up the hex color for a bucket. Returns an owned `String`
/// (the config drives these, so they're not `&'static`). Returns
/// an empty string for unknown buckets — callers feed the result to
/// `parse_hex_rgb` which rejects empty strings, so an unknown
/// bucket renders as a transparent / default-color ring.
fn bucket_hex(b: Bucket, colors: &RingColors) -> String {
    match b {
        Bucket::Normal => colors.normal.clone(),
        Bucket::Warning => colors.warning.clone(),
        Bucket::Throttled => colors.throttled.clone(),
    }
}

/// Internal label (different from the gjs `ICON` table — gjs used
/// these as theme names; the rust port routes through
/// `static_svg_path()` instead). Kept for tests + introspection.
#[allow(dead_code)]
fn bucket_name(b: Bucket) -> &'static str {
    match b {
        Bucket::Normal => "normal",
        Bucket::Warning => "warning",
        Bucket::Throttled => "throttled",
    }
}

/// Compute the path to a cached SVG for the given percentage.
///
/// One file per integer percentage (0..100), shared across buckets via
/// the bucket color baked into the SVG `stroke` attribute. The path is
/// what we send as `IconName` so hosts that can render SVG natively
/// (KDE/QtSvg, GNOME with a registered `libpixbufloader-svg.so`) use it
/// directly at the panel's actual icon size — HiDPI panels get crisp
/// pixels instead of upscaled 22×22 bitmap.
///
/// The SNI spec defines `IconName` as a Freedesktop-compliant theme
/// name; absolute file paths are an unofficial extension supported by
/// every major SNI implementation. The upstream AppIndicator extension
/// acknowledges this as a "HACK" in `appIndicator.js::_getIconData`:
///
/// ```js
/// // HACK: icon is a path name. This is not specified by the API,
/// // but at least indicator-sensors uses it.
/// if (name[0] === '/') { ... }
/// ```
///
/// Hosts without SVG support (notably Fedora Atomic / Bluefin /
/// Silverblue with empty gdk-pixbuf `loaders.cache`, or hosts where the
/// SVG loader isn't registered) will fail `GdkPixbuf.Pixbuf
/// .get_file_info_async()` and the AppIndicator extension logs
/// "Invalid image format" and returns null — at which point the
/// extension falls through to `IconPixmap` (the in-memory ARGB bytes
/// from `render_pixmap`), so the icon still renders. We always send
/// both: the SVG file as `IconName` for hosts that can use it, and the
/// ARGB bytes as `IconPixmap` as the universal fallback.
pub fn ring_svg_path(pct: i64) -> std::path::PathBuf {
    let clamped = pct.clamp(0, 100);
    let dir = std::env::var("TMPDIR")
        .ok()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir());
    dir.join(format!("minimax-quota-ring-{clamped}.svg"))
}

/// Path to a pre-written SVG for the given static-state icon name
/// (`normal`, `warning`, `throttled`, `error`, `offline`). See
/// `ring_svg_path()` for the full rationale on why we send SVG file
/// paths as `IconName`. The static SVGs are written to
/// `${TMPDIR}/minimax-quota-{name}.svg` once at startup by
/// `write_static_svgs()`.
pub fn static_svg_path(name: &str) -> std::path::PathBuf {
    let dir = std::env::var("TMPDIR")
        .ok()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir());
    dir.join(format!("minimax-quota-{name}.svg"))
}

/// SVG template for a static icon — a single filled `<circle r="9">`
/// at the center of the 22×22 viewBox, matching the radius of the
/// ring's track and progress arcs so the static dot and the dynamic
/// ring share the same overall footprint. The file is just a UTF-8
/// text blob; the host's SVG renderer handles the rest.
fn svg_static(color: &str) -> String {
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 22 22" width="22" height="22"><circle cx="11" cy="11" r="9" fill="{color}"/></svg>"#
    )
}

/// Write the project's static SVG icons into `${TMPDIR}`. Idempotent —
/// skips files that already exist (subsequent restarts reuse the cached
/// SVG). Called once at startup.
///
/// Each static icon is a single `<circle r="9" fill="{color}">` — a
/// solid dot. The set covers the five states the chip can be in:
///
///   - `normal`:    from `colors.normal`  (provider-defined)
///   - `warning`:   from `colors.warning` (provider-defined)
///   - `throttled`: from `colors.throttled` (provider-defined; also
///                  matches `quota-error` for visual consistency with
///                  the legacy gjs chip)
///   - `error`:     same as throttled — the tray uses one red for both
///                  "exhausted" and "fetch failed"
///   - `offline`:   gray (`OFFLINE_COLOR`)
pub fn write_static_svgs(colors: &RingColors) {
    let dir = std::env::var("TMPDIR")
        .ok()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir());
    // The throttled color also serves as the error dot color; we
    // intentionally collapse these into a single visual state so
    // the user gets the same "I need attention" cue either way.
    let entries: [(&str, &str); 5] = [
        ("normal",    &colors.normal),
        ("warning",   &colors.warning),
        ("throttled", &colors.throttled),
        ("error",     &colors.throttled),
        ("offline",   OFFLINE_COLOR),
    ];
    for (name, color) in entries {
        let path = dir.join(format!("minimax-quota-{name}.svg"));
        if path.exists() { continue; }
        if let Err(e) = std::fs::write(&path, svg_static(color)) {
            log::warn!("icon: failed to write static SVG {path:?}: {e}");
        }
    }
}

/// Write the SVG ring for `pct` + `bucket` to disk, returning the path.
/// Idempotent — skips the write when the file already exists (cache hit
/// on subsequent polls, saves an `svg_for()` template construction per
/// refresh in the steady-state "no change" case).
///
/// The SVG file is plain UTF-8 text (~400 bytes vs the ~11 KB PNG it
/// used to be) and is the source-of-truth for the same string that
/// `render_pixmap()` rasterizes for `IconPixmap` — so hosts that load
/// the file get bit-for-bit the same shapes the ARGB bytes contain,
/// just rendered at the panel's target size instead of upscaled from
/// 22×22.
pub fn write_ring_svg(pct: i64, bucket: Bucket, colors: &RingColors) -> std::path::PathBuf {
    let path = ring_svg_path(pct);
    if path.exists() { return path; }
    let svg = svg_for(pct, &bucket_hex(bucket, colors));
    if let Err(e) = std::fs::write(&path, svg) {
        log::warn!("icon: failed to write ring SVG to {path:?}: {e}");
    }
    path
}

/// SVG template for the ring icon — three layers, identical to the gjs
/// `renderRingSvg()` output so the tiny-skia-rendered pixmap matches
/// what the gjs version produced via `magick -density 600`. Hosts that
/// render SVG natively (KDE/QtSvg, GNOME with a registered
/// `libpixbufloader-svg.so`) read this file directly via SNI's
/// `IconName`; hosts without SVG support fall through to the in-memory
/// ARGB bytes from `render_pixmap()`:
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

/// Render the ring icon for `(pct, bucket)` and rasterize it to an
/// ARGB32 byte buffer matching the StatusNotifierItem IconPixmap
/// property: `(width, height, bytes)`. Bytes are laid out in the
/// host's native uint32 byte order so Cogl/GTK can interpret them
/// directly — see the byte-order note in `pixmap_to_bgra` below.
///
/// The pixmap is composed of three tiny-skia primitives drawn in order
/// (back to front):
///
///   1. Faded full-circle track (stroke, opacity 0.25)
///   2. Progress arc (stroke, round caps, dasharray, rotated −90°)
///   3. Inner dot (fill, opacity 1.0)
///
/// `colors` controls the ring + dot hue. MiniMax's defaults match the
/// gjs `RING_COLOR` table (green/yellow/red); forked providers can
/// override.
///
/// Returns `None` only if `Pixmap::new` fails (effectively impossible
/// for a 22×22 pixmap, but the API is fallible). Callers fall back to
/// a theme icon name in that case.
pub fn render_pixmap(pct: i64, bucket: Bucket, colors: &RingColors) -> Option<(u32, u32, Vec<u8>)> {
    let (r, g, b) = parse_hex_rgb(&bucket_hex(bucket, colors))?;
    let mut pixmap = Pixmap::new(ICON_SIZE, ICON_SIZE)?;

    // 1. Track: stroke at 25% opacity. The alpha channel of the Paint
    //    multiplies the stroke color — track_opacity=0.25 yields a
    //    25%-opacity ring in the same hue as the progress arc.
    let mut track_paint = Paint::default();
    track_paint.set_color(Color::from_rgba8(r, g, b, track_alpha()));
    track_paint.anti_alias = true;

    let track_path = {
        let mut pb = PathBuilder::new();
        pb.push_circle(11.0, 11.0, RING_RADIUS);
        pb.finish()?
    };

    let mut track_stroke = Stroke::default();
    track_stroke.width = RING_STROKE;
    track_stroke.line_cap = LineCap::Butt;

    pixmap.stroke_path(
        &track_path,
        &track_paint,
        &track_stroke,
        Transform::identity(),
        None,
    );

    // 2. Progress arc: same circle, but with the dashed pattern matching
    //    gjs `arc - halfStroke`. The dash array must sum to a positive
    //    finite value (tiny-skia rejects zero-length patterns); at pct=0
    //    fg_arc is 0 but fg_rest is the full circumference so this
    //    never triggers.
    let circ = 2.0 * std::f32::consts::PI * RING_RADIUS;
    let filled = (pct.clamp(0, 100) as f32 / 100.0) * circ;
    let half_stroke = RING_STROKE / 2.0;
    let fg_arc = (filled - half_stroke).max(0.0);
    let fg_rest = (circ - fg_arc).max(0.0);

    let mut arc_paint = Paint::default();
    arc_paint.set_color(Color::from_rgba8(r, g, b, 255));
    arc_paint.anti_alias = true;

    let arc_path = {
        let mut pb = PathBuilder::new();
        pb.push_circle(11.0, 11.0, RING_RADIUS);
        pb.finish()?
    };

    let mut arc_stroke = Stroke::default();
    arc_stroke.width = RING_STROKE;
    arc_stroke.line_cap = LineCap::Round;
    // StrokeDash::new returns None when the array is empty or has an
    // odd length — at this point both fg_arc and fg_rest are ≥0 and
    // sum to a positive value, so .expect() is safe and never fires.
    arc_stroke.dash = StrokeDash::new(vec![fg_arc, fg_rest], 0.0);

    // SVG's `transform="rotate(-90 11 11)"` rotates the path around
    // the icon center so the dasharray starts at 12 o'clock. tiny-skia's
    // Transform::from_rotate_at gives us the same "rotate around a
    // point" semantics.
    let arc_xform = Transform::from_rotate_at(-90.0, 11.0, 11.0);

    pixmap.stroke_path(
        &arc_path,
        &arc_paint,
        &arc_stroke,
        arc_xform,
        None,
    );

    // 3. Inner dot: filled circle. Tiny-skia ignores stroke vs fill
    //    via fill_path; we don't need a stroke here.
    let mut dot_paint = Paint::default();
    dot_paint.set_color(Color::from_rgba8(r, g, b, 255));
    dot_paint.anti_alias = true;

    let dot_path = {
        let mut pb = PathBuilder::new();
        pb.push_circle(11.0, 11.0, INNER_DOT_RADIUS);
        pb.finish()?
    };

    pixmap.fill_path(
        &dot_path,
        &dot_paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );

    Some((ICON_SIZE, ICON_SIZE, pixmap_to_bgra(&pixmap)))
}

/// Convert a `#rrggbb` hex string into `(r, g, b)` u8 components.
/// Returns `None` if the string isn't exactly 7 bytes starting with `#`
/// followed by 6 hex digits; in practice we only feed it `bucket_hex()`
/// constants so the None branch is unreachable.
fn parse_hex_rgb(s: &str) -> Option<(u8, u8, u8)> {
    let bytes = s.as_bytes();
    if bytes.len() != 7 || bytes[0] != b'#' { return None; }
    let r = u8::from_str_radix(std::str::from_utf8(&bytes[1..3]).ok()?, 16).ok()?;
    let g = u8::from_str_radix(std::str::from_utf8(&bytes[3..5]).ok()?, 16).ok()?;
    let b = u8::from_str_radix(std::str::from_utf8(&bytes[5..7]).ok()?, 16).ok()?;
    Some((r, g, b))
}

/// 0.25 opacity → 8-bit alpha. Matches the gjs `stroke-opacity="0.25"`
/// on the track ring. Centralized so the conversion is in one place
/// (64/255 ≈ 0.251, close enough to the rendered value to be visually
/// indistinguishable from the exact 0.25).
fn track_alpha() -> u8 {
    (TRACK_OPACITY * 255.0).round() as u8
}

/// Convert tiny-skia's premultiplied RGBA bytes to host-endian uint32
/// layout ([B, G, R, A] on little-endian x86, [A, R, G, B] on big-endian).
///
/// The SNI spec says IconPixmap is "ARGB32 in network byte order" — but
/// the reference implementations (KDE plasma's kstatusnotifieritem, the
/// gjs appindicator extension's Cogl pipeline) all pass the bytes in
/// *host-endian* uint32 order. The receiving end (Cogl's ARGB_8888 →
/// Cairo → GdkPixbuf) interprets the bytes as a native uint32, so we
/// must send the host layout — otherwise alpha gets swapped with blue
/// and the icon becomes mostly transparent, which Cogl/the watcher
/// report as a missing icon (the "three dots" placeholder).
///
/// tiny-skia's internal buffer is premultiplied RGBA. We iterate the
/// raw bytes (no per-pixel demultiplication) to preserve bit-exact
/// behavior with the previous resvg-based pipeline — fully-opaque
/// pixels match their source color exactly, and semi-transparent track
/// pixels keep their premultiplied scaling (which is what the receiving
/// Cogl/ARGB_8888 pipeline expects when interpreting the buffer as
/// premultiplied, as it does).
fn pixmap_to_bgra(pixmap: &Pixmap) -> Vec<u8> {
    let data = pixmap.data();
    let mut out = Vec::with_capacity(data.len());
    for px in data.chunks_exact(4) {
        // px is [R, G, B, A] premultiplied. Swap to host-endian uint32
        // order: [B, G, R, A] on little-endian (x86, aarch64).
        out.push(px[2]); // B
        out.push(px[1]); // G
        out.push(px[0]); // R
        out.push(px[3]); // A
    }
    out
}

/// Cache key — bucket + remaining pct. We round pct to a step so we
/// don't render a new pixmap for every single tick of an integer percent.
/// Currently unused (the disk-cache uses the full `pct` value), but
/// retained for future use if we add an in-memory pixmap cache.
#[allow(dead_code)]
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
        // Defaults match the gjs RING_COLOR table (green/yellow/red).
        // The compile-time defaults in provider::default_ring_colors
        // are the source of truth; this test guards against
        // accidental edits.
        let colors = crate::provider::default_ring_colors();
        assert_eq!(bucket_hex(Bucket::Normal, &colors), "#3a9d4d");
        assert_eq!(bucket_hex(Bucket::Warning, &colors), "#f6d32d");
        assert_eq!(bucket_hex(Bucket::Throttled, &colors), "#e01b24");
    }

    #[test]
    fn ring_colors_per_instance_override() {
        // A per-instance config can override ring colors (e.g. an
        // orange scheme for one provider, blue for another). The
        // bucket_hex lookup honors whatever the instance passes in.
        let alt = RingColors {
            normal:   "#3366ff".to_string(),
            warning:  "#ff9900".to_string(),
            throttled: "#9933ff".to_string(),
        };
        assert_eq!(bucket_hex(Bucket::Normal, &alt), "#3366ff");
        assert_eq!(bucket_hex(Bucket::Warning, &alt), "#ff9900");
        assert_eq!(bucket_hex(Bucket::Throttled, &alt), "#9933ff");
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
        let result = render_pixmap(80, Bucket::Normal, &crate::provider::default_ring_colors());
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
        let (_w, _h, bytes) = render_pixmap(50, Bucket::Normal,
                                            &crate::provider::default_ring_colors()).unwrap();
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
        let (_w, _h, bytes) = render_pixmap(100, Bucket::Normal,
                                            &crate::provider::default_ring_colors()).unwrap();
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

    // ------------------------------------------------------------------
    // SVG-on-disk path functions
    //
    // These tests don't touch the filesystem directly (the path
    // computation functions just return a `PathBuf`) — they lock in
    // the `.svg` extension and the TMPDIR-or-temp-dir fallback so
    // regressions in path layout surface here, not in a confused user
    // staring at a "three dots" placeholder.
    // ------------------------------------------------------------------

    #[test]
    fn ring_svg_path_uses_svg_extension() {
        let p = ring_svg_path(80);
        assert!(p.to_str().unwrap().ends_with("minimax-quota-ring-80.svg"),
                "expected .svg extension; got {p:?}");
    }

    #[test]
    fn ring_svg_path_clamps_out_of_range_pct() {
        // Same clamping rule as the old PNG path — out-of-range pct
        // gets pinned to 0 or 100 in the filename so a hostile or
        // buggy refresh can't write `minimax-quota-ring--5.svg`.
        assert!(ring_svg_path(0).to_str().unwrap().ends_with("-0.svg"));
        assert!(ring_svg_path(100).to_str().unwrap().ends_with("-100.svg"));
        assert!(ring_svg_path(-5).to_str().unwrap().ends_with("-0.svg"));
        assert!(ring_svg_path(150).to_str().unwrap().ends_with("-100.svg"));
    }

    #[test]
    fn static_svg_path_uses_svg_extension() {
        for name in ["normal", "warning", "throttled", "error", "offline"] {
            let p = static_svg_path(name);
            assert!(p.to_str().unwrap().ends_with(&format!("minimax-quota-{name}.svg")),
                    "expected minimax-quota-{name}.svg; got {p:?}");
        }
    }

    #[test]
    fn svg_static_renders_single_filled_circle() {
        // The static icons are what `write_static_svgs()` writes — one
        // circle r=9 at the center, fill=color. Validating the
        // template here is enough because the file is just `svg_static`
        // serialized via `std::fs::write`.
        let s = svg_static("#3a9d4d");
        assert!(s.starts_with("<svg"));
        assert!(s.contains("viewBox=\"0 0 22 22\""));
        assert!(s.contains("width=\"22\""));
        assert!(s.contains("height=\"22\""));
        assert!(s.contains("cx=\"11\" cy=\"11\" r=\"9\""));
        assert!(s.contains("fill=\"#3a9d4d\""));
        // No stroke / dasharray / track — the static icons are solid
        // dots, not ring layers. If a future refactor adds the ring
        // layers here by accident, the static path would no longer be
        // a single filled circle and this test catches it.
        assert!(!s.contains("stroke="),
                "static SVG should have no stroke; got: {s}");
        assert!(!s.contains("stroke-dasharray"),
                "static SVG should have no dasharray; got: {s}");
    }

    #[test]
    fn svg_static_uses_each_static_color() {
        // The five static-state colors — green/yellow/red/red/grey.
        // The two reds are intentional: throttled and error share the
        // same color, matching the legacy gjs RING_COLOR + theme
        // fallback.
        assert!(svg_static("#3a9d4d").contains("fill=\"#3a9d4d\""));
        assert!(svg_static("#f6d32d").contains("fill=\"#f6d32d\""));
        assert!(svg_static("#e01b24").contains("fill=\"#e01b24\""));
        assert!(svg_static("#9a9996").contains("fill=\"#9a9996\""));
    }

    /// Smoke test: writing the static SVGs to a tempdir succeeds and
    /// produces valid UTF-8 text files with the expected names. Uses
    /// `TMPDIR` override so we don't pollute the real `/tmp`. Requires
    /// `unsafe { std::env::set_var(...) }` on Rust ≥1.86 where
    /// env-mutation became unsafe.
    #[test]
    fn write_static_svgs_writes_to_tmpdir() {
        use std::sync::Mutex;
        // Serialize tests that mutate TMPDIR so concurrent cargo-test
        // jobs don't fight each other.
        static TMPDIR_LOCK: Mutex<()> = Mutex::new(());
        let _g = TMPDIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let tmp = std::env::temp_dir().join("minimax-icon-static-test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let prev = std::env::var_os("TMPDIR");
        // SAFETY: serialized via TMPDIR_LOCK above; this test runs
        // single-threaded with respect to other tests that mutate
        // TMPDIR. cargo test runs each test in its own thread, but
        // the lock ensures no two tests that read `TMPDIR` overlap.
        unsafe { std::env::set_var("TMPDIR", &tmp); }

        write_static_svgs(&crate::provider::default_ring_colors());

        // Restore TMPDIR so the rest of the suite isn't affected.
        if let Some(v) = prev {
            // SAFETY: see above.
            unsafe { std::env::set_var("TMPDIR", v); }
        } else {
            // SAFETY: see above.
            unsafe { std::env::remove_var("TMPDIR"); }
        }

        // Verify all five files exist with non-zero content.
        for name in ["normal", "warning", "throttled", "error", "offline"] {
            let path = tmp.join(format!("minimax-quota-{name}.svg"));
            let body = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("missing/wrong-type {path:?}: {e}"));
            assert!(body.starts_with("<svg"),
                    "{path:?} should start with <svg; got {body:?}");
            assert!(body.contains("circle"),
                    "{path:?} should contain <circle>; got {body:?}");
        }

        // Idempotent — second call doesn't re-write or fail. (No
        // observable change since the files are byte-identical, but
        // the cache-hit path takes the early return.)
        write_static_svgs(&crate::provider::default_ring_colors());

        let _ = std::fs::remove_dir_all(&tmp);
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
            let (_w, _h, bytes) = render_pixmap(pct, bucket,
                &crate::provider::default_ring_colors()).unwrap();
            std::fs::write(
                format!("/tmp/icon_{}_{:?}.argb", pct, bucket), &bytes).unwrap();
        }
        assert!(Path::new("/tmp/icon_100_Normal.argb").exists());
    }
}
