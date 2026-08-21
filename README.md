# MiniMax Quota Tray

A standalone freedesktop tray indicator for MiniMax API quota. Supports
both the **Coding Plan** and the **Token Plan** via your own API key.
Talks directly to the MiniMax API — no agent or plugin system required.

![Menu preview](menu.txt)

Implements the [KDE StatusNotifierItem][sni] + `com.canonical.dbusmenu`
protocols, so it works on any panel that speaks SNI: KDE Plasma,
GNOME Shell (with the [AppIndicator][appindicator] extension), XFCE,
swaybar, Waybar, Cairo-Dock, etc. No GTK/Qt deps in the Rust binary —
the host's panel does all the rendering from the dbusmenu tree.

[sni]: https://www.freedesktop.org/wiki/Specifications/StatusNotifierItem/
[appindicator]: https://github.com/ubuntu/gnome-shell-extension-appindicator

> The gjs implementation has been retired. The Rust port is the
> only supported implementation on `main`. (~5000 LOC across 14
> modules, 126 unit tests, ~5.5 MB release binary, ~10 MB RSS.) No GUI
> library at all — talks to D-Bus directly and renders the icon as ARGB
> bytes via `resvg` (or a raw BGRA circle routine for the static
> states). No `~/.so` link deps beyond libc + libsecret + libdbus;
> works on any Linux distro without needing gtk3/libappindicator/etc.
> installed.

## What it shows

- **Chip** in the top bar: `<Plan> <remaining>%` with an icon that flips
  to a warning glyph when usage crosses the configured thresholds.
- **Menu** (Coding Plan — both windows compute in `%/h` mode; token plans
  show `tok/h`):
  ```
  Plan: Coding Plan
    5h: 80% left · resets in 4h
    ██████████████████░░░░
    · on pace to have ~62% left at reset (15%/h)
    weekly: 100% left · resets in 6d 8h
    ███████████████████████
    ───
    Refresh now
    Open dashboard
    Set API Key…
    ───
    Quit
  ```
- **Burn-rate row** — once the tray has ~10 minutes of polling history
  for a window, it estimates the burn rate from that window's own sample
  history, and a row under the window's bar shows the projection. Each
  window has its own history — a quiet 5h window doesn't pollute the
  weekly rate, and vice versa. The 5h window's row is the one that drives
  the chip's warning flip; other windows' rows are informational.
  - healthy (token-based plan): `· on pace to have ~48% left at reset (40 tok/h)`
  - healthy (Coding Plan, pct-based): `· on pace to have ~62% left at reset (15%/h)`
    *(verified live 2026-08-18: the Coding Plan returns
    `current_interval_total_count=0`, `current_interval_usage_count=0`,
    `current_weekly_total_count=0`, and `current_weekly_usage_count=0`.
    Both windows track consumption via `*_remaining_percent` only, so
    the rate is %/h on the Coding Plan. Token plans / any provider that
    exposes real count fields will use the `tok/h` variant.)*
  - **Pct-only windows with no signal are suppressed** — on the Coding
    Plan at 120s polls, per-poll drops are ~0.01–0.05% on weekly usage,
    well below the 1% precision of `*_remaining_percent`. The percent
    barely ticks, so the slope over the 1h lookback is meaningless. A
    pct-only window only gets a row when its rate is non-zero (i.e. the
    percent has actually crossed a 1% boundary recently). Token-counting
    providers keep the row for all windows — their rate is computed from
    real count deltas, which stays meaningful at any window length.
  - warning (projected exhaustion before reset):
    `⚠ 1.2k tok/h → exhausts ~1h 5m before reset` (or `⚠ 60%/h → exhausts ~22m before reset`)
    — and the top-bar chip flips to the warning color based on the 5h window
    alone, even when the remaining-% thresholds look fine.

  The projection resets on every window rollover; it's an estimate, not a
  promise — bursty usage will make it conservative or optimistic accordingly.

## Requirements

### Runtime

