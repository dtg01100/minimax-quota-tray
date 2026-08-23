# Burn-rate math

The math behind the per-window informational row under each quota
bar. Companion to [`docs/gjs-parity.md`](gjs-parity.md) — gjs
parity is about **decisions**; this doc is about the **math**.

## What the row says

Two variants, picked by `decide_burn_row` → `burn_row_label`:

```text
normal:  · on pace to have ~48% left at reset (40 tok/h)
warning: ⚠ 40 tok/h → exhausts ~1h 5m before reset
```

For pct-mode windows (Coding Plan), the rate is in `%/h` instead
of `tok/h`. For currency-mode windows (`count_unit: "cents"`),
the rate is in `$/h`. For windows where the parser also
identified a model id, a `· $X/h` cost fragment is appended using
the cached per-model price table.

## Inputs

For each window, on each successful poll:

* `Window` — the parsed values from the API response
  (`total`, `used`, `remaining_pct`, `start_at`, `reset_at`,
  `count_unit?`, `currency?`, `model?`).
* `history: &[Sample]` — every past sample for this window's id
  (`Vec<Sample>` capped at 480 samples, ~16h at 120s baseline).
* `now: i64` — current epoch ms.
* `BurnConfig` — `enabled`, `min_history_ms` (default 10 min),
  `lookback_ms` (default 1h), `use_epoch_average` (default true).

## The sample

```rust
pub struct Sample {
    pub t: i64,                // ms since epoch
    pub used: i64,             // tokens consumed at this poll
    pub total: i64,            // tokens for the epoch (0 for pct-only)
    pub remaining_pct: i64,    // integer 0-100
    pub start_at: i64,         // epoch start, ms
    pub reset_at: i64,         // epoch reset, ms
}
```

`record_sample(history, window)` is called from `do_refresh()` on
every successful poll. It appends a `Sample` and evicts the oldest
if the history grows past `BURN_MAX_SAMPLES = 480`.

## Gating

`compute_burn` returns `None` when:

* `history` is empty.
* `config.enabled` is false.
* `history.len() < 2` (need at least 2 points for a slope).
* `now - history[0].t < config.min_history_ms` (need at least
  10 minutes of history by default).
* `remaining_ms <= 0` (window is already past reset).

This is the projection gate. The row doesn't show until the
feature has enough data.

## Slope math

`slope_per_hour(samples, key)` is a least-squares fit of `key`
over time, returned as the slope per hour:

```text
t0 = samples[0].t
n = samples.len()
sx = Σ (s.t - t0)        for s in samples
sy = Σ key(s)             for s in samples
mx = sx / n
my = sy / n
num = Σ ((s.t - t0) - mx) * (key(s) - my)   for s in samples
den = Σ ((s.t - t0) - mx)²                  for s in samples
slope_per_hour = (num / den) * 3.6e6
```

Returns `None` when `n < 2` or `den <= 0` (zero time-variance —
all samples have the same timestamp).

We use this twice:

* `slope_per_hour(recent, "used")` — token rate.
* `slope_per_hour(recent, "remaining_pct")` — pct rate (negated
  below because `remaining_pct` drops as usage goes up).

## The "recent" window

The recent samples are the last `lookback_ms` (default 1h) of
samples, oldest-first:

```rust
let mut recent: Vec<Sample> = Vec::with_capacity(history.len());
for s in history.iter().rev() {
    if now - s.t > config.lookback_ms { break; }
    recent.push(*s);
}
recent.reverse();
```

If `recent.len() < 2`, return `None` — the lookback window has
too few points.

## Rate selection

```text
token_rate = slope_per_hour(recent, "used")    if recent.any(|s| s.used > 0)
            else None

if config.use_epoch_average && window.start_at > 0:
    elapsed_ms = now - window.start_at
    avg = (window.used / elapsed_ms) * 3.6e6
    if avg > 0:
        token_rate = max(token_rate.unwrap_or(avg), avg)

pct_rate = -slope_per_hour(recent, "remaining_pct")
            if slope < 0 else None
```

Two rates are computed:

1. **Token rate** — the rate of `used` consumption per hour.
   Initialized from the recent slope; if `use_epoch_average` is on
   and the window has an epoch start, `max`'d with the whole-epoch
   average.
2. **Pct rate** — the rate of `remaining_pct` drop per hour.
   Negated because `remaining_pct` decreases.

The epoch-average floor is the gjs parity decision: it catches
"user was quiet for most of the epoch but just hit a burst". The
recent slope alone misses this.

## Mode selection

