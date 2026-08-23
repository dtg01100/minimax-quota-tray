# freedesktop.org integration

This document describes the desktop integration contract for
`llm-quota-tray`: the metadata files installed alongside the binary
and the runtime protocol hooks that make the tray behave like a
native freedesktop-conforming application.

## Specs targeted

| Concern | Spec | Files |
|---|---|---|
| App menu / launcher entry | [Desktop Entry Spec](https://specifications.freedesktop.org/desktop-entry-spec/latest/) | `packaging/llm-quota-tray.desktop` |
| App catalog metadata | [AppStream Spec](https://specifications.freedesktop.org/appstream/) | `packaging/io.github.dtg01100.llm-quota-tray.metainfo.xml` |
| Icon resolution | [Icon Theme Spec](https://specifications.freedesktop.org/icon-theme-spec/) | `packaging/icons/hicolor/scalable/apps/llm-quota-tray.svg` |
| Launch animation | [XDG Activation](https://specifications.freedesktop.org/xdg-activation/) | `--token=<token>` CLI flag, `$XDG_ACTIVATION_TOKEN` env var, `src/activation.rs` |
| Portal dispatch | [Desktop Portal](https://flatpak.github.io/xdg-desktop-portal/) | `src/portal_openuri.rs`, `src/notify.rs` (OpenURI + Notification portal paths) |

## Installed files

`install.sh` copies the three metadata files into the XDG Base
Directory locations (defaults shown; `XDG_DATA_HOME` overrides):

| Source | Destination | Purpose |
|---|---|---|
| `packaging/llm-quota-tray.desktop` | `$XDG_DATA_HOME/applications/llm-quota-tray.desktop` | App launcher entry, autostart checkbox in `gnome-control-center`, shell context menu |
| `packaging/io.github.dtg01100.llm-quota-tray.metainfo.xml` | `$XDG_DATA_HOME/appdata/io.github.dtg01100.llm-quota-tray.metainfo.xml` | AppStream index (`gnome-software`, KDE Discover) |
| `packaging/icons/hicolor/16x16/apps/llm-quota-tray.png` | `$XDG_DATA_HOME/icons/hicolor/16x16/apps/llm-quota-tray.png` | hicolor theme icon, 16×16 |
| `packaging/icons/hicolor/22x22/apps/llm-quota-tray.png` | `$XDG_DATA_HOME/icons/hicolor/22x22/apps/llm-quota-tray.png` | hicolor theme icon, 22×22 (panel size) |
| `packaging/icons/hicolor/24x24/apps/llm-quota-tray.png` | `$XDG_DATA_HOME/icons/hicolor/24x24/apps/llm-quota-tray.png` | hicolor theme icon, 24×24 (GNOME top bar) |
| `packaging/icons/hicolor/32x32/apps/llm-quota-tray.png` | `$XDG_DATA_HOME/icons/hicolor/32x32/apps/llm-quota-tray.png` | hicolor theme icon, 32×32 |
| `packaging/icons/hicolor/48x48/apps/llm-quota-tray.png` | `$XDG_DATA_HOME/icons/hicolor/48x48/apps/llm-quota-tray.png` | hicolor theme icon, 48×48 (app drawer) |
| `packaging/icons/hicolor/64x64/apps/llm-quota-tray.png` | `$XDG_DATA_HOME/icons/hicolor/64x64/apps/llm-quota-tray.png` | hicolor theme icon, 64×64 |
| `packaging/icons/hicolor/96x96/apps/llm-quota-tray.png` | `$XDG_DATA_HOME/icons/hicolor/96x96/apps/llm-quota-tray.png` | hicolor theme icon, 96×96 (HiDPI launcher) |
| `packaging/icons/hicolor/128x128/apps/llm-quota-tray.png` | `$XDG_DATA_HOME/icons/hicolor/128x128/apps/llm-quota-tray.png` | hicolor theme icon, 128×128 (HiDPI launcher) |
| `packaging/icons/hicolor/256x256/apps/llm-quota-tray.png` | `$XDG_DATA_HOME/icons/hicolor/256x256/apps/llm-quota-tray.png` | hicolor theme icon, 256×256 (large launcher / Settings) |
| `packaging/icons/source/llm-quota-tray.svg` | *(not installed)* | Master SVG; regeneration source for the PNGs |
| `packaging/llm-quota-tray.desktop` | `$HOME/.config/autostart/llm-quota-tray.desktop` | XDG autostart entry — wires up GNOME Settings' "Startup Applications" / KDE Autostart toggle. Parallel to the systemd user service above (the service is the canonical boot path; this entry exists so the user-facing toggle in the shell is functional). Double-firing at login is deduped by the daemon's PID lock (`src/lock.rs`). |

`update-desktop-database` and `gtk-update-icon-cache` are invoked
best-effort after install so the new files become visible to the
desktop shell immediately. Missing tools are not fatal — the cache
is rebuilt lazily on the next session start either way.

## Multi-instance model

The `.desktop` file declares `Exec=llm-quota-tray %u` with a
single `StartupNotify=true` and no `--instance=` flag. Multiple
concurrent instances follow the same model as the existing systemd
service unit pattern:

- The `.desktop` file launches the **default** instance.
- Each additional instance is owned by a copy of the
  `llm-quota-tray.service` systemd unit (renamed and given an
  `--instance=<name>` ExecStart flag). The desktop shell's autostart
  checkbox for the additional instance requires a corresponding
  `.desktop` file with the same `Exec=` line plus the
  `--instance=<name>` flag — those are not shipped by default but
  are trivial to template from the canonical one.

This matches how Firefox (one `.desktop`, multi-profile) and Docker
(one `.desktop`, multi-container) handle the same problem: one
canonical entry for the typical case, opt-in duplication for power
users.

## XDG Activation

The `.desktop` file's `StartupNotify=true` instructs compliant
desktop shells (GNOME, KDE) to generate an [XDG Activation
token](https://specifications.freedesktop.org/xdg-activation/)
when launching the app via the shell. The shell writes the token
to one of:

1. `$XDG_ACTIVATION_TOKEN` env var (canonical XDG route), or
2. `--token=<token>` CLI argument (substituted via the shell's
   token-aware Exec= machinery).

`src/activation.rs` resolves the token once at startup
(`activation::init()` from `main()`, immediately after
`instance::init()`). The token is forwarded to the two portal
call sites:

- `portal_openuri::open(url, activation_token)` — passed as the
  `activation_token` vardict key on the `OpenURI` portal call
  (OpenURI portal v4+). Animates the handler-selection dialog
  from the originating click.
- `notify::send(tag, title, body, urgency, activation_token)` —
  passed as the `activation_token` vardict key on the
  `AddNotification` portal call (Notification portal v2+).
  Animates the bucket-transition notification from the
  originating click when the transition fires close enough to
  launch that the token is still valid.

The direct-Notifications fallback does **not** honor activation
tokens (the bare `org.freedesktop.Notifications.Notify` API has
no field for it). The token is simply omitted on that path.

### Why this matters

Without the activation token, the user clicks the chip in their
app launcher and the dashboard URL opens in a browser with no
visible continuity — the chooser dialog (if any) appears out of
nowhere, the notification appears unattached. With the token,
the shell animates from the originating click to the resulting
portal dialog, matching how every native app behaves. The user
sees the same animation they get from clicking a link in
GNOME Files or a notification in KDE Connect.

## What's deliberately not done

These are all real freedesktop standards, but each would
materially expand the project's surface area in ways that
conflict with its lightweight charter:

- **`org.freedesktop.Application` D-Bus interface** — required for
  `DBusActivatable=true` autostart. Adds a second D-Bus
  interface, lifecycle event handling, and CLI flag handlers
  for `Actions=`. The systemd service unit already provides
  equivalent autostart semantics.
- **`Actions=` in the `.desktop` file** — exposes menu actions
  on the desktop shell's app context menu. Requires CLI
  handlers for each action. The tray's own dbusmenu is already
  richer (per-window rows, live burn-rate projections).
- **`<screenshots>` / `<releases>` in the AppStream metadata** —
  would require asset screenshots per release and lockstep
  discipline between `CHANGELOG.md` and the metainfo file.
  The CHANGELOG stays the source of truth.
- **`flatpak` / `snap` manifests** — the project's install path
  is `git clone && ./install.sh`; packaging manifest maintenance
  is out of scope for this slice.
- **`IconThemePath` (KDE SNI extension)** — would publish a custom
  icon theme at runtime. The SVG-as-`IconName` path already works
  on hosts that prefer name-based lookup.
- **GSettings / dconf schema** — would replace the per-instance
  JSON config. Explicitly avoided: per-instance namespacing,
  arbitrary providers, and arbitrary ring colors are first-class
  features of the current config design.

Each can be revisited when there's a concrete need (Flathub
packaging would justify the flatpak manifest + screenshots;
flatpak sandboxing would justify the `org.freedesktop.Application`
interface; etc.).

## Operating notes

### D-Bus signal emissions are bounded

Every SNI signal emitted by the daemon
(`NewIcon`/`NewTitle`/`NewStatus` for the chip,
`ItemsUpdated`/`LayoutUpdated` for the menu) is wrapped in
`emit_signal_with_timeout()` (`src/sni.rs`) with a 5-second
budget, and the explicit `RegisterStatusNotifierItem` call at
startup gets the same treatment. The chip state in
`SharedState` is updated **before** the signal is fired, so a
missed signal only delays the panel's view by one poll cycle
(`refresh_seconds`) — it can never deadlock the daemon.

We observed the original bug exactly once: after a back-to-back
restart (SIGTERM the old daemon, start the new one within a few
seconds), the new daemon hung in `render_initial` because the
SNI watcher was still cleaning up the previous daemon's
registration and `Connection::emit_signal()` blocked indefinitely
against it. The daemon never logged the third "started; refresh
every" line and the chip stayed at its initial fallback icon
(`dialog-information-symbolic`) forever. Recovery required a
clean `systemctl --user stop` + `sleep 3` + `start`. The fix
removes that whole failure mode.
