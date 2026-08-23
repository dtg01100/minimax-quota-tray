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
# Drop the XDG autostart copy (so the GNOME Settings toggle
# reflects "not installed" after uninstall). The systemd symlink
# in `default.target.wants/` is also a file — remove it explicitly
# so `disable` warnings don't fire on next login.
rm -f "$HOME/.config/autostart/llm-quota-tray.desktop"
rm -f "$HOME/.config/systemd/user/default.target.wants/llm-quota-tray.service"
echo "removed: binary + systemd unit + autostart entry"

# Remove the freedesktop metadata files installed by install.sh.
# Same XDG Base Directory defaults as install.sh; XDG_DATA_HOME
# overrides the data root for both.
XDG_DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
DESKTOP_DIR="$XDG_DATA_HOME/applications"
METAINFO_DIR="$XDG_DATA_HOME/appdata"
ICON_BASE="$XDG_DATA_HOME/icons/hicolor"
rm -f "$DESKTOP_DIR/llm-quota-tray.desktop"
rm -f "$METAINFO_DIR/io.github.dtg01100.llm-quota-tray.metainfo.xml"
# Drop the PNG fallbacks at every size we ship. Same set install.sh
# writes — keeps uninstall/install in lockstep if the size list ever
# changes. We also drop any leftover SVG from older installs (the
# launcher SVG was removed from the install set; this catches the
# stale file from previous installs).
for size in 16x16 22x22 24x24 32x32 48x48 64x64 96x96 128x128 256x256; do
  rm -f "$ICON_BASE/$size/apps/llm-quota-tray.png"
done
rm -f "$ICON_BASE/scalable/apps/llm-quota-tray.svg"
echo "removed: freedesktop metadata (.desktop, metainfo, icons)"

# Refresh the desktop and icon caches so the launcher stops
# showing the uninstalled entry on the next panel render. Both
# tools are best-effort, matching install.sh.
command -v update-desktop-database >/dev/null 2>&1 \
  && update-desktop-database "$DESKTOP_DIR" 2>/dev/null || true
command -v gtk-update-icon-cache >/dev/null 2>&1 \
  && gtk-update-icon-cache -f -t "$XDG_DATA_HOME/icons/hicolor" 2>/dev/null || true

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