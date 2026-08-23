# FAQ

Common questions that don't fit cleanly into a single doc.
For symptom-driven walkthroughs see
[`troubleshooting.md`](troubleshooting.md).

## Installation & setup

### How do I install it?

```sh
git clone https://github.com/dtg01100/minimax-quota-tray
cd minimax-quota-tray
./install.sh
```

The script builds (incremental — first time takes ~1–2 min,
subsequent rebuilds are seconds), installs binary + systemd
unit + .desktop + icon, and starts the service if a graphical
session is detected. See [`README.md` — Install](../README.md#install)
and `install.sh`'s inline comments for every file it copies.

### What distros are supported?

Any distro with:
- Rust 1.75+ (MSRV, see `Cargo.toml`)
- A systemd user instance (for the service unit — the binary
  itself runs fine without systemd)
- `cargo` and `systemctl` on PATH

Tested on Fedora 41, Bluefin 44, Ubuntu 24.04. Should work
on any other distro with no changes — there's no distro-
specific code.

### Can I run it without systemd?

Yes. The binary is a normal executable — just run it directly:

```sh
~/.local/bin/llm-quota-tray
```

You'll lose `Restart=on-failure` and the `After=graphical-
session.target` ordering, but the daemon itself doesn't care.
Useful for debugging or testing in a non-systemd container.

### Does it need GTK or libappindicator at install time?

**No.** The binary talks to SNI directly via `zbus`, no
`libgtk-3`, no `libappindicator`, no `libdbusmenu-glib`
linking. Only at *runtime* does it need `libdbus` (via zbus,
ubiquitous on modern Linux) and `libsecret` (via the
`secret-tool` CLI — used only for the optional `--set-key`
interactive flow).

## Authentication & the keyring

### How does the daemon get my API key?

It stores one entry in libsecret (GNOME Keyring, KWallet,
KeePassXC's secret-service bridge, etc.) under the attribute
`application llm-quota-tray`. Use the menu's "Set API Key…"
item or the `llm-quota-tray --set-key` CLI flow.

```sh
# Manual equivalent (in case the menu prompt is broken):
secret-tool store --label="llm-quota-tray" application llm-quota-tray
# (type key, Enter, Ctrl-D)
```

See [`troubleshooting.md` — keyring](troubleshooting.md)
for the unlock-required-on-every-login gotcha.

### Why a separate `--set-key` bash script and not just call `secret-tool` from the binary?

The `--set-key` flag shells out to `secret-tool` (zenity /
kdialog for the password prompt, with a terminal-fallback if
neither is installed). Linking `libsecret` directly from Rust
panics on tokio-runtime-context conflicts with zbus (see
[`src/keyring.rs`](../src/keyring.rs) — long comment block at
the top). Shelling out is uglier but avoids the runtime issue
and matches how every other freedesktop tool does it.

### What if my provider uses OAuth or session cookies?

OAuth bearer tokens work out of the box — set the token in the
keyring and use the `Authorization: Bearer` auth style. Session
cookies (rare for quota APIs) are not currently supported; open
an issue with the provider URL to add a `Cookie` auth style.

## Configuration

### Where's the config file?

`~/.config/llm-quota-tray/config.json` for the default instance,
`~/.config/llm-quota-tray-<name>/config.json` for named
instances. Written with mode `0600` on first run from
[`config.example.json`](../config.example.json). Every field is
documented in [`config-schema.md`](config-schema.md).

### Can I edit the config while the daemon is running?

Yes — the daemon re-reads the file on every SIGHUP:

```sh
systemctl --user kill --signal=HUP llm-quota-tray.service
```

Or, in the menu, "Refresh now" triggers a re-read.

### How do I run a second tray for a different account?

See [`multi-instance.md`](multi-instance.md). TL;DR:

```sh
# Copy the config + tweak the new instance's name.
mkdir -p ~/.config/llm-quota-tray-work
cp ~/.config/llm-quota-tray/config.json ~/.config/llm-quota-tray-work/
# Edit `instance` field in the copy, then:
llm-quota-tray --instance work
```

Or use the systemd template + drop-in pattern documented there.

## Tray integration

### I'm on GNOME and don't see the chip.

GNOME has no built-in SNI support. Install the
`appindicator` extension:

```sh
# Fedora / Bluefin
sudo dnf install gnome-shell-extension-appindicator

# Ubuntu / Debian
sudo apt install gnome-shell-extension-appindicator
```

Then enable it via GNOME Tweaks or `gnome-extensions enable
appindicatorsupport@rgcjonas.gmail.com`. Log out and back in
for the extension to register its SNI watcher.

If the chip is *still* missing after enabling, see
[`troubleshooting.md` — "The tray icon doesn't appear at all"](troubleshooting.md#the-tray-icon-doesnt-appear-at-all)
for the full diagnostic walkthrough.

### My chip appears, disappears, and reappears cyclically.

Old bug — fixed in the latest release. If you're on a version
older than 0.3.1, upgrade. If you're on 0.3.1+ and still see
it, file an issue with the output of:

```sh
RUST_LOG=llm_quota_tray::sni=debug systemctl --user restart llm-quota-tray.service
journalctl --user -u llm-quota-tray.service -f
```

### The chip shows but the menu is empty / shows "loading…".

The daemon is fetching but the parse hasn't completed yet.
First refresh takes ~500 ms after the initial render. If the
menu is *still* empty after a minute, the parse is failing —
enable debug logging (see [`logging.md`](logging.md)) and
look for `parse error` lines.

### Does this work on KDE / XFCE / sway?

Yes — any host that speaks SNI works. KDE has native support
(no extension needed). XFCE needs
`xfce4-statusnotifier-plugin`. sway / Waybar reads SNI via
`gtk3-layer-shell` + a tray applet.

## Development

### How do I add a new provider?

See [`port-guide.md`](port-guide.md). Short version:

1. Copy `examples/providers/template.json` (or a similar
   existing template) to `examples/providers/<yourprovider>.json`.
2. Fill in the JSON: endpoint URL, auth style, parse plan.
3. Drop the template into your config dir:
   `cp examples/providers/<yourprovider>.json ~/.config/llm-quota-tray/config.json`
4. Edit any per-user fields (label, instance name).
5. Restart the service.

No Rust changes needed for "Simple" tracks. For "Hard" tracks
(where the JSON needs a sidecar or a custom shape), see the
sidecar pattern in [`port-guide.md`](port-guide.md#when-you-need-a-sidecar).

### How do I add a new auth style?

The auth-style enum is in [`src/fetch.rs`](../src/fetch.rs).
Add a variant, the header-builder match arm, and a docstring.
There's no other touch point — auth is data-driven from config
and the enum drives everything downstream.

### How do I run the test suite?

```sh
cargo test --release
# 261 passed, 0 failed, 4 ignored (currently-ignored are
# integration tests that need a real session bus — see
# tests/integration.rs).

# Integration tests (require a session dbus):
cargo test --release -- --ignored
```

### How do I cut a release?

See [`RELEASING.md`](../RELEASING.md) — versioning policy,
CHANGELOG discipline, tag signing, post-release smoke checks.

### The docs reference `gjs-parity.md` — what's that?

A load-bearing "don't change these decisions" doc inherited
from the original gjs + GTK3 implementation that this project
was ported from. If you're tempted to "fix" something that
looks weird, check that doc first — many of those decisions
exist for solid reasons (gjs bug workarounds, panel quirks on
specific distros, etc.) and re-introducing them would regress
specific hosts.
