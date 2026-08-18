# MiniMax Quota Tray

A standalone GNOME Shell tray indicator for MiniMax API quota. Supports
both the **Coding Plan** and the **Token Plan** via your own API key.
Talks directly to the MiniMax API — no agent or plugin system required.

![Menu preview](menu.txt)

## What it shows

- **Chip** in the top bar: `<Plan> <remaining>%` with an icon that flips
  to a warning glyph when usage crosses the configured thresholds.
- **Menu**:
  ```
  Plan: Coding Plan
    5h: 80% left · resets in 4h
    ██████████████████░░░░
    · on pace to have ~48% left at reset (40 tok/h)
    weekly: 100% left · resets in 6d 8h
    ███████████████████████
    · on pace to have ~96% left at reset (40 tok/h)
    ───
    Refresh now
    Open dashboard
    Set API Key…
    ───
    Quit
  ```
- **Burn-rate rows** — once the tray has ~10 minutes of polling history
  for a window, it estimates the token burn rate using that window's own
  sample history, and a row under the window's bar always shows the
  projection. The 5h and weekly windows each get their own row, computed
  independently — a quiet 5h window doesn't pollute the weekly rate, and
  vice versa.
  - healthy (token-based plan): `· on pace to have ~48% left at reset (40 tok/h)`
  - healthy (Coding Plan, pct-based): `· on pace to have ~62% left at reset (15%/h)`
    *(Coding Plan's API returns 0 for `current_interval_total_count` and
    `current_interval_usage_count`; consumption is tracked via
    `current_interval_remaining_percent` only, so the rate is %/h)*
  - idle (no recent token usage): `· on pace to have ~98% left at reset (0 tok/h)`
  - warning (projected exhaustion before reset):
    `⚠ 1.2k tok/h → exhausts ~1h 5m before reset` (or `⚠ 60%/h → exhausts ~22m before reset`)
    — and the top-bar chip flips to the warning color based on the 5h window
    alone, even when the remaining-% thresholds look fine. The weekly row
    can warn independently of the chip.

  The projection resets on every window rollover; it's an estimate, not a
  promise — bursty usage will make it conservative or optimistic accordingly.

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
and T19 verifies the 5h rollover does not clear the weekly history.

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
  its own row, computed from its own history — a quiet 5h window does not
  pollute the weekly rate. The 5h window's warning also flips the chip
  to yellow. The rate is the max of the recent slope over `lookback_ms`
  (default 1h) and the whole-epoch average; set `use_epoch_average: false`
  to react to short-term spikes only. `enabled: false` turns the feature
  off entirely. Note the projection needs history — it appears ~10 min
  after startup and resets on every window rollover (per window).
- **Polling cadence** — `refresh_seconds` is the baseline (default 120s, peer-aligned).
  The actual interval is adaptive: when remaining quota drops below the
  yellow threshold, polls speed up to `refresh_seconds / 2`; below the
  red threshold, `refresh_seconds / 4`. After consecutive errors, the
  interval backs off exponentially up to `refresh_max_backoff_seconds`
  (default 600). Each poll also has 0-5s of jitter to spread load.

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

- **Three dots where the icon should be** — the static status icons
  (`icons/quota-{normal,warning,throttled,offline,error}.svg`) aren't being
  resolved by the icon theme. The installer copies them into
  `~/.local/share/icons/hicolor/scalable/apps/` and refreshes the icon
  cache; if that step was skipped, the icons won't appear. Inspect with
  `gjs -c "imports.gi.versions.GTK='3.0'; const t=Gtk.IconTheme.get_default(); print(t.lookup_icon('quota-normal', 16, 0)?.get_filename() ?? 'NOT FOUND');"`
- **`Argument password may not be null`** — your GNOME Keyring is locked.
  Unlock it or re-enter the key from the menu (click chip → **Set API Key…**).
- **`Requiring Gtk, version 3.0: ... '4.0' is already loaded`** — make
  sure nothing in your pipeline imports `gi://Gdk` without
  `?version=3.0`. Drop the bare `Gdk` import.
- **`secret_service_create_item_dbus_path: assertion ... collection_path != NULL`**
  — the GNOME Keyring daemon isn't reachable. Check that
  `gnome-keyring-daemon --components=secrets` is running.

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
`minimax-quota-tray.js`. Everything else (tray UI, keyring, scheduler,
network monitor) stays as-is.

#### 1. Auth header — `fetchQuota()` (around line 172)

```js
message.request_headers.append('Authorization', `Bearer ${apiKey}`);
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

#### 2. Response parser — `parsePayload()` and `parseWindow()` (lines 199–224)

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
| Script filename               | `minimax-quota-tray.js` → e.g. `openai-quota.js`  |
| systemd unit                  | `minimax-quota.service`                           |
| Config dir                    | `~/.config/minimax-quota/`                        |
| Keyring schema name           | `org.dlafreniere.minimax-quota`                   |
| Keyring `application` attr    | `minimax-quota`                                   |
| Keyring item label            | `MiniMax API Key`                                 |

Change them in: the script's `KEY_SCHEMA`, `KEY_ATTRIBUTES`, `KEY_LABEL`,
`CONFIG_DIR`/`CONFIG_PATH`; the `ExecStart=` line in the `.service` unit;
and `install.sh` / `uninstall.sh` (the paths and `secret-tool` argv). Pick
a single provider-specific token (e.g. `openai-quota`) and use it
consistently across all six — that's what keeps multiple forks from
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

`fetchQuota()`: already uses Bearer — no change if Provider X is the same.

`parsePayload()` rewrite (drop-in replacement for lines 219–224):

```js
function parsePayload(payload) {
  if (!payload.daily) throw new Error('Provider X returned no daily window');
  const makeWindow = (key, label) => {
    const w = payload[key];
    return {
      // `label` is what shows in the menu ("daily: 80% left · resets in 18h").
      // `id` is optional and only used by the parser for its own bookkeeping;
      // the UI never looks at it.
      label,
      total: Number(w.limit) || 0,
      used:  Number(w.used)  || 0,
      remaining_pct: Math.max(0, Math.min(100, 100 * (1 - w.used / w.limit))),
      resetAt: Date.now() + Number(w.reset_in_ms || 0),
      throttled: false,
    };
  };
  // First window = primary (drives the top-bar chip). Put the more urgent
  // window first if you want the chip to reflect it.
  return [makeWindow('daily',   'daily'),
          makeWindow('monthly', 'monthly')];
}
```

That's it — no other code in the project needs to move.

## License

MIT © David Lafreniere
