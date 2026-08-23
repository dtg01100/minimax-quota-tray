# Performance

Resource budgets, scaling, and the knobs that affect them. If
you're tuning for a low-power box or trying to understand why
your instance is using more RAM than this doc promises, read
on.

## Steady-state budgets (single instance)

These are the numbers the daemon is designed around. Measured
on a Fedora 41 / x86_64 box with one `Minimax` config and no
other instances running.

| Resource | Idle | During refresh | Burst (set-key / network reconnect) |
|---|---|---|---|
| RSS | ~5–7 MB | ~6–8 MB | ~9–12 MB (allocates a transient reqwest connection pool) |
| CPU | 0% (asleep) | <50 ms wall per refresh | <200 ms |
| Open files | 6 (binary, journald socket, libdbus socket, libsecret socket, PID lock, `/proc/<self>/status`) | +1 transient reqwest socket | same |
| Threads | 7 (tokio runtime) | same | +1 short-lived |
| Network | — | 1 GET / refresh + 0–1 pricing-table GET / 6 h | 1 SET + 1 GET / set-key |

Idle means "between refreshes, nothing happening". The daemon
is fully event-driven on tokio, so the only recurring work is
the [`refresh_seconds`](../src/scheduler.rs) timer (default
120 s — see [`config-schema.md`](config-schema.md) — `Config.refresh_seconds` field).

## What dominates the budget

- **`tiny-skia` rendering** is the only non-trivial per-refresh
  cost. Drawing the ring + center dot at 32×32 RGBA is ~10–20 µs
  per chip — the `tiny-skia` path is single-allocation and the
  pixmap buffer (32 × 32 × 4 = 4 KiB) is reused across refreshes.
- **HTTP requests** are the biggest wall-clock contributor. One
  refresh is one GET (~200–500 ms total: TCP + TLS + request +
  response + parse). The pricing-table refresh (every 6 h) is
  larger but amortized.
- **libdbus / libsecret** calls are sub-millisecond on the hot
  path. Only `secret-tool` subprocesses (set-key) cost real time
  (~100–200 ms cold spawn).

## Polling knobs

All of these are in [`config.json`](config-schema.md):

| Field | Default | Effect |
|---|---|---|
| `refresh_seconds` | 120 | Wall-clock between full refreshes (API call + parse + chip + menu rebuild). |
| `pricing_refresh_every_n_polls` | 180 | Pricing table refetch frequency in poll cycles (~36 h at default). |
| `connectivity_debounce_ms` | 1000 | Network-event debounce window — re-tries coalesce. |
| `pricing_url` / `pricing_model_field` | unset | If unset, the pricing-table fetch is a no-op (cost fragments never appear in menu rows). |

If your provider rate-limits aggressively (some return 429 at
sub-1-minute intervals), bump `refresh_seconds` to 300+ to stay
well clear of the limit. There's no adaptive backoff — we trust
the user to read the provider's docs.

## Multi-instance scaling

Each instance is a separate process with its own tokio
runtime, dbus connection, libsecret context, and tray chip.
Linear cost per instance:

- RSS: +5–7 MB
- CPU: negligible (mostly idle)
- One refresh per `refresh_seconds` per instance

Practical upper limit depends on your provider's rate limit and
your panel's tolerance for chips. 5–10 instances is fine; 50+
will visibly crowd a 24px-tall panel.

The `naming-suffix` rule ([`multi-instance.md`](multi-instance.md))
lets you spawn unlimited instances from one template — there's
no in-process multiplexing.

## Build profile

The release profile in `Cargo.toml` is tuned for size:

```toml
[profile.release]
opt-level = "z"     # minimize binary size (~1.4 MB stripped)
lto = true
codegen-units = 1
strip = true
panic = "abort"
```

A second profile, `release-debug`, inherits release but keeps
symbols for readable stack traces:

```sh
cargo build --profile release-debug
# ~4 MB binary with frame pointers + symbols retained.
```

There's no `dev` profile override — `cargo run` uses the
default dev profile, which is fine for testing (binary is
~30 MB unoptimized but the perf difference is invisible at
our scale).

## Known scaling limits

- **No HTTP/2 multiplexing.** Each refresh opens a fresh
  reqwest connection (via rustls). Adding
  `reqwest = { features = ["http2"] }` would cut TLS handshake
  time by ~50% per refresh but isn't currently enabled (would
  add ~200 KiB to the binary for marginal benefit at our refresh
  cadence).
- **No icon cache between refreshes.** Each refresh re-renders
  the same SVG-to-ARGB pixmap if the percentage hasn't changed.
  Adding an LRU keyed by `(pct, bucket, colors)` would save
  ~10 µs per refresh when the percentage is steady. Not worth
  the code complexity today.
- **Single refresh at a time.** `tokio::spawn` on `do_refresh()`
  is implicitly serialized through the orchestrator's
  `tokio::select!` arm — overlapping refreshes (e.g. a
  network-triggered refresh racing the timer) coalesce.
  This is a correctness feature, not a perf one.

## Profiling recipes

```sh
# RSS over time, 1 Hz, 60 samples.
for i in $(seq 1 60); do
  ps -o rss= -p "$(systemctl --user show llm-quota-tray.service -p MainPID --value)"
  sleep 1
done | awk '{print NR, $1}'
# Idle should be flat; bumps correspond to refreshes.

# CPU time consumed in last hour.
journalctl --user -u llm-quota-tray.service --since "1 hour ago" \
  | head -1  # confirm output is flowing

# Wall time per refresh (look at the timestamp diff between
# consecutive "started; refresh every" or "fetching pricing
# endpoint" lines and the next major lifecycle event).
journalctl --user -u llm-quota-tray.service --since "10 minutes ago" \
  | grep -E "fetching pricing endpoint|loaded.*model prices"
```

## Comparison to alternatives

The project's README claims ~12–15 MB RSS at idle; that
number is vs. the original gjs + GTK3 + libappindicator build
which was ~25 MB. The 10 MB delta is from dropping the GTK
runtime (libgtk-3, libgdk, libpango, libcairo) and the
libappindicator shim — neither is needed when talking SNI
directly via `zbus`. The binary itself is ~1.4 MB stripped
(vs. ~3 MB for the gjs build's js bundle).
