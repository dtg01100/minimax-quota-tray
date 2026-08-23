# Systemd unit examples

Sample systemd user units for pinning named instances to
specific config dirs. The default install (`install.sh`) only
registers one service, `llm-quota-tray.service`, that runs
the *default* instance (no `--instance=` flag).

If you want a named instance to also start on login, you
need a *second* service unit. Copy this example and adjust
the three flags that differ between instances:

1. The **description** (so `systemctl --user status` shows
   the right name)
2. The **`ExecStart`** path / `--instance=` value
3. The **filename** itself (must match the systemd unit name)

## Files in this directory

| File | Instance | Notes |
|---|---|---|
| `llm-quota-tray-codex.service` | `codex` | The example in [`docs/multi-instance.md`](../../docs/multi-instance.md) — for a second OpenAI account labelled "codex". |

## How to use

```sh
# 1. Set up the instance's config dir (one-time, see multi-instance.md).
mkdir -p ~/.config/llm-quota-tray-codex
cp <provider-template>.json ~/.config/llm-quota-tray-codex/config.json
$EDITOR ~/.config/llm-quota-tray-codex/config.json   # set instance: "codex"

# 2. Copy the example unit into your user systemd dir, renaming it
#    so the systemd unit name matches the instance name.
cp examples/systemd/llm-quota-tray-codex.service \
   ~/.config/systemd/user/llm-quota-tray-codex.service

# 3. Reload systemd + enable + start.
systemctl --user daemon-reload
systemctl --user enable --now llm-quota-tray-codex.service
systemctl --user status llm-quota-tray-codex.service
```

## Pinning multiple instances

There's no built-in templating (no `llm-quota-tray@.service`)
because each instance has its own config filename, keyring
attribute, lock file, and bus name — they're independent
enough that a single templated unit would just shift the
parameterization into systemd's `%i` spec, which adds a layer
of indirection without saving much. The pattern is: one
hand-written service file per named instance, kept in version
control alongside your team's runbook or playbook.

For multi-instance running without per-instance services
(e.g. ad-hoc from a terminal), see
[`docs/multi-instance.md`](../../docs/multi-instance.md#setting-up-a-second-instance).