- Linux, glibc ≥ 2.31 (uses epoll + io_uring syscalls)
- A panel that speaks `org.kde.StatusNotifierItem`:
  - **KDE Plasma** — works natively, no setup
  - **GNOME Shell** — needs the [AppIndicator extension][appindicator] (the
    gnome-shell-extension-appindicator package on Fedora/Ubuntu)
  - **XFCE** with `xfce4-panel` + the `xfce4-statusnotifier-plugin`
  - **swaybar**, **Waybar**, **Cairo-Dock**, **trayer-srg** — all work
    natively (they speak SNI directly)
- `libsecret` for the keyring (any libsecret provider — GNOME Keyring,
  KWallet, KeePassXC's secret-service bridge)
- A MiniMax API key (stored in the keyring under the `quota-tray`
  application, in `~/.config/.config/quota-tray/key`, or in the
  `MINIMAX_API_KEY` env var)

### Build (for `install.sh`, which compiles from source)

- Rust toolchain (install via [rustup](https://rustup.rs/))

## Install

```bash
git clone https://github.com/dtg01100/minimax-quota-tray.git
cd minimax-quota-tray
./install.sh
```

The installer compiles the Rust binary if needed
(`cargo build --release` — first build is ~1-2 min, subsequent rebuilds
are seconds since `target/` is cached), copies it to `~/.local/bin/`,
installs the systemd unit to `~/.config/systemd/user/`, and writes a
default config to `~/.config/quota-tray/config.json` (skipped if it
already exists). After it finishes, click the chip in your panel →
**Set API Key…** to store your key in your libsecret provider
(GNOME Keyring, KWallet, etc.).

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
optionally purges your config dir and the stored key from your libsecret
provider.

## Tests

The poll scheduler has a unit harness that simulates overlapping
`refresh()` calls — the exact scenario that used to spawn multiple,
permanently self-rescheduling polling chains:

```bash
./tests/run.sh
```

It imports the real app module with `MINIMAX_QUOTA_TEST=1` (which skips
`main()`), swaps the network / tray / menu / notification hooks for fakes,
and asserts the single-flight invariant: at most one poll timeout is ever
armed, and an explicit refresh arriving mid-fetch is queued exactly once.
The suite also covers offline handling, the no-key state, backoff after
errors, threshold-notification dedup, and the burn-rate projection
(history gating, rollover resets, the warn/don't-warn decision, the
projected-% at reset label, and the chip bucket flip — driven under a
stubbed clock so the slope math is deterministic). The burn-rate
coverage exercises both windows independently: T18 verifies the weekly
rate is computed from weekly samples (a quiet 5h doesn't pollute it),
T19 verifies the 5h rollover does not clear the weekly history, and T21
verifies the pct-only suppression rule — pct-only windows with no signal
(skip the weekly row on Coding Plan, skip idle 5h on any pct-only API)
while token-counting providers keep the row for all windows.

`tests/regression-scheduler.test.js` documents the bug this guards against:
it runs a faithful replica of the pre-fix scheduler (extracted from commit
`d4d07cd`, where every manual refresh spawned a second self-rescheduling
poll chain) head-to-head against the fixed app — Part A reproduces the
stacked chains and request burst, Part B proves the fixed code has exactly
one chain and no burst. The suite is red against the pre-fix algorithm and
green against the fix; the real-timer Part B test is the decisive detector
if the cancel-before-arm logic is ever removed.

## Configuration

`~/.config/quota-tray/config.json` (one instance = one config = one
tray icon; see [Multiple instances](#multiple-instances) below for
running more than one):

```json
{
  "endpoint":      "https://api.minimax.io/v1/api/openplatform/coding_plan/remains",
  "dashboard_url": "https://platform.minimax.io/console/plan",
  "label":         "Coding Plan",

  "shape": {
    "entries_path": "/model_remains",
    "windows": [
      { "id": "5h",     "field_prefix": "current_interval",
        "start_unit_ms": 1000, "reset_unit_ms": 1, "reset_is_absolute_epoch": false },
      { "id": "weekly", "field_prefix": "current_weekly",
        "start_field": "weekly_start_time", "reset_field": "weekly_remains_time",
        "start_unit_ms": 1000, "reset_unit_ms": 1, "reset_is_absolute_epoch": false }
    ],
    "error_envelope": {
      "code_path": "/base_resp/status_code",
      "message_path": "/base_resp/status_msg",
      "success_codes": [0]
    }
  },

  "ring_colors": {
    "normal":   "#3a9d4d",
    "warning":  "#f6d32d",
    "throttled": "#e01b24"
  },

  "auth": { "type": "bearer" },
  "user_agent": "minimax-quota-tray",

  "refresh_seconds": 120,
  "refresh_min_seconds": 15,
  "refresh_max_backoff_seconds": 600,

  "thresholds": { "yellow": 60, "red": 85 },
  "burn_warning": {
    "enabled": true,
    "min_history_ms": 600000,
    "lookback_ms": 3600000,
    "use_epoch_average": true
  }
}
```

- **`endpoint`** — full URL to GET. Per-instance.
- **`dashboard_url`** — opened by the **Open dashboard** menu item.
- **`label`** — shown in the chip and menu header.
- **`shape`** — describes the JSON response: where to find the
  entry, how many windows to extract, and what field-name prefixes
  / unit multipliers / error-envelope paths to use. See
  `src/provider.rs` for the full schema; the worked example in
  [Porting to another provider](#porting-to-another-provider)
  below shows how to define a new shape.
- **`ring_colors`** — the chip's three bucket-state colors. The
  center dot uses the same color as the ring, so the chip reads as
  "ring with center" — orange scheme for one tray, blue for another,
  green/yellow/red (default) for a third, etc.
- **`auth`** — how the API key is sent. One of:
  - `{ "type": "bearer" }` — `Authorization: Bearer <key>` (default)
  - `{ "type": "header", "name": "x-api-key" }` — `<header>: <key>`
  - `{ "type": "custom", "name": "Authorization", "format": "Token {key}" }` —
    `<header>: <format>` with `{key}` substituted
  - `{ "type": "query_param", "name": "key" }` — appends `?key=<key>` to the URL
- **`user_agent`** — User-Agent prefix (version auto-appended).
- **`thresholds`** — the chip's bucket transitions fire when **used**
  % exceeds these (yellow at 60% used, red at 85% used). The chip
  label always shows **remaining** %.
- **`burn_warning`** — burn-rate projection. After the tray has collected
  `min_history_ms` (default 10 min) of polling history for a window, it
  estimates the burn rate for that window and shows a row under its
  bar — informational (`on pace to have ~X% left at reset`) whenever
  there's enough data, switching to a `⚠` warning when the trend projects
  the window exhausting before it resets. Each window gets its own
  history so a quiet window does not pollute another's rate.
  The rate is the max of the recent slope over `lookback_ms` (default 1h)
  and the whole-epoch average; set `use_epoch_average: false` to react
  to short-term spikes only. `enabled: false` turns the feature off
  entirely. The projection needs history — it appears ~10 min after
  startup and resets on every window rollover (per window).
  **Pct-only suppression:** on providers whose count fields are 0/0
  (Coding Plan and any pct-only API), the integer-percent signal can't
  fit a meaningful slope at weekly scale — per-poll drops are ~0.01–0.05%
  on weekly usage, well below 1% precision. A pct-only window only gets
  a row when its rate is actually non-zero. Token-counting providers keep
  the row for all windows since their rate is computed from real count
  deltas.
- **Polling cadence** — `refresh_seconds` is the baseline (default 120s, peer-aligned).
  The actual interval is adaptive: when remaining quota drops below the
  yellow threshold, polls speed up to `refresh_seconds / 2`; below the
  red threshold, `refresh_seconds / 4`. After consecutive errors, the
  interval backs off exponentially up to `refresh_max_backoff_seconds`
  (default 600). Each poll also has 0-5s of jitter to spread load.

## Multiple instances

One binary, many tray icons. Each instance is its own tray with its
own config, colors, endpoint, keyring entry, and lock file — they
don't collide. Run them with `--instance=<name>` (or
`QUOTA_INSTANCE=<name>` env var).

```sh
# Default tray — `~/.config/quota-tray/`, keyring app `quota-tray`,
# lock at `$XDG_RUNTIME_DIR/quota-tray.pid`
minimax-quota-tray

# Concurrent instance for a different provider — `~/.config/quota-tray-codex/`,
# keyring app `quota-tray-codex`, lock at `$XDG_RUNTIME_DIR/quota-tray-codex.pid`.
# Each tray icon can have its own colors / endpoint / auth / etc.
minimax-quota-tray --instance=codex
```

For a persistent second tray (e.g. Codex running on every login),
install a second systemd unit. `minimax-quota.service` is the
template — copy it to `minimax-quota-codex.service` and add
`--instance=codex` to the `ExecStart=` line:

```sh
cp ~/.config/systemd/user/minimax-quota.service \
   ~/.config/systemd/user/minimax-quota-codex.service
# Edit the copy: change Description= and add ` --instance=codex`
# to the end of ExecStart=
systemctl --user daemon-reload
systemctl --user enable --now minimax-quota-codex.service
```

What gets namespaced per instance:

| | Default | `--instance=codex` |
|---|---|---|
| Config dir | `~/.config/quota-tray/` | `~/.config/quota-tray-codex/` |
| Config file | `…/config.json` | `…/config.json` |
| Lock file | `$XDG_RUNTIME_DIR/quota-tray.pid` | `$XDG_RUNTIME_DIR/quota-tray-codex.pid` |
| Keyring `application` | `quota-tray` | `quota-tray-codex` |
| Keyring `label` | `quota-tray API Key` | `quota-tray-codex API Key` |
| Legacy fallback key file | `~/.config/.config/quota-tray/key` | `~/.config/.config/quota-tray-codex/key` |
| Static SVGs | `minimax-quota-*.svg` (shared) | (shared — same names) |

The static SVGs in `${TMPDIR}/` are shared across instances because
they're per-color, not per-instance, and the colors come from each
instance's config. SNI hosts pick them up by `IconName`; the same
files work for both instances because each instance's chip is
distinguished by the live ARGB `IconPixmap` (which IS per-instance,
in the running tray process).

A second instance with the same name (no `--instance=`, or two
`--instance=foo` simultaneously) fails immediately with
`another instance is already running` — the lock file is
instance-scoped, so this only conflicts with itself.

## How it works

The Rust binary implements both the `org.kde.StatusNotifierItem`
interface (the chip properties + click handlers) and the
`com.canonical.dbusmenu` interface (the menu tree) directly over
D-Bus. The host panel reads SNI properties and walks the dbusmenu tree
to render a native menu.

```
   ┌──────────────────────────┐
   │   minimax-quota-tray      │   ~5.2 MB ELF, no GUI library;
   │                          │   links libc + libsecret + libdbus only
   └────────────┬─────────────┘
                │
   ┌────────────┴─────────────┐
   │  zbus + custom dbusmenu    │   StatusNotifierItem properties
   │  server (no proxy)        │   + com.canonical.dbusmenu tree
   └────────────┬─────────────┘
                │
   Host's SNI panel ←  reqwest →  endpoint from config.json
                ↓
   secret-service crate  →  libsecret (any provider)
```

The Rust port doesn't draw anything itself — the host's panel reads the
SNI properties and renders an icon, and walks the dbusmenu tree to
build a native menu (a KDE Plasma `KMenu`, a Waybar menu, etc.). This
is why the Rust binary is ~5.2 MB instead of the 25+ MB you'd get with
a linked-in GTK.

The keyring read/write goes through the `secret-service` crate
directly (blocking API on a `spawn_blocking` thread; libsecret's
D-Bus calls would otherwise deadlock the tokio reactor). The crate
also writes through `tokio::process::Command` if needed for
compatibility with libsecret versions that misbehave on
`create_item`'s argument negotiation.

## Troubleshooting

- **Right-click on the icon does nothing** — your panel doesn't speak
  SNI, or it's SNI-aware but the AppIndicator bridge is missing. On
  GNOME you need the AppIndicator extension; on KDE Plasma it works
  natively; on swaybar/Waybar it works natively too.

- **Icon shows three dots** — the Rust binary writes ring SVGs to
  `$TMPDIR` (default `/tmp`) at startup. The SVG file path is sent as
  SNI `IconName`, and the host loads it via `GdkPixbuf` (GNOME
  AppIndicator) or `QtSvg` (KDE). On hosts without an SVG loader
  registered in gdk-pixbuf's `loaders.cache` (notably Fedora Atomic /
  Bluefin / Silverblue, where the `gdk-pixbuf2` RPM ships with an
  empty loader directory), the file load fails and the host falls
  through to the in-memory ARGB bytes sent via SNI `IconPixmap` —
  so the icon should still render. If it doesn't, check that
  `${TMPDIR}/minimax-quota-*.svg` exists, is readable, and the directory
  isn't read-only. The icon-pixmap fallback renders regardless of
  whether the SVG file load succeeded, so a missing SVG file will
  never produce a blank panel on its own — if you see the three
  dots, the `IconPixmap` path is what's broken, not the file path.

- **`Argument password may not be null`** — your keyring daemon is
  locked. Unlock it or re-enter the key from the menu (click chip →
  **Set API Key…**).

- **`secret_service_create_item_dbus_path: assertion ... collection_path != NULL`**
  — no libsecret daemon is reachable. Check that gnome-keyring-daemon
  (`--components=secrets`), kwalletd, or KeePassXC's secret-service
  bridge is running.

- **`MINIMAX_API_KEY` env var works but keyring doesn't** — your
  panel session doesn't have the libsecret `$DBUS_SESSION_BUS_ADDRESS`
  reachable, so the binary falls through to the env var. This is
  fine — just keep the env var set in your shell rc file or systemd
  unit's `Environment=` line.

## Porting to another provider

The tray infrastructure (AppIndicator, keyring, adaptive polling,
stale-on-error fallback, offline detection) is provider-agnostic.
The HTTP/JSON surface is fully config-driven — **no provider
constants live in source**. To port this tray at a different API,
edit one config file. `src/provider.rs` holds only the type
definitions (`AuthConfig`, `PlanShape`, `WindowShape`, `RingColors`)
and neutral compile-time defaults (the classic green/yellow/red
ring colors and a single generic window shape) — none of it
names a specific provider.

### What's in `config.json`

Every field that varies between providers lives in
`~/.config/quota-tray/config.json` (or
`~/.config/quota-tray-<instance>/config.json` for a named
instance):

| Field | What it controls |
|---|---|
| `endpoint` | Full URL to GET |
| `dashboard_url` | Opened by the **Open dashboard** menu item |
| `label` | Shown in the chip and menu header |
| `shape` | JSON response structure: entry path, windows, error envelope |
| `ring_colors` | Hex colors for normal / warning / throttled |
| `auth` | How the API key is sent (Bearer / header / custom / query param) |
| `user_agent` | User-Agent prefix (version auto-appended) |
| `thresholds`, `refresh_*`, `burn_warning` | UI cadence + bucket thresholds |

The `shape` field is the only one that requires understanding the
provider's JSON layout — see [Configuration](#configuration) for
the full schema, and the worked example below for a full re-target.

### What the UI consumes

The `Window` struct (in `src/burn.rs`) is the abstract shape every
provider maps into:

```rust
pub struct Window {
    pub id: String,           // unique within the windows vec, used as
                              // the menu row label and burn-rate history key
    pub total: i64,
    pub used: i64,
    pub remaining_pct: i64,   // 0..100; drives chip + bar
    pub reset_at: i64,        // absolute ms; drives "resets in X"
    pub start_at: i64,        // optional; epoch start for burn projection
}
```

Rules for the provider→Window mapping (encoded by `WindowShape`
fields in `config.json`'s `shape`):

- **N windows.** There's no fixed limit — `shape.windows` is a
  `Vec<WindowShape>` of arbitrary length. The tray renders one
  menu row per window. MiniMax ships with 2 (`5h`, `weekly`);
  a 3-window provider (e.g. minute/hour/day) just adds another
  `WindowShape` entry to its config.
- **Window length is derived dynamically** — `burn::compute_burn`
  reads `start_at` and `reset_at` from each `Window` and computes
  the window length on the fly. No constant "5h" or "7d" is baked
  in; the `id` is just a UI label.
- **`id` is the window's UI label and history key.** The first
  window drives the chip percentage; convention is to put the
  rolling short-interval window first. Pick something stable —
  changing the id loses accumulated burn-rate history.
- **`remaining_pct` is the source of truth.** If your provider
  returns `used`/`total`, the parser computes
  `100 - (100 * used / total)`.
- **`reset_at` is an absolute ms-since-epoch.** If your provider
  gives a duration ("resets in 3h 20m"), set `reset_unit_ms: 1`
  and leave `reset_is_absolute_epoch: false` — the parser computes
  `reset_at = now_ms + raw_reset`. If your provider returns an
  absolute epoch instead, set `reset_is_absolute_epoch: true` and
  the parser uses the value directly.
- **`start_at` is optional** (only used by the burn-rate
  projection). Omit it (`0`) if your provider doesn't return an
  epoch start; the projection then uses the recent slope alone.

### Worked example: porting to a hypothetical `/v1/usage` endpoint

Suppose Provider X exposes:

```text
GET https://api.provider.com/v1/usage
x-api-key: <key>
→ { "daily":   { "limit": 1000,  "used": 120,  "reset_in_ms": 7200000 },
    "monthly": { "limit": 30000, "used": 4500, "reset_in_ms": 2592000000 } }
```

The whole port is one config file at
`~/.config/quota-tray-provider-x/config.json`:

```json
{
  "endpoint":      "https://api.provider.com/v1/usage",
  "dashboard_url": "https://provider.com/dashboard",
  "label":         "Provider X",

  "shape": {
    "entries_path": "/",
    "windows": [
      {
        "id": "daily",
        "field_prefix": "daily",
        "reset_field": "reset_in_ms",
        "reset_unit_ms": 1,
        "reset_is_absolute_epoch": false
      },
      {
        "id": "monthly",
        "field_prefix": "monthly",
        "reset_field": "reset_in_ms",
        "reset_unit_ms": 1,
        "reset_is_absolute_epoch": false
      }
    ]
  },

  "ring_colors": {
    "normal":   "#3366ff",
    "warning":  "#9933ff",
    "throttled": "#cc00ff"
  },

  "auth": { "type": "header", "name": "x-api-key" },
  "user_agent": "quota-tray-provider-x"
}
```

Launch it: `minimax-quota-tray --instance=provider-x`. The tray icon
shows the blue/purple/pink palette, hits `api.provider.com`, sends
`x-api-key: <key>`, renders two menu rows (`daily: 88% left`,
`monthly: 85% left`), and refreshes on the configured cadence. No
code change needed.

### Forking vs multi-instance

For "I want a tray for a different API", don't fork — run a second
instance. Forks are only worth it if you want to add a new auth
scheme (`AuthConfig::Bearer`/`Header`/`Custom`/`QueryParam` already
covers the common cases) or if you want a different binary name.

If you do fork, three names are worth updating so the fork can
coexist with the original:

| What                          | Where                                             |
| ----------------------------- | ------------------------------------------------- |
| Script filename               | `minimax-quota-tray` → e.g. `openai-quota-tray` |
| systemd unit                  | `minimax-quota.service`                           |
| Service `Description=`         | text shown in `systemctl --user status`         |
| Cargo crate name              | `Cargo.toml` `[package].name`                     |

The config dir, lock file, and keyring `application` attribute are
all derived from the instance name (default: `quota-tray`); forking
the binary doesn't change those.


## License

MIT © David Lafreniere
