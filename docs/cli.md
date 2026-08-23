# CLI reference

Every flag and env var `llm-quota-tray` accepts. The binary is a
single-purpose daemon — the CLI is small on purpose. The one
non-obvious surface is `--instance=<name>`, which namespaces the
config dir, lock file, keyring entry, and tray icon.

## Flags

### `--instance=<name>`

Run as a named instance instead of the default. Each instance has
its own config dir, lock file, and keyring `application` attribute,
so multiple instances never collide.

| | Default | `--instance=codex` |
|---|---|---|
| Config dir | `~/.config/llm-quota-tray/` | `~/.config/llm-quota-tray-codex/` |
| Lock file | `$XDG_RUNTIME_DIR/llm-quota-tray.pid` | `$XDG_RUNTIME_DIR/llm-quota-tray-codex.pid` |
| Keyring `application` | `llm-quota-tray` | `llm-quota-tray-codex` |
| Keyring `label` | `llm-quota-tray API Key` | `llm-quota-tray-codex API Key` |

See [`docs/multi-instance.md`](multi-instance.md) for the full
namespace rules and worked examples (systemd unit per instance,
.desktop file per instance, etc.).

**Equivalent env var**: `QUOTA_INSTANCE=<name>` (fallback if the
flag is absent).

### `--token=<token>` / `--token <token>`

XDG Activation token from the desktop shell. Forwarded to the
freedesktop portals (OpenURI, Notification) so the resulting
dialogs/notifications animate from the originating click. See
[`docs/freedesktop-integration.md`](freedesktop-integration.md)
for the full spec.

**Equivalent env var**: `$XDG_ACTIVATION_TOKEN`.

**Precedence**: `--token=<token>` flag > `$XDG_ACTIVATION_TOKEN`.
Empty tokens are treated as "no token" per the spec.

### `--set-key` / `--set_api_key`

One-shot helper: prompt for the API key (via a graphical `zenity` /
`kdialog` dialog), write it to the keyring via `keyring::set`,
print a status line, and exit. The daemon subsystems (lock, SNI,
refresh loop) never come up — this is purely a key-entry flow.

```sh
llm-quota-tray --set-key
# → enter your API key, hit Enter
# → "API key stored. Launch the tray normally to start polling."

llm-quota-tray --set-key --instance=codex
# → write to ~/.config/llm-quota-tray-codex/'s keyring entry
```

After the key is stored, the next normal `llm-quota-tray` launch
will find it in the keyring.

This flag is what `examples/providers/README.md` and the per-
provider templates mean by "Set the API key with `--set-key`".
Without it, the tray would boot, render "No API key configured",
and require a click through the menu to enter the key.

### `-h` / `--help`

Not implemented. The binary is small enough that this doc + the
README serve as the reference. If you actually need a `--help`,
file an issue.

### `-V` / `--version`

Not implemented. The version is in the User-Agent string sent on
every poll (`<user_agent>/<version>`).

## Environment variables

### `QUOTA_INSTANCE`

Fallback for `--instance=<name>` when the flag is absent. Same
namespace rules.

### `XDG_ACTIVATION_TOKEN`

Fallback for `--token=<token>`. The desktop shell writes this when
launching the tray via the `.desktop` file.

### `LLM_API_KEY`

API-key fallback when no Secret Service entry exists. The keyring
read order in `src/keyring.rs`:

1. Secret Service (current AND legacy `application` attribute).
2. `LLM_API_KEY` env var.

The env var is documented in [`SECURITY.md`](../SECURITY.md) as
the systemd escape hatch for environments without a keyring
(remote servers, headless CI). It's also useful as a quick way to
test a new config without first running `--set-key`.

### `RUST_LOG`

Standard `env_logger` filter. Default `info` (set in `main.rs`).
Useful levels:

```sh
RUST_LOG=debug ./target/debug/llm-quota-tray
# → per-poll: history len, bucket transition, fetch latency

RUST_LOG=llm_quota_tray=trace,keyring=debug ./target/debug/llm-quota-tray
# → just our crate + keyring at trace level
```

### `HOME`

Where the config dir lives. Tests mutate this so they can use a
temp dir; users typically don't need to set it.

### `XDG_RUNTIME_DIR`

Where the lock file lives. Default `/tmp` if unset (per the
freedesktop spec fallback).

### `TMPDIR`

Where the static SVG icons and per-pct ring SVGs live. Default
`/tmp`. The SNI `IconName` references the SVG under this dir; the
in-memory ARGB `IconPixmap` is the universal fallback.

### `DISPLAY` / `WAYLAND_DISPLAY`

Read implicitly by the host panel (not by the tray itself). The
systemd user unit uses `After=graphical-session.target` so the
service starts after the session is up — at which point the
session manager has propagated these env vars into the service's
environment.

## File-system surface

The daemon touches these paths per instance:

```text
$HOME/.config/llm-quota-tray[-<name>]/config.json   # config (load + write-on-init)
$XDG_RUNTIME_DIR/llm-quota-tray[-<name>].pid       # lock (O_EXCL create + Drop rm)
$TMPDIR/llm-quota-tray-ring-*.svg                   # per-pct ring icons
$TMPDIR/llm-quota-tray-static-{normal,warning,throttled,error,offline}.svg  # static icons
$XDG_DATA_HOME/applications/llm-quota-tray.desktop  # installed by install.sh
$XDG_DATA_HOME/appdata/io.github.dtg01100.llm-quota-tray.metainfo.xml
$XDG_DATA_HOME/icons/hicolor/<size>/apps/llm-quota-tray.png  # 8 sizes
$HOME/.config/autostart/llm-quota-tray.desktop      # GNOME/KDE autostart
$HOME/.config/systemd/user/llm-quota-tray.service   # systemd unit (and default.target.wants/ symlink)
```

The keyring entries are namespaced by `application = <instance>`
and labeled `<instance> API Key`.

## Exit codes

* `0` — clean shutdown (Quit menu, signal, or `--set-key` complete).
* `1` — fatal error during startup (logged via `env_logger` to
  stderr). Also returned by `--set-key` if the prompt or
  keyring write fails.
* The "another instance is already running" path returns `0`
  with a stderr line — by design (we'd rather a second launch be
  a no-op than a hard error for shell scripts).

## Signals

* `SIGINT` (`Ctrl-C` from a terminal) and `SIGTERM` (from
  `systemctl --user stop`) both trigger the orchestrator to return
  and the daemon to exit cleanly. The PID lock is released by the
  `Lock::Drop` impl.

## Argument parsing quirks

* The instance parser accepts `--instance=<name>` and `--instance
  <name>` forms. `--instance` at the end of argv with no value
  falls through to the env var / default.
* The activation-token parser accepts `--token=<value>` and
  `--token <value>` forms. Anything else (e.g. `--tokenized`) is
  left for other CLI handlers — matching too greedily would
  swallow unrelated flags.
* The two parsers are independent (different modules, different
  init order). `instance::init()` runs first (it namespaces the
  lock/config dir/keyring), then `activation::init()` runs.
