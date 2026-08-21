# Changelog

All notable changes to `llm-quota-tray` are documented here. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added (slice 2: dynamic per-model pricing lookup)

- **`src/pricing.rs`** — new module. `ModelPricing`,
  `PriceTable`, `fetch_pricing_blocking()` (parses OpenRouter's
  `/api/v1/models` shape), and `cost_per_hour()` (returns the
  `$X/h` fragment for a model+rate, or `None` when sub-tenth-cent
  / model unknown / no table).
- **`Config::pricing_endpoint`** — URL of the price-list endpoint.
  OpenRouter's `/api/v1/models` is fully public and works
  unchanged. `#[serde(default)]` so existing configs parse.
- **`Config::pricing_refresh_polls`** — optional cadence (in
  successful polls) for re-fetching the table. `None` = fetch
  once at startup.
- **`WindowShape::pricing_model_path`** — JSON pointer to the
  model id field in the API response entry. Parser reads it onto
  `Window.model`; renderer looks the model up in the price table.
- **`Window::model`** — the model id (parser-populated when the
  path is set). `#[serde(default)]`.
- **`AppState::price_table` + `polls_since_pricing_refresh`** —
  cached table + refresh counter. Startup fetches happen in
  `main::run()`; periodic refresh in `do_refresh()` (best-effort,
  failures keep the previous table).
- **`util::burn_row_label`** — `cost_fragment: Option<&str>`
  parameter. When `Some`, the rate portion is suffixed with
  ` · $X/h`. `None` preserves the legacy one-rate label.
- **`OpenRouter inference prototype`** —
  `examples/providers/openrouter-inference.json` — a Python sidecar
  probes `/api/v1/chat/completions` every N seconds, captures the
  model id from the response, and emits parser-shaped JSON. The
  tray fetches `/api/v1/models` on startup, finds the model's
  per-token prices, and the burn row reads
  `· on pace to have ~95% left at reset (40 tok/h · $0.4/h)`.

### Notes (slice 2)
- The pricing lookup is **independent** of the parser's token
  math. It only ADDS a `$X/h` fragment next to the existing
  `tok/h` or `%/h` rate; nothing about the rate itself changes.
- Sub-tenth-cent rates are hidden (`cost_per_hour` returns
  `None`). Keeps the row clean for cheap-model / low-volume
  workloads where the dollar rate would be meaningless noise.
- `prompt_share` is hardcoded to 0.5 in `build_menu_state` —
  a balanced chat workload is a sensible default. Full split
  pricing would need the parser to expose prompt vs completion
  token counts separately; deferred to a future slice.
- All 184 unit tests pass (was 164; +20 new for slice 2).

### Added (slice 1: currency-aware burn rows)
- **Currency-aware burn rows** — `WindowShape.count_unit` and
  `WindowShape.currency` (both `Option<String>`, both
  `#[serde(default)]`). When `count_unit` is `"cents"` or
  `"milliunits"`, the burn-row label re-renders from `40 tok/h` to
  `$0.4/h` (or `$5/h`, `$123/h` depending on tier). pct-only windows
  are unaffected — the rate stays in `%/h`. Default is `"tokens"`
  for all existing configs, no behavior change.
- **`util::fmt_cost`** — cost formatter matching `fmt_rate`'s tier
  system (`$0.005` → `$0.0050` style; 4/3/2-decimal tiers under
  $100, integer at $100+). Trailing zeros stripped, mirroring
  `fmt_rate`'s gjs parity.
- **`util::burn_row_label`** — signature extended with
  `count_unit: Option<&str>` and `currency: Option<&str>` (both
  backward-compatible: defaults preserve legacy `tok/h` output).
- **`Window`** — two new fields (`count_unit`, `currency`),
  `#[serde(default)]` so deserialization of older snapshots is
  tolerant.
