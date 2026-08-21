#!/usr/bin/env bash
#
# install.sh — install minimax-quota-tray for the current user.
#
# Builds the Rust release binary if it's not already built, copies it to
# ~/.local/bin/, installs the systemd user unit, and writes a default
# config. No gjs / gtk / libayatana-appindicator required at install time.
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

BIN_SRC="$ROOT/target/release/minimax-quota-tray"
BIN_DEST="$HOME/.local/bin/minimax-quota-tray"
SERVICE_DEST="$HOME/.config/systemd/user/minimax-quota.service"
# Default config dir uses a neutral basename (`quota-tray`) so the
# path doesn't bake the provider name in. Named instances append
# `-<name>` (e.g. `quota-tray-coding`, `quota-tray-openai`).
CONFIG_DIR="$HOME/.config/quota-tray"
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
# Rust binary links against libsecret + libdbus — both ubiquitous on
# modern Linux. Warn (don't fail) if libsecret-tools is missing, since
# the binary degrades gracefully (env-var fallback, see README).
command -v secret-tool >/dev/null 2>&1 || {
  echo "warning: secret-tool not installed — only MINIMAX_API_KEY env var will work" >&2
  echo "  try: dnf install libsecret-tools   /   apt install libsecret-tools" >&2
}

# Build the release binary if it's not there or the source is newer.
# `cargo build --release` is incremental — a fresh checkout will take
# ~1-2 min (cold link), subsequent rebuilds are seconds.
if [ ! -x "$BIN_SRC" ] || [ -n "$(find "$ROOT/src" -newer "$BIN_SRC" 2>/dev/null | head -1)" ]; then
  echo "building minimax-quota-tray (release)…"
  (cd "$ROOT" && cargo build --release)
fi

# Install binary (rename so it doesn't carry the .js suffix anymore)
install -d "$HOME/.local/bin"
install -m 0755 "$BIN_SRC" "$BIN_DEST"
echo "installed: $BIN_DEST"

# Install systemd unit
install -d "$HOME/.config/systemd/user"
install -m 0644 "$ROOT/minimax-quota.service" "$SERVICE_DEST"
echo "installed: $SERVICE_DEST"

# First-run config (don't clobber an existing one)
if [ ! -f "$CONFIG_DEST" ]; then
  install -d -m 0700 "$CONFIG_DIR"
  install -m 0600 "$ROOT/config.example.json" "$CONFIG_DEST"
  echo "wrote default config: $CONFIG_DEST"
fi

systemctl --user daemon-reload

# Enable + start (only if a graphical session is reachable)
if [ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]; then
  systemctl --user enable --now minimax-quota.service || true
  echo
  echo "Service enabled and started."
  echo "Next: click the chip in your panel → 'Set API Key…' to store your key."
else
  echo
  echo "Service installed but not started (no graphical session detected)."
  echo "After logging into your desktop, run:"
  echo "  systemctl --user enable --now minimax-quota.service"
fi