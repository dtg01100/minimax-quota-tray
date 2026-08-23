# Logging

How to read, filter, and route the daemon's logs.

## Quick start

```sh
# Default — INFO and above from all modules, stderr.
journalctl --user -u llm-quota-tray.service -f

# Debug-level for the SNI module only.
RUST_LOG=llm_quota_tray::sni=debug systemctl --user restart llm-quota-tray.service
journalctl --user -u llm-quota-tray.service -f

# Verbose across the board (network reconnect chatter, watcher
# recovery, keyring SearchItems, etc.).
RUST_LOG=debug systemctl --user restart llm-quota-tray.service
```

`RUST_LOG` is the standard [`env_logger`](https://docs.rs/env_logger)
syntax: `target[=level][,target[=level]]*`. Default if unset
or invalid is `info` (set in
[`src/main.rs`](../src/main.rs) via `default_filter_or("info")`).

## Format

`env_logger` defaults — `2026-08-23T17:35:07Z WARN  llm_quota_tray::sni SNI signal NewIcon: emission failed: ...`

- ISO-8601 timestamp (`%Y-%m-%dT%H:%M:%S%z`)
- Level (`ERROR`, `WARN`, `INFO`, `DEBUG`, `TRACE`) in a fixed-
  width column
- Module path (no logs are emitted from anonymous functions or
  `fn main`'s body directly — they're tagged with the calling
  module)
- Message

Timestamps are UTC regardless of the host's timezone (deliberate
— log lines from multiple hosts correlate trivially).

## Per-module guide

A short tour of what gets logged at what level so you know
what crank to turn when chasing a specific symptom. Counts are
the static `log::*!` invocation count in each module as of the
last audit.

| Module | INFO | WARN | DEBUG | When you'll see it |
|---|---|---|---|---|
| `llm_quota_tray` (main / orchestrator) | 11 | 11 | 5 | Lifecycle: `instance: ""`, `started:`, `fetching pricing endpoint`, `loaded N model prices`, `API key updated via menu`, `SIGTERM, exiting`, `ctrl-c, exiting`. Warnings: HTTP retry exhaustion, set-key failures, menu-channel close, polling drift. |
| `llm_quota_tray::sni` | 1 | 4 | 5 | One INFO on `SNI watcher re-appeared on …` (the recovery fix). Warnings on every signal-emission timeout / broken pipe. Debug for explicit-registration failures and the recovery-task lifecycle. **If you see a steady stream of `SNI signal NewIcon: emission failed: Broken pipe` every refresh cycle, the watcher has restarted without the recovery hook firing — restart the service to clear it.** |
| `llm_quota_tray::keyring` | 1 | 0 | 8 | INFO once at startup if a **legacy-attribute** key is found (with a one-line `llm-quota-tray --set-key` migration hint). Debug for every D-Bus call against `org.freedesktop.secrets` (proxy build, SearchItems, OpenSession, Unlock, GetSecret). |
| `llm_quota_tray::network` | 0 | 0 | 3 | Debug for NM `StateChanged` reactions and ForceRefresh plumbing. Silent at INFO. |
| `llm_quota_tray::notify` | 0 | 0 | 2 | Debug for notification dispatch decisions. Silent at INFO. |
| `llm_quota_tray::icon` | 0 | 2 | 0 | Warn on disk-write failures (TMPDIR full / read-only). Silent at INFO. |
| `llm_quota_tray::config` | 1 | 1 | 0 | Info on default-config write at first-run. Warn on parse / IO errors (falls back to defaults). |

Modules not in the table above have no `log::` calls today —
they're silent at every level. They're either pure-functional
(`util`, `parse`, `provider`, `pricing`, `scheduler`, `menu`,
`activation`) or one-shot startup code with no failure modes
worth logging (`lock`, `instance`).

## Reading the journal

Useful queries:

```sh
# Last 50 lines, no pager, for copy-pasting into bug reports.
journalctl --user -u llm-quota-tray.service -n 50 --no-pager

# Only warnings and errors since boot.
journalctl --user -u llm-quota-tray.service -p warning --no-pager

# A specific time window (replace with whatever your distro
# accepts — `journalctl --since` and `--until` take most human-
# readable timestamps).
journalctl --user -u llm-quota-tray.service \
  --since "2026-08-23 13:00" --until "2026-08-23 13:30" --no-pager

# Live tail, in the foreground.
journalctl --user -u llm-quota-tray.service -f
```

## Common diagnostic recipes

**"The chip disappeared but the service is active"**
```sh
journalctl --user -u llm-quota-tray.service -n 100 --no-pager \
  | grep -E "SNI signal|watcher re-appeared|started"
```
A burst of `SNI signal NewIcon: emission failed: Broken pipe`
without a matching `watcher re-appeared` log line means the
recovery task isn't seeing the NameOwnerChanged signal —
restart the service to force a fresh SNI connection. (Should
not happen since the watcher-restart fix landed.)

**"The keyring keeps prompting me for unlock"**
```sh
RUST_LOG=llm_quota_tray::keyring=debug systemctl --user restart llm-quota-tray.service
journalctl --user -u llm-quota-tray.service -f
```
Look for `SearchItems` / `Unlock` failures — most often the
GNOME Keyring isn't running or the collection is locked because
the user logged out and back in without unlocking the keyring
ring first.

**"I set the key but the daemon keeps saying 'No API key configured'"**
```sh
RUST_LOG=llm_quota_tray=debug systemctl --user restart llm-quota-tray.service
journalctl --user -u llm-quota-tray.service -n 60 --no-pager \
  | grep -i "key\|secret"
```
The `--set-key` flow shells out to `secret-tool store` — look
for "secret-tool failed" lines and rerun `secret-tool store
application llm-quota-tray` by hand to see the literal error.

## What does NOT get logged

Deliberately, to keep logs un-spammy and un-sensitive:

- **API response bodies.** Even on 4xx/5xx, we log only the
  status code and a sanitized 80-char error snippet (see
  [`fetch.rs::sanitize_error_snippet`](../src/fetch.rs)). The
  raw JSON stays in memory only.
- **The API key itself.** Never logged, even in `trace!` (we
  don't have a `trace!` macro at all). The keyring attribute
  *name* (`application llm-quota-tray`) is logged; the *value*
  never is.
- **Per-window refresh data** (used/minute, projected runout)
  at INFO — it's already visible in the menu / chip. Set
  `RUST_LOG=llm_quota_tray::burn=debug` to see the raw sample
  appends.

## Stderr vs journal

The daemon writes to stderr (via `env_logger`'s default).
systemd captures stderr into the journal, so `journalctl -u
llm-quota-tray.service -f` always has the full stream. If you
run the binary directly (not under systemd), the same lines
go to your terminal.