- **OpenRouter prototype** —
  `examples/providers/openrouter-prototype.json` demonstrates the
  end-to-end flow: a small Python sidecar polls
  `https://openrouter.ai/api/v1/auth/key` (which returns USD floats
  and a `is_free_tier` boolean), scales to integer cents, and emits
  the tray's parser-shaped JSON. Run as
  `llm-quota-tray --instance=openrouter-prototype` alongside the
  default instance. The existing `examples/providers/openrouter.json`
  (token-usage probe) is unchanged.

### Notes (slice 1)
- This is a **prototype slice**, not a finished feature. The
  `currency` field is display-only metadata in v1 — `fmt_cost`
  always renders `$`. Extending to other currencies is a one-line
  change when there's a second currency to motivate it.
- No new fetches were added. The prototype uses the existing
  adapter/proxy pattern (same as `examples/providers/cohere.json`),
  which keeps the binary free of provider-specific fetch logic.
- No changes to the default MiniMax instance behavior; existing
  configs parse unchanged.

## [0.3.0] - 2026-08-21

First git tag on the repository. `Cargo.toml` had reached `version =
"0.2.0"` during the rename refactor (`4e93fad`); this tag marks the
first user-facing release and bundles the docs + provider-templates
work that landed on `main` through `c6ecad9`.

### Added
- **Provider templates** — 10 sample `config.json` files in
  `examples/providers/` for popular LLM providers (MiniMax, OpenAI,
  Anthropic, Mistral, Together, Groq, Cohere, Google AI Studio,
  OpenRouter, DeepSeek). Native-shape for MiniMax; partial / adapter
  patterns for the rest. The schema-drift guard
  (`config::tests::provider_templates_deserialize`) parses every
  template at build time and fails on missing fields or type errors
  (`c6ecad9`).
- **Multi-instance support** — a single binary serves as multiple
  concurrent tray icons via `--instance=<name>` (or
  `QUOTA_INSTANCE=<name>`). Each instance has its own config dir,
  lock file, keyring `application` attribute, and tray chip
  (`d978d87`, `ed1726e`, `e53b58b`).
- **N-window `PlanShape`** — the parser reads an arbitrary number of
  quota windows from a single payload (`Vec<WindowShape>`). The first
  window drives the chip; subsequent windows render as menu rows
  (`e53b58b`, `762e60c`).
- **Per-provider ring colors** — `RingColors` split into `inner`
  (status dot: normal/warning/throttled) and `outer` (percentage-fill
  arc). Legacy flat-shape configs still parse (`ff211e3`).
- **Config-driven provider abstraction** — `AuthConfig` (bearer /
  header / custom / query_param), `ErrorEnvelope`, unit multipliers,
  and reset semantics (duration vs. absolute epoch) are all
  config-driven. No provider constants live in source
  (`ed1726e`, `762e60c`).
- **SVG cache invalidation on color change** — `icon::write_ring_svg`
  now invalidates the `${TMPDIR}/llm-quota-tray-*.svg` files when
  `ring_colors` change between polls (`e0d3886`).
- **Documentation set** — `docs/architecture.md`, `docs/config-schema.md`,
  `docs/development.md`, `docs/gjs-parity.md`, `docs/port-guide.md`,
  and this `CHANGELOG.md`. The README's Tests section now reflects
  the actual Rust test surface (~155 unit + 2 ignored integration
  tests) and links to the new docs.

### Changed
- **Package rename** ⚠️ breaking — `Cargo.toml` `[package].name` is
  now `llm-quota-tray` (was `minimax-quota-tray`); all user-visible
  paths, keyring attributes, and the `LLM_API_KEY` env var follow
  (`4e93fad`, `13dc789`). Migration: rename your config dir from
  `~/.config/minimax-quota-tray/` to `~/.config/llm-quota-tray/` (or
  use a fresh install).
