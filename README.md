# MiniMax Quota Tray

A standalone GNOME Shell tray indicator for MiniMax API quota. Supports
both the **Coding Plan** and the **Token Plan** via your own API key.
Talks directly to the MiniMax API — no Hermes, no other agent, no
plugin system.

![Menu preview](menu.txt)

## What it shows

- **Chip** in the top bar: `<Plan> <remaining>%` with an icon that flips
  to a warning glyph when usage crosses the configured thresholds.
- **Menu**:
  ```
  Plan: Coding Plan
    5h: 95% left · resets in 4h 32m
    ████████████████████░░
    weekly: 100% left · resets in 6d 8h
    ███████████████████████
    ⚠ Throttled
    ───
    Refresh now
    Open dashboard
    Set API Key…
    ───
    Quit
  ```

## Requirements

- GNOME Shell 45+ (tested on 50.3)
- `gjs` ≥ 1.86 (for ESM imports)
- `libgtk-3`, `libsecret`, `libsoup-3.0` (GObject Introspection typelibs)
- `libayatana-appindicator` (or the **AppIndicator** GNOME Shell extension
  enabled from <https://extensions.gnome.org>)
- `secret-tool` (from `libsecret-tools`)
- A MiniMax API key

## Install

```bash
git clone https://github.com/dtg01100/minimax-quota-tray.git
cd minimax-quota-tray
./install.sh
```

The installer copies the script to `~/.local/bin/`, the systemd unit to
`~/.config/systemd/user/`, and writes a default config to
`~/.config/minimax-quota/config.json` (skipped if it already exists).
After it finishes, click the chip in your top bar → **Set API Key…** to
store your key in GNOME Keyring.

If you're not in a graphical session, run the installer from a desktop
session or start the service manually after logging in:
```bash
systemctl --user enable --now minimax-quota.service
```

## Uninstall

```bash
./uninstall.sh
```

Stops and disables the service, removes the installed files, and
optionally purges your config dir and the stored key from GNOME Keyring.

## Configuration

`~/.config/minimax-quota/config.json`:

```json
{
  "plan": "coding_plan",
  "refresh_seconds": 60,
  "plans": {
    "coding_plan": {
      "endpoint":      "https://api.minimax.io/v1/api/openplatform/coding_plan/remains",
      "dashboard_url": "https://api.minimax.chat/user-center/payment/balance",
      "label": "Coding Plan"
    },
    "token_plan": {
      "endpoint":      "https://api.minimax.io/v1/token_plan/remains",
      "dashboard_url": "https://platform.minimax.io/user-center/payment/token-plan",
      "label": "Token Plan"
    }
  },
  "icon_name":    "appointment-soon-symbolic",
  "warning_icon": "dialog-warning-symbolic",
  "thresholds":   { "yellow": 60, "red": 85 }
}
```

- `plan` — `"coding_plan"` or `"token_plan"`. Switch by editing + restarting.
- `plans.<id>.endpoint` — override to point at a proxy.
- `thresholds` — the warning icon swap fires when **used** % exceeds these
  (i.e., yellow at 60% used, red at 85% used). The chip label always shows
  **remaining** %.

## How it works

```
   ┌──────────────────────────┐
   │   minimax-quota-tray.js  │   gjs ESM, GTK 3, Soup 3, Secret-1
   └────────────┬─────────────┘
                │
   ┌────────────┴─────────────┐
   │  AyatanaAppIndicator3     │   StatusNotifierItem via the
   │                           │   AppIndicator GNOME extension
   └────────────┬─────────────┘
                │
   GTK Menu  ←  Soup-3.0  →  api.minimax.io/v1/{coding_plan|token_plan}/remains
                ↓
   Secret-1 (read) → GNOME Keyring (login)
   secret-tool    ← writes via Gio.Subprocess stdin pipe
```

No custom widgets in the menu — SNI menus render `Gtk.ProgressBar` and
`Gtk.DrawingArea` inconsistently (the trough blends with the background,
`draw` signals don't fire reliably). The bar is Unicode block characters
(`█` / `░`) with Pango color markup applied to the menu item's child
`GtkLabel` via `item.get_child().set_markup()`.

The keyring write goes through `secret-tool` (GNOME's own CLI) via
`Gio.Subprocess` with `STDIN_PIPE` — direct argv, no shell, no temp
file. The `Secret-1` gjs binding for `password_store_sync` has
unreliable arg semantics across libsecret versions, so writes are
shelled out; reads use `Secret.password_lookup_sync` directly.

## Troubleshooting

- **Three dots where the icon should be** — your icon theme doesn't have
  the configured `icon_name`. The default `appointment-soon-symbolic`
  ships with Adwaita. Inspect with
  `gjs -c "imports.gi.versions.GTK='3.0'; const t=Gtk.IconTheme.get_default(); print(t.lookup_icon('your-icon', 16, 0)?.get_filename() ?? 'NOT FOUND');"`
- **`Argument password may not be null`** — your GNOME Keyring is locked.
  Unlock it or re-enter the key from the menu (click chip → **Set API Key…**).
- **`Requiring Gtk, version 3.0: ... '4.0' is already loaded`** — make
  sure nothing in your pipeline imports `gi://Gdk` without
  `?version=3.0`. Drop the bare `Gdk` import.
- **`secret_service_create_item_dbus_path: assertion ... collection_path != NULL`**
  — the GNOME Keyring daemon isn't reachable. Check that
  `gnome-keyring-daemon --components=secrets` is running.

## License

MIT © David Lafreniere