```rust
let count_provider = window.total > 0;
let (mode, rate, unit) = match (count_provider, token_rate, pct_rate) {
    (true, Some(tr), _)   => ("token", tr, "token"),
    (_,    _,     Some(p)) => ("pct",   p,  "pct"),
    (false, _,     _)      => ("idle",  0.0, "pct"),
    _                     => ("idle",  0.0, "token"),
};
```

The decision tree:

| `total > 0` | `token_rate` | `pct_rate` | `mode` | `unit` |
|---|---|---|---|---|
| yes | `Some(_)` | * | `token` | `token` |
| any | any | `Some(_)` | `pct` | `pct` |
| no | * | * | `idle` | `pct` |
| yes | `None` | `None` | `idle` | `token` |

`mode` and `unit` are the same when the rate is non-zero, but
`unit` stays stable at zero (`idle`) so the label says `0 tok/h`
for a token-plan idle, not `0%/h`. Coding Plan (pct-only) idle
windows keep `unit = "pct"`.

## Projection math

```rust
let remaining_ms = max(0, window.reset_at - now);

if mode == "pct" && rate > 0.0 {
    let hours_to_zero = window.remaining_pct / rate;
    exhaust_ms = hours_to_zero * 3.6e6;
    projected_pct_left =
        (window.remaining_pct - rate * remaining_ms / 3.6e6).max(0.0);
} else if mode == "token" && rate > 0.0 && window.total > 0 {
    let used_at_reset = window.used + rate * remaining_ms / 3.6e6;
    projected_pct_left =
        (100.0 * (window.total - used_at_reset) / window.total).max(0.0);
    let hours_to_zero = (window.total - window.used) / rate;
    exhaust_ms = hours_to_zero * 3.6e6;
}
```

`exhaust_ms` is the projected time until the window hits 0%
(remaining% drops to 0 for pct, used hits total for token). The
label uses it for the `⚠ → exhausts ~X before reset` warning
when `exhaust_ms < remaining_ms`.

`projected_pct_left` is the projected remaining% at the time of
reset, clamped to `[0, 100]`. The label uses it for
`· on pace to have ~N% left at reset`.

## The suppression rule

```rust
pub fn decide_burn_row(...) -> Option<BurnResult> {
    let burn = compute_burn(...)?;
    if burn.unit == "pct" && burn.rate_per_hour == 0.0 {
        return None;  // pct idle → row suppressed
    }
    Some(burn)
}
```

This is the rule that hides the row on Coding Plan's weekly window
when usage is flat:

* Coding Plan returns 0/0 for `total` and `used`. The token_rate
  path returns `None` (no `used > 0` ever). The pct_rate path can
  fire — but on a 1h lookback, per-poll drops are ~0.01–0.05%
  on weekly usage, well below 1% precision. The slope over a 1h
  lookback is meaningless on that signal.
* `unit == "pct"` (always, for pct-only providers) AND
  `rate_per_hour == 0.0` → row suppressed.

Token-counting providers (where `total > 0` and `used` moves) get
the row unconditionally, even at rate=0 — the row carries useful
info (`0 tok/h → no consumption recently`).

The rule is **hard-coded**, not configurable. Making it optional
would defeat the point.

## Window independence

Each window has its own `Vec<Sample>` keyed by `Window.id` (the
string from config). When `do_refresh` runs, it calls
`record_sample` for each window into the corresponding history:

```rust
for w in &windows {
    record_sample(&mut histories[w.id.clone()], w);
}
```

A quiet 5h window doesn't pollute the weekly rate, and vice versa.
Adding or removing a window from the config doesn't reshuffle
which history belongs to which window across restarts — that's
why we key on `id`, not on position.

The `BURN_MAX_SAMPLES = 480` cap is a memory bound; the lookback
window is at most 30 samples (1h / 120s), so the cap doesn't
affect the projection.

## Rollover behavior

When `window.reset_at < now` (the window has rolled over), the
parser produces a new `Window` with `remaining_pct = 100` (or
whatever the new epoch starts at). The burn-rate projection
naturally resets because:

* `recent` is the last 1h of samples — some of those are from
  the old epoch with low remaining%, some from the new epoch with
  high remaining%. The slope flattens or inverts.
* The epoch-average floor updates because `window.used` and
  `window.start_at` both update to the new epoch's values.

The 5h rollover doesn't clear the weekly history — they're
separate `Vec<Sample>` keyed by separate `id`s.

## Worked example (Coding Plan)

Real Coding Plan values (verified 2026-08-18):

```json
{
  "current_interval_total_count":       0,
  "current_interval_usage_count":       0,
  "current_interval_remaining_percent": 80,
  "current_interval_status":            1,
  "current_weekly_total_count":         0,
  "current_weekly_usage_count":         0,
  "current_weekly_remaining_percent":   90
}
```

