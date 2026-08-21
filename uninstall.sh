#!/usr/bin/env bash
#
# uninstall.sh — remove llm-quota-tray from the current user.
#
set -euo pipefail

# Stop + disable service (ignore errors if it was never enabled)
systemctl --user stop    llm-quota-tray.service 2>/dev/null || true
systemctl --user disable llm-quota-tray.service 2>/dev/null || true
systemctl --user daemon-reload                   2>/dev/null || true

# Remove installed files
rm -f "$HOME/.local/bin/llm-quota-tray"
rm -f "$HOME/.config/systemd/user/llm-quota-tray.service"
echo "removed: binary + systemd unit"

# Optionally remove config dir
if [ -d "$HOME/.config/llm-quota-tray" ]; then
  read -rp "Remove ~/.config/llm-quota-tray/ (config dir)? [y/N] " ans
  if [[ "$ans" =~ ^[Yy]$ ]]; then
    rm -rf "$HOME/.config/llm-quota-tray"
    echo "removed: config dir"
  fi
fi

# Optionally remove keyring entry
if command -v secret-tool >/dev/null 2>&1; then
  if secret-tool lookup application llm-quota-tray >/dev/null 2>&1; then
    read -rp "Remove stored API key from your libsecret provider? [y/N] " ans
    if [[ "$ans" =~ ^[Yy]$ ]]; then
      secret-tool clear application llm-quota-tray 2>/dev/null \
        && echo "removed: keyring entry"
    fi
  fi
fi

# Optional: drop cached PNG ring icons the binary writes to TMPDIR
if [ -n "${TMPDIR:-}" ] && [ -d "$TMPDIR" ]; then
  rm -f "$TMPDIR"/llm-quota-tray-ring-*.png \
        "$TMPDIR"/llm-quota-tray-static-*.png 2>/dev/null || true
fi

echo
echo "Uninstalled."