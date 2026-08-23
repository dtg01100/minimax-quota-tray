# Launcher icon — source-of-truth SVG

This directory holds the master SVG for the launcher /
autostart / file-manager icon. The tray chip's icon is
**not** rendered from this SVG — it's rasterized at runtime
by [`src/icon.rs`](../../../src/icon.rs) using `tiny-skia`
so it can match the live quota percentage. This SVG is only
for the static launcher surfaces that can't run our renderer.

## File layout

```
packaging/icons/
├── source/
│   ├── llm-quota-tray.svg          ← this directory — single master
│   └── README.md                   ← (this file)
└── hicolor/
    ├── 16x16/apps/llm-quota-tray.png
    ├── 22x22/apps/llm-quota-tray.png
    ├── 24x24/apps/llm-quota-tray.png
    ├── 32x32/apps/llm-quota-tray.png
    ├── 48x48/apps/llm-quota-tray.png
    ├── 64x64/apps/llm-quota-tray.png
    ├── 96x96/apps/llm-quota-tray.png
    ├── 128x128/apps/llm-quota-tray.png
    └── 256x256/apps/llm-quota-tray.png
```

`install.sh` copies the nine PNGs into
`$XDG_DATA_HOME/icons/hicolor/<size>/apps/`. The master
SVG is **not** installed (see the install-time comment
block in [`install.sh`](../../../install.sh) for why).

## Regenerating the PNGs from the SVG

You only need to do this when the master SVG changes (geometry,
palette, anchor angle). Most icon edits should happen in the
SVG; the PNGs are just raster fallbacks for hosts without a
registered `libpixbufloader-svg.so` (Linuxbrew, immutable
distros like Bluefin / Fedora Atomic).

### Prerequisites

Any one of:

- [`rsvg-convert`](https://gitlab.gnome.org/GNOME/librsvg) —
  `sudo dnf install librsvg2-tools` / `sudo apt install librsvg2-bin`
- [Inkscape](https://inkscape.org/) — `sudo dnf install inkscape` /
  `sudo apt install inkscape`
- `magick convert` — ImageMagick, slowest but most portable

### Procedure (rsvg-convert — preferred)

From the repo root:

```sh
cd packaging/icons/source
for size in 16 22 24 32 48 64 96 128 256; do
  out="../hicolor/${size}x${size}/apps/llm-quota-tray.png"
  mkdir -p "$(dirname "$out")"
  rsvg-convert -w "$size" -h "$size" \
    --keep-aspect-ratio \
    llm-quota-tray.svg \
    -o "$out"
done
```

### Verifying

```sh
# Confirm the PNGs match the SVG visually.
for f in ../hicolor/*/apps/llm-quota-tray.png; do
  file "$f"     # should report PNG image data, 8-bit/color RGBA, non-interlaced
done
```

### Common pitfalls

- **16-bit PNGs.** `gdk-pixbuf` caps at 8-bit; if the renderer
  produces 16-bit/color RGBA, the launcher silently fails to
  render the icon. If you see blank launcher entries, re-run
  with an explicit depth flag (e.g. `rsvg-convert ... --format=png`
  doesn't have this, but `inkscape --export-type=png` does).
  The most recent regeneration commit (`icon: align launcher
  orientation with tray chip anchor`) includes a fix for this.
- **Background color.** The SVG has no background; rasterizers
  default to transparent. Don't add a fill to the SVG body
  expecting a launcher-style rectangle.
- **Orientation anchor.** The static arc is anchored at
  12 o'clock and grows clockwise 240° to 8 o'clock — same
  geometry as the runtime tray chip. If you change the chip's
  anchor in `src/icon.rs::write_ring_svg`, mirror it here.

## Icon contract summary

For the full reason behind the PNG-only install set (vs.
shipping a single scalable SVG), see
[`docs/freedesktop-integration.md`](../../../docs/freedesktop-integration.md)
— "Icons" section.
