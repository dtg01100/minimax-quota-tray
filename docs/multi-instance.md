# Multi-instance guide

How to run two or more `llm-quota-tray` processes concurrently,
each as its own tray icon targeting a different API (or the same
API with different colors), and how to keep them running across
reboots.

This is the long-form companion to the README's "Multiple instances"
section. The README has the quick reference table; this doc has the
namespace rules in depth, the systemd unit template, the .desktop
file template, and the failure modes.

## Why multi-instance

The binary is fully generic — every provider-specific field lives
in the per-instance config. That makes "I want a tray for a
different API" a one-line invocation:

```sh
llm-quota-tray --instance=codex
```

No code fork needed. No separate binary needed. Two processes
with different `--instance=` flags have:

- Different config dirs (`~/.config/llm-quota-tray/` vs
  `~/.config/llm-quota-tray-codex/`).
- Different lock files (`$XDG_RUNTIME_DIR/llm-quota-tray.pid` vs
  `$XDG_RUNTIME_DIR/llm-quota-tray-codex.pid`).
- Different keyring `application` attributes (`llm-quota-tray` vs
  `llm-quota-tray-codex`).
- Different tray chips (different colors, different icons, different
  labels).
- Different `--set-key` flows (write to the right keyring entry).

## Namespace derivation

Every per-instance path is derived from one string: the instance
name. The default is `""` (empty); named instances append `-<name>`.

| Source of instance name | Priority |
|---|---|
| `--instance=<name>` CLI flag | 1 |
| `QUOTA_INSTANCE=<name>` env var | 2 |
| `""` (default) | 3 |

Derivation lives in `src/instance.rs`. Every namespace uses
`config_dir_basename()`:

```rust
if is_default() {
    "llm-quota-tray".to_string()
} else {
    format!("llm-quota-tray-{}", name())
}
```

Which then drives:

- `config_path_for(instance) → ~/.config/<basename>/config.json`
  (`src/config.rs`).
- `lock_path() → ${XDG_RUNTIME_DIR:-/tmp}/<basename>.pid`
  (`src/instance.rs`).
- `keyring_application() → <basename>` (used as the
  `application` attribute on Secret Service items — see
  `src/keyring.rs`).
- `label() → "<basename> API Key"` (the human-readable item label
  shown in `seahorse` / `ksecretservice-viewer`).

