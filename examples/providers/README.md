# Provider config templates

Sample `config.json` files for popular LLM providers. Each one is a
complete, valid instance config — drop it into
`~/.config/llm-quota-tray-<name>/config.json`, set the API key, and
launch the tray with `llm-quota-tray --instance=<name>`.

The tray itself is **provider-agnostic**: no provider branding is
baked into the source. Each template below describes only the
endpoint, JSON shape, and auth style needed to talk to that
provider. Everything else (ring colors, refresh cadence,
thresholds, burn-warning settings) is independent of the API and
can be tuned per-instance.

## Files in this directory

| File | Provider | Endpoint shape fit |
|---|---|---|
| `minimax.json` | MiniMax Coding Plan / Token Plan | ✅ native — the parser's contract was designed for this shape |
| `openrouter.json` | OpenRouter | ⚠️ partial — returns floats, no `remaining_percent` field; needs a small adapter |
| `deepseek.json` | DeepSeek | ⚠️ partial — balance is a string, no `remaining_percent` field |
| `mistral.json` | Mistral AI | ⚠️ partial — daily usage buckets, no `remaining_percent` |
| `together.json` | Together AI | ⚠️ partial — billing credit returns int balance |
| `openai.json` | OpenAI | ⚠️ adapter required — daily usage, no remaining/total |
| `anthropic.json` | Anthropic | ⚠️ admin-only endpoint, otherwise headers |
| `groq.json` | Groq | ⚠️ no public quota endpoint — needs header-to-JSON sidecar |
| `google-gemini.json` | Google AI Studio / Gemini | ⚠️ no public quota endpoint — needs sidecar |
| `cohere.json` | Cohere | ⚠️ no public quota endpoint — needs sidecar |

The **shape fit** column tells you whether the provider's natural
JSON response can be parsed without modification or whether you
need a small adapter (a proxy that injects the fields the parser
expects, or a shell pipeline that rewrites the body before the
tray sees it).

> The root `config.example.json` is the same template as
> `minimax.json` — it's the config the tray writes on first launch
> if no `config.json` exists yet. The two are kept in sync.

## How the parser reads each window

For every window in `shape.windows[]`, the parser looks for these
fields in the first entry of the payload (where `entries_path`
points):

```text
{field_prefix}_total_count       (i64) — quota total for the window
{field_prefix}_usage_count       (i64) — quota consumed in the window
{field_prefix}_remaining_percent (i64) — integer 0-100, the chip's primary signal
{start_field}                    (i64) — window start, epoch scaled by start_unit_ms
{reset_field}                    (i64) — reset: DURATION or ABSOLUTE EPOCH, scaled by reset_unit_ms
```

`start_field` defaults to `start_time`; `reset_field` defaults to
`remains_time`. Override either when the provider uses different
names (see the Mistral weekly window — its `start_field` is set to
`weekly_start_time`).

`reset_is_absolute_epoch: false` (the default) means
`reset_field` is a **duration** — the parser adds `now_ms` to get
the absolute reset timestamp. `true` means the value is already an
absolute epoch.

`start_unit_ms` and `reset_unit_ms` are the unit-conversion
multipliers. If a field is in epoch seconds, set the multiplier to
`1000`. If it's already in milliseconds, use `1`. If it's in
microseconds, use `0.001` (not supported — only integer
multipliers; multiply the field by `0` and let the tray show zeros,
or scale the upstream response).

The chip displays **`{prefix}_remaining_percent`** clamped to
0–100. This is the **only field the chip reads directly** —
everything else (`_total_count`, `_usage_count`, `start_time`,
`remains_time`) drives the menu rows and burn-rate projection.

## When a provider doesn't expose `*_remaining_percent`

Many Western providers (OpenAI, Anthropic, Google, Mistral) expose
**consumption** stats — "you used N tokens today" — but not a
**remaining** stat. The tray can't compute a percentage without
either:

1. The provider's known tier limit, or
2. A configured ceiling, or
3. A proxy that injects a `*_remaining_percent` field based on
   known limits or the response of a sibling endpoint.

The templates below that hit this case either:

- **Use the endpoint as-is and accept that the chip will show 0%
  unless `*_remaining_percent` is set** — useful for tracking
  consumption (the menu row still shows `used / total` when both
  are present).
- **Document the missing field and point at a 5-line adapter
  pattern** (a sidecar `socat`/`caddy`/`nginx` config that wraps
  the response with computed fields).

If you're adding a brand-new provider that returns
`*_remaining_percent` directly (the MiniMax, DeepSeek, Zhipu,
Moonshot style), the template is straightforward — see
`minimax.json` for the canonical shape.

## Using a template

```sh
# Pick a name for the instance — used for config dir, keyring, lock file
INSTANCE=openrouter

# Create the per-instance config dir
mkdir -p ~/.config/llm-quota-tray-${INSTANCE}

# Drop the template in
cp examples/providers/openrouter.json \
   ~/.config/llm-quota-tray-${INSTANCE}/config.json

# Edit it — at minimum, set endpoint and (if needed) ring colors
$EDITOR ~/.config/llm-quota-tray-${INSTANCE}/config.json

# Set the API key (one-time — stored in the OS keyring)
llm-quota-tray --instance=${INSTANCE} --set-key
# (then choose "Set API Key…" in the tray menu for subsequent runs)

# Launch
llm-quota-tray --instance=${INSTANCE}
```

## Validating a template at build time

`cargo test provider_templates_deserialize` deserializes every
`.json` file in this directory as a `Config` and fails the build on
schema drift. If you add or edit a template, run the test (or just
`cargo test`) to catch typos before the tray hits a malformed
config at runtime. The test lives at the bottom of
`src/config.rs::tests`; it walks `examples/providers/`, parses each
template, and asserts that required fields (endpoint, label, at
least one window, non-zero refresh cadence) are populated.

## Adding a new template

When you find a new LLM API with a clean shape, copy any of the
files in this directory as a starting point. The minimum fields:

```json
{
  "endpoint": "https://api.example.com/v1/quota",
  "dashboard_url": "https://example.com/dashboard",
  "label": "Example Plan",
  "shape": {
    "entries_path": "/",
    "windows": [
      {
        "id": "primary",
        "field_prefix": "primary",
        "start_unit_ms": 1000,
        "reset_unit_ms": 1,
        "reset_is_absolute_epoch": false
      }
    ]
  },
  "refresh_seconds": 120,
  "refresh_min_seconds": 15,
  "refresh_max_backoff_seconds": 600,
  "thresholds": { "yellow": 60, "red": 85 }
}
```

Every other field has a compile-time default — see `provider.rs`
for the defaults. The template above is the minimum viable config;
the full templates in this directory add explanatory `_comment*`
keys, brand-aligned `ring_colors`, and sensible burn-warning
thresholds.

For a brand-new auth style, extend the `AuthConfig` enum in
`provider.rs` (`bearer` / `header` / `custom` / `query_param` are
the four currently supported styles).