# gjs parity decisions

What the Rust port kept verbatim from the gjs implementation, what it
redesigned, and — most importantly — **why**. The codebase has many
`// matches gjs X()` comments; this doc is the consolidated reference so
the next contributor doesn't "fix" something load-bearing.

## What the gjs implementation was

A single-file GNOME Shell extension (`llm-quota-tray@…`) that used
`libappindicator` (the AppIndicator family of GObject classes) to
register a SNI item from JavaScript. It polled the same MiniMax
endpoint, parsed the same JSON, and rendered the same chip + menu. It
was retired when the move to a tokio-first Rust port became cheaper
than maintaining the JS dependency chain.

The Rust port is the **only** supported implementation on `main`. The
gjs commit history is preserved on the `gjs/` branch.

## Decisions kept 1:1 (don't change without checking this list)

### 1. Threshold notifications are upward-only

The chip can flip freely from Normal → Warning → Throttled based on
the remaining%, but **a notification only fires when the bucket rank
moves up** (`main.rs:511`).

```rust
if prev_rank != BucketRank::NoData && new_rank > prev_rank { notify }
```

Why: an already-depressed user doesn't need to hear "good news" when
their quota refills. The original gjs implementation gated the same
way; flipping the polarity would spam the user on every reset.

### 2. Rate is `max(recent_slope, epoch_average)`

`burn::compute_burn` (`burn.rs:166`) takes the **max** of:

- Least-squares slope over the `lookback_ms` window of recent samples
  (default 1h).
- Whole-epoch average: `(window.used as f64 / (now - start_at) as f64) * 3.6e6`.

The floor handles the case where a user has been quiet for most of
the epoch but just hit a burst — the recent slope alone is high
during the burst and zero during the quiet stretch, which would let
the projection underestimate. The epoch average is a sanity floor.

`use_epoch_average: false` (in `BurnConfig`) disables the floor — use
this for short-epoch providers where the epoch average is meaningless.
But the **default** is on, and the gjs implementation also used the
floor.

### 3. Pct-only windows suppress idle rows

On the Coding Plan (MiniMax's percent-based API), `total` and `used`
are both 0 — only `*_remaining_percent` moves. At 120s polls, a weekly
window's remaining_pct drops by 0.01–0.05% per poll, well below the
1% precision of the integer percent field. The slope over a 1h lookback
is meaningless on that signal.

The fix (`burn::compute_burn`, `burn.rs:228`, and the unit tests in
`src/burn.rs::tests`):

- A window is `pct-only` when `window.total == 0`.
- `decide_burn_row` returns `None` (no row in the menu) when the rate
  is 0 on a pct-only window.
- Token-counting providers (where `total > 0` and `used` moves) get
  the row unconditionally, even at rate=0, because the row carries
  useful info (`0 tok/h → no consumption recently`).

This rule is **hard-coded**, not configurable. Making it optional
would defeat the point — the user would have to know which mode their
provider is in to pick the right setting.

### 4. Adaptive polling cadence (`scheduler::next_interval`)

The interval is `base` when remaining% is comfortably above yellow,
`base / 2` when below yellow, `base / 4` when below red, multiplied
by `2^fail_streak` for backoff, capped at `max_backoff_seconds`.

This mirrors `gjs::nextIntervalSeconds()` exactly. The reasoning is
**peer alignment**: if any tray instance has to chase a fast-depleting
quota, it polls fast; the cadence must not drift between implementations
or behavior would diverge.

### 5. Stale PID lock takeover

`Lock::acquire` (`lock.rs:30`) tries O_EXCL create first; if the
existing PID's `/proc/<pid>` is gone, it replaces. This is the gjs
behavior — when the daemon crashes hard and systemd restarts it, the
new process should take over the lock cleanly, not print
`"already running"` and exit.

The takeover is **best-effort**: if `/proc` is missing or unreadable,
we treat the lock as stale and replace (`lock.rs:90`). `lock.rs`
explicitly says: *"we'd rather run two instances than refuse to start."*

### 6. Bucket-rank enum values match gjs (Normal=0, Warning=1, Throttled=2)

The enum has a sentinel `NoData = -1` that the dedup logic treats as
"below Normal" — so the first successful fetch never fires a
notification (`prev_rank == NoData` short-circuits at `main.rs:511`).
This is the gjs behavior; a fresh install should not spam "Normal"
notifications.

### 7. Static SVG cache under `$TMPDIR`

At startup the tray writes three SVG files (one per bucket + the
throttled static) under `${TMPDIR:-/tmp}/`. The path is sent as SNI
`IconName`; the in-memory ARGB `IconPixmap` is always set as a
fallback for hosts that can't render SVG (see "Icon shows three dots"
in README troubleshooting).

