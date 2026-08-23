# Module reference

Every Rust source file in `src/`, what it owns, what it exposes, and
where to look first when something breaks. Roughly 11,000 lines
across 19 modules — start here before grepping.

## At-a-glance ownership map

| Concern | Module | Lines |
|---|---|---|
| Entry point + subsystem wiring | [`main.rs`](#mainrs) | 1784 |
| StatusNotifierItem + dbusmenu over D-Bus | [`sni.rs`](#snirs) | 989 |
| Menu tree state + dbusmenu wire format | [`menu.rs`](#menurs) | 768 |
| Burn-rate projection math | [`burn.rs`](#burnrs) | 789 |
| Secret Service (libsecret) wrapper | [`keyring.rs`](#keyringrs) | 800 |
| Icon rasterizer (tiny-skia) | [`icon.rs`](#iconrs) | 1147 |
| Per-instance namespace (CLI/env) | [`instance.rs`](#instancers) | 196 |
| Provider-agnostic types | [`provider.rs`](#providerrs) | 623 |
| Per-instance config loader | [`config.rs`](#configrs) | 568 |
| HTTP fetch + auth dispatch | [`fetch.rs`](#fetchrs) | 499 |
| JSON → `Vec<Window>` parser | [`parse.rs`](#parsers) | 588 |
| Adaptive polling scheduler | [`scheduler.rs`](#schedulerrs) | 114 |
| Single-instance PID lock | [`lock.rs`](#lockrs) | 224 |
| NetworkManager watcher | [`network.rs`](#networkrs) | 175 |
| Threshold notifications | [`notify.rs`](#notifyrs) | 343 |
| Desktop Portal OpenURI | [`portal_openuri.rs`](#portal_openurirs) | 248 |
| Formatting helpers | [`util.rs`](#utilrs) | 589 |
| Per-model pricing lookup | [`pricing.rs`](#pricingrs) | 425 |
| XDG Activation token plumbing | [`activation.rs`](#activationrs) | 190 |

LOC totals from `wc -l` at last update; see `docs/development.md` for
the test-count matrix.

---

## `main.rs`

The entry point and the only place where all subsystems are wired
together. Owns the tokio runtime, the `AppState` (per-window burn
history + last good data + connectivity state), the `Tray` (SNI
handle), and the orchestrator loop.

**Public surface**: `main()` (entry), `AppState`, `BucketRank`,
`do_refresh()`, `orchestrator()`. The rest is private to the module.

**Where to look when**:

| Symptom | Look at |
|---|---|
| Tray doesn't poll / refreshes too fast | `do_refresh`, `scheduler::next_interval`, `AppState.fail_streak` |
| Burn row missing or wrong | `compute_wait_ms`, `decide_burn_row` in `burn.rs` (called here) |
| Threshold notification fires (or doesn't) | `BucketRank::from_remaining`, the `_last_bucket` dedup gate at `do_refresh` end |
| Two instances colliding | `instance::init()` then `Lock::acquire()` — both happen here |
| "another instance is already running" | the `Lock::acquire() → Ok(None)` branch |

**Threading**: `#[tokio::main] rt-multi-thread`. Tasks: orchestrator
(poll + menu + net events), zbus SNI/dbusmenu server, NM watcher,
signal handler. All cross-task state is `Arc<Mutex<T>>` — there are
no `RwLock`s anywhere. HTTP + keyring work is wrapped in
`spawn_blocking` so the reactor never blocks.

**Constants**: `BURN_MAX_SAMPLES = 480` (≈16h at 120s baseline; oldest-
first eviction).

---

## `sni.rs`

`org.kde.StatusNotifierItem` + `com.canonical.dbusmenu` over D-Bus
via `zbus`. Pure Rust — no libappindicator, no GTK, no `libloading`.

**Public surface**:

* `Tray` — handle to the registered SNI item + dbusmenu tree.
* `Tray::new(...)` — builds the SNI object, registers on session bus,
  spawns the SNI server task. Returns once registration completes.
* `Tray::update(...)` — refreshes the chip state (icon name, ARGB
  pixmap, tooltip desc, status). Emits `NewIcon`/`NewTitle`/`NewStatus`.
* `Tray::apply_menu(...)` — installs a new `MenuInner`, bumps the
  revision, emits `ItemsUpdated` + `LayoutUpdated` +
  `ItemPropertiesUpdated`.

**Key behaviors**:

* `MENU_PATH = "/Menu"` (NOT `/NO_DBUSMENU` — that sentinel makes the
  GNOME AppIndicator extension's `menuPath` getter return null and
  permanently blocks the `ready` signal). `item_is_menu = true` is
  required for some hosts (notably the GNOME AppIndicator extension)
  to wire clicks to the dbusmenu tree.
* `emit_signal_with_timeout(name, fut)` wraps every D-Bus signal
  emission in a 5-second `tokio::time::timeout`. Without this,
  `render_initial` can hang forever on the first `new_icon` call when
  the watcher is in a bad state — observed once after a back-to-back
  restart. The chip state in `SharedState` is updated before the
  signal fires, so a missed signal only delays the panel's view by one
  poll cycle.
* `StatusNotifierItem` is the `#[interface]` implementation that
  exposes SNI properties. `DBusMenu` is the `#[interface]` for the
  dbusmenu tree.
* `ToolTip` property returns `(icon_name, (title, description), has_icon)` —
  gjs parity for the second-arg accessibility hint.
* `items_updated(update_id, removed_ids)` — always passes an empty
  `removed_ids` (we never remove items dynamically). Without this,
  `libdbusmenu-glib`'s proxy fails to read the signal and the
  AppIndicator extension never initializes the menu.

**Constants**: `SNI_PATH = "/StatusNotifierItem"`, `MENU_PATH = "/Menu"`,
`WATCHER_NAME = "org.kde.StatusNotifierWatcher"`, `WATCHER_PATH =
"/StatusNotifierWatcher"`, `SIGNAL_EMIT_TIMEOUT = 5s`.

---

## `menu.rs`

The dbusmenu tree state + the `com.canonical.dbusmenu` wire format.
A flat `Vec<MenuItem>` keyed by integer id (root is `0`, plan header
is `1`, per-window rows are `100..199`, action items are `300..306`).

**Public surface**:

* `enum MenuCommand { Refresh, OpenDashboard, SetApiKey, Quit }` —
  dispatched to the main loop via an `mpsc::Sender<MenuCommand>` set
  at construction.
* `MenuInner` — the full tree state (items + parent→children map +
  revision counter).
* `MenuInner::new()` — initialize the static items (root, header,
  action items, separators).
* `MenuInner::set_header(text)`, `set_throttled(text, visible)`,
  `set_error(text, visible)`, `rebuild_window_rows(labels, bars, burns)`
  — mutate state, bump revision.
* `build_layout_response(state, parent_id, recurse)`,
  `build_properties(item)`, `build_child_variants(state, parent_id,
  recurse)` — produce the wire-format types dbusmenu clients expect.
* Constants: `ROOT_ID = 0`, `HEADER_ID = 1`, `THROTTLED_ID = 200`,
  `ERROR_ID = 210`, `SEP_1_ID = 300`, `REFRESH_ID = 301`, etc.

**Key behaviors**:

* `children_of` lists every potential child id (including hidden
  ones). Visibility is tracked on the `MenuItem`, not via structural
  membership — so hiding a row doesn't trigger an `ItemsUpdated`
  signal, only `ItemPropertiesUpdated`.
* `max_window_slots` grows monotonically; when the window count
  shrinks the extra slots are kept hidden. This stabilizes IDs
  across restarts so the dbusmenu client doesn't churn.
* The `MenuCommand` action is set at construction (via
  `MenuItem::action(id, label, cmd)`) and read by the dbusmenu
  `Event("clicked")` handler in `sni.rs::DBusMenu::event`.

---

## `burn.rs`

Per-window burn-rate sample history + the projection math. The
`Window` type is the abstract shape every provider maps into; the
`Sample` type is one per-poll observation.

**Public surface**:

* `struct Window { id, total, used, remaining_pct, start_at, reset_at, count_unit?, currency?, model? }` —
  the universal per-window shape.
* `struct Sample { t, used, total, remaining_pct, start_at, reset_at }`.
* `struct BurnResult { rate_per_hour, mode, unit, exhaust_ms, remaining_ms, exhaust_before_reset, projected_pct_left }`.
* `struct BurnConfig { enabled, min_history_ms, lookback_ms, use_epoch_average }`
  — defaults: `enabled=true`, `min_history_ms=10*60*1000`,
  `lookback_ms=60*60*1000`, `use_epoch_average=true`.
* `fn slope_per_hour(samples, key) -> Option<f64>` — least-squares
  slope of `key` ("used" or "remaining_pct") per hour. Returns
  `None` for <2 samples or zero time-variance.
* `fn compute_burn(window, history, now, config) -> Option<BurnResult>` —
  the projection math. Returns `None` when feature is disabled or
  history hasn't accumulated `min_history_ms` yet.
* `fn decide_burn_row(window, history, now, config) -> Option<BurnResult>`
  — same as `compute_burn` but applies the pct-only suppression rule
  (a pct-only window with rate=0 returns `None`; a token-counting
  window keeps the row).

**Mode selection** (the subtle bit): `mode` is `'token'` when
`window.total > 0` and a token-rate exists, `'pct'` when a pct-rate
exists, `'idle'` otherwise. `unit` is `'token'` for token-mode and
`'pct'` for pct-mode — `unit` is what the label uses so an idle token
plan still says "0 tok/h" rather than "0%/h". See
[`burn-rate.md`](burn-rate.md) for the math.

**Gjs-parity decisions** (don't change without reading):

* `rate = max(recent_slope, epoch_average)` — the floor catches
  bursty usage after a long quiet stretch.
* Pct-only idle windows suppress the row (Coding Plan).
* Token-plan idle windows keep the row (carries useful info).
* Rate resets on every window rollover (per window, independently).

---

## `keyring.rs`

Freedesktop Secret Service wrapper — async zbus calls against the
session bus, **no** subprocess. The `secret-service = "3"` crate
panics from inside a tokio worker thread (`zbus::utils::block_on`
collision), so the project talks Secret Service directly via zbus 5.x.

**Public surface**:

* `async fn get() -> Option<String>` — look up the API key.
  Priority: Secret Service (current AND legacy attribute name) →
  `LLM_API_KEY` env var.
* `async fn set(value: &str) -> Result<()>` — write the key with
  the current attribute name. Migrates any legacy-attribute item
  found in the same collection.
* `async fn clear() -> Result<()>` — delete items matching either
  attribute name. Exposed for a future "Clear stored key" menu item.
* `fn secret_to_key(bytes: &[u8]) -> Option<String>` — UTF-8 decode
  + trim trailing whitespace + drop empty results. Trimming matters
  because `secret-tool store` via a shell pipe can persist trailing
  newlines that reqwest's header parser rejects.

**Key behaviors**:

* Spec compliance targets the canonical Secret Service spec at
  https://specifications.freedesktop.org/secret-service/latest/.
  Service: `OpenSession`, `SearchItems`, `Unlock`, `ReadAlias`.
  Collection: `CreateItem(properties, secret, replace_bool)` — the
  spec puts `replace` as a `Boolean`. Item: `Delete`, `GetSecret(session)`,
  `SetSecret(secret)`.
* Attribute migration: searches BOTH `application = llm-quota-tray`
  (current) AND `application = minimax-quota` (legacy, pre-rename).
  On a successful `set()`, any legacy-attribute item is deleted
  opportunistically. We never auto-migrate on read — the user might
  be running an old tray instance in parallel.
* Wire shape: `Secret { session, parameters, value, content_type }`,
  zbus signature `(oayays)`. The `Secret` struct's `Type` derive is
  pinned by a unit test.
* Lazy session-bus connection via `tokio::sync::OnceCell<Connection>`.

---

## `icon.rs`

Pure-Rust icon rasterizer via `tiny-skia`. Three primitives drawn
into a 22×22 ARGB32 buffer:

1. A faded full-circle track (`stroke`, opacity 0.25).
2. A progress arc (`stroke`, `linecap=round`, dasharray computed
   from the percentage).
3. A center dot (`fill`) — without it the icon reads as a thin
   curved line.

**Public surface**:

* `enum Bucket { Normal, Warning, Throttled }`.
* `fn bucket_for(remaining_pct, throttled, yellow, burn) -> Bucket` —
  matches gjs `bucketForChip()`. `Warning` flips on remaining% OR on
  burn-rate projecting exhaustion before reset. `Throttled` only when
  the window is exhausted (`pct <= 0`) — the chip stays yellow while
  depleting, then falls through to the static throttled dot at 0%.
* `fn ring_svg_path(pct, colors) -> PathBuf` — file path under
  `${TMPDIR:-/tmp}/llm-quota-tray-ring-{pct:03d}-{colors_hash}.svg`.
* `fn static_svg_path(name, colors) -> PathBuf` — file path for a
  static icon (throttled/warning/error/offline). The `name` is one of
  `"throttled"`, `"warning"`, `"error"`, `"offline"`.
* `fn write_static_svgs(colors)` — writes all four statics to disk
  at startup. Invalidates when `colors` change between polls.
* `fn write_ring_svg(pct, bucket, colors) -> PathBuf` — writes the
  per-pct SVG and returns its path. The path goes to SNI's
  `IconName`.
* `fn render_pixmap(pct, bucket, colors) -> Option<(u32, u32, Vec<u8>)>`
  — returns the live ARGB bytes for SNI's `IconPixmap`. Always
  `Some` (the universal fallback).
* `fn cache_step(pct) -> i64` — quantization step (every 2% for a
  stable, small cache).

**Key behaviors**:

* PNG encoding is **disabled** in `Cargo.toml`. We emit raw ARGB32
  bytes to SNI's `IconPixmap`. Hosts that prefer `IconName` look up
  the SVG file under `${TMPDIR}`.
* The two-channel color model (see `provider::RingColors`): inner
  dot is the bucket's status color (Normal/Warning/Throttled), outer
  ring is the percentage-fill color.
* `colors.cached_hash` (FNV-1a of all four color strings) is the
  filename suffix — same colors = same path = same file = cache hit
  across instances.

---

## `instance.rs`

Per-instance identity. Each instance is its own tray with its own
config dir, lock file, and keyring `application` attribute.

**Public surface**:

* `fn init() -> &'static str` — call once from `main()` before any
  path-sensitive code runs. Returns the resolved instance name
  (`""` for the default instance).
* `fn resolve() -> String` — parse CLI + env without storing.
* `fn parse<I>(args) -> (String, bool)` — full CLI parse: instance
  name + `--set-key` flag.
* `fn wants_set_key() -> bool` — true when `--set-key` is on argv.
* `fn name() -> &'static str` — the resolved instance name.
* `fn is_default() -> bool` — true when the name is empty.
* `fn config_dir_basename() -> String` — `llm-quota-tray` (default) or
  `llm-quota-tray-<name>` (named instance). Used by config dir,
  keyring `application`, and keyring `label`.
* `fn lock_path() -> PathBuf` — `${XDG_RUNTIME_DIR:-/tmp}/<basename>.pid`.
* `fn keyring_application() -> String` — equals `config_dir_basename()`.

**Key behaviors**:

* `--instance=<name>` CLI flag (preferred) → `QUOTA_INSTANCE=<name>`
  env var → `""` (default).
* Both `--token=<token>` (in `activation.rs`) and `--instance=` parsing
  must happen before any path-sensitive code runs.

---

## `provider.rs`

Provider-agnostic types — no provider branding in source. Holds the
types other modules deserialize into, plus compile-time defaults.

**Public surface**:

* `struct RingColors { inner: BucketColors, outer: String, cached_hash: u64 }` —
  the two-channel color model.
* `struct BucketColors { normal, warning, throttled: String }` —
  the inner dot's status colors.
* `fn default_inner_colors() -> BucketColors` — classic gjs green/yellow/red.
* `const DEFAULT_OUTER_COLOR: &str = "#3584e4"` — neutral GNOME blue.
* `fn default_ring_colors() -> RingColors` — both defaults composed.
* `enum AuthConfig { Bearer, Header { name }, Custom { name, format }, QueryParam { name } }` —
  tagged enum, serialized as `{"type": "..."}`. Defaults to `Bearer`.
* `AuthConfig::build(&self, api_key) -> (String, String)` —
  `(header_name, header_value)` for the request.
* `AuthConfig::apply_to_endpoint(&self, endpoint, api_key) -> String` —
  URL rewriting for `QueryParam` (percent-encoded).
* `struct WindowShape { id, field_prefix, start_field?, reset_field?, start_unit_ms, reset_unit_ms, reset_is_absolute_epoch, count_unit?, currency?, pricing_model_path? }`.
* `struct PlanShape { entries_path, windows: Vec<WindowShape>, error_envelope?: ErrorEnvelope }`.
* `struct ErrorEnvelope { code_path, message_path, success_codes }`.
* `fn default_shape() -> PlanShape` — single generic window.
* `const DEFAULT_USER_AGENT: &str = "llm-quota-tray"`.

**Wire compatibility**: `RingColors` accepts three wire shapes via
the `RingColorsRepr` shuttle:

1. `{ "inner": {...}, "outer": "..." }` — canonical.
2. `{ "normal": "...", "warning": "...", "throttled": "..." }` — legacy
   flat. Becomes `inner`; `outer` defaults to neutral accent.
3. `{}` — empty. Both default.

---

## `config.rs`

Per-instance config loader. Reads `~/.config/llm-quota-tray[-<name>]/config.json`.

**Public surface**:

* `struct Config { endpoint, dashboard_url, label, shape, ring_colors, auth, user_agent, refresh_seconds, refresh_min_seconds, refresh_max_backoff_seconds, thresholds, burn_warning, pricing_endpoint?, pricing_refresh_polls? }`.
* `struct Thresholds { yellow: i64, red: i64 }`.
* `const CONFIG_DIR_BASE = "llm-quota-tray"`, `CONFIG_FILENAME = "config.json"`.
* `fn config_path_for(instance: &str) -> PathBuf` —
  `~/.config/<base>[-<instance>]/config.json`.
* `fn config_path() -> PathBuf` — backwards-compatible alias.
* `fn load_for(instance: &str) -> Config` — read + parse (or defaults).
* `fn load() -> Config` — backwards-compatible alias.
* `fn load_or_init() -> Result<Config>` — read; if missing, write
  defaults to disk with `0600` perms, then return.

**Key behaviors**:

* `load_or_init` writes the config file with mode `0600` (matches
  gjs; `std::fs::write` would inherit the umask).
* Malformed JSON → `Config::default()` + a `warn!` log line. The tray
  still boots with sensible defaults instead of refusing to start.

**Tests**: includes the schema-drift guard
`provider_templates_deserialize` which walks `examples/providers/*.json`
and asserts each parses as a `Config` with non-empty required fields.

---

## `fetch.rs`

HTTPS fetch via reqwest (rustls-tls — no OpenSSL). Blocking API,
wrapped in `tokio::task::spawn_blocking` from the polling loop.

**Public surface**:

* `pub use reqwest::blocking::Client as HttpClient` — re-exported so
  callers can refer to `fetch::Client`.
* `fn build_client(user_agent_prefix: &str) -> Result<Client>` —
  builds the shared blocking reqwest client (15s timeout, 10s connect
  timeout, version-appended User-Agent).
* `fn fetch_windows_blocking(client, endpoint, api_key, auth, shape) -> Result<Vec<Window>>` —
  the data-driven HTTP driver. Applies `QueryParam` auth to the URL,
  then dispatches header auth, then parses JSON, then runs the
  configured `error_envelope` reader, then calls `parse::parse_plan`.
* `fn sanitize_error_snippet(raw: &str) -> String` — truncates to
  80 chars and redacts whole lines containing `bearer` /
  `authorization` / `api-key` / `api_key` / `token` (case-
  insensitive). Used on HTTP error bodies before they reach the
  menu / journald.

**Why rustls**: OpenSSL/GnuTLS are heavy build-time deps; the
`rustls-tls` feature is pure Rust. Provider-specific bits (auth,
User-Agent, JSON shape) come from `Config` — this module is
provider-agnostic.

---

## `parse.rs`

JSON → `Vec<Window>`. All provider-specific details come from the
`PlanShape` — no provider constants live here.

**Public surface**:

* `fn parse_plan(payload: &Value, shape: &PlanShape, now_ms: i64) -> Result<Vec<Window>>` —
  returns one `Window` per `WindowShape` entry. Errors on missing
  entry array / empty array.

**Field-by-field parser contract** (what the parser reads off the
entry for each `WindowShape`):

```text
{field_prefix}_total_count        (i64) — quota total for the window
{field_prefix}_usage_count        (i64) — quota consumed in the window
{field_prefix}_remaining_percent  (i64) — integer 0-100; the chip's primary signal
{start_field}                     (i64) — epoch start, scaled by start_unit_ms
{reset_field}                     (i64) — duration or absolute epoch, scaled by reset_unit_ms
{pricing_model_path}              (str) — model id (used by pricing lookup)
```

Missing optional fields degrade to zero (defensive — providers
sometimes drop fields). Floats are truncated to integers (the
parser's `num()` helper).

**First-entry selection**: `entries_path = "/"` means the root
object is the entry (single-window providers); otherwise the path is
treated as a JSON pointer into an array and `entries[0]` is read.

---

## `scheduler.rs`

Adaptive polling scheduler — pure function, no state.

**Public surface**:

* `fn next_interval(base, min_seconds, max_backoff, remaining_pct, yellow, red, fail_streak) -> u64` —
  returns the next sleep in seconds.

**Math** (mirrors gjs `nextIntervalSeconds()`):

```text
used = 100 - remaining_pct
adaptive = used >= red   ? base / 4
         : used >= yellow ? base / 2
         : base
adaptive = adaptive.max(min_seconds)  // never hammer the API
if fail_streak > 0:
    adaptive *= 2^min(fail_streak, 8)  // 2^u64 wraps at exponent 64
    adaptive = min(adaptive, max_backoff)
```

The `fail_streak.min(8)` cap prevents `2^64` overflow; the
`max_backoff` cap stops the interval growing past
`refresh_max_backoff_seconds`.

---

## `lock.rs`

Single-instance PID lock at `${XDG_RUNTIME_DIR:-/tmp}/<basename>.pid`.

**Public surface**:

* `struct Lock` — RAII wrapper; `Drop` releases the lock.
* `fn Lock::acquire() -> Result<Option<Self>>` — `Ok(Some(_))` on
  acquire, `Ok(None)` if a live instance holds it, `Err` on I/O
  failure.
* `fn Lock::release(&self)` — best-effort `rm`; idempotent.

**Semantics**:

* `O_EXCL` create (`OpenOptions::create_new(true)`) for atomic
  check-then-write. If the file exists, read its PID; if
  `/proc/<pid>` is gone, take over.
* On platforms without `/proc` (rare), the lock is best-effort: we'd
  rather run two instances than refuse to start.
* `lock.rs` explicitly says: *"we'd rather run two instances than
  refuse to start"* — the takeover is the safer failure mode.

---

## `network.rs`

NetworkManager `StateChanged` watcher. Graceful degradation when NM
isn't reachable (headless CI, minimal WMs) — the tray just stays
online.

**Public surface**:

* `enum NetEvent { Connectivity(bool), ForceRefresh }` — sent to the
  main loop via `mpsc::Sender`.
* `async fn spawn_watcher(tx: mpsc::Sender<NetEvent>) -> Result<()>` —
  spawns a tokio task that watches NM and forwards transitions.
  Returns `Ok(())` without spawning if no system bus is reachable.

**State semantics**:

* Initial state read at startup. If offline, send `Connectivity(false)`
  immediately so the tray skips the first fetch.
* Transition to offline → `Connectivity(false)`. The orchestrator
  cancels any pending fetch and renders the offline icon.
* Transition to online → `Connectivity(true)` AND `ForceRefresh`. The
  orchestrator restarts polling immediately, skipping backoff.

---

## `notify.rs`

Threshold notifications via the freedesktop Notification portal,
with a direct `org.freedesktop.Notifications` fallback.

**Public surface**:

* `enum Urgency { Low, Normal, Critical }` — three levels per the
  freedesktop `urgency` hint.
* `async fn send(tag, title, body, urgency, activation_token)` — best-
  effort dispatch. Portal first; falls through to direct
  Notifications on portal failure.

**Key behaviors**:

* Portal-first (sandbox-friendly, canonical replace semantics).
* Direct fallback uses the `x-canonical-private-synchronous` hint
  for gjs-parity dedup behavior.
* `tag` is the dedup key — passed as the portal `id` and as the
  libnotify-server synchronous hint.
* `activation_token` is forwarded as the portal `activation_token`
  vardict key (Notification portal v2+). Stale tokens (None) are
  silently omitted. The direct-Notifications fallback ignores it.

---

## `portal_openuri.rs`

`org.freedesktop.portal.OpenURI` wrapper — sandbox-friendly
replacement for `xdg-open(1)`.

**Public surface**:

* `async fn open(uri: &str, activation_token: Option<&str>) -> Result<()>`.

**Spec**: targets https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.OpenURI.html.

* **Bus**: `org.freedesktop.portal.Desktop`
* **Path**: `/org/freedesktop/portal/desktop`
* **Interface**: `org.freedesktop.portal.OpenURI`
* **Method**: `OpenURI(IN s parent_window, IN s uri, IN a{sv} options, OUT o handle)`

URL open is fire-and-forget — we drop the request handle without
subscribing to `Response`. The caller (currently `main.rs`'s
OpenDashboard handler) doesn't have a failure surface to care about;
the user dismissing the chooser simply means the link doesn't open.

The legacy `xdg-open(1)` subprocess fallback is kept inline in
`main.rs` for hosts without `xdg-desktop-portal` running.

---

## `util.rs`

Formatting helpers shared across modules. Pure functions — no state.

**Public surface**:

* `fn fmt_duration(ms: i64, floor: bool) -> String` — "5m", "1h 5m",
  "2d 3h". `floor=true` matches gjs `fmtAge()`; `floor=false`
  matches gjs `fmtReset()` (ceil minutes).
* `fn fmt_age(ms: i64) -> String` — "30s", "5m", "2h". Floor.
* `fn fmt_rate(rate: f64) -> String` — "850", "1.2k", "12k". Strips
  decimal `.0` (gjs parity).
* `fn fmt_cost(cents_per_hour: f64, currency: Option<&str>) -> String` —
  "$0.005", "$0.40", "$5.23", "$123". Tier-based decimal count,
  trailing zeros stripped. Currency symbol from
  `currency_symbol(currency)` (USD/EUR/GBP/JPY/CNY/KRW/INR; unknown → `$`).
* `fn bar_markup(fraction_pct: i64) -> String` — 22-column ASCII bar
  using U+2588 (full) + U+2591 (light shade). gjs parity (avoids
  Pango markup because some SNI menu renderers don't support it).
* `fn burn_row_label(burn, count_unit, currency, cost_fragment) -> String` —
  the per-window informational line. `count_unit = "cents"` /
  `"milliunits"` switches to `fmt_cost`; `cost_fragment = Some(s)`
  appends `· s` to the rate portion.
* `fn window_label(label, remaining_pct, resets_in_ms, stale: bool) -> String`
  — the per-window primary row label.

---

## `pricing.rs`

Dynamic per-model pricing lookup. Provider-agnostic — the wire
shape it expects is OpenRouter's `/api/v1/models` (decimal-string
prices).

**Public surface**:

* `struct ModelPricing { prompt_per_token, completion_per_token, input_cache_read_per_token? }`.
* `type PriceTable = HashMap<String, ModelPricing>`.
* `fn fetch_pricing_blocking(client: &HttpClient, url: &str) -> Result<PriceTable>` —
  blocking HTTP fetch + parse. Reuses the existing `HttpClient` so
  the project doesn't ship two TLS stacks.
* `fn cost_per_hour(table, model, tokens_per_hour, prompt_share) -> Option<String>` —
  returns the `· $X/h` cost fragment, or `None` when the model isn't
  in the table, the rate is sub-tenth-cent (hidden as noise), or the
  model id is `None`.

**Refresh cadence** (in `Config`):

* `pricing_endpoint: Option<String>` — URL of the price-list endpoint.
  `None` → no pricing fetch, no cost fragment.
* `pricing_refresh_polls: Option<u64>` — every N successful polls,
  re-fetch the table. `None` → fetch once at startup.

**Wiring**: `AppState.price_table` (in `main.rs`) holds the cached
table. `AppState.polls_since_pricing_refresh` gates the periodic
re-fetch inside `do_refresh()` (best-effort; failures keep the
previous table).

---

## `activation.rs`

XDG Activation token plumbing. The desktop shell provides the token
when launching the binary via the `.desktop` file (StartupNotify).

**Public surface**:

* `fn init()` — resolve the token once at startup. Stores it in a
  process-global `OnceLock<Option<String>>`.
* `fn resolve<I: IntoIterator<Item = String>>(args) -> Option<String>` —
  parse CLI + env without storing. Factored out so tests can drive
  it without mutating the `OnceLock`.
* `fn get() -> Option<&'static str>` — look up the captured token.

**Precedence**: `--token=<value>` CLI flag → `--token <value>`
two-token form → `$XDG_ACTIVATION_TOKEN` env var → `None`.
Empty tokens are treated as "no token" per the spec.

**Call sites**: `portal_openuri::open(uri, activation_token)`,
`notify::send(tag, title, body, urgency, activation_token)`. Both
forward the token via the `activation_token` vardict key on the
portal call. The direct-Notifications fallback ignores it.

The token is **never logged, never persisted, never handed to
anything that isn't a portal call**. Stale tokens (None, expired)
are silently omitted — the portal shows the notification/dialog
without a launch animation.
