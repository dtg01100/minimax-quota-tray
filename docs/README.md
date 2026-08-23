# Documentation index

User and developer docs for `llm-quota-tray`. The README at the
repo root is the public-facing entry point; this directory holds the
deeper reference material.

## For users

| Doc | Purpose |
|---|---|
| [`README.md`](../README.md) | Overview, install, usage, multi-instance, troubleshooting |
| [`config-schema.md`](config-schema.md) | Every field in `config.json` with default, type, and behavior |
| [`multi-instance.md`](multi-instance.md) | Running multiple trays concurrently — namespace rules, lock files, systemd units |
| [`troubleshooting.md`](troubleshooting.md) | Common issues + diagnostic walkthroughs (icon missing, keyring, etc.) |
| [`faq.md`](faq.md) | Common "how do I…" questions — install, auth, config, tray integration, dev |
| [`../examples/providers/README.md`](../examples/providers/README.md) | Provider template catalog — when to use each shape, how to template a new one |
| [`../SECURITY.md`](../SECURITY.md) | Vulnerability disclosure policy |
| [`security-model.md`](security-model.md) | Threat model — assets, trust boundaries, network egress, supply chain, hardening |

## For developers

| Doc | Purpose |
|---|---|
| [`architecture.md`](architecture.md) | Subsystem map, request lifecycle, threading model, state machine |
| [`modules.md`](modules.md) | Per-module reference: every `src/*.rs` with purpose, public API, key behaviors |
| [`development.md`](development.md) | Build profiles, test counts, how to add providers/auth/icons |
| [`cli.md`](cli.md) | Every CLI flag + env var the binary accepts |
| [`burn-rate.md`](burn-rate.md) | The math behind the per-window burn-rate projection — slopes, epochs, suppression |
| [`freedesktop-integration.md`](freedesktop-integration.md) | Desktop Entry / AppStream / XDG Activation / Portal contracts |
| [`gjs-parity.md`](gjs-parity.md) | Decisions kept verbatim from the gjs port — load-bearing "don't change this" list |
| [`port-guide.md`](port-guide.md) | Porting to a new provider — Simple vs Hard tracks, sidecar patterns |
| [`logging.md`](logging.md) | Per-module log guide, `RUST_LOG` recipes, common diagnostic queries |
| [`performance.md`](performance.md) | RSS/CPU/polling budgets, scaling with multiple instances, profiling recipes |
| [`glossary.md`](glossary.md) | Single-page term index (bucket, shape, epoch, sentinel, …) with deep links |
| [`../RELEASING.md`](../RELEASING.md) | Release process: versioning, CHANGELOG discipline, tag signing, post-release checks |

## Cross-references

* [`../CHANGELOG.md`](../CHANGELOG.md) — every user-visible change since 0.1.0, grouped by
  feature slice (Keep a Changelog 1.1).
* [`../Cargo.toml`](../Cargo.toml) — the dependency manifest with comments
  explaining each non-obvious dependency choice.
* [`../install.sh`](../install.sh) — the install script with inline comments
  covering every file it copies and why.
* [`../packaging/`](../packaging/) — the freedesktop metadata files
  (`.desktop`, `.metainfo.xml`, hicolor PNG icons).

## Reading order suggestions

* **New user, just installed**: README → troubleshooting (only if
  something looks wrong) → multi-instance (only if you want a second tray).
* **Porting to a new provider**: port-guide → config-schema → examples/providers/README.
* **Contributing code**: development → architecture → modules → gjs-parity
  (so you know what NOT to "fix").
* **Cutting a release**: RELEASING → CHANGELOG.