**Don't bake the provider name into the path**. The basename is
`llm-quota-tray` even for instances targeting a different API —
the per-instance dirs only differ in the `-<name>` suffix, which is
under your control. This keeps the codebase free of provider
references (see the gjs-parity doc for the "no provider branding
in source" rule).

## Setting up a second instance

Step-by-step for `codex`:

```sh
# 1. Create the per-instance config dir
mkdir -p ~/.config/llm-quota-tray-codex

# 2. Drop the config in (edit to match the provider)
cp examples/providers/<matching>.json \
   ~/.config/llm-quota-tray-codex/config.json
$EDITOR ~/.config/llm-quota-tray-codex/config.json

# 3. Store the API key in the right keyring entry
llm-quota-tray --instance=codex --set-key
# → enter the key, hit Enter

# 4. Launch the second tray
llm-quota-tray --instance=codex
# → second chip appears in your panel
```

Both processes now run concurrently:

```sh
ps aux | grep llm-quota-tray
# user  12345  ... llm-quota-tray                       (default instance)
# user  12346  ... llm-quota-tray --instance=codex     (codex instance)
```

## Pinning instances to systemd units

For a tray that survives reboots, each instance needs its own
systemd unit. The `llm-quota-tray.service` template ships in the
repo root. To create a second instance:

```sh
cp ~/.config/systemd/user/llm-quota-tray.service \
   ~/.config/systemd/user/llm-quota-tray-codex.service

$EDITOR ~/.config/systemd/user/llm-quota-tray-codex.service
```

Two edits:

1. Change `Description=` to identify the instance.
2. Add ` --instance=codex` to the end of `ExecStart=`.

The `examples/systemd/llm-quota-tray-codex.service` file is the
worked example.

Then:

```sh
systemctl --user daemon-reload
systemctl --user enable --now llm-quota-tray-codex.service
systemctl --user status llm-quota-tray-codex.service
```

A template you can sed-substitute:

```ini
[Unit]
Description=LLM Quota Tray Indicator (instance: %i)
After=graphical-session.target

[Service]
Type=simple
ExecStart=%h/.local/bin/llm-quota-tray --instance=%i
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
```

Saved as `llm-quota-tray@.service`, this template takes the
instance name as the systemd `%i` specifier. One template, many
instances:

```sh
systemctl --user enable --now llm-quota-tray@codex.service
systemctl --user enable --now llm-quota-tray@openai.service
```

The repo currently ships the non-templated form
(`llm-quota-tray.service` for the default instance); the template
above is a future enhancement.

## Pinning instances to .desktop files

The shipped `.desktop` file (`packaging/llm-quota-tray.desktop`)
launches the **default** instance. Named instances need their own
.desktop file if you want them in `gnome-software`, the
autostart dialog, or shell context menus.

For an instance you want to autostart on login but not enable in
the systemd sense, copy the `.desktop` and add the instance flag:

```sh
cp ~/.local/share/applications/llm-quota-tray.desktop \
   ~/.local/share/applications/llm-quota-tray-codex.desktop

$EDITOR ~/.local/share/applications/llm-quota-tray-codex.desktop
```

Two edits:

1. Change `Name=` to identify the instance.
2. Change `Exec=llm-quota-tray` to `Exec=llm-quota-tray --instance=codex`.
3. Optionally change `Icon=llm-quota-tray` to a different icon name
   to make the launcher entry visually distinct from the default
   tray's icon (not just a different chip).

This is the GNOME Settings "Startup Applications" / KDE Autostart
pattern. Both shells look for `.desktop` files in
`$XDG_CONFIG_DIRS/autostart` and `$XDG_CONFIG_HOME/autostart`.

## Collision scenarios

### Same `--instance=` from two terminals

The second launch fails immediately:

```
Lock::acquire → Ok(None)
fatal: another instance is already running
```

The lock file is instance-scoped, so this only conflicts with
itself. Different `--instance=` flags have different lock files
and don't collide.

### Stale PID file after a crash

`Lock::acquire` checks `/proc/<pid>` and takes over if the holder
is dead (`src/lock.rs:90`). On rare systems without `/proc`, the
stale lock persists; `rm` it manually:

```sh
rm "$XDG_RUNTIME_DIR/llm-quota-tray-codex.pid"
```

### Two `--set-key` flows at the same time

The `--set-key` flow doesn't acquire the single-instance lock —
it's a one-shot helper that exits before any daemon subsystem
comes up. Two simultaneous `--set-key` flows against the same
instance both write to the same Secret Service `application`
attribute. Last writer wins, no corruption.

### Two instances polling the same endpoint

If you point two instances at the same `endpoint`, both will poll
independently. The rate of inbound requests to the provider
doubles; if your provider has aggressive rate limiting, this can
trip a 429. Fix: use one instance per endpoint, or set a higher
`refresh_seconds` on the second instance.

## Namespacing the secret across instances

`keyring::get` searches the **current** `application` attribute
first; only falls back to the **legacy** `application` if no
current-attribute item is found. So two instances running
concurrently each find their own key.

The migration logic (in `dbus_set`): on every successful `set()`
for an instance, any legacy-attribute item is deleted. So if you
run `llm-quota-tray --set-key --instance=codex` while the default
instance has a legacy-attribute key, only the *codex* legacy
attribute gets migrated — the default's legacy key is untouched.
The migration is per-instance.

This is documented in detail in [`src/keyring.rs`](../src/keyring.rs)'s
module-level docs.

## Cross-references

* [`docs/cli.md`](cli.md) — the `--instance=` flag and `QUOTA_INSTANCE`
  env var.
* [`docs/architecture.md`](architecture.md#multi-instance-namespace) —
  the namespace derivation in the subsystem map.
* [`README.md`](../README.md#multiple-instances) — the short
  reference.
* [`examples/systemd/llm-quota-tray-codex.service`](../examples/systemd/llm-quota-tray-codex.service) —
  the worked second-instance systemd unit.
