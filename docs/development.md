# Development

How to build, test, and modify this codebase. If you're fixing a bug or
adding a feature, start here.

## Build profiles

Defined in `Cargo.toml`:

| Profile           | What it does                                         | When to use                         |
|-------------------|------------------------------------------------------|-------------------------------------|
| `release`         | `opt-level = "z"`, `lto`, `codegen-units = 1`, `strip`, `panic = "abort"` | The production build (`install.sh` uses this) |
| `release-debug`   | Inherits release, `strip = false` (keeps symbols)    | When you need a backtrace from a release-config bug |
| `dev`             | Default debug                                        | Day-to-day `cargo test` work        |

```sh
cargo build --release                  # ~4.4 MB stripped binary
cargo build --profile release-debug    # bigger but with symbols for `addr2line`
cargo build                            # debug; fastest to compile
```

The release profile is aggressive on purpose — see
[`docs/architecture.md`](architecture.md) for the RSS target (~10 MB
resident) and the dependency list (no GTK, no libappindicator, only
libc + libsecret + libdbus).

## Tests

The test surface is **261 unit tests across 19 modules** (was 212 at
the previous audit; +49 added across the SNI-recovery + doc-tests +
clippy-hardening rounds since), plus 2 ignored integration tests. Run
them all:

```sh
cargo test                             # unit tests only — fast, no D-Bus needed
cargo test -- --ignored                # + integration tests (need session D-Bus)
cargo test config::tests::provider_templates_deserialize  # the schema-drift guard alone
# (no `cargo test --doc` — this is a binary-only crate)
```

### Test counts per module (current)

| Module              | Tests | Notes                                            |
|---------------------|-------|--------------------------------------------------|
| `main.rs`           | 30    | Bucket rank transitions, AppState, do_refresh branching, env-var precedence |
| `icon.rs`           | 28    | Ring rendering, ARGB pixmap, bucket selectors, SVG caching |
| `fetch.rs`          | 26    | Auth dispatch, query-param rewriting, error sanitization, header-builder matrix |
| `util.rs`           | 25    | Time + format helpers (the menu label formatters) + 5 `///` doc-examples |
| `burn.rs`           | 18    | `slope_per_hour`, `compute_burn`, `decide_burn_row`, pct-only suppression |
| `provider.rs`       | 17    | `AuthConfig::apply_to_endpoint`, default schemas, RingColors / PlanShape / AuthConfig builders |
| `pricing.rs`        | 14    | Per-model price lookup, `cost_per_hour` formatting |
| `parse.rs`          | 14    | `parse_plan` against canonical + legacy shapes + count_unit/currency/pricing_model_path |
| `config.rs`         | 13    | Includes the schema-drift guard for `examples/providers/*.json` |
| `menu.rs`           | 13    | dbusmenu tree construction, MenuCommand dispatch |
| `keyring.rs`        | 11    | `secret_to_key` trimming, env-var precedence + 2 ignored D-Bus tests |
| `sni.rs`            | 10    | SNI property dispatch + watcher-recovery task + emit_signal_with_timeout contract |
| `activation.rs`     | 9     | CLI parsing (`--token=`, `--token <v>`, empty, absent, mixed), precedence |
| `scheduler.rs`      | 8     | `next_interval` adaptive + backoff                |
| `notify.rs`         | 7     | Urgency byte mapping + Send + Sync + portal vs fallback dispatch |
| `instance.rs`       | 6     | CLI flag resolution, basename derivation         |
| `portal_openuri.rs` | 6     | Spec constants + signature pin + portal vs fallback selection |
| `lock.rs`           | 5     | Acquire + stale takeover + Drop + cross-process PID validation |
| `network.rs`        | 5     | NetEvent equality + StateChanged → NetEvent mapping |
| **Total**            | **261** | (was 212 at the previous audit; +49 since)        |

### Integration tests (`tests/integration.rs`)

Two tests, both `#[ignore]`'d because they need a session D-Bus:

- `binary_starts_under_session_dbus` — spawn the release binary, wait
  2s, kill, assert stderr contains the startup line. Headless smoke.
