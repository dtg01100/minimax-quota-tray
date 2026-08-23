# Changelog

All notable changes to `llm-quota-tray` are documented here. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added (slice 4: freedesktop.org integration)

- **`packaging/llm-quota-tray.desktop`** — Desktop Entry
  (Type=Application, Exec=`llm-quota-tray`, Icon=llm-quota-tray,
  Categories=Network;Monitor;, StartupNotify=true,
  StartupWMClass). Passes `desktop-file-validate`. Provides the
  shell-launchable identity that the tray has been missing —
  visible in `gnome-software`, KDE Discover, the autostart
  dialog, and the file manager's "Open With" menus.
- **`packaging/io.github.dtg01100.llm-quota-tray.metainfo.xml`** —
  AppStream metadata (summary, description, URLs, MIT license,
  Categories, developer, OARS content rating, `launchable` link
  to the `.desktop` file). Passes `appstreamcli validate`. No
  `<screenshots>` / `<releases>` blocks — keeping the CHANGELOG
  as the single source of truth for release notes.
- **`packaging/icons/source/llm-quota-tray.svg`** — master SVG
  (256×256 viewBox); not installed directly but is the
  regeneration source for every hicolor PNG. A ring with center
  dot, anchored at 12 o'clock and sweeping 240° clockwise to
  8 o'clock — the same 12-o'clock start the chip uses
  (`icon::write_ring_svg`'s `transform="rotate(-90 11 11)"`),
  so launcher and chip read as the same family at every size.
  The 120° gap sits in the upper-left quadrant. Drawn in the
  project's default outer accent color (`#3584e4` / `#1c71d8`);
  the missing-third silhouette doubles as a quota-meter metaphor
  that distinguishes the launcher (always 2/3 by design) from
  the chip (a complete ring at 100%).
- **`src/activation.rs`** — new module. Reads the XDG Activation
  token from `--token=<token>` CLI flag or `$XDG_ACTIVATION_TOKEN`
  env var (in that order), stores it in a `OnceLock`, and
  exposes it via `activation::get()`. The two portal call sites
  (`portal_openuri::open`, `notify::send`) now accept an
  `Option<&str>` activation token and forward it as the
  `activation_token` vardict key. The direct-Notifications
  fallback ignores the token (the spec field doesn't exist).
  9 new unit tests cover CLI parsing (`--token=`, `--token <v>`,
  empty, absent, mixed with other flags), precedence
  (CLI > env), and the resolve() helper.
- **`install.sh`** — now copies the three metadata files into
  `$XDG_DATA_HOME/{applications,appdata,icons/hicolor/scalable/apps}`
  (respecting `XDG_DATA_HOME` per the XDG Base Directory Spec),
  and runs `update-desktop-database` + `gtk-update-icon-cache`
  best-effort so the new entries become visible to the desktop
  shell immediately.
- **`docs/freedesktop-integration.md`** — documents the
  integration contract: which freedesktop specs are targeted
  (Desktop Entry, AppStream, Icon Theme, XDG Activation, Desktop
  Portal), what each metadata file does, the multi-instance model
  the `.desktop` represents, and what's deliberately **not**
  done (DBusActivatable, Actions, IconThemePath, GSettings) —
  each documented as a future-work item with the trigger that
  would justify adding it.

### Notes (slice 4)
- **Lightweight charter preserved.** The activation-token plumbing
  adds ~130 lines to the codebase (one new module + two
  parameter-list changes). Binary size unchanged (4.4 MB). No new
  runtime dependencies, no new D-Bus interfaces, no new crates.
  The three metadata files total 3,195 bytes on disk — well
  under any meaningful footprint threshold.
- **Multi-instance semantics.** One canonical `.desktop` file for
  the default instance; named instances follow the existing
  systemd-service-template pattern (copy the service unit, add
  `--instance=<name>`). No per-instance `.desktop` files are
  generated automatically — matches the Firefox multi-profile
  / Docker multi-container convention.
- All 197 unit tests pass (was 188; +9 new for activation
  parsing/resolution).

### Changed

- **Launcher icon orientation** — rotated the static arc to
  anchor at 12 o'clock and grow clockwise 240° to 8 o'clock,
  the same geometry the tray chip uses (`icon::write_ring_svg`'s
  `transform="rotate(-90 11 11)"`). The 120° gap now sits in
  the upper-left quadrant between the arc's 8-o'clock terminus
  and the 12-o'clock start. Launcher and chip now read as the
  same family at every size — the launcher silhouette is "always
  2/3 fill" by design, the chip is its "live" version with the
  progress arc growing out of 12 o'clock. Source:
  `packaging/icons/source/llm-quota-tray.svg`, all nine hicolor
  PNGs regenerated.

### Fixed (freedesktop.org integration follow-ups)

- **`packaging/llm-quota-tray.desktop`** — added the missing
  `Icon=llm-quota-tray` field (the icon was installed but never
  referenced, so the launcher entry showed the generic
  placeholder), widened `Categories` from `Network;` to
  `Network;Monitor;` (one main + one additional, per the Desktop
  Entry spec), and dropped `%u` from `Exec=` (the binary doesn't
  accept URL arguments).
- **`uninstall.sh`** — now removes the three freedesktop metadata
  files installed by `install.sh` (`.desktop`, `.metainfo.xml`,
  icon) and reruns `update-desktop-database` /
  `gtk-update-icon-cache` so stale entries don't linger in the
  launcher cache after uninstall.
- **GitHub URL** — the repo URL in `CHANGELOG.md` link
  references, `README.md` clone command, `RELEASING.md` clone
  command, and both `<url>` blocks in the metainfo was the stale
  `dtg01100/llm-quota-tray` name; now points at
  `dtg01100/minimax-quota-tray` (matching the git remote).
  `appstreamcli validate` previously warned both URLs as
  `url-not-reachable`.
- **App icon palette** — the launcher icon
  (`packaging/icons/hicolor/scalable/apps/llm-quota-tray.svg`)
  previously used the throttled-bucket color (`#e01b24`), which
  reads as "something is wrong" anywhere outside the tray itself
  (autostart UI, "Open With" menu, app drawer). Replaced with a
  ring + center dot in the project's default outer accent color
  (`#3584e4` / `#1c71d8`), with the top 1/3 missing as a
  quota-meter metaphor — same family as the chip, neutral
  palette, distinct silhouette.
- **Launcher icon: SVG → PNG-only install set.** Originally shipped
  as a single SVG under `scalable/apps/` (the canonical modern
  best-practice), with three PNG fallbacks. The Icon Theme Spec
  says the launcher always prefers a scalable variant when one
  exists, so on hosts without a registered
  `libpixbufloader-svg.so` (Linuxbrew-based hosts and immutable
  distros like Bluefin / Fedora Atomic / Silverblue, which ship
  `librsvg` but not the gdk-pixbuf SVG loader), loading the SVG
  fails silently and the launcher entry shows blank. The new
  install set is **PNGs only, at every common launcher size**
  (16, 22, 24, 32, 48, 64, 96, 128, 256) so the launcher always
  finds a file the loader can render at the size it asks for.
  The master SVG moves to `packaging/icons/source/llm-quota-tray.svg`
  (regeneration source for the PNGs, not installed). Regenerated
  the PNGs at 8-bit color depth after a first pass produced
  16-bit/color RGBA — `gdk-pixbuf` caps at 8-bit and silently
  failed to render the 16-bit variant.
- **XDG autostart wiring** — `install.sh` now also copies the
  `.desktop` to `~/.config/autostart/llm-quota-tray.desktop`,
  wiring up the GNOME Settings "Startup Applications" / KDE
  Autostart toggle. The systemd user service remains the
  canonical boot path (gives us `Restart=on-failure` and
  `After=graphical-session.target`); the autostart entry is a
  parallel UI affordance. Double-firing at login is safe — the
  daemon's per-instance PID lock (`src/lock.rs`) detects the
  live second instance and exits. `uninstall.sh` removes both
  the autostart copy and the systemd symlink in
  `default.target.wants/`.
- **SNI signal emissions are now bounded.** `Tray::update()` and
  `Tray::apply_menu()` route every D-Bus signal emission
  (`NewIcon`, `NewTitle`, `NewStatus`, `ItemsUpdated`,
  `LayoutUpdated`) through a new `emit_signal_with_timeout()`
  helper with a 5-second budget. The chip state in
  `SharedState` is updated before the signal fires, so a missed
  signal only delays the panel's view by one poll cycle — it
  never deadlocks the daemon. The `RegisterStatusNotifierItem`
  call at `Tray::new()` time gets the same treatment. Fixes the
  `render_initial` hang we observed after a back-to-back restart
  (the watcher was still cleaning up the previous daemon's SNI
  registration; the new daemon's `new_icon` call blocked
  indefinitely). Three unit tests (`emit_signal_with_timeout_*`)
  lock in the contract.

### Added (slice 3: freedesktop-portal migration)

- **`src/portal_openuri.rs`** — new module. `open(uri)` calls
  `org.freedesktop.portal.OpenURI.OpenURI(parent_window, uri, options)`
  via the session bus. Spec at
  <https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.OpenURI.html>.
- **`src/notify.rs`** — `send()` now tries the
  `org.freedesktop.portal.Notification.AddNotification(id, vardict)`
  portal first. The portal `id` argument replaces the previous
  `x-canonical-private-synchronous` libnotify-server hint: it's
  canonical, documented, and honored by every portal backend.
  `Urgency::to_priority()` maps the libnotify-shaped urgency
  (`Low` / `Normal` / `Critical`) to the portal's `priority`
  vardict (`low` / `normal` / `urgent`).
- Both portal paths have a direct D-Bus fallback
  (`xdg-open(1)` for `OpenURI`, raw `org.freedesktop.Notifications.Notify`
  for the notifications) so the tray still works on hosts without
  `xdg-desktop-portal` running (headless CI, minimal WMs).

### Removed

- **`prompt_with_secret_tool` (main.rs)** — the third fallback in
  the `Set API Key…` interactive flow. It spawned `sh -c` with
  the user-controlled instance name interpolated into a shell
  string; the in-source SAFETY comment already flagged this as a
  footgun. The remaining fallbacks (`zenity`, `kdialog`) cover the
  GUI cases; the documented terminal escape hatch
  (`secret-tool store --label=<label> application <app>`) is
  preserved in the final error message. `libsecret-tools` is no
  longer a runtime dependency for `--set-key`.

### Notes (slice 3)
- **Scope deliberately limited.** SNI (StatusNotifierItem) and
  Secret Service have no portal above them — SNI is itself a
  freedesktop protocol, and `org.freedesktop.portal.Secret` is an
  opaque per-app master-secret API (not a keyring replacement).
  `keyring.rs`, `sni.rs`, and `lock.rs` are unchanged. `network.rs`
  is unchanged: the `NetworkMonitor` portal is a thin wrapper over
  NM with no capability win for a non-sandboxed tray.
- **Two new unit tests** (`portal_openuri::tests`): the spec-pinned
  constants and a compile-time signature pin so a future arg-tuple
  drift surfaces as a build failure, not a runtime panic at the
  portal boundary.
- All 188 unit tests pass (was 184; +4 new for the portal paths).

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

[Unreleased]: https://github.com/dtg01100/minimax-quota-tray/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/dtg01100/minimax-quota-tray/releases/tag/v0.3.0

[Keep a Changelog]: https://keepachangelog.com/en/1.1.0/
[Semantic Versioning]: https://semver.org/spec/v2.0.0.html