For the 5h window (current_interval_*):

* `total = 0` → `count_provider = false`.
* 5h samples with `remaining_pct` dropping from 80 → 79 → 78 → 77 → 76
  every 2 min over 10 min.
* `slope_per_hour(samples, "remaining_pct") = -3` (5% drop over
  10 min = 30%/h).
* Negated: `pct_rate = 3`.
* `token_rate = None` (no `used > 0` ever; epoch-average floor also
  doesn't fire because `window.used == 0`).
* `(false, None, Some(3))` → `(mode="pct", rate=3.0, unit="pct")`.
* `remaining_ms ≈ 4 * 3.6e6` (4h until reset).
* `hours_to_zero = 76 / 3 ≈ 25h` → `exhaust_ms ≈ 90 * 3.6e6`.
* `projected_pct_left = max(0, 80 - 3 * (4 * 3.6e6) / 3.6e6) = 80 - 12 = 68`.
* `exhaust_ms (90h) > remaining_ms (4h)` → `exhaust_before_reset = false`.

Row reads:

```
  · on pace to have ~68% left at reset (3/h)
```

For the weekly window (current_weekly_*):

* `total = 0` → `count_provider = false`.
* Weekly samples with `remaining_pct` flat at 90 → 90 → 90 → 90 → 90.
* `slope_per_hour(samples, "remaining_pct") = 0` (no drop).
* `pct_rate = None` (we negate `if slope < 0`, but slope is 0).
* `token_rate = None`.
* `(false, None, None)` → `(mode="idle", rate=0.0, unit="pct")`.
* `decide_burn_row` returns `None` because `unit == "pct"` and
  `rate == 0.0`.

The weekly row is suppressed. The chip still shows the weekly
window's data (`90% left`) in the menu; only the burn-rate row
is hidden. This is gjs parity.

## Worked example (token plan)

Realistic token-plan values:

```json
{
  "current_interval_total_count":       1000,
  "current_interval_usage_count":       200,
  "current_interval_remaining_percent": 80,
  ...
}
```

5h samples with `used` ramping 0 → 200 → 400 → 600 → 800 → 1000
over 10 min (10x faster than the Coding Plan example):

* `total = 1000` → `count_provider = true`.
* `slope_per_hour(samples, "used") = 6000` (1000 tokens / 10 min
  = 6000/h).
* `epoch_average = (200 / (5*60*60*1000)) * 3.6e6 = 200 / 18_000_000 * 3.6e6 = 40/h`.
* `token_rate = max(6000, 40) = 6000` (the floor only kicks in if
  recent slope < average; here the recent slope is much higher).
* `(true, Some(6000), _)` → `(mode="token", rate=6000, unit="token")`.
* `remaining_ms ≈ 4 * 3.6e6`.
* `used_at_reset = 200 + 6000 * (4*3.6e6) / 3.6e6 = 200 + 24000 = 24200`.
* `projected_pct_left = max(0, 100 * (1000 - 24200) / 1000) = max(0, -23.2) = 0`.
* `hours_to_zero = (1000 - 200) / 6000 = 0.133h` → `exhaust_ms ≈ 8 * 60_000` (8 min).
* `exhaust_ms (8min) < remaining_ms (4h)` → `exhaust_before_reset = true`.

Row reads:

```
  ⚠ 6k tok/h → exhausts ~8m before reset
```

The chip flips to `Warning` because the burn rate projects
exhaustion before reset — even though `remaining_pct = 80` looks
healthy. This is the burn-driven flip that `bucket_for` in
`icon.rs` checks.

## The cost fragment (slice 2 feature)

When the parser populates `Window.model` (via
`WindowShape::pricing_model_path`) and the cached price table
knows the model id, the burn row appends `· $X/h`:

```
  · on pace to have ~95% left at reset (40 tok/h · $0.4/h)
```

The cost fragment is computed in `pricing::cost_per_hour`:

```rust
let prompt_tokens = tokens_per_hour * prompt_share;
let completion_tokens = tokens_per_hour * (1.0 - prompt_share);
let usd_per_hour =
    prompt_tokens * pricing.prompt_per_token +
    completion_tokens * pricing.completion_per_token;
```

`prompt_share` is currently hardcoded to `0.5` (50/50 split) —
a balanced chat workload is the sensible default. The full split
(prompt vs completion token counts separately) would need the
parser to expose them — deferred to a future slice.

Sub-tenth-cent rates are hidden (`cost_per_hour` returns `None`).
Keeps the row clean for cheap-model / low-volume workloads.