- `rss_under_target` — spawn, read `/proc/<pid>/status`, assert
  `VmRSS < 20 MB`. Regression guard against accidentally re-adding a
  heavy dep (libgtk, etc.).

Run with:

```sh
cargo test --test integration -- --ignored --nocapture
```

### What the README used to describe (and why this doc exists)

The README's Tests section previously described
`tests/regression-scheduler.test.js` and `./tests/run.sh` — those
existed in the **gjs** implementation and were retired with it. The
gjs test matrix (T18 = weekly rate from weekly samples, T19 = 5h
rollover doesn't clear weekly history, T21 = pct-only suppression)
maps to the **Rust** `#[test]` functions in `src/burn.rs` and
`src/main.rs` — see the per-test names below for the new locations.

| Old gjs test     | Where it lives now                                                |
|------------------|-------------------------------------------------------------------|
| T18              | `src/burn.rs::tests::rate_is_computed_from_window_specific_samples` (in the burn tests module) |
| T19              | `src/burn.rs::tests::weekly_history_survives_5h_rollover`          |
| T21              | `src/burn.rs::tests::pct_only_window_with_no_signal_suppresses_row`|
| `regression-scheduler.test.js` Part A/B | `src/main.rs::tests` (orchestrator + scheduler tests under a stubbed clock) |

If you're looking for a particular gjs test and can't find it,
grep `cargo test -- --list` and check the Rust test names — many were
preserved with the same intent.

### Manual smoke test

```sh
# Build + launch with a one-time key entry
cargo build
./target/debug/llm-quota-tray --set-key
# → enter your API key, then tray appears with the chip

# Trigger a fresh refresh from the menu (right-click → Refresh)
# Or from the CLI: click the chip, choose "Refresh now"

# Quit, check the lock was released
ls "$XDG_RUNTIME_DIR/llm-quota-tray.pid"  # → file gone
```

### Pre-commit checklist

1. `cargo build --release` succeeds (no new warnings).
2. `cargo test` green (~261 tests).
3. `cargo test -- --ignored` green if you changed the integration tests.
4. If you touched `examples/providers/*.json`, `cargo test config::tests::provider_templates_deserialize` green.
5. If you added a config field with a non-default default, add a
   round-trip test in `src/config.rs::tests`.

## How to add a new provider template

The minimum change is one file in `examples/providers/`:

```sh
cp examples/providers/minimax.json examples/providers/<name>.json
$EDITOR examples/providers/<name>.json
cargo test config::tests::provider_templates_deserialize
```

Fill in:

- `endpoint`, `dashboard_url`, `label`
- `shape.entries_path` — JSON pointer to the entry array
- `shape.windows[]` — at least one entry, see `docs/config-schema.md`
- `ring_colors` — pick a palette that doesn't clash with your other trays
- `auth` — `bearer` / `header` / `custom` / `query_param` per the provider's API docs

Add `_comment*` keys to document non-obvious field choices (see
`examples/providers/minimax.json` for the template style). The schema
drift guard catches missing required fields.

Then commit and tell users:

```sh
mkdir -p ~/.config/llm-quota-tray-<name>
cp examples/providers/<name>.json ~/.config/llm-quota-tray-<name>/config.json
llm-quota-tray --instance=<name> --set-key
```

## How to add a new auth style

This **requires** a code change. Edit `src/provider.rs`:

1. Add a variant to `AuthConfig` (`provider.rs:185`).
2. Add the dispatch case to `AuthConfig::build` (`provider.rs:216`).
3. If it needs URL rewriting, add it to `apply_to_endpoint` (`provider.rs:232`).
4. Add tests to `src/provider.rs::tests` or the relevant module.
5. Add an example to `examples/providers/README.md`'s "AuthConfig" section.

Then add a template in `examples/providers/` that exercises the new
variant.

## How to add a new icon shape

The icon is rendered by `src/icon.rs` (1,189 lines, 28 tests). Three
entry points:

- `icon::render_pixmap(pct, bucket, &ring_colors)` — returns the live
  ARGB bytes for SNI `IconPixmap`.
- `icon::write_ring_svg(pct, bucket, &ring_colors)` — writes a path
  under `$TMPDIR` for SNI `IconName`.
- `icon::static_svg_path(name, &ring_colors)` — for the static
  throttled/warning glyphs.

If you want a different rendering (e.g. solid bar instead of ring),
add a new module under `src/icon_*.rs` and route from `do_refresh`'s
icon-selection block (`main.rs:469`).

The `Bucket` enum (`main.rs:465` call site) decides which rendering
path to use. Currently `Throttled` uses a static glyph; the other two
use the live ring render. If your new shape wants to replace this
distinction, edit `main.rs:469` and add tests to `src/icon.rs::tests`.

## Debugging

### Logs

Set `RUST_LOG=debug` or `RUST_LOG=trace` for verbose output:

```sh
RUST_LOG=debug ./target/debug/llm-quota-tray
# → per-poll: history len, bucket transition, fetch latency

RUST_LOG=llm_quota_tray=trace,keyring=debug ./target/debug/llm-quota-tray
# → just our crate + keyring at trace level
```

The default is `info` — set in `main.rs:114`.

### D-Bus introspection

While the tray is running:

```sh
# Watch SNI property updates
dbus-monitor --session "type='signal',interface='org.kde.StatusNotifierItem'"

# Watch dbusmenu events (menu clicks)
dbus-monitor --session "type='signal',interface='com.canonical.dbusmenu'"

# Inspect the SNI object directly
gdbus call --session --dest org.freedesktop.DBus \
  --object-path /StatusNotifierItem \
  --method org.freedesktop.DBus.Properties.GetAll \
  org.kde.StatusNotifierItem
```

### Backtraces from a release-config bug

Build with `release-debug` (keeps symbols, panics still abort):

```sh
cargo build --profile release-debug
RUST_BACKTRACE=1 ./target/release-debug/llm-quota-tray
```

Symbols are preserved by the absence of `strip` in
`[profile.release-debug]`, so `addr2line`/`rustfilt` produce useful
output.

### Inspect the static SVG cache

```sh
ls -la "${TMPDIR:-/tmp}/llm-quota-tray-*.svg"
```

Three files at startup, one per bucket + the throttled static. They
get overwritten when `ring_colors` change (cache invalidation: see the
recent `fix(icon): invalidate SVG cache when ring_colors change`
commit).

### Reset everything (last resort)

```sh
# Stop the service
systemctl --user stop llm-quota-tray.service

# Wipe PID file (if stale) and config
rm -f "$XDG_RUNTIME_DIR/llm-quota-tray.pid"
rm -rf ~/.config/llm-quota-tray

# Wipe keyring entry (GNOME Keyring)
secret-tool clear application llm-quota-tray

# Rebuild + reinstall
cargo build --release
./install.sh
```

## Module ownership guide

If you're hunting for a piece of behavior:

| Behavior                                | Module(s)                                              |
|-----------------------------------------|--------------------------------------------------------|
| Tray icon (SNI)                         | `sni.rs`                                               |
| Menu tree (dbusmenu)                    | `menu.rs` + the menu-related signals in `sni.rs`       |
| Icon rendering (ARGB + SVG)             | `icon.rs`                                              |
| HTTP fetch                              | `fetch.rs`                                             |
| JSON → `Vec<Window>`                    | `parse.rs`                                             |
| Provider-agnostic types                 | `provider.rs`                                          |
| `Config` loading + defaults             | `config.rs`                                            |
| Burn-rate projection                    | `burn.rs`                                              |
| Poll cadence (adaptive + backoff)       | `scheduler.rs`                                         |
| Single-instance lock                    | `lock.rs`                                              |
| Multi-instance namespace                | `instance.rs`                                          |
| Keyring read/write                      | `keyring.rs`                                           |
| NetworkMonitor StateChanged watch       | `network.rs`                                           |
| Threshold notifications                 | `notify.rs`                                            |
| Subsystem wiring + lifecycle            | `main.rs`                                              |
| Small helpers                           | `util.rs`                                              |

See [`docs/architecture.md`](architecture.md) for the call graph
between these. For a per-module reference (purpose, public API, key
behaviors of each `src/*.rs`), see [`docs/modules.md`](modules.md).