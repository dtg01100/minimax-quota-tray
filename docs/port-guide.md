# Port guide

How to add the tray for a new LLM API. The "Simple" track is enough for
most providers — copy a template, edit three fields, run. The "Hard"
track is for providers whose JSON shape doesn't match the parser's
contract at all (you'll need a sidecar).

> **Read this first if you're new:** [`docs/config-schema.md`](config-schema.md)
> for the field-by-field reference, [`docs/architecture.md`](architecture.md)
> for the call graph.

---

## Which track do I need?

| If your provider's API…                                              | Track   | Time     |
|----------------------------------------------------------------------|---------|----------|
| Returns `{prefix}_remaining_percent` directly (integer 0-100)        | Simple  | 5 min    |
| Returns `total` / `used` floats or strings, but no percent           | Simple  | 10 min   |
| Returns only consumption (no total / no remaining), no public endpoint | Hard   | 30 min   |
| Exposes quota only in response headers, not a JSON body              | Hard    | 30 min   |
| Exposes no quota info at all                                         | Stop    | n/a      |

---

## Simple track

The tray's parser reads fields whose names are **declared in the
config** (`shape.windows[].field_prefix` + field suffixes). As long as
your JSON has *something* in `{prefix}_total_count`,
`{prefix}_usage_count`, and/or `{prefix}_remaining_percent`, you don't
need to write any code.

### Step 1 — pick the closest template

```sh
ls examples/providers/
# anthropic.json  cohere.json  deepseek.json  google-gemini.json  groq.json
# minimax.json    mistral.json  openai.json   openrouter.json    together.json
```

The closest match by shape:

| Your provider looks like…                    | Start from        |
|-----------------------------------------------|-------------------|
| MiniMax / Zhipu / Moonshot style              | `minimax.json`    |
| Float balance, no percent field               | `openrouter.json` |
| String balance, no percent field              | `deepseek.json`   |
| Daily-bucket consumption, no total            | `mistral.json`    |

### Step 2 — copy and edit four fields

```sh
cp examples/providers/<closest>.json ~/.config/llm-quota-tray-<name>/config.json
$EDITOR  ~/.config/llm-quota-tray-<name>/config.json
```

The four fields you'll change for almost every port:

| Field           | What to put                                                                                  |
|-----------------|----------------------------------------------------------------------------------------------|
| `endpoint`      | Your provider's quota/usage/balance endpoint URL.                                            |
| `dashboard_url` | The browser page to open from "Open dashboard".                                              |
| `label`         | What shows in the chip + menu header (e.g. `"Mistral Pro"`, `"OpenRouter"`, `"My Company"`). |
| `auth`          | One of `{"type":"bearer"}`, `{"type":"header","name":"x-api-key"}`, `{"type":"custom",…}`, `{"type":"query_param","name":"key"}`. |

The `shape.windows[]` may also need edits if your field names differ.
See [`docs/config-schema.md`](config-schema.md#windowshape-providerrs254)
for the field-by-field contract.

### Step 3 — drop your API key into the keyring

```sh
llm-quota-tray --instance=<name> --set-key
# → enter the key, hit Enter
```

Or set `LLM_API_KEY=<your-key>` in your shell rc — the keyring falls
back to the env var if `secret-tool` isn't available.

### Step 4 — launch and verify

```sh
llm-quota-tray --instance=<name>
# → chip should appear in your panel within a few seconds
```

If the chip stays red, the issue is almost always the **shape** — your
endpoint returned something the parser couldn't map. Move to the
**Hard** track below, or check the diagnostic checklist at the bottom
of this doc.

### Step 5 — pin the systemd unit (optional)

For a tray that survives reboots:

```sh
cp llm-quota-tray.service ~/.config/systemd/user/llm-quota-tray-<name>.service
$EDITOR ~/.config/systemd/user/llm-quota-tray-<name>.service
# Change Description= and add ` --instance=<name>` to the end of ExecStart=

systemctl --user daemon-reload
systemctl --user enable --now llm-quota-tray-<name>.service
```

Different instances never collide — each has its own config dir, lock
file, and keyring `application` attribute. See
[`docs/architecture.md`](architecture.md#multi-instance-namespace)
for the namespace rules.

---

## Hard track

Use this when your provider's API doesn't return the parser's expected
fields at all. You'll add a **sidecar** — a tiny HTTP server that
sits between the tray and your provider, and translates the response
into the parser's shape.

### When you need a sidecar

- The provider returns quota only in **response headers** (Groq, Cohere,
  Anthropic with non-admin keys).
- The provider returns **consumption** without **total** or **remaining**
  (OpenAI `/v1/usage`, Mistral `/v1/usage`) — your sidecar injects the
  total from your known tier limit.
- The provider returns **floats** as cents/dollars and the tray's
  integer percent field can't see sub-dollar precision (OpenRouter).
- The provider requires **multi-header auth** (Anthropic with both
  `x-api-key` and `anthropic-version`) that the `AuthConfig` enum
  can't express.
- The provider requires **request signing**, **OAuth refresh**, or any
  other upstream concern the tray's blocking reqwest client can't handle.

### Pattern: sidecar that wraps the upstream response

```
   ┌──────────────┐  curl/probe  ┌───────────┐  provider API  ┌──────────┐
   │ llm-quota-   │ ───────────► │ sidecar   │ ─────────────► │ upstream │
   │ tray         │   HTTP GET   │ (Python/  │  HTTPS         │ provider │
   │ (pointed at  │ ◄─────────── │  Go/Node) │ ◄───────────── │          │
   │ 127.0.0.1)   │ parser-shaped│           │  native shape  │          │
   └──────────────┘    JSON      └───────────┘                └──────────┘
```

The sidecar's job: take whatever shape the upstream returns, and
emit JSON shaped like:

```json
{
  "data": [{
    "{field_prefix}_total_count":       <i64>,
    "{field_prefix}_usage_count":       <i64>,
    "{field_prefix}_remaining_percent": <i64 0-100>,
    "{field_prefix}_start_time":        <epoch>,
    "{field_prefix}_remains_time":      <ms or epoch, see reset_is_absolute_epoch>
  }]
}
```

`entries_path` in your config points at the array (`"/data"`, `"/"`, `"/balance_infos"`, etc.).

### Pattern A — jq wrapper (simplest)

For providers with bearer auth and a 1-shot probe that returns the
right info, you can do the entire sidecar as a shell pipeline + a
5-line `socat`/`caddy` listener.

**Worked example — DeepSeek balance string → int:**

```sh
# ~/.local/bin/deepseek-quota-sidecar.sh
#!/usr/bin/env bash
socat TCP-LISTEN:8088,fork,reuseaddr SYSTEM:'
curl -s -H "Authorization: Bearer '"$DEEPSEEK_API_KEY"'" \
  https://api.deepseek.com/user/balance | jq ".balance_infos[0] | . as \$b | {
    balance_total_count:       (\$b.total_balance  | tonumber * 100 | floor),
    balance_usage_count:       (((\$b.total_balance | tonumber) - (\$b.granted_balance | tonumber)) * 100 | floor),
    balance_remaining_percent: ((((\$b.total_balance | tonumber) - (\$b.granted_balance | tonumber)) / (\$b.total_balance | tonumber)) * 100 | floor),
    balance_start_time:        (now | floor),
    balance_remains_time:      31536000000
  } | {balance_infos: [.]}"
'
```

Then your config's `endpoint` is `http://127.0.0.1:8088/`. Auth in the
config goes unused (the sidecar handles it) — set `{"type":"bearer"}`
anyway because the tray's fetch code requires some `AuthConfig`
variant. See `examples/providers/deepseek.json` for the full version.

**Worked example — OpenRouter USD floats → cents:**

```sh
curl -s -H "Authorization: Bearer $OPENROUTER_KEY" \
  https://openrouter.ai/api/v1/key | jq '.data | . as $d | {
    credit_total_count:       (.limit * 100 | floor),
    credit_usage_count:       (.usage * 100 | floor),
    credit_remaining_percent: (((.limit - .usage) / .limit * 100) | floor),
    credit_start_time:        (now | floor),
    credit_remains_time:      2592000000
  } | {data: .}'
```

### Pattern B — Python proxy (for headers-only providers)

When the upstream quota signal is in **response headers** (Groq,
Cohere), you need a proxy that makes an HTTP request, reads the
headers, and emits them as JSON.

**Worked example — Groq rate-limit headers:**

```python
#!/usr/bin/env python3
"""groq-quota-sidecar.py — minimal Groq rate-limit proxy."""
import http.server, json, os, time, urllib.request

GROQ_KEY = os.environ['GROQ_KEY']
GROQ_TPM = int(os.environ.get('GROQ_TPM', '6000'))

class Q(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        req = urllib.request.Request(
            'https://api.groq.com/openai/v1/models',
            headers={'Authorization': f'Bearer {GROQ_KEY}'})
        r = urllib.request.urlopen(req)
        rem = int(r.headers['x-ratelimit-remaining-tokens'])
        rst = int(r.headers['x-ratelimit-reset-tokens'])
        now = int(time.time())
        body = json.dumps({
            'tokens_total_count':       GROQ_TPM,
            'tokens_usage_count':       GROQ_TPM - rem,
            'tokens_remaining_percent': int(rem / GROQ_TPM * 100),
            'tokens_start_time':        now - rst,
            'tokens_remains_time':      rst * 1000,
        }).encode()
        self.send_response(200)
        self.send_header('content-type', 'application/json')
        self.send_header('content-length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *a): pass

http.server.HTTPServer(('127.0.0.1', 8088), Q).serve_forever()
```

Run with `systemd --user`, set `GROQ_KEY` + `GROQ_TPM` (your tier's TPM
cap) in the unit's `Environment=`. Point the tray's config at
`http://127.0.0.1:8088/`. See `examples/providers/groq.json` for the
matching `shape` block.

**Why the tier TPM is hardcoded:** Groq (and Cohere, and Anthropic
admin) don't expose the cap in any queryable form — it's only known to
the user. The sidecar must accept it as a config value.

### Pattern C — multi-header auth

The `AuthConfig` enum (`provider.rs`) handles single-header auth.
When the provider needs two (Anthropic's `x-api-key` + `anthropic-version`,
for instance), you have two options:

1. **Drop the version into the user-agent** if the provider allows it
   (most don't — Anthropic specifically requires `anthropic-version` as
   its own header).
2. **Extend `AuthConfig`** to support `MultiHeader { headers: Vec<(String, String)> }`.
   This is a code change; see [`docs/development.md`](development.md#how-to-add-a-new-auth-style).
3. **Run a sidecar** that sets both headers upstream, then emits the
   parser-shaped JSON. The Anthropic template does this
   (`examples/providers/anthropic.json`); see `_rate_limit_proxy` /
   `_admin_adapter` in that file.

For Anthropic specifically, **the sidecar pattern is the recommended
path** — extending `AuthConfig` for a single provider is more code than
the 30-line sidecar.

### Pattern D — OAuth / token refresh

Same idea: sidecar. The tray's keyring stores a single string token;
the sidecar can do the OAuth dance, refresh, and cache the result.
Point the tray's config at `http://127.0.0.1:8088/`; let the sidecar
handle the auth.

---

## Diagnostic checklist

When the chip stays red or the menu shows an error:

1. **Read the stderr.** The tray logs the endpoint, label, and shape at
   startup (`main.rs`) and the fetch error per poll
   (`fetch::sanitize_error_snippet` redacts the API key before logging).
   `RUST_LOG=debug` shows more.

2. **Run a `curl` against the endpoint with the same headers the tray
   sends.** Diff the response against the field names your
   `shape.windows[].field_prefix` expects. Most "shape mismatch" bugs
   are spelling (`total` vs `total_count`, `remaining` vs
   `remaining_percent`).

3. **Check `entries_path`.** The parser walks `entries_path` as a JSON
   pointer (`/data`, `/balance_infos`, etc.). A wrong path silently
   yields an empty windows vec → "API returned no quota windows" in
   the menu.

4. **Check the field suffixes.** The parser reads:
   ```text
   {prefix}_total_count
   {prefix}_usage_count
   {prefix}_remaining_percent
   ```
   Not `{prefix}_total`, not `{prefix}_used`. If your provider returns
   `usage_total` and `usage_used`, set `field_prefix: "usage"` and
   the parser does the right thing.

5. **For pct-only providers (Coding Plan style):** the parser expects
   `*_remaining_percent` as an **integer 0..100**. If your provider
   returns it as a float (`80.5`) the parser truncates to 80. That's
   expected — the chip's percent is integer-precision by design (see
   `main.rs` for the bucket-rank enum that reads it).

6. **For duration-vs-epoch reset fields:** if the menu shows
   `"resets in 1970-01-01"` or `"resets in 50 years"`, your
   `reset_is_absolute_epoch` is wrong:
   - `false` (default) → the parser adds `now_ms` to the raw value
     (provider returns "ms until reset").
   - `true` → the parser uses the raw value as an absolute epoch.

7. **For sidecar setups:** point your browser at `http://127.0.0.1:8088/`
   and confirm the body looks right. If the sidecar's response is
   empty or wrong, the tray will silently fall through to "0 / 0 left".

8. **If two instances are colliding:** check `$XDG_RUNTIME_DIR/`.
   Each instance has its own PID file (`llm-quota-tray-<name>.pid`).
   A stale PID file whose owning process is gone is taken over by
   `Lock::acquire` (`lock.rs`); a live holder is refused with
   `"another instance is already running; exiting"`.

---

## Worked example 1 — Google AI Studio (Gemini)

You have a Google AI Studio API key and want a tray that shows your
rate-limit budget. Gemini doesn't expose a public quota endpoint, but
its responses carry `x-ratelimit-*` headers similar to Groq's.

**The path:**

1. Copy `examples/providers/google-gemini.json` to
   `~/.config/llm-quota-tray-gemini/config.json`.

2. Spin up a Groq-style sidecar that hits any Gemini endpoint, captures
   the `x-ratelimit-*` headers, and emits the parser-shaped JSON.
   The Gemini template (`examples/providers/google-gemini.json`) has
   the full python sidecar.

3. Run the sidecar under systemd:
   ```ini
   # ~/.config/systemd/user/gemini-quota-sidecar.service
   [Unit]
   Description=Gemini quota sidecar
   After=network-online.target

   [Service]
   ExecStart=/usr/bin/python3 %h/.local/bin/gemini-quota-sidecar.py
   Environment=GEMINI_KEY=YOUR_KEY
   Environment=GEMINI_RPM_LIMIT=15
   Restart=on-failure

   [Install]
   WantedBy=default.target
   ```

4. `systemctl --user daemon-reload && systemctl --user enable --now gemini-quota-sidecar.service`.

5. Set `endpoint` in your config to `http://127.0.0.1:8088/quota` (or
   wherever the sidecar listens — match the listener's path).

6. `llm-quota-tray --instance=gemini --set-key` → drop a dummy key
   (the tray will store it but the sidecar uses its own auth).

7. `llm-quota-tray --instance=gemini`. The chip shows `Gemini <RPM-remaining>%`
   and the menu row tells you when the next reset window opens.

## Worked example 2 — porting a brand-new provider with native shape

Imagine Provider X exposes:

```
GET https://api.provider.com/v1/quota
Authorization: Bearer ***
→ { "primary":   { "limit": 1000,  "used": 120,  "reset_in_ms": 7200000 },
    "secondary": { "limit": 30000, "used": 4500,  "reset_in_ms": 2592000000 } }
```

The whole port is one config file. No code, no sidecar:

```json
{
  "endpoint":      "https://api.provider.com/v1/quota",
  "dashboard_url": "https://provider.com/dashboard",
  "label":         "Provider X",
  "shape": {
    "entries_path": "/",
    "windows": [
      {
        "id": "primary",
        "field_prefix": "primary",
        "reset_field":  "reset_in_ms",
        "reset_unit_ms": 1,
        "reset_is_absolute_epoch": false
      },
      {
        "id": "secondary",
        "field_prefix": "secondary",
        "reset_field":  "reset_in_ms",
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
  "auth": { "type": "bearer" }
}
```

Notes:
- `field_prefix: "primary"` → the parser reads `primary_total_count`,
  `primary_usage_count`, `primary_remaining_percent` off the entry.
  Provider X doesn't return those names — it returns `primary.limit`,
  `primary.used`, `primary.reset_in_ms`. That mismatch is **the
  whole reason** this template wouldn't work directly.
- Either rename the upstream keys (if you control the provider), or
  write a 5-line jq wrapper that does the rename.

## Worked example 3 — porting a provider with no `*_remaining_percent`

OpenAI is the canonical case. `/v1/usage` returns:

```json
{ "data": [
  { "aggregation_timestamp": 1720747200,
    "n_context_tokens_total": 4500,
    "n_generated_tokens_total": 1200 }
] }
```

The provider returns consumption but no total, no remaining. You
hardcode the tier's TPM in the sidecar:

```sh
TIER_TPM=600000
curl -s -H "Authorization: Bearer $OPENAI_KEY" \
  https://api.openai.com/v1/usage | jq --argjson cap $TIER_TPM '
(.data | last) as $d | {
  data: [{
    bucket_total_count:       $cap,
    bucket_usage_count:       ($d.n_context_tokens_total + $d.n_generated_tokens_total),
    bucket_remaining_percent: ((($cap - ($d.n_context_tokens_total + $d.n_generated_tokens_total)) / $cap * 100) | floor),
    bucket_start_time:        $d.aggregation_timestamp,
    bucket_remains_time:      ((86400 - (($d.aggregation_timestamp % 86400))) * 1000)
  }]
}'
```

Then point the tray at the sidecar and set `entries_path: "/data"`,
`field_prefix: "bucket"`. The full template is in
`examples/providers/openai.json`.

## Worked example 4 — multi-window non-MiniMax

Some providers return more than one window natively. If they do, add a
`WindowShape` per window:

```json
{
  "shape": {
    "entries_path": "/",
    "windows": [
      { "id": "minute", "field_prefix": "minute_window",  "start_unit_ms": 1000, "reset_unit_ms": 1, "reset_is_absolute_epoch": false },
      { "id": "hour",   "field_prefix": "hour_window",    "start_unit_ms": 1000, "reset_unit_ms": 1, "reset_is_absolute_epoch": false },
      { "id": "day",    "field_prefix": "day_window",     "start_unit_ms": 1000, "reset_unit_ms": 1, "reset_is_absolute_epoch": false }
    ]
  }
}
```

The parser iterates `windows[]` and emits one `Window` per entry. The
first window drives the chip percentage; subsequent windows render as
additional menu rows. There's no fixed limit — three is fine, five is
fine, ten is fine. Each window gets its own burn-rate history keyed
by `id` (`main.rs`).

If your provider returns the windows in a single flat JSON object
(not an array), point `entries_path` at the object's keys
(`/minute_window` etc.) — the parser always uses `entries[0]`, so
that works only when there's exactly one window. For multiple windows
in a single object, normalize to an array in your sidecar.

---

## What the parser does NOT do

Things that look like parser responsibilities but are not:

- **Aggregating across multiple entries.** The parser reads `entries[0]`
  only. If your provider returns daily buckets, put TODAY first in the
  array (sort desc, take head).
- **Caching.** Every poll is a fresh HTTP GET. The tray has no
  client-side cache.
- **Retry.** One HTTP attempt per poll; backoff is across polls
  (`scheduler.rs`), not within a single poll.
- **Schema migration.** If your provider changes field names, update
  the config — there's no version negotiation.
- **Cross-instance deduplication.** Each instance polls independently.

---

## Committing a new template

Once your port works, contribute the template back so the next person
doesn't repeat the work:

```sh
# Copy your working config back to the repo, preserving comments
cp ~/.config/llm-quota-tray-<name>/config.json \
   examples/providers/<name>.json

# Edit the _comment* keys to explain the non-obvious field choices
$EDITOR examples/providers/<name>.json

# The schema-drift guard parses every template at build time
cargo test config::tests::provider_templates_deserialize

# Open a PR — include:
#   - the new template (required)
#   - any docs updates (optional — only if the template introduces a new pattern)
```

See [`docs/development.md`](development.md#how-to-add-a-new-provider-template)
for the full contribution checklist.