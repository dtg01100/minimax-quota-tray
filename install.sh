#!/usr/bin/env bash
#
# install.sh — install llm-quota-tray for the current user.
#
# Builds the Rust release binary if it's not already built, copies it to
# ~/.local/bin/, installs the systemd user unit, writes a default config,
# and installs the freedesktop.org metadata files (.desktop,
# .metainfo.xml, hicolor icon). No gjs / gtk / libayatana-appindicator
# required at install time.
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

BIN_SRC="$ROOT/target/release/llm-quota-tray"
BIN_DEST="$HOME/.local/bin/llm-quota-tray"
SERVICE_DEST="$HOME/.config/systemd/user/llm-quota-tray.service"
# XDG Base Directory Spec paths — defaults match the spec when
# XDG_DATA_HOME is unset. Used for the .desktop / .metainfo / icon
# installs below.
XDG_DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
DESKTOP_DIR="$XDG_DATA_HOME/applications"
METAINFO_DIR="$XDG_DATA_HOME/appdata"
ICON_DIR="$XDG_DATA_HOME/icons/hicolor/scalable/apps"
# Default config dir uses a neutral basename (`llm-quota-tray`) so the
# path doesn't bake the provider name in. Named instances append
# `-<name>` (e.g. `llm-quota-tray-coding`, `llm-quota-tray-openai`).
CONFIG_DIR="$HOME/.config/llm-quota-tray"
CONFIG_DEST="$CONFIG_DIR/config.json"

# Sanity checks
command -v cargo >/dev/null 2>&1 || {
  echo "error: cargo (Rust toolchain) not found" >&2
  echo "  install via rustup: https://rustup.rs/" >&2
  exit 1
}
command -v systemctl >/dev/null 2>&1 || {
  echo "error: systemctl not found (not a systemd system?)" >&2
  exit 1
}
# The Rust binary links against libdbus (via zbus) — ubiquitous on
# modern Linux. libsecret is reached through D-Bus as well, so
# any libsecret provider (gnome-keyring, KWallet, KeePassXC's
# secret-service bridge) is picked up at runtime with no extra
# build dep. The CLI tooling (`secret-tool`, `zenity`, `kdialog`)
# is only needed for the optional `--set-key` interactive flow —
# missing tools fall back gracefully: zenity → kdialog → terminal
# escape hatch documented in the error message.

# `cargo build --release` is incremental — a fresh checkout will take
# ~1-2 min (cold link), subsequent rebuilds are seconds.
if [ ! -x "$BIN_SRC" ] || [ -n "$(find "$ROOT/src" -newer "$BIN_SRC" 2>/dev/null | head -1)" ]; then
  echo "building llm-quota-tray (release)…"
  (cd "$ROOT" && cargo build --release)
fi

# Install binary
install -d "$HOME/.local/bin"
install -m 0755 "$BIN_SRC" "$BIN_DEST"
echo "installed: $BIN_DEST"

# Install systemd unit
install -d "$HOME/.config/systemd/user"
install -m 0644 "$ROOT/llm-quota-tray.service" "$SERVICE_DEST"
echo "installed: $SERVICE_DEST"

# Install the XDG autostart entry. This wires up GNOME Settings'
# "Startup Applications" / KDE's "Autostart" toggle for the user —
# toggling "Start Automatically" creates / removes this file
# (or sets `Hidden=true`). Note: this is in addition to the
# systemd service above, not a replacement — the systemd service
# is the canonical boot path (it gives us Restart=on-failure and
# After=graphical-session.target). The autostart .desktop is here
# purely so the user-facing toggle in the shell's settings panel
# is functional. Double-firing at login is safe: the daemon's
# per-instance PID lock (`src/lock.rs`) detects the live second
# instance and exits.
AUTOSTART_DIR="$HOME/.config/autostart"
AUTOSTART_DEST="$AUTOSTART_DIR/llm-quota-tray.desktop"
install -d "$AUTOSTART_DIR"
install -m 0644 "$ROOT/packaging/llm-quota-tray.desktop" "$AUTOSTART_DEST"
echo "installed: $AUTOSTART_DEST"

# First-run config (don't clobber an existing one)
if [ ! -f "$CONFIG_DEST" ]; then
  install -d -m 0700 "$CONFIG_DIR"
  install -m 0600 "$ROOT/config.example.json" "$CONFIG_DEST"
  echo "wrote default config: $CONFIG_DEST"
