# Security model

What's stored where, what's trusted, what the attack surface
is, and what assumptions we make about the host.

This page is *complementary* to [`../SECURITY.md`](../SECURITY.md),
which is the **disclosure policy** (how to report a vuln). This
doc is the **threat model** — what we defend against, what we
don't, and why.

## Assets

| Asset | Sensitivity | Stored at | Notes |
|---|---|---|---|
| Provider API key | High (bearer credential) | libsecret, attribute `application llm-quota-tray`, label `llm-quota-tray` | Plaintext on disk inside the user's encrypted keyring. Never logged, never passed on the cmdline. |
| Provider quota responses | Low (read-only usage data) | Memory only | Parsed then dropped; not written to disk. |
| Quota history (per-window sample series) | Low | `$TMPDIR/llm-quota-tray-history-*.json` | Burn-rate math's input. Mode `0600`, in tmpfs on most distros. Cleared on uninstall only if `TMPDIR` survives. |
| Static SVG icons | None | `$TMPDIR/llm-quota-tray-*.svg` | Public — also live in `packaging/icons/`. |
| Per-instance config | Low (no secrets) | `~/.config/llm-quota-tray[-<name>]/config.json` | Mode `0600`. |
| PID lock file | None | `$XDG_RUNTIME_DIR/llm-quota-tray[-<name>].lock` | Contains PID and a start-time fingerprint. Mode `0600`. |
| Logs | Low | journald | Never contain the API key or full API responses. See [`logging.md`](logging.md#what-does-not-get-logged). |

## What we trust

1. **The session D-Bus is the user.** No authentication on the
   session bus — any process running as the user can talk to
   our SNI interface, dbusmenu, and Secret Service proxy.
   That's by design (D-Bus design — sandboxed apps would use
   the portals instead), but it means:
   - Another user-process could open our dbusmenu and click
     "Quit". Annoying, not a vuln.
   - Another user-process could call
     `org.freedesktop.secrets` and read our keyring entry.
     That's a generic libsecret threat model, not ours.
   - Another user-process could send us a malformed
     `com.canonical.dbusmenu` event. We handle unknown events
     as no-ops.
2. **The system bus is also the user.** Same threat model as
   the session bus for our purposes — we don't talk to the
   system bus.
3. **`secret-tool` is honest.** We shell out to `secret-tool
   store / lookup / clear` and trust its exit code + stdout.
   `secret-tool` is part of `libsecret-tools` and shipped by
   distros; we don't pin a version.
4. **`xdg-open` is honest.** For the "Open dashboard" menu
   action. We pass the provider's dashboard URL as a single
   argv; the URL comes from the user's own config file.

## What we don't trust

1. **The provider's quota endpoint.** We never execute
   response data — the parser is purely data-driven (`JsonValue`
   → typed fields), with no `eval`-equivalent.
2. **The user's config file.** It's loaded with `serde_json`
   (no JSON5, no eval). Bad config falls back to defaults with
   a warning logged.
3. **Network responses.** All HTTP responses go through
   `reqwest` + `rustls`. TLS cert validation is on (default).
   We don't have an opt-out for self-signed endpoints.
4. **Environment variables from the session.** `RUST_LOG`,
   `TMPDIR`, `XDG_RUNTIME_DIR`, `XDG_DATA_HOME`, `XDG_SESSION_TYPE`,
   `XDG_CURRENT_DESKTOP`, `DISPLAY`, `WAYLAND_DISPLAY` are
   read. We never pass them as shell arguments.

## Network egress

Per refresh, the daemon makes **one** HTTPS GET to the user's
configured `endpoint`, plus zero or one to `pricing_url` if
configured. Both are read-only — no POSTs, no PUTs, no cookies
sent. The exact request shape (headers, query params) is in
the per-auth-style section of [`config-schema.md`](config-schema.md).

If the endpoint is down or returns a non-2xx, the daemon retries
on the next refresh — no exponential backoff, no jitter.
Bumping `refresh_seconds` is the user-side knob.

