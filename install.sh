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
if [ -f "$ROOT/packaging/icons/hicolor/scalable/apps/llm-quota-tray.svg" ]; then
  install -d "$ICON_DIR"
  install -m 0644 \
    "$ROOT/packaging/icons/hicolor/scalable/apps/llm-quota-tray.svg" \
    "$ICON_DIR/llm-quota-tray.svg"
  echo "installed: $ICON_DIR/llm-quota-tray.svg"
fi
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
