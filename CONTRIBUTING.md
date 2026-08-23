# Contributing

Thanks for considering a contribution. This project is intentionally
small — ~11,000 lines of Rust across 19 modules — so a single
PR can land quickly once you know the shape of the codebase.

## Where to start

* **Users installing or configuring** → start with
  [`README.md`](../README.md).
* **Developers new to the codebase** → read
  [`docs/architecture.md`](docs/architecture.md) first (subsystem
  map + request lifecycle), then [`docs/modules.md`](docs/modules.md)
  (per-module reference), then
  [`docs/gjs-parity.md`](docs/gjs-parity.md) **before changing
  anything that looks weird** — many of those "weird" decisions
  are load-bearing for specific distros / panel quirks.
* **Porting to a new provider** →
  [`docs/port-guide.md`](docs/port-guide.md). The "Simple" track
  is config-only (no Rust needed).
* **Cutting a release** → [`RELEASING.md`](RELEASING.md).

## Development environment

* **Rust 1.75+** (the `rust-version` in `Cargo.toml`). Most
  contributors use `rustup` so they can test against stable +
  MSRV easily.
* A graphical Linux desktop for testing — the daemon's whole
  purpose is rendering to a panel, so headless CI can build
  and unit-test but can't visually verify.
* `libdbus`, `libsecret` available at runtime (ubiquitous on
  Linux; not build deps).
* `update-desktop-database` and `gtk-update-icon-cache` if
  you want to verify a packaging change end-to-end (both
  optional — see `install.sh`).

## Build, test, lint

```sh
# Build.
cargo build --release           # production binary (used by install.sh)
cargo build                     # debug build (faster, larger, runnable)
cargo build --profile release-debug   # release with symbols, for backtraces

# Test.
cargo test                      # 261 unit tests, ~5s, no D-Bus needed
cargo test --release            # same, in release mode
cargo test -- --ignored         # + 2 integration tests (need session D-Bus)
cargo test config::tests::provider_templates_deserialize   # schema-drift guard

# Lint (the project uses defaults; no extra tooling required).
cargo doc --no-deps --release   # builds API docs; target is zero warnings
cargo clippy                    # standard rustfmt + clippy rules
cargo fmt --check               # formatting
```

`cargo doc` must produce zero warnings — the rendered docs.rs page
is part of the project's public surface. Bare URLs, unresolved
intra-doc links, and unclosed HTML tags are all errors.

## Project conventions

A few things this codebase does that aren't universal Rust defaults:

* **No `unwrap()` in production paths.** Test code can `unwrap`,
  production code should propagate `Result` and let `main()` turn
  it into a clean exit. There's a long comment in
  [`src/keyring.rs`](../src/keyring.rs) explaining why the keyring
  module shells out to `secret-tool` instead of linking
  `libsecret` directly.
* **Public functions are documented.** New `pub fn`s should come
  with a `///` block describing the behavior. The `# Errors` and
  `# Examples` sections are encouraged for functions that return
  `Result` or have user-visible effects.
* **gjs-parity decisions are load-bearing.** If you're tempted to
  "fix" something that looks weird, check
  [`docs/gjs-parity.md`](docs/gjs-parity.md) first.
* **Multi-instance correctness.** Every config file, PID lock,
  keyring entry, log line, and bus name must include the instance
  namespace. See [`docs/multi-instance.md`](docs/multi-instance.md).
* **No deps without rationale.** Every dependency in `Cargo.toml`
  has an inline comment explaining why it's there. Adding a dep?
  Add the comment too.

## Commit & PR style

* One logical change per commit. "Fix typo" + "refactor provider"
  in one commit makes `git bisect` impossible.
* Commit message subject line: `<area>: <what changed>`, lowercase,
  no trailing period. Examples that the history follows:
  * `fix(sni): re-register when SNI watcher restarts`
  * `docs: rustdoc hygiene, doc-examples in util, modules.md LOC refresh`
  * `test: add record_sample tests for epoch rollover + eviction invariants`
* Body explains **why**, not what. The diff shows what.
* Reference the issue number in the body if there is one.

## Adding a new provider

`Simple` track (preferred — no Rust changes):

1. Copy the closest existing template from
   [`examples/providers/`](../examples/providers/).
2. Fill in endpoint URL, auth style, parse plan. The schema is
   documented in [`docs/config-schema.md`](docs/config-schema.md).
3. Drop the file into `~/.config/llm-quota-tray-<name>/config.json`,
   set the API key, restart the service.
4. If the template works well for others, commit it back to
   `examples/providers/` so the next person to port the same
   provider has a head start.

`Hard` track (Rust changes for shapes the parse plan can't
express): see [`docs/port-guide.md`](docs/port-guide.md#hard-track).

## Pull request checklist

- [ ] `cargo test --release` is green.
- [ ] `cargo doc --no-deps --release` has zero warnings.
- [ ] `cargo clippy` is clean (or you've added a `#[allow(...)]`
      with an inline comment justifying it).
- [ ] If you changed user-visible behavior, `CHANGELOG.md`
      `[Unreleased]` section has an entry under the right slice.
- [ ] If you added a new public API, every `pub fn` / `pub struct`
      has a `///` block, and ideally an `# Examples` section.
- [ ] If you touched config schema, the
      `provider_templates_deserialize` test in `src/config.rs`
      still passes (it walks `examples/providers/` and validates
      every template).

## Reporting issues

Use the GitHub issue tracker. Include:

* The daemon version (`llm-quota-tray --version` — see
  [`docs/cli.md`](docs/cli.md)).
* The output of `journalctl --user -u llm-quota-tray.service -n 100
  --no-pager`.
* Your distro and panel (Fedora 41 + GNOME 47 with
  appindicator extension, Bluefin 44 + Wayland, KDE Plasma 6, etc.).
* A minimal `config.json` that reproduces the issue, with the
  API key redacted.

## Security

See [`SECURITY.md`](SECURITY.md) for the disclosure policy.
Don't file security issues as public bug reports — use the
contact channel there.
