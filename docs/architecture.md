# Architecture

How the 6,875-line Rust codebase fits together. If you want to understand
or modify one piece, start here.

## Subsystem map

```
                       CLI flags / env (instance::init)
                                 │
                                 ▼
                         ┌──────────────┐
                         │  main.rs     │
                         │  ─────────   │
                         │  • tokio     │
                         │  • spawn     │
                         │    subsystem │
                         │    handles   │
                         └──────┬───────┘
                                │
       ┌──────────┬─────────────┼─────────────┬──────────────┐
       ▼          ▼             ▼             ▼              ▼
   ┌───────┐ ┌─────────┐  ┌──────────┐  ┌──────────┐  ┌─────────┐
   │ lock  │ │ config  │  │ network  │  │   keyring│  │  sni    │
   │ (PID) │ │ (load)  │  │ (NM watch│  │(secret-  │  │ (Tray + │
   │       │ │         │  │  State-  │  │ tool sub)│  │  dbus-  │
   │       │ │         │  │  Changed)│  │          │  │  menu)  │
   └───┬───┘ └────┬────┘  └─────┬────┘  └─────┬────┘  └────┬────┘
       │          │             │             │            │
       ▼          ▼             │             │            ▼
   $XDG_RUNTIME_DIR/<inst>.pid  │             │      SNI + dbusmenu
   O_EXCL create + /proc/<pid> │             │      over session D-Bus
   liveness take-over           │             │            │
                                │             │            │
              ┌─────────────────┘             │   mpsc<MenuCommand>
              │                               │            │
              ▼                               │            ▼
        NetEvent { Connectivity, ForceRefresh }     orchestrator()
              │                                       (tokio::select)
              │                                       │
              │                                       ▼
              │                              do_refresh()
              │                                       │
              │   ┌──────────┬──────────┬──────────────┼────────────┐
              │   ▼          ▼          ▼              ▼            ▼
              │ fetch   parse        burn::compute  build_menu_  icon::
              │ (HTTP   (data-driven, decide_burn_  state        render
              │  GET)   PlanShape    row                                 │
              │   │       →Window)       │                              │
              │   │          │            │                              │
              │   ▼          ▼            ▼                              ▼
              │ Vec<Window>         record_sample → histories[window.id]
              │   │                                            │
              │   └────► AppState.last_good                    ▼
              │                                        tray.update() →
              │                                        chip + dbusmenu
              │                                                       │
              └───────────────────────────────►  notify::send() ◄─────┘
                                                (on bucket-rank ↑)
```

## The request lifecycle (one poll cycle)

The orchestrator in `main.rs::orchestrator` (`main.rs`) selects on three
input streams — sleep timer, menu commands, network events — and collapses
all of them into the same operation: `do_refresh()`. That function is the
center of the universe; here is what it does.

### 1. Offline check (`main.rs`)

If `AppState.offline == true`, render the offline menu and skip the rest.
Returns `refresh_seconds * 1000` so the loop re-arms the normal cadence
without burning a "no data" chip.

### 2. Keyring read (`main.rs`)

`keyring::get()` is run on `tokio::task::spawn_blocking` because it shells
out to `secret-tool(1)` (synchronous subprocess). Priority:

1. `secret-tool lookup application <instance>` → Secret Service
2. Legacy plaintext file at `~/.config/.config/<instance>/key`
   (`keyring.rs`, `keyring.rs`)
3. `LLM_API_KEY` env var

