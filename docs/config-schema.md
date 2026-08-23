# Config schema

The authoritative reference for every field in
`~/.config/llm-quota-tray/config.json` (or
`~/.config/llm-quota-tray-<instance>/config.json`). For an end-to-end
worked example, see README's "Porting to another provider" section.

> The tray has **no provider constants in source**. Every field that
> varies between providers lives in this file. Adding a new provider is
> a config-file change, not a code change (unless the provider needs a
> brand-new `AuthConfig` variant or a non-window-driven response).

The Rust types this doc mirrors are in `src/provider.rs`, `src/config.rs`,
and `src/burn.rs`. The schema-drift guard is `provider_templates_deserialize`
in `src/config.rs::tests` — it walks `examples/providers/*.json` and
parses each one, so a typo or missing field fails the build.

## `Config` (`config.rs`)

Top-level config. All fields except `endpoint`, `dashboard_url`, `label`,
and one `WindowShape` are optional and have compile-time defaults.

| Field                         | Type                | Default          | Source              |
|-------------------------------|---------------------|------------------|---------------------|
| `endpoint`                    | string (required)   | —                | `Config::endpoint`  |
| `dashboard_url`               | string (required)   | —                | `Config::dashboard_url` |
| `label`                       | string (required)   | —                | `Config::label`     |
| `shape`                       | `PlanShape`         | see `default_shape` (`provider.rs`) | `Config::shape` |
| `ring_colors`                 | `RingColors`        | see `default_ring_colors` (`provider.rs`) | `Config::ring_colors` |
| `auth`                        | `AuthConfig`        | `Bearer`         | `Config::auth`      |
| `user_agent`                  | string              | `"llm-quota-tray"` (`DEFAULT_USER_AGENT`) | `Config::user_agent` |
| `refresh_seconds`             | u64                 | `120`            | `Config::refresh_seconds` |
| `refresh_min_seconds`         | u64                 | `15`             | `Config::refresh_min_seconds` |
| `refresh_max_backoff_seconds` | u64                 | `600`            | `Config::refresh_max_backoff_seconds` |
| `thresholds`                  | `Thresholds`        | `{yellow: 60, red: 85}` | `Config::thresholds` |
| `burn_warning`                | `BurnConfig`        | see below        | `Config::burn_warning` |

`_comment*` keys are allowed anywhere and ignored by serde. The
`examples/providers/*.json` files use them heavily — copy that
convention.

## `RingColors` (`provider.rs`) — three accepted wire shapes

`RingColors` is the tray's two-channel color model: an inner dot that
shows status (normal/warning/throttled) and an outer ring that shows
remaining quota as an arc. Three wire shapes deserialize, via
`RingColorsRepr` (`provider.rs`):

### Canonical (preferred)

```json
{
  "ring_colors": {
    "inner": {
      "normal":   "#3a9d4d",
      "warning":  "#f6d32d",
      "throttled": "#e01b24"
    },
    "outer": "#3584e4"
  }
}
```

- `inner.normal` / `warning` / `throttled`: status-bucket colors.
  Three independent `#RRGGBB` strings; each may be omitted and falls
  back to `default_inner_colors` (`provider.rs`).
- `outer`: percentage-arc color, single `#RRGGBB`. Default
  `DEFAULT_OUTER_COLOR = "#3584e4"` (`provider.rs`).

### Legacy flat

For backward compat with pre-split configs:

```json
{
  "ring_colors": {
    "normal":   "#3a9d4d",
    "warning":  "#f6d32d",
    "throttled": "#e01b24"
  }
}
```

Top-level `normal/warning/throttled` map to `inner.*`; `outer` defaults
to the neutral blue. The deserializer detects this shape by checking
that `inner` is `None` and any of the legacy fields is `Some`
(`provider.rs`).

### Empty

```json
{ "ring_colors": {} }
```

Equivalent to omitting the field — both `inner` and `outer` fall back
to defaults.

## `BucketColors` (`provider.rs`)

```json
{ "normal": "#RRGGBB", "warning": "#RRGGBB", "throttled": "#RRGGBB" }
```

Used as `RingColors::inner`. All three fields are optional `String`s
that default to `"#3a9d4d"` / `"#f6d32d"` / `"#e01b24"`. The shape is
trivial, but the defaults are explicit in `default_inner_colors`
(`provider.rs`) — gjs parity.

## `AuthConfig` (`provider.rs`)

Tagged enum, serialized as `{"type": "..."}`. The runtime dispatch is
in `AuthConfig::build` (`provider.rs`) and
`AuthConfig::apply_to_endpoint` (`provider.rs`).

### `bearer` (default)

```json
{ "auth": { "type": "bearer" } }
```

Sends `Authorization: Bearer <key>`. The common case (OpenAI,
Anthropic, Mistral, MiniMax, …).

