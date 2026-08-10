#!/usr/bin/env bash
#
# install.sh — install minimax-quota-tray for the current user.
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

BIN_DEST="$HOME/.local/bin/minimax-quota-tray.js"
SERVICE_DEST="$HOME/.config/systemd/user/minimax-quota.service"
CONFIG_DIR="$HOME/.config/minimax-quota"
CONFIG_DEST="$CONFIG_DIR/config.json"

# Sanity checks
command -v gjs >/dev/null 2>&1 || {
  echo "error: gjs not installed" >&2
  echo "  try: dnf install gjs   (Fedora)   /   apt install gjs   (Debian/Ubuntu)" >&2
  exit 1
}
command -v secret-tool >/dev/null 2>&1 || {
  echo "error: secret-tool not installed" >&2
  echo "  try: dnf install libsecret-tools   /   apt install libsecret-tools" >&2
  exit 1
}
command -v systemctl >/dev/null 2>&1 || {
  echo "error: systemctl not found (not a systemd system?)" >&2
  exit 1
}

# Install binary
install -d "$HOME/.local/bin"
install -m 0755 "$ROOT/minimax-quota-tray.js" "$BIN_DEST"
echo "installed: $BIN_DEST"

# Install systemd unit
install -d "$HOME/.config/systemd/user"
install -m 0644 "$ROOT/minimax-quota.service" "$SERVICE_DEST"
echo "installed: $SERVICE_DEST"

# First-run config (don't clobber an existing one)
if [ ! -f "$CONFIG_DEST" ]; then
  install -d "$CONFIG_DIR"
  install -m 0600 "$ROOT/config.example.json" "$CONFIG_DEST"
  echo "wrote default config: $CONFIG_DEST"
fi

systemctl --user daemon-reload

# Enable + start (only if a graphical session is reachable)
if [ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]; then
  systemctl --user enable --now minimax-quota.service || true
  echo
  echo "Service enabled and started."
  echo "Next: click the chip in your top bar → 'Set API Key…' to store your key."
else
  echo
  echo "Service installed but not started (no graphical session detected)."
  echo "After logging into your desktop, run:"
  echo "  systemctl --user enable --now minimax-quota.service"
fi