fi

# Install freedesktop metadata. These are the files that let the
# desktop shell (gnome-shell, KDE Plasma, XFCE, …) recognise us
# as an application rather than a mystery binary:
#   - .desktop      → app launcher, autostart dialog, gnome-software
#   - .metainfo.xml → AppStream catalog (gnome-software index,
#                     KDE Discover, Flathub listings)
#   - icon          → app icon outside the tray (autostart UI,
#                     gnome-control-center, file manager thumbs)
if [ -f "$ROOT/packaging/llm-quota-tray.desktop" ]; then
  install -d "$DESKTOP_DIR"
  install -m 0644 "$ROOT/packaging/llm-quota-tray.desktop" \
    "$DESKTOP_DIR/llm-quota-tray.desktop"
  echo "installed: $DESKTOP_DIR/llm-quota-tray.desktop"
fi
if [ -f "$ROOT/packaging/io.github.dtg01100.llm-quota-tray.metainfo.xml" ]; then
  install -d "$METAINFO_DIR"
  install -m 0644 \
    "$ROOT/packaging/io.github.dtg01100.llm-quota-tray.metainfo.xml" \
    "$METAINFO_DIR/io.github.dtg01100.llm-quota-tray.metainfo.xml"
  echo "installed: $METAINFO_DIR/io.github.dtg01100.llm-quota-tray.metainfo.xml"
fi
# Install the hicolor-theme icons. We intentionally ship ONLY PNGs
# (no SVG under `scalable/`) for the launcher icon, despite the
# canonical modern best-practice being SVG. Reason: the freedesktop
# Icon Theme Spec says the launcher always prefers a scalable
# variant when one exists, and silently uses it; on hosts without a
# registered `libpixbufloader-svg.so` (notably Linuxbrew-based and
# immutable distros like Bluefin / Fedora Atomic / Silverblue with
# an empty `loaders.cache`), loading the SVG fails and the launcher
# entry shows blank. Shipping PNGs at every common size (16, 22, 24,
# 32, 48, 64, 96, 128, 256) means there's always a file the loader
# can render at the size the panel/launcher asks for. The canonical
# master SVG lives at `packaging/icons/source/llm-quota-tray.svg`
# and is the regeneration source for the PNGs.
#
# `ICON_DIR` (used later in the cache-refresh step) points at the
# 256x256 directory — any non-empty hicolor subdir works for
# `gtk-update-icon-cache`.
ICON_BASE="$XDG_DATA_HOME/icons/hicolor"
ICON_DIR="$ICON_BASE/256x256/apps"
for size in 16x16 22x22 24x24 32x32 48x48 64x64 96x96 128x128 256x256; do
  src="$ROOT/packaging/icons/hicolor/$size/apps/llm-quota-tray.png"
  if [ -f "$src" ]; then
    dest="$ICON_BASE/$size/apps"
    install -d "$dest"
    install -m 0644 "$src" "$dest/llm-quota-tray.png"
    echo "installed: $dest/llm-quota-tray.png"
  fi
done
# Refresh the icon and desktop caches so the freshly-installed
# files become visible to the desktop shell immediately, without
# requiring a logout. Both `update-desktop-database` and
# `gtk-update-icon-cache` are best-effort — not all distros ship
# them, and the cache is rebuilt lazily on the next session start
# either way.
command -v update-desktop-database >/dev/null 2>&1 \
  && update-desktop-database "$DESKTOP_DIR" 2>/dev/null || true
command -v gtk-update-icon-cache >/dev/null 2>&1 \
  && gtk-update-icon-cache -f -t "$XDG_DATA_HOME/icons/hicolor" 2>/dev/null || true

systemctl --user daemon-reload

# Enable + start (only if a graphical session is reachable)
if [ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]; then
  systemctl --user enable --now llm-quota-tray.service || true
  echo
  echo "Service enabled and started."
  echo "Next: click the chip in your panel → 'Set API Key…' to store your key."
else
  echo
  echo "Service installed but not started (no graphical session detected)."
  echo "After logging into your desktop, run:"
  echo "  systemctl --user enable --now llm-quota-tray.service"
fi
