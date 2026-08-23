# LLM Quota Tray

A provider-agnostic freedesktop tray indicator for LLM API quota. One
binary, many APIs — every provider-specific bit (endpoint, JSON shape,
auth style, ring colors) lives in `~/.config/llm-quota-tray/config.json`.
The shipped example config points at the MiniMax API (Coding Plan +
Token Plan), but the tray is fully generic; a fork or a second
instance can target any other LLM API.

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
> modules, 150+ unit tests, ~5.5 MB release binary, ~10 MB RSS.) No GUI
> library at all — talks to D-Bus directly and renders the icon as ARGB
> bytes via `tiny-skia` (or a raw BGRA circle routine for the static
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
    providers keep the row for all windows since their rate is computed from
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
- An LLM-provider API key (stored in the keyring under the `llm-quota-tray`
  application, in `~/.config/.config/llm-quota-tray/key`, or in the
  `LLM_API_KEY` env var)

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
default config to `~/.config/llm-quota-tray/config.json` (skipped if it
already exists). After it finishes, click the chip in your panel →
**Set API Key…** to store your key in your libsecret provider
(GNOME Keyring, KWallet, etc.).

If you're not in a graphical session, run the installer from a desktop
session or start the service manually after logging in:
```bash
systemctl --user enable --now llm-quota-tray.service
```

## Uninstall

```bash
./uninstall.sh
```

Stops and disables the service, removes the installed files, and
optionally purges your config dir and the stored key from your libsecret
provider.

## Tests

The Rust port carries **~212 unit tests across 19 modules**, plus two
ignored integration tests that need a session D-Bus.

```bash
cargo test                                 # unit tests (no D-Bus needed)
cargo test -- --ignored                    # + integration tests (D-Bus smoke + RSS guard)
cargo test config::tests::provider_templates_deserialize  # schema-drift guard for examples/providers/*.json
```

What the suite covers:

- **Single-flight invariant** — the orchestrator's `tokio::select!`
  arms at most one poll timeout; an explicit Refresh arriving mid-fetch
  is queued exactly once (no stacked polling chains, no request bursts).
- **Offline handling** — `NetEvent::Connectivity(false)` skips polling;
  `Connectivity(true)` force-refreshes and clears backoff.
- **No-key state** — menu shows `"No API key configured"` and re-arms
  the normal cadence (no exponential backoff while waiting for the user).
- **Backoff after errors** — `scheduler::next_interval` doubles up to
  `max_backoff_seconds`, resets on success or menu Refresh.
- **Threshold notification dedup** — `_last_bucket` is upward-only;
  rank decrease does not fire a notification.
- **Burn-rate projection** — `slope_per_hour` over `lookback_ms`,
  `max`'d with the epoch-average floor when `use_epoch_average: true`.
- **Window-specific history** — the weekly rate is computed from weekly
  samples (a quiet 5h doesn't pollute it); the 5h rollover does not
  clear the weekly history.
- **Pct-only suppression** — pct-only windows with no signal suppress
  the row (skip the weekly row on Coding Plan, skip idle 5h on any
  pct-only API); token-counting providers keep the row for all windows.
- **Lock takeover** — stale PID files (whose owner `/proc/<pid>` is
  gone) are taken over cleanly; live holders are refused.
- **Keyring trimming** — `secret-tool store` via a shell pipe can
  persist trailing newlines; `secret_to_key` strips them so reqwest
  doesn't reject the `Authorization` header.
- **RSS regression guard** — `tests/integration.rs::rss_under_target`
  fails if the binary exceeds 20 MB resident (catches accidental
  re-introduction of GTK/libappindicator).

The gjs-era test fixtures (`tests/run.sh`,
`tests/regression-scheduler.test.js`) were retired with the gjs
implementation. Their coverage maps to the Rust unit tests in
`src/burn.rs::tests`, `src/main.rs::tests`, and `src/scheduler.rs::tests`;
see [`docs/development.md`](docs/development.md) for the per-test
locations.

For the build profiles, debug tooling (logs, D-Bus introspection,
backtraces), and how to add new tests / provider templates, see
[`docs/development.md`](docs/development.md). For the architecture map
and call graph, see [`docs/architecture.md`](docs/architecture.md). For
the full config schema, see [`docs/config-schema.md`](docs/config-schema.md).
For the per-module reference and what each Rust file owns, see
[`docs/modules.md`](docs/modules.md). For CLI flags and env vars, see
[`docs/cli.md`](docs/cli.md). For the burn-rate math deep-dive, see
[`docs/burn-rate.md`](docs/burn-rate.md). For the full troubleshooting
walkthrough, see [`docs/troubleshooting.md`](docs/troubleshooting.md).

## Configuration

`~/.config/llm-quota-tray/config.json` (one instance = one config = one
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
    "inner": {
      "normal":   "#3a9d4d",
      "warning":  "#f6d32d",
      "throttled": "#e01b24"
    },
    "outer": "#3584e4"
  },

  "auth": { "type": "bearer" },
  "user_agent": "llm-quota-tray",

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
- **`ring_colors`** — the chip's two color channels, each
  configurable independently:
  - **`inner`** colors the center dot (and the solid static-state
    icons) — three bucket states: `normal` / `warning` /
    `throttled`. This is the **status** channel; it flips through
    green/yellow/red as the tray's state changes.
  - **`outer`** colors the outer ring (track + progress arc) — a
    single hex color. This is the **remaining-quota** channel; the
    *length* of the arc encodes the percentage fill, the hue just
    has to be visually distinct from the inner dot. Default is a
    neutral blue accent (`#3584e4`) so the ring reads as a
    "progress meter" regardless of which bucket the inner dot is
    in. Examples: orange scheme for one tray (`inner: {normal:
    "#ff9900", warning: "#ff5500", throttled: "#ff0000"}`, any
    `outer`), blue for another, green/yellow/red (default) for a
    third.
  - Legacy configs that put `normal`/`warning`/`throttled` directly
    under `ring_colors` (pre-split) still parse — those fields
    become `inner` and `outer` defaults to the neutral accent.
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
# Default tray — `~/.config/llm-quota-tray/`, keyring app `llm-quota-tray`,
# lock at `$XDG_RUNTIME_DIR/llm-quota-tray.pid`
llm-quota-tray

# Concurrent instance for a different provider — `~/.config/llm-quota-tray-codex/`,
# keyring app `llm-quota-tray-codex`, lock at `$XDG_RUNTIME_DIR/llm-quota-tray-codex.pid`.
# Each tray icon can have its own colors / endpoint / auth / etc.
llm-quota-tray --instance=codex
```

For a persistent second tray (e.g. Codex running on every login),
install a second systemd unit. `llm-quota-tray.service` is the
template — copy it to `llm-quota-tray-codex.service` and add
`--instance=codex` to the `ExecStart=` line:

```sh
cp ~/.config/systemd/user/llm-quota-tray.service \
   ~/.config/systemd/user/llm-quota-tray-codex.service
# Edit the copy: change Description= and add ` --instance=codex`
# to the end of ExecStart=
systemctl --user daemon-reload
systemctl --user enable --now llm-quota-tray-codex.service
```

What gets namespaced per instance:

| | Default | `--instance=codex` |
|---|---|---|
| Config dir | `~/.config/llm-quota-tray/` | `~/.config/llm-quota-tray-codex/` |
| Config file | `…/config.json` | `…/config.json` |
| Lock file | `$XDG_RUNTIME_DIR/llm-quota-tray.pid` | `$XDG_RUNTIME_DIR/llm-quota-tray-codex.pid` |
| Keyring `application` | `llm-quota-tray` | `llm-quota-tray-codex` |
| Keyring `label` | `llm-quota-tray API Key` | `llm-quota-tray-codex API Key` |
| Legacy fallback key file | `~/.config/.config/llm-quota-tray/key` | `~/.config/.config/llm-quota-tray-codex/key` |
| Static SVGs | `llm-quota-tray-*.svg` (shared) | (shared — same names) |

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
   │   llm-quota-tray         │   ~5.2 MB ELF, no GUI library;
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
  `${TMPDIR}/llm-quota-tray-*.svg` exists, is readable, and the directory
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

- **`LLM_API_KEY` env var works but keyring doesn't** — your
  panel session doesn't have the libsecret `$DBUS_SESSION_BUS_ADDRESS`
  reachable, so the binary falls through to the env var. This is
  fine — just keep the env var set in your shell rc file or systemd
  unit's `Environment=` line.

For more troubleshooting + the full diagnostic checklist when the chip
stays red after a port, see [`docs/port-guide.md`](docs/port-guide.md#diagnostic-checklist).
For a comprehensive walkthrough (keyring, SNI, panel hosts, RSS, etc.),
see [`docs/troubleshooting.md`](docs/troubleshooting.md).

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
`~/.config/llm-quota-tray/config.json` (or
`~/.config/llm-quota-tray-<instance>/config.json` for a named
instance):

| Field | What it controls |
|---|---|
| `endpoint` | Full URL to GET |
| `dashboard_url` | Opened by the **Open dashboard** menu item |
| `label` | Shown in the chip and menu header |
| `shape` | JSON response structure: entry path, windows, error envelope |
| `ring_colors` | Two color channels: `inner: {normal, warning, throttled}` (status dot) + `outer` (percentage-fill ring) |
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
  menu row per window. The LLM provider ships with 2 (`5h`, `weekly`);
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
    "monthly": { "limit": 30000, "used": 4500,  "reset_in_ms": 2592000000 } }
```

The whole port is one config file at
`~/.config/llm-quota-tray-provider-x/config.json`:

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
    "inner": {
      "normal":   "#3366ff",
      "warning":  "#9933ff",
      "throttled": "#cc00ff"
    },
    "outer": "#ff66cc"
  },

  "auth": { "type": "header", "name": "x-api-key" },
  "user_agent": "llm-quota-tray-provider-x"
}
```

Launch it: `llm-quota-tray --instance=provider-x`. The tray icon
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
| Script filename               | `llm-quota-tray` → e.g. `openai-quota-tray` |
| systemd unit                  | `llm-quota-tray.service`                           |
| Service `Description=`         | text shown in `systemctl --user status`         |
| Cargo crate name              | `Cargo.toml` `[package].name`                     |

The config dir, lock file, and keyring `application` attribute are
all derived from the instance name (default: `llm-quota-tray`); forking
the binary doesn't change those.

### When the simple port isn't enough

The example above covers the *native-shape* case — your provider
returns `{prefix}_remaining_percent` directly. Most Western providers
(OpenAI, Anthropic, Google, Mistral, Groq, Cohere) don't, and need a
sidecar. For the full port guide — Simple vs. Hard tracks, worked
examples for the four hard cases (headers-only / consumption-only /
multi-header auth / OAuth), and the diagnostic checklist — see
[`docs/port-guide.md`](docs/port-guide.md).

## License

MIT © David Lafreniere