Missing key → render `"No API key configured"` menu, return normal
cadence (don't back off — the user has to click "Set API Key…").

### 3. HTTP fetch (`main.rs`)

`fetch::fetch_windows_blocking` (in `fetch.rs`) is also `spawn_blocking`-
wrapped. It does:

- Apply `AuthConfig::apply_to_endpoint` for `QueryParam` style.
- Build `(name, value)` from `AuthConfig::build` for header style.
- `client.get(endpoint).header(...)` (or `Authorization: Bearer ***`).
- Truncate response to ~64KB, run it through `fetch::sanitize_error_snippet`
  to redact any `sk-…` pattern before logging.
- Parse JSON into `serde_json::Value`.

### 4. Parser (`parse.rs`)

`parse::parse_plan(value, &PlanShape, now_ms) -> Result<Vec<Window>>`
walks the JSON using the data-driven rules in the per-instance
`PlanShape`:

- `entries_path` (`"/model_remains"` for MiniMax) selects which array to
  read.
- For each `WindowShape`, read `{field_prefix}_total_count`,
  `_usage_count`, `_remaining_percent` plus start/reset fields with
  unit multipliers. `reset_is_absolute_epoch` toggles duration vs.
  epoch semantics (`parse.rs`).
- Errors: if `error_envelope` is configured and the `code_path` value
  isn't in `success_codes`, the parser returns a clean `Err` rather than
  silently treating zero values as the new normal.

### 5. State update + sample recording (`main.rs`)

On success:
- `last_good = Some(windows)`, `last_good_at = now_ms()`, `fail_streak = 0`.
- For each window, `record_sample(&mut histories[window.id], &w)` appends a
  `Sample` and evicts the oldest if `histories[id].len() > BURN_MAX_SAMPLES`
  (480 samples ≈ 16h at 120s baseline; `main.rs`).

### 6. Burn projection (`burn::compute_burn`, `main.rs`)

For each window: `decide_burn_row(Some(window), &histories[id], now_ms(),
&cfg.burn_warning)` returns `Option<BurnResult>`. The math:

- Token rate: least-squares slope of `used` over the `lookback_ms` (1h)
  window of recent samples, **max'd with** the whole-epoch average when
  `use_epoch_average` is on (the gjs floor: catches "user is on a quiet
  week but the epoch average is still >0 because of last week's burst").
- Pct rate: same math on `remaining_pct`, negated.
- Mode is `'token'` when `window.total > 0` and `token_rate.is_some()`,
  `'pct'` when `pct_rate.is_some()`, else `'idle'`.
- **Pct-only suppression** (`main.rs:compute_burn` and
  `burn.rs:compute_burn`): idle rows on pct-only windows (Coding Plan
  with 0/0 total/usage) keep `unit = 'pct'` but the menu renders them
  only when `rate > 0` — see `decide_burn_row` (`burn.rs`).

### 7. Menu build (`build_menu_state` + `menu.rs`)

`build_menu_state(&cfg.label, &[(Window, Option<&BurnResult>)], …)` returns
a `MenuState` — a flat vec of typed rows the menu tree builder turns into
the dbusmenu tree. `install_menu_into(m, menu_state)` walks the vec and
mutates `MenuInner`.

### 8. Icon render (`icon.rs`)

`icon::bucket_for(pct, false, yellow, red, primary_burn.as_ref())` chooses
`Normal | Warning | Throttled` based on the configured thresholds **and**
the burn-rate projection (the chip flips to warning even when remaining%
looks fine if the 5h rate projects exhaustion before reset — `README.md`
documents this; the bucket selector is in `main.rs`).

For `Normal | Warning`, the tray calls `icon::write_ring_svg(pct, bucket,
&cfg.ring_colors)` which writes a path under `$TMPDIR` and returns it; the
path goes to SNI's `IconName`. Hosts that can't render SVG fall through to
the in-memory ARGB bytes from `icon::render_pixmap` (the `IconPixmap`
property; always set, never blank — see "Icon shows three dots" in
README troubleshooting). For `Throttled`, the tray uses a static warning
glyph instead of a ring.

### 9. Threshold notification (`main.rs`)

If the bucket rank increased since the last successful refresh (dedup is
**upward only** — gjs parity), `notify::send()` fires a notification via
the freedesktop `org.freedesktop.Notifications` bus. Normal → warning
fires `"… running low"`, warning → throttled fires `"… throttled"` at
critical urgency.

### 10. Re-arm

`scheduler::next_interval(base, min, max_backoff, pct, yellow, red,
fail_streak)` (`scheduler.rs`) returns the next sleep in ms:

- Adaptive: below yellow → `base`, below red → `base / 2`, below
  `red` → `base / 4`. Floor is `min_seconds`.
- Backoff: `2^fail_streak * adaptive`, capped at `max_backoff`.
- The returned ms is the `next_interval_ms` re-armed by `orchestrator`.

## Threading model

One `#[tokio::main]` runtime (`main.rs`), `rt-multi-thread`. Tasks:

| Task                          | Owns                                | Sleeps on              |
|-------------------------------|-------------------------------------|------------------------|
| Main                          | signals (`ctrl_c`, SIGTERM, menu Quit) | `tokio::signal::*`   |
| Orchestrator                  | poll cadence + menu + net events    | `tokio::time::sleep`   |
| Network watcher               | NM D-Bus `StateChanged` stream      | `SignalStream::next()` |
| zbus SNI server (in-process)  | SNI/dbusmenu method dispatch        | property signal stream |
| `spawn_blocking` ×2 per poll  | HTTP + keyring subprocess           | one-shot blocking      |

There is no `thread_local!` state. The SNI handle and `AppState` are
shared via `Arc<…>`. All cross-task mutation goes through
`tokio::sync::Mutex`.

## State machine (per window)

```
   startup
     │
     ▼
   NoData (bucket rank = -1, never emitted)
     │
     ▼  first successful fetch
   Normal  ◄─────────────────────────┐
     │                              │
     ▼  remaining_pct < 50          │ remaining_pct ≥ 50
   Warning ─────────────────────────┘ (no notification, gjs parity:
     │                              upward-only)
     ▼  remaining_pct ≤ 0
   Throttled
     │
     ▼  remaining_pct ≥ 50 (rare)
   Warning → Normal
     (no notification on the way down — already-depressed user
      doesn't need to hear "good news")
```

The bucket rank for the chip is computed in `main.rs`; the threshold
notifications gate on `prev_rank < new_rank` at `main.rs`.

## The data path in one line

```
Config ─► PlanShape ─► fetch (HTTP) ─► parse ─► Vec<Window>
                                                   │
                                ┌──────────────────┼──────────────────┐
                                ▼                                     ▼
                        record_sample                          compute_burn
                          histories[id]                       (least-squares)
                                │                                     │
                                └──────────► build_menu_state ◄───────┘
                                              │
                                              ▼
                                       tray.apply_menu
                                              +
                                       tray.update (chip)
                                              +
                                       notify::send (on ↑)
```

## What runs where when

| User action                        | Path                                               |
|------------------------------------|----------------------------------------------------|
| Click "Refresh now" in menu        | dbusmenu `Event("clicked")` → `cmd_tx` → `cmd_rx` → `MenuCommand::Refresh` arm of `orchestrator::select!` → `do_refresh()` (forces `fail_streak = 0`) |
| Click "Open dashboard"             | Same path, `MenuCommand::OpenDashboard` → `open_url(cfg.dashboard_url)` via `xdg-open` |
| Click "Set API Key…"               | Same path, `MenuCommand::SetApiKey` → `secret-tool store` over a `tokio::process::Command` (synchronous stdin pipe) → `do_refresh()` |
| Click "Quit"                       | Same path, `MenuCommand::Quit` → `shutdown_tx.try_send(())` → orchestrator returns → main loop sees signal → drops `_lock` → PID file removed by `Lock::Drop` |
| NM state changes to disconnected   | `network::run_watcher` → `NetEvent::Connectivity(false)` → `AppState.offline = true` → `render_out_of_menu(tray, cfg, true)` |
| NM state changes to connected      | Same path with `Connectivity(true)` **plus** `ForceRefresh` → `fail_streak = 0` + immediate `do_refresh()` |

## Failure paths the code is built around

- **API returns error JSON** — `parse_plan` returns `Err`; `do_refresh`
  increments `fail_streak`, re-renders the last good data with a
  "stale by N minutes" footer, and re-arms with exponential backoff.
- **API returns 200 with empty array** — `do_refresh:531` treats as
  soft error: same handling as `Err`, doesn't burn a chip.
- **Network drops mid-poll** — reqwest returns `Err`; same fail-streak
  path as above.
- **NM is unreachable** — `network::spawn_watcher` returns `Ok(())`
  without spawning; tray just stays "online". gjs parity.
- **`secret-tool` missing** — `keyring::get` falls through to the
  legacy file (`~/.config/.config/<inst>/key`), then to
  `LLM_API_KEY`. Silent — no startup failure.
- **Two launches with the same `--instance=`** — second instance's
  `Lock::acquire` returns `Ok(None)` (`lock.rs`), main loop prints
  `"another instance is already running; exiting"` and returns 0
  (`main.rs`).
- **Stale PID lock after a crash** — `Lock::acquire` checks
  `/proc/<pid>` and takes over if the holder is dead (`lock.rs`).

## Where to look when…

| Symptom                                        | Look first                                          |
|------------------------------------------------|-----------------------------------------------------|
| Chip won't update                              | `icon::write_ring_svg` (`icon.rs`), SNI `IconName`/`IconPixmap` signals (`sni.rs`) |
| Menu doesn't render                            | dbusmenu `MENU_PATH` set to `/Menu` (`sni.rs`), `apply_menu` (`sni.rs`) |
| Burn row shows when it shouldn't               | `burn::decide_burn_row` (`burn.rs`), `cfg.burn_warning.use_epoch_average` |
| Wrong endpoint hit                              | `Config::endpoint`, `Config::shape` (`config.rs`) |
| Two trays colliding                            | `instance::name()` (`instance.rs`), `Lock::acquire` (`lock.rs`) |
| Key not sticking                                | `keyring-26` — the secret-tool subprocess rationale |
| Polling too fast / too slow                    | `scheduler::next_interval` (`scheduler.rs`), `cfg.thresholds` |
| "another instance is already running" but it's not | `Lock::acquire` takeover logic (`lock.rs`) — manually `rm` `$XDG_RUNTIME_DIR/<inst>.pid` |

## Conventions worth knowing

- All per-instance namespace lives in `instance.rs` — read it before
  adding any new path-sensitive code. The instance name determines
  config dir, lock file, and keyring `application` attribute in one place.
- `Arc<Mutex<T>>` is the shared-state pattern. There's no `RwLock`
  anywhere; reads are short and use the standard Mutex.
- All HTTP + keyring work is `spawn_blocking` — the tokio runtime
  must not block on either.
- `src/util.rs` holds small helpers (`now_ms`, `open_url`, format
  helpers); consult before reaching for a one-off dep.
- Provider-neutral compile-time defaults live in `provider.rs`. Never
  add a provider-named constant there.