- **Icon rasterizer** — `tiny-skia` replaces `resvg`/`usvg` for the
  live chip. PNG encoding is **disabled** in `Cargo.toml` (raw ARGB
  bytes go straight to SNI `IconPixmap`), dropping `png`, `flate2`,
  `miniz_oxide`, `fdeflate`, `crc32fast`, `simd-adler32`, `adler2`
  from the build tree. Net binary size ~5.5 MB
  (`529eeba`).
- **Icon source** — `IconName` points at an SVG file under `$TMPDIR`
  (per-host loader support) with the `IconPixmap` ARGB bytes as the
  always-set fallback. Hosts without an SVG loader
  (e.g. Fedora Atomic) still render via the pixmap path
  (`6adec5c`, `921f643`).
- **Chip bucket semantics** — burn-rate projection can flip the chip
  to warning independently of the remaining-% thresholds (matches gjs
  `bucketForChip`); no red ring on remaining % alone
  (`4bdd1c2`, `11f4fa5`, `3571733`).
- **SNI interface** — fields exposed as D-Bus properties (not
  methods); the dbusmenu path is `/Menu` (was `/NO_DBUSMENU` sentinel,
  which blocked the AppIndicator extension's `ready` signal)
  (`cc51874`, `cc95e72`).
- **BGRA byte order** — `IconPixmap` uses host-endian BGRA, not
  spec-literal ARGB (`ad20133`).
- **SNI bus acquisition** — `sni.rs` acquires the bus name correctly;
  verified working on a host panel (`1e3749b`).
- **Adaptive polling + jitter** — `refresh_seconds` baseline;
  `refresh_seconds / 2` below yellow, `/ 4` below red; exponential
  backoff up to `refresh_max_backoff_seconds` after consecutive errors
  (`c417ed6`, `a53544f`).
- **Stale-on-error** — the tray keeps the last good data on error and
  annotates "stale by N min" in the menu row (`a53544f`).

### Fixed
- **Keyring write panic** — the `secret-service = "3"` crate's sync API
  internally calls `zbus::utils::block_on(...)`, which panics with
  `"Cannot start a runtime from within a runtime"` when invoked from
  inside a tokio worker thread. The user's "key doesn't stick" bug
  (daemon crash → systemd restart 5s later → no key written) is
  fixed by shelling out to `secret-tool(1)` instead. Per-call cost
  ~20–50ms vs. 120s baseline cadence (`d078df4`).
- **SVG cache staleness** — the icon SVG under `$TMPDIR` is now
  re-written when `ring_colors` change between polls (`e0d3886`).
- **Dashboard URL routing** — fixed to point at `/console/plan`
  (`b34db9b`).

### Removed
- **gjs implementation** — the GNOME Shell JavaScript extension
  (`llm-quota-tray@…`) is retired. Preserved on the `gjs/` branch for
  history. The Rust port on `main` is the only supported
  implementation.

## Pre-tag history (0.1.0 → 0.2.0 → 0.3.0)

No git tags existed before this release. The `Cargo.toml` `version`
field tracked commits:

- **`0.1.0`** — initial commit `0a15152` (2026-08-10). Standalone
  GNOME Shell JavaScript tray indicator for the MiniMax quota API.
  Subsequent commits through the gjs era added adaptive polling,
  stale-on-error, and the burn-rate projection.
- **`0.2.0`** — version reached during the rename refactor `4e93fad`
  (2026-08-21). No git tag was cut; the rename is the breaking change
  that prompted `0.2.0`.
- **`0.3.0`** — this tag. Bundles the docs and provider-templates work
  that landed on `main` after the rename.

The conventional-changelog commits across the Rust port era
(`6f2ecbf` … `c6ecad9`) are listed inline above by short SHA so the
provenance is traceable without forcing a tag-by-tag breakdown.

[Unreleased]: https://github.com/dtg01100/llm-quota-tray/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/dtg01100/llm-quota-tray/releases/tag/v0.3.0

[Keep a Changelog]: https://keepachangelog.com/en/1.1.0/
[Semantic Versioning]: https://semver.org/spec/v2.0.0.html