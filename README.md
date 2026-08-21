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
- A MiniMax API key (stored in the keyring, in `~/.config/minimax-quota/key`,
  or in `MINIMAX_API_KEY` env var)

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
default config to `~/.config/minimax-quota/config.json` (skipped if it
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

`~/.config/minimax-quota/config.json`:

```json
{
  "plan": "coding_plan",
  "refresh_seconds": 120,
  "refresh_min_seconds": 15,
  "refresh_max_backoff_seconds": 600,
  "plans": {
    "coding_plan": {
      "endpoint":      "https://api.minimax.io/v1/api/openplatform/coding_plan/remains",
      "dashboard_url": "https://platform.minimax.io/console/plan",
      "label": "Coding Plan"
    },
    "token_plan": {
      "endpoint":      "https://api.minimax.io/v1/token_plan/remains",
      "dashboard_url": "https://platform.minimax.io/console/plan",
      "label": "Token Plan"
    }
  },
  "thresholds": { "yellow": 60, "red": 85 },
  "burn_warning": {
    "enabled": true,
    "min_history_ms": 600000,
    "lookback_ms": 3600000,
    "use_epoch_average": true
  }
}
```

- `plan` — `"coding_plan"` or `"token_plan"`. Switch by editing + restarting.
- `plans.<id>.endpoint` — override to point at a proxy.
- `thresholds` — the warning icon swap fires when **used** % exceeds these
  (i.e., yellow at 60% used, red at 85% used). The chip label always shows
  **remaining** %.
- `burn_warning` — burn-rate projection. After the tray has collected
  `min_history_ms` (default 10 min) of polling history for a window, it
  estimates the token burn rate for that window and shows a row under its
  bar — informational (`on pace to have ~X% left at reset`) whenever
  there's enough data, switching to a `⚠` warning when the trend projects
  the window exhausting before it resets. Each window (5h, weekly) gets
  its own history so a quiet 5h window does not pollute the weekly rate.
  The 5h window's warning also flips the chip to yellow. The rate is the
  max of the recent slope over `lookback_ms` (default 1h) and the
  whole-epoch average; set `use_epoch_average: false` to react to
  short-term spikes only. `enabled: false` turns the feature off entirely.
  Note the projection needs history — it appears ~10 min after startup
  and resets on every window rollover (per window).
  **Pct-only suppression:** on providers whose count fields are 0/0
  (Coding Plan and any pct-only API), the integer-percent signal can't
  fit a meaningful slope at weekly scale — per-poll drops are ~0.01–0.05%
  on weekly usage, well below 1% precision. A pct-only window only gets
  a row when its rate is actually non-zero. The 5h window keeps the row
  whenever there's been a recent tick; the weekly row stays hidden on
  Coding Plan. Token-counting providers keep the row for all windows
  since their rate is computed from real count deltas.
- **Polling cadence** — `refresh_seconds` is the baseline (default 120s, peer-aligned).
  The actual interval is adaptive: when remaining quota drops below the
  yellow threshold, polls speed up to `refresh_seconds / 2`; below the
  red threshold, `refresh_seconds / 4`. After consecutive errors, the
  interval backs off exponentially up to `refresh_max_backoff_seconds`
  (default 600). Each poll also has 0-5s of jitter to spread load.

## How it works

The Rust binary implements both the `org.kde.StatusNotifierItem`
interface (the chip properties + click handlers) and the
`com.canonical.dbusmenu` interface (the menu tree) directly over
D-Bus. The host panel reads SNI properties and walks the dbusmenu tree
to render a native menu.

```
   ┌──────────────────────────┐
   │   minimax-quota-tray      │   ~5.5 MB ELF, no GUI library;
   │                          │   links libc + libsecret + libdbus only
   └────────────┬─────────────┘
                │
   ┌────────────┴─────────────┐
   │  zbus + custom dbusmenu    │   StatusNotifierItem properties
   │  server (no proxy)        │   + com.canonical.dbusmenu tree
   └────────────┬─────────────┘
                │
   Host's SNI panel ←  reqwest →  api.minimax.io/v1/{coding_plan|token_plan}/remains
                ↓
   secret-service crate  →  libsecret (any provider)
```

```
   ┌──────────────────────────┐
   │   minimax-quota-tray      │   ~5.5 MB ELF, no GUI library;
   │                          │   links libc + libsecret + libdbus only
   └────────────┬─────────────┘
                │
   ┌────────────┴─────────────┐
   │  zbus + custom dbusmenu    │   StatusNotifierItem properties
   │  server (no proxy)        │   + com.canonical.dbusmenu tree
   └────────────┬─────────────┘
                │
   Host's SNI panel ←  reqwest →  api.minimax.io/v1/{coding_plan|token_plan}/remains
                ↓
   secret-service crate  →  libsecret (any provider)
```

The Rust port doesn't draw anything itself — the host's panel reads the
SNI properties and renders an icon, and walks the dbusmenu tree to
build a native menu (a KDE Plasma `KMenu`, a Waybar menu, etc.). This
is why the Rust binary is ~5.5 MB instead of the 25+ MB you'd get with
a linked-in GTK.

The keyring write goes through `secret-tool` (libsecret's CLI) via
`tokio::process::Command` with `Stdio::piped()` — direct argv, no
shell, no temp file. The `secret-service` rust crate's `create_item`
has unreliable arg semantics across libsecret versions, so writes are
shelled out; reads use the crate directly.

## Troubleshooting

- **Right-click on the icon does nothing** — your panel doesn't speak
  SNI, or it's SNI-aware but the AppIndicator bridge is missing. On
  GNOME you need the AppIndicator extension; on KDE Plasma it works
  natively; on swaybar/Waybar it works natively too.

- **Icon shows three dots** — the Rust binary writes ring PNGs to
  `$TMPDIR` (default `/tmp`) at startup. If `TMPDIR` is read-only or
  tmpfs is too small, the icons fall back to a static dot. Check
  `/tmp/minimax-quota-*.png` exists and is readable.

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

The tray infrastructure (AppIndicator, keyring, adaptive polling, stale-on-error
fallback, offline detection) is provider-agnostic. Only the HTTP/JSON surface
is MiniMax-specific. This section maps every provider-specific touchpoint so
you can fork this into a tray for any quota-aware API.

### What's already configurable (no code change)

You can repoint the tray at any HTTPS endpoint that returns JSON, as long as
the shape matches what the parser expects (see below). Everything else is
config-driven:

- `plans.<id>.endpoint` — full URL to GET
- `plans.<id>.dashboard_url` — opened by the **Open dashboard** menu item
- `plans.<id>.label` — shown in the chip and menu header
- `thresholds`, `refresh_seconds`, `refresh_min_seconds`,
  `refresh_max_backoff_seconds`

You can rename or add plan IDs freely in `config.json`; the `plan` field picks
the active one.

### What requires code changes

All provider-specific code lives in two short sections of
`src/fetch.rs` (HTTP shape) and `src/parse.rs` (JSON → window
mapping). Everything else (tray UI, keyring, scheduler, network
monitor) stays as-is.

#### 1. Auth header — `src/fetch.rs::fetch_windows_blocking()` (around line 33)

```rust
let resp = client
    .get(endpoint)
    .bearer_auth(api_key)
    .send()
    .context("HTTP request")?;
```

Common alternatives:

| Provider            | Header                              |
| ------------------- | ----------------------------------- |
| OpenAI / Anthropic  | `Authorization: Bearer <key>`       |
| Google Gemini       | `x-goog-api-key: <key>`             |
| Mistral             | `Authorization: Bearer <key>`       |
| Custom (header)     | `x-api-key: <key>`                  |
| Custom (query)      | append `?key=<key>` to the endpoint |

If you need more than one header per request, append more
`request_headers.append(...)` calls. If the provider uses cookies/session
auth, skip the keyring entirely and load the token from a file or env var in
`loadApiKey()`.

#### 2. Response parser — `src/parse.rs::parse_coding_plan()` and friends

This is the only piece tightly coupled to MiniMax's JSON shape. The MiniMax
`/remains` endpoint returns:

```json
{
  "model_remains": [
    {
      "model_name": "general",
      "current_interval_total_count": 500,
      "current_interval_usage_count": 25,
      "current_interval_remaining_percent": 95,
      "remains_time": 16320000,
      "current_interval_status": 1,
      "current_weekly_total_count": 5000,
      "current_weekly_usage_count": 0,
      "current_weekly_remaining_percent": 100,
      "weekly_remains_time": 561600000,
      "current_weekly_status": 1
    }
  ]
}
```

`parseWindow()` reads the `current_interval_*` / `current_weekly_*` fields and
produces the array of "windows" the UI consumes:

```js
{
  id: '5h',                // unique within the windows array; used to look up by .id
  label: '5h',             // (currently unused by the UI; keep it descriptive)
  total: 500,              // for sanity / future display
  used: 25,
  remaining_pct: 95,       // 0..100; drives chip + bar
  resetAt: <ms epoch>,     // absolute time; drives "resets in X" countdown
  startAt: <ms epoch>,     // optional; epoch start, drives the burn projection
  throttled: false,        // optional; flips the ⚠ Throttled menu line.
                       // Derived from remaining_pct (window exhausted), NOT from a status field.
}
```

`startAt` is only needed for the burn-rate projection (it floors the rate
with the whole-epoch average and detects rollover). Omit it (0) if your
provider doesn't return an epoch start; the projection then uses the recent
slope alone and the used-drop rollover check.

To port, rewrite these two functions to map your provider's payload into the
same window shape. Rules:

- **Always return 1–2 windows** (the UI is laid out for a short-window +
  long-window pair). To run with a single window, return an array with one
  entry; the menu and burn-rate row render automatically for each window
  the parser returns. The chip's primary window is matched by `id === '5h'`
  in `setChip()`.
- **`id` must be `'5h'` for the chip** — `setChip()` looks up
  `windows.find((w) => w.id === '5h')` to pick the percentage shown in the
  top-bar label. Rename consistently in `parseWindow()` and `setChip()`.
- **`remaining_pct` is the source of truth.** If your provider returns
  `used`/`total` instead, compute `100 * (1 - used/total)` here.
- **`resetAt` is an absolute ms-since-epoch.** If your provider gives a
  duration ("resets in 3h 20m"), do `Date.now() + durationMs` here.
- **`throttled` is optional** — derived from `remaining_pct <= 0` (window
  exhausted). The tray deliberately ignores any `*_status` field from the
  provider response: the official MiniMax-AI/cli documents that enum as
  `1=normal / 2=exhausted / 3=unlimited`, so reading `status===1` would
  falsely flag every healthy window as throttled. If your provider returns a
  similar status enum, ignore it and rely on `remaining_pct`.

The UI has **no hardcoded window labels or IDs** — `updateMenu()` reads each
window's `label` field and the chip uses `windows[0]` (the first window in
the array). Return any number of windows (1, 2, 3…) in any order; the menu
will show one label+bar row per window. Put the window you want shown in the
top-bar chip first (the convention here is the rolling short-interval
window, since that's the one most likely to need urgent attention).

#### 3. Multiple plans in one payload — `parsePayload()`

If your provider returns several "plans" or "tiers" in one response (as
MiniMax does with `model_remains` keyed by `model_name`), pick the entry
yourself:

```js
const entry = entries.find((e) => e.model_name === 'general') || entries[0];
```

Replace `'general'` with whatever tag identifies the bucket you want to
display. To support choosing between buckets at runtime, expose them as
separate `plans.<id>` entries in `config.json` (different endpoints) — the
existing `plan` selector already does this.

### Optional: rename the binary + service + keyring entry

If you fork this for a different provider, three names are worth updating so
the two can coexist on one machine (the keyring schema, in particular, is
global per user):

| What                          | Where                                             |
| ----------------------------- | ------------------------------------------------- |
| Script filename               | `minimax-quota-tray` → e.g. `openai-quota-tray`     |
| systemd unit                  | `minimax-quota.service`                           |
| Config dir                    | `~/.config/minimax-quota/`                        |
| Keyring schema name           | `org.dlafreniere.minimax-quota`                   |
| Keyring `application` attr    | `minimax-quota`                                   |
| Keyring item label            | `MiniMax API Key`                                 |

Change them in: `src/keyring.rs` (`LABEL`, `attrs()`), the config
schema (`src/config.rs`), the `ExecStart=` line in `minimax-quota.service`,
and `install.sh` / `uninstall.sh` (the paths and `secret-tool` argv).
Pick a single provider-specific token (e.g. `openai-quota`) and use
it consistently across all six — that's what keeps multiple forks from
stomping on each other's keyring entries.

### Worked example: porting to a hypothetical `/v1/usage` endpoint

Suppose Provider X exposes:

```
GET https://api.provider.com/v1/usage
Authorization: Bearer <key>
→ { "daily": { "limit": 1000, "used": 120, "reset_in_ms": 7200000 },
    "monthly": { "limit": 30000, "used": 4500, "reset_in_ms": 2592000000 } }
```

`config.json`:

```json
{
  "plan": "primary",
  "plans": {
    "primary": {
      "endpoint": "https://api.provider.com/v1/usage",
      "dashboard_url": "https://provider.com/dashboard",
      "label": "Provider X"
    }
  }
}
```

`fetch_windows_blocking()`: already uses Bearer — no change if Provider X is the same.

`parse_coding_plan()` rewrite — drop-in for the parser. The shape
the UI consumes is the same; map your provider's payload into a
`Vec<Window>` and the rest of the code is unchanged. The fields
the UI needs:

```rust
pub struct Window {
    pub id: &'static str,    // unique within the windows vec
    pub total: i64,
    pub used: i64,
    pub remaining_pct: i64,  // 0..100; drives chip + bar
    pub reset_at: i64,       // absolute ms; drives "resets in X"
    pub start_at: i64,       // optional; epoch start for burn projection
}
```

Always return 1–2 windows. The UI is laid out for a short-window +
long-window pair. To run with a single window, return one entry —
the menu and burn-rate row render automatically for each window.

## License

MIT © David Lafreniere