### `header`

```json
{ "auth": { "type": "header", "name": "x-api-key" } }
```

Sends `<name>: <key>`. Use for providers that take a custom header
(Anthropic, Google AI Studio).

### `custom`

```json
{ "auth": { "type": "custom", "name": "Authorization", "format": "Token {key}" } }
```

Sends `<name>: <format>` with `{key}` substituted at request time
(`provider.rs`). Use for auth schemes that don't fit a single
header shape (`Token`, `ApiKey-V1 …`, etc.).

### `query_param`

```json
{ "auth": { "type": "query_param", "name": "key" } }
```

Appends `?<name>=<key>` to the endpoint URL. No header sent. The fetch
code handles the URL rewriting in `apply_to_endpoint` — see `fetch.rs`
(`provider.rs`).

## `PlanShape` (`provider.rs`)

Where to find the entry array and which windows to extract.

```json
{
  "shape": {
    "entries_path": "/model_remains",
    "windows": [ /* WindowShape[] — see below */ ],
    "error_envelope": { /* optional — see below */ }
  }
}
```

| Field           | Type             | Required | Notes                                                |
|-----------------|------------------|----------|------------------------------------------------------|
| `entries_path`  | string           | yes      | JSON pointer into the response (e.g. `"/model_remains"`, `"/"`, `"/data"`). |
| `windows`       | `WindowShape[]`  | yes      | At least one window.                                  |
| `error_envelope`| `ErrorEnvelope`? | no       | If present, non-success codes return `Err` from the parser. |

### Default shape (`provider.rs`)

`default_shape()` returns a single-window `PlanShape` with
`entries_path = "/"` and one window of `field_prefix = ""`. The tray
can boot before a config file exists (it writes `config.example.json`
on first launch via `config::load_or_init`).

## `WindowShape` (`provider.rs`)

Describes how to extract one quota window from the first entry of
`entries_path`.

```json
{
  "id": "5h",
  "field_prefix": "current_interval",
  "start_field":   "start_time",
  "reset_field":   "remains_time",
  "start_unit_ms": 1000,
  "reset_unit_ms": 1,
  "reset_is_absolute_epoch": false
}
```

| Field                    | Type    | Default      | What it controls                                       |
|--------------------------|---------|--------------|--------------------------------------------------------|
| `id`                     | string  | — (required) | UI label (chip/menu row) **and** burn-rate history key. Pick something stable — changing it loses history. |
| `field_prefix`           | string  | — (required) | The parser reads `{prefix}_total_count`, `{prefix}_usage_count`, `{prefix}_remaining_percent` off the entry. |
| `start_field`            | string  | `"start_time"` | Field name for window start. Override when the provider uses a non-standard name (Mistral weekly: `"weekly_start_time"`). |
| `reset_field`            | string  | `"remains_time"` | Field name for the reset time. Same rationale.    |
| `start_unit_ms`          | i64     | — (required) | Multiplier applied to `start_field` to get ms-since-epoch. `1000` for epoch seconds, `1` for ms, etc. |
| `reset_unit_ms`          | i64     | — (required) | Multiplier applied to `reset_field`. Same logic.       |
| `reset_is_absolute_epoch`| bool    | `false`      | `false` → parser computes `reset_at = now_ms + raw_reset` (duration). `true` → parser uses `raw_reset` as an absolute epoch. |

The fields the parser actually reads off the entry:

```text
{field_prefix}_total_count        (i64)  — quota total for the window
{field_prefix}_usage_count        (i64)  — quota consumed in the window
{field_prefix}_remaining_percent  (i64)  — integer 0..100; the chip's
                                          primary signal
{start_field}                     (i64)  — epoch start, scaled by start_unit_ms
{reset_field}                     (i64)  — duration or absolute epoch,
                                          scaled by reset_unit_ms
```

If the provider returns `used`/`total` but **not** `*_remaining_percent`
(most Western providers), the parser falls back to
`100 - (100 * used / total)`. If both `used` and `total` are 0 (Coding
Plan), the percent is taken from `*_remaining_percent` directly. If
neither source is available, the chip shows 0% — see `examples/providers/README.md`'s
"When a provider doesn't expose `*_remaining_percent`" section for the
adapter pattern.

## `ErrorEnvelope` (`provider.rs`)

```json
{
  "error_envelope": {
    "code_path":    "/base_resp/status_code",
    "message_path": "/base_resp/status_msg",
    "success_codes": [0]
  }
}
```

| Field           | Type      | Notes                                              |
|-----------------|-----------|----------------------------------------------------|
| `code_path`     | string    | JSON pointer to the status code (e.g. `"/base_resp/status_code"`). |
| `message_path`  | string    | JSON pointer to a human-readable error message.    |
| `success_codes` | int[]     | Values that mean "success". Anything else → parser returns `Err`. |