The daemon does **not** phone home. No telemetry, no update
checks, no crash reports. The list of outbound destinations
is exactly `(endpoint, optional pricing_url, secret-tool
probing only on set-key)`.

## Secret Service (libsecret)

We talk to libsecret through the `secret-tool` CLI, which
itself talks D-Bus to `org.freedesktop.secrets`. The
collection we use is the default unlocked collection — we
don't create or manage collections.

Two attribute names are recognized for backward compatibility:

- **Current:** `application llm-quota-tray` (recommended).
- **Legacy:** `org.freedesktop.Secret.Generic` with a matching
  `xdg:schema` field. Read transparently for migration; new
  writes always use the current name. A one-line INFO log at
  startup notes the legacy entry and prompts the user to re-run
  `llm-quota-tray --set-key` to migrate.

`secret-tool` reads are wrapped with a prompt-fallback chain:
zenity → kdialog → terminal. The interactive prompt is the
user authenticating to their own keyring daemon.

## D-Bus interfaces we own

| Bus | Interface | Object path | Purpose |
|---|---|---|---|
| Session | `org.kde.StatusNotifierItem` | `/StatusNotifierItem` | Tray chip — queried by the SNI watcher (panel). |
| Session | `com.canonical.dbusmenu` | `/Menu` | Dropdown menu. |
| Session | `org.kde.StatusNotifierItem-<pid>-1` | — | Well-known bus name (one per instance, matched to PID so multiple instances coexist). |

All three are well-known, documented protocols. We don't
introduce any custom D-Bus interface that an attacker could
exploit to escalate.

We *consume* these (read-only):

- `org.freedesktop.secrets` (via `secret-tool`)
- `org.freedesktop.portal.OpenURI` (for "Open dashboard")
- `org.freedesktop.Notifications` (for chip-bucket-rank
  increases)
- `org.freedesktop.NetworkManager` (for the connectivity
  watcher — optional, gracefully degrades if absent)
- `org.kde.StatusNotifierWatcher` (auto-discovery + recovery)

## Supply chain

- All deps are pinned in `Cargo.lock`. The release build is
  reproducible byte-for-byte given the same toolchain.
- `Cargo.toml` lists every dep with an inline comment
  explaining *why* it's there (e.g. why `tiny-skia` vs
  `usvg+resvg`, why `secret-tool` shell-out vs the
  `secret-service` Rust crate). Reviewers should sanity-check
  these on any PR that touches deps.
- No `git` URL deps. Every dep is on crates.io.
- Build is hermetic — no `build.rs` network calls.

## Hardening checklist for paranoid users

1. Run the daemon under a dedicated user account (the lock
   file under `XDG_RUNTIME_DIR` and the config dir under
   `~/.config` make this awkward today, but doable with
   bind-mounts + XDG env overrides).
2. Restrict the session D-Bus to a whitelist of trusted
   apps (systemd unit + `DBusFilter` rules in
   `/etc/dbus-1/session.conf`).
3. Use a keyring collection that requires re-auth on every
   read (GNOME Keyring's "Unlock on login" setting — off by
   default).
4. Set `endpoint` and `pricing_url` to hostnames you trust;
   the daemon has no scheme-pinning and will happily talk
   `http://` if you configure it (we don't validate the
   scheme).
5. Run the daemon under `systemd --user` with
   `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6` (the
   only three it needs) to prevent accidental DNS / Unix-
   socket chatter to daemons it shouldn't be talking to.

## What we explicitly do NOT defend against

- **A root user on the host.** Root can read
  `~/.config/llm-quota-tray/` and the keyring collection
  regardless of file modes.
- **A local user with the same UID.** Same — by POSIX
  definition, file permissions don't separate same-UID
  processes.
- **Kernel-level compromise.** If your kernel is pwned, you
  have bigger problems than this tray indicator.
- **Physical access to an unlocked screen.** Same as any
  foreground UI app — the API key is one menu click away
  for anyone who can see your desktop.
