#!/usr/bin/env bash
#
# uninstall.sh — remove minimax-quota-tray from the current user.
#
set -euo pipefail

# Stop + disable service (ignore errors if it was never enabled)
systemctl --user stop    minimax-quota.service 2>/dev/null || true
systemctl --user disable minimax-quota.service 2>/dev/null || true
systemctl --user daemon-reload                   2>/dev/null || true

# Remove installed files
rm -f "$HOME/.local/bin/minimax-quota-tray.js"
rm -f "$HOME/.config/systemd/user/minimax-quota.service"
echo "removed: script + systemd unit"

# Optionally remove config dir
if [ -d "$HOME/.config/minimax-quota" ]; then
  read -rp "Remove ~/.config/minimax-quota/ (config dir)? [y/N] " ans
  if [[ "$ans" =~ ^[Yy]$ ]]; then
    rm -rf "$HOME/.config/minimax-quota"
    echo "removed: config dir"
  fi
fi

# Optionally remove keyring entry
if command -v secret-tool >/dev/null 2>&1; then
  if secret-tool lookup application minimax-quota >/dev/null 2>&1; then
    read -rp "Remove stored API key from GNOME Keyring? [y/N] " ans
    if [[ "$ans" =~ ^[Yy]$ ]]; then
      secret-tool clear application minimax-quota 2>/dev/null && echo "removed: keyring entry"
    fi
  fi
fi

echo
echo "Uninstalled."