If omitted, the parser always treats the response as success (the
status code lives in the HTTP layer, which `fetch_windows_blocking`
already handles).

## `Thresholds` (`config.rs`)

```json
{ "thresholds": { "yellow": 60, "red": 85 } }
```

Both fields are required when `thresholds` is present. They drive the
chip's bucket transitions:

- `yellow` (default `60`) — `used >= yellow` flips the chip to warning.
- `red` (default `85`) — `used >= red` flips the chip to throttled.

`used` here means `100 - remaining_pct` (the chip label still shows
**remaining** %). The burn-rate projection can also flip the chip to
warning independently (see `main.rs` and `burn::compute_burn`).

## `BurnConfig` (`burn.rs`)

```json
{
  "burn_warning": {
    "enabled": true,
    "min_history_ms": 600000,
    "lookback_ms": 3600000,
    "use_epoch_average": true
  }
}
```

| Field               | Type | Default       | What it controls                                  |
|---------------------|------|---------------|---------------------------------------------------|
| `enabled`           | bool | `true`        | Master switch. `false` → no burn row, no warning. |
| `min_history_ms`    | i64  | `600000` (10m) | Minimum history length before any burn row appears. |
| `lookback_ms`       | i64  | `3600000` (1h) | The "recent slope" window for the rate computation. |
| `use_epoch_average` | bool | `true`        | When true, the rate is `max(recent_slope, epoch_average)`. When false, recent slope alone (reacts to short-term spikes; misses long-quiet-then-bursty usage). |

Defaults match the gjs implementation. The pct-only suppression rule
(burn row hidden when rate = 0 on a Coding Plan-style provider) is
hard-coded in `burn::decide_burn_row` (`burn.rs`) and is not
configurable — it would defeat the purpose to make it optional.

## Compiled defaults at a glance

These are the values used when a config field is missing or the config
file doesn't exist at all. Pulled directly from `provider.rs` and
`config.rs`:

```rust
default_inner_colors()   // { normal: "#3a9d4d", warning: "#f6d32d", throttled: "#e01b24" }
DEFAULT_OUTER_COLOR      // "#3584e4"
default_ring_colors()    // { inner: default_inner_colors(), outer: DEFAULT_OUTER_COLOR }
DEFAULT_USER_AGENT       // "llm-quota-tray"
default_shape()          // { entries_path: "/", windows: [{ id: "", field_prefix: "", start_unit_ms: 1, reset_unit_ms: 1, reset_is_absolute_epoch: false }] }
default_refresh_min_seconds()  // 15
Thresholds::default      // { yellow: 60, red: 85 }  — but Config::default uses literal {yellow:60, red:85}, see config
BurnConfig::default      // { enabled: true, min_history_ms: 600000, lookback_ms: 3600000, use_epoch_average: true }
AuthConfig::default      // Bearer
Config::default          // { endpoint: "", dashboard_url: "", label: "Quota", shape: default_shape(),
                          //   ring_colors: default_ring_colors(), auth: Bearer,
                          //   user_agent: DEFAULT_USER_AGENT, refresh_seconds: 120,
                          //   refresh_min_seconds: 15, refresh_max_backoff_seconds: 600,
                          //   thresholds: {yellow:60, red:85}, burn_warning: BurnConfig::default() }
```

## What does NOT go in config

Things that look like they might belong but are hard-coded by design:

- **The keyring `application` attribute.** Derived from the instance
  name (`instance::keyring_application` → `config_dir_basename`). Per-instance
  namespacing happens here, not via config, so two instances can never
  collide.
- **The lock file path.** Same derivation: `${XDG_RUNTIME_DIR}/<basename>.pid`.
- **The status-bucket cutoffs.** A fixed `50% remaining` line in
  `BucketRank::from_remaining` (`main.rs`) — configurable thresholds
  govern the chip; the rank enum is for notification dedup.
- **The Notification cooldown / re-arm logic.** `_last_bucket` is in-process
  state (`main.rs::AppState`).
- **The polling jitter (`0-5s`).** Baked into the orchestrator's
  adaptive cadence — see the gjs parity doc for why.

## How the schema-drift guard works

`provider_templates_deserialize` in `src/config.rs::tests` walks
`examples/providers/*.json` and parses each as a `Config`. Any
deserialization failure fails the test. It catches:

- Typos in field names (serde ignores unknown fields by default; this
  test asserts the *required* fields parse, not that extras are
  rejected).
- Wrong types (`refresh_seconds: "120"` — a string — fails).
- Missing required fields (`endpoint`, `label`, at least one window).

It does **not** catch: missing optional fields with non-default
behavior (because the defaults apply silently). Add a test in
`src/config.rs::tests` if you have a config field with a non-default
default you care about.