The gjs implementation did the same; the SNI cache-invalidation rule
(`fix(icon): invalidate SVG cache when ring_colors change`) is a
Rust-port fix for a gjs bug where config edits weren't reflected.

### 8. Keyring write goes through stdin

`keyring.rs:65` spawns `secret-tool store --label … application …
-stdin` and pipes the secret to its stdin. The secret **never** appears
in process argv or `/proc/<pid>/cmdline`. The gjs implementation used
`secret-service.create_item(..., password)` directly; the subprocess
route is the Rust port's workaround for the secret-service crate's
tokio-runtime-context panic (see "What changed" below).

### 9. Polling jitter

`tokio::time::sleep(sleep_dur)` with `sleep_dur = max(1, wait_ms)`
(`main.rs:239`) provides 1ms of jitter in the "fire immediately" path
so two simultaneous launches don't synchronize. The gjs implementation
applied `0–5s` of jitter; the Rust port keeps the same semantic with a
smaller jitter window (the secret-tool subprocess adds its own
unpredictability).

### 10. Notification urgency matches bucket

- Normal → Warning: `notify::Urgency::Normal`
- Warning → Throttled: `notify::Urgency::Critical`

gjs parity. Lowering the Throttled notification's urgency to Normal
would defeat its purpose; the user genuinely needs to see this one.

### 11. Reset at = `now_ms + raw_reset` for duration-based reset fields

The parser (`parse.rs`) does the duration math **once** at parse time,
not at every menu render. `Window::reset_at` is always an absolute
ms-since-epoch from there on. This is gjs parity.

### 12. Lockfile `O_EXCL` semantics

`Lock::acquire` uses `OpenOptions::create_new(true)` so the
check-then-write is atomic. The gjs implementation did the same with
`GFile.create()`; the Linux filesystem guarantees apply identically.

## Decisions the Rust port **redesigned** (and why)

### 1. Pure Rust + zbus instead of GTK + libappindicator

gjs: GNOME Shell + libappindicator + libgtk.
Rust: zbus + tiny-skia (ARGB pixmap) + `std::fs::write` (SVG cache).

Why: ~4.4 MB stripped ELF, ~6 MB RSS, no GUI library at all. The host
panel does all rendering from the SNI properties + dbusmenu tree.
This is the primary motivation for the port — see README's intro.

### 2. `secret-tool` subprocess instead of `secret-service` crate

gjs: called `secret-service.create_item` via GJS bindings.
Rust: tried the `secret-service = "3"` crate with the
`rt-tokio-crypto-rust` feature.

Why: the crate's sync API internally calls
`zbus::utils::block_on(...)`, which panics with
`"Cannot start a runtime from within a runtime"` when invoked from
inside a tokio worker thread. Every keyring read from `do_refresh()`
and every write from the menu's "Set API Key…" ran on a worker
thread; the panic propagated through the `spawn_blocking` `JoinHandle`,
killed the daemon, and systemd restarted it 5s later with no key
written — the user-visible "key doesn't stick" bug. Fix commit:
`d078df4 fix(keyring): replace secret-service crate with secret-tool subprocess`.

The Rust port pays ~20–50ms per subprocess call, which is well below
the 120s baseline cadence. Documented in `keyring.rs:13-26`.

### 3. tiny-skia renders the ARGB bytes

gjs: passed a `cairo_surface_t` to libappindicator; the panel
rendered via Cairo.
Rust: tiny-skia path API draws three primitives (track, progress
arc, center dot) and emits raw ARGB32 bytes to SNI's `IconPixmap`.

Why: no libcairo dependency. The icon at 22×22 is two circles and a
stroked arc — well within tiny-skia's headroom. PNG encoding is
intentionally **disabled** (`Cargo.toml:18`): emitting raw ARGB
removes `png`, `flate2`, `miniz_oxide`, `fdeflate`, `crc32fast`,
`simd-adler32`, `adler2` from the build tree.

### 4. tokio runtime instead of GLib main loop

gjs: GLib's `MainLoop`; async via `Gio.Task`.
Rust: `#[tokio::main]` `rt-multi-thread`.

Why: reqwest, the secret-tool subprocess, and zbus all have native
tokio integration. The gjs implementation used blocking libsecret +
libcurl via GJS bindings; the Rust port keeps that blocking flavor
inside `spawn_blocking` so the tokio reactor never blocks.

### 5. Per-window burn history keyed by `Window.id` (`String`)

gjs: keyed by window position (`windows[0]`, `windows[1]`).
Rust: keyed by the user-defined `id` field.

Why: positions shift if the user reorders windows in their config or
adds a new one. The id is stable across restarts (the user picks it
once). Documented in `main.rs:81-90`. This means **changing the id
loses accumulated burn history** — README's "Window length is derived
dynamically" section explicitly calls this out.

### 6. `record_sample` evicts to `BURN_MAX_SAMPLES = 480`

gjs: kept all samples (unbounded growth until a window reset).
Rust: `VecDeque`-style oldest-first eviction at 480 samples ≈ 16h at
120s baseline.

Why: a long-lived tray with a slow-changing weekly window would
accumulate samples forever. The eviction caps memory without
affecting the lookback window (480 > 1h × 3600s / 120s = 30).

### 7. SNI ToolTip description

gjs: `indicator.set_label(label, guide)` with `guide` carrying
"Coding Plan — 80% left".
Rust: SNI `ToolTip` description field (`sni.rs:83`).

Why: surface accessibility info to screen readers and panels that
show hover tooltips. The SNI spec's `ToolTip` property takes a
3-tuple `(icon_name, (title, description), has_icon)`; the description
carries the same hint as gjs's `guide` argument. This is a strict
superset of the gjs behavior — the chip itself never carries a visible
text label (gjs parity, `sni.rs:14`).

### 8. Static-state glyph uses a hand-written BGRA circle routine

gjs: cairo drew the throttled glyph on a `cairo_surface_t`.
Rust: tiny-skia for the live ring; a 30-line raw BGRA circle-draw
for the static throttled/warning glyphs.

Why: tiny-skia is overkill for a static icon. The raw bytes-from-math
path saves the path-API overhead on startup (3 icons × tiny-skia path
allocations = ~15-20ms; the raw routine is <1ms per icon). Documented
in `icon.rs` and the `Cargo.toml` comment.

### 9. Tests live in `#[cfg(test)] mod tests` blocks per module

gjs: separate `tests/run.sh` harness that imported the real app module
with `LLM_QUOTA_TEST=1` and swapped hooks for fakes.
Rust: standard `cargo test`. `tests/integration.rs` covers the
headless D-Bus boot smoke + RSS guard, marked `#[ignore]` for CI.

Why: cargo's built-in test runner is the convention. The gjs harness
in `./tests/run.sh` was a 600-line hand-rolled test runner; the Rust
port's `cargo test -- --list` is the equivalent. The gjs tests (T18,
T19, T21 from `tests/regression-scheduler.test.js`) are preserved as
Rust unit tests in `src/burn.rs::tests` and `src/main.rs::tests` —
see `docs/development.md`'s "What the README used to describe" table.

## Decisions that look like they could be "fixed"

These are intentional. Don't change them without reading the linked
context.

| Looks wrong                                                | Actually right because…                                              | Where                  |
|------------------------------------------------------------|----------------------------------------------------------------------|------------------------|
| `~/.config/.config/<instance>/key` (doubled `.config/`)     | The legacy plaintext fallback for installs that pre-date the libsecret path; preserved for compat. | `keyring.rs:107`, `keyring.rs:191` |
| Polling at `refresh_seconds / 2` and `/ 4` is "too fast"   | Peer alignment with gjs; lets a depleting quota catch up before reset. | `scheduler.rs:24`      |
| Threshold notifications don't fire on rank decrease         | Upward-only is intentional — already-depressed users don't need "good news". | `main.rs:511`          |
| Two `spawn_blocking` calls per poll (keyring + HTTP)       | Necessary because both `secret-tool` and reqwest are blocking; the tokio reactor must not block on either. | `main.rs:367`, `main.rs:395` |
| `refresh_min_seconds` is 15 by default                      | Floor on the adaptive cadence; lower → risk of hammer-the-API on a depleting account. | `scheduler.rs:31`      |
| `BURN_MAX_SAMPLES = 480` looks low                          | At 120s polls it's 16h, far past the 1h lookback; the cap is for memory, not correctness. | `main.rs:60`           |
| `default_shape()` is a single window with `field_prefix = ""` | Lets the tray boot before any config exists.                          | `provider.rs:309`      |
| Static SVG files written to `${TMPDIR}`                    | Per-color cache; SNI hosts pick them up via `IconName`. `IconPixmap` is the always-set fallback. | `main.rs:175`, `icon.rs` |
| First refresh is unconditional (`wait_ms = 0`)             | Without it the user waits `refresh_seconds` (default 120s) before seeing any data. | `main.rs:228`          |

## Future directions (NOT yet decisions)

- **WebSocket streaming providers.** Some LLM APIs push quota updates
  via WS. The data path is currently poll-driven; a WS subscription
  could replace the polling task but would require per-provider
  handling. Not planned.
- **GNOME Shell native indicator (without AppIndicator extension).**
  GNOME 45+ removed libappindicator support. The Rust port relies on
  the AppIndicator extension being installed on GNOME; a fallback to
  a top-bar widget via the GNOME Shell D-Bus API would require a
  separate companion extension. Out of scope.
- **Templated body for non-`%` providers.** The
  `examples/providers/README.md` adapter section describes how to wrap
  a 5-line proxy; an in-tray template engine would shrink that but
  adds complexity. Not planned.