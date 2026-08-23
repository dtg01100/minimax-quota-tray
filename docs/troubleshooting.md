# Troubleshooting

Common issues and how to diagnose them. Organized by symptom;
each section starts with the quickest check and ends with the
deepest debug path.

Most of these are covered briefly in the README's Troubleshooting
section; this doc is the longer reference.

## The tray icon doesn't appear at all

**Quickest checks**:

1. Does the panel you're using speak SNI?
   - **KDE Plasma** — works natively.
   - **GNOME Shell** — needs the [AppIndicator extension](https://github.com/ubuntu/gnome-shell-extension-appindicator).
     On Fedora: `gnome-shell-extension-appindicator`. On Ubuntu: same.
   - **XFCE** with `xfce4-panel` — install `xfce4-statusnotifier-plugin`.
   - **swaybar**, **Waybar**, **Cairo-Dock**, **trayer-srg** — work natively.

2. Is the daemon running?
   ```sh
   systemctl --user status llm-quota-tray.service
   # OR if you launched it directly:
   ps aux | grep llm-quota-tray
   ```

3. Is there a startup error?
   ```sh
   journalctl --user -u llm-quota-tray.service -n 100
   # OR run from a terminal:
   RUST_LOG=debug ~/.local/bin/llm-quota-tray
   ```

**Deep diagnosis**: with the daemon running, watch for SNI signals:

```sh
dbus-monitor --session "type='signal',interface='org.kde.StatusNotifierItem'"
# → the daemon should emit NewIcon + NewTitle + NewStatus shortly after startup
```

If you see the signals but the icon isn't there, the panel isn't
SNI-aware (check #1 again). If you don't see the signals, the
daemon didn't successfully register — check the daemon's stderr.

## Right-click does nothing

Either your panel doesn't speak SNI (see above), or the
AppIndicator bridge is missing:

- **KDE**: works natively. If right-click doesn't show the menu,
  check `System Tray Settings → "Provide SNI item"` is enabled.
- **GNOME**: install `gnome-shell-extension-appindicator`. Without
  it, GNOME 45+ silently drops SNI items.

## Icon shows three dots

The Rust binary writes ring SVGs to `${TMPDIR:-/tmp}` at startup
via `icon::write_static_svgs` and `icon::write_ring_svg`. The path
is sent as SNI `IconName`; the host loads it via `GdkPixbuf`
(GNOME AppIndicator) or `QtSvg` (KDE). The in-memory ARGB bytes
sent via `IconPixmap` is the always-set fallback.

**Why three dots**:

1. **Host can't render SVG** (notably Fedora Atomic / Bluefin /
   Silverblue, where `gdk-pixbuf2` ships with an empty loader
   directory): the SVG file load fails, the host falls through to
   `IconPixmap`. If the `IconPixmap` is also missing or wrong,
   the host shows its generic "image-loading" placeholder —
   three dots.

2. **`${TMPDIR}/llm-quota-tray-*.svg` doesn't exist**: the daemon
   failed to write the SVGs. Check `RUST_LOG=trace` for any
   `icon::write_*` errors. Permissions: TMPDIR must be writable.

3. **The `IconPixmap` path is broken**: rare, but possible. The
   daemon sends raw ARGB32 bytes in host-endian BGRA byte order
   (per the spec-literal interpretation; some hosts expect the
   opposite and render transparent pixels). Check the
   `host_endian` setting if your panel shows nothing.

**Fix**: install the SVG loader (`gdk-pixbuf2-svg` on Fedora), OR
rely on the pixmap path by ensuring the daemon writes correctly.

## "Argument password may not be null"

Your keyring daemon is locked. The tray calls `keyring::get` →
Secret Service `OpenSession` → `SearchItems` → `Item.GetSecret`
chain; locked collections return an "argument may not be null"-
shaped error.

**Fix**: unlock your keyring daemon, or re-enter the key from the
menu: click chip → **Set API Key…** → enter your key → the tray
calls `keyring::set` which writes a new item to the unlocked
collection.

## `secret_service_create_item_dbus_path: assertion ... collection_path != NULL`

No libsecret daemon is reachable. Check:

```sh
# GNOME
ps aux | grep gnome-keyring-daemon
# Should show: --components=secrets (at least)

# KWallet
ps aux | grep kwalletd5
# OR
ps aux | grep kwalletd6

# KeePassXC's secret-service bridge (Settings → Enable Secret Service Integration)
ps aux | grep keepassxc
```

If no daemon is running, the tray falls through to the `LLM_API_KEY`
env var. This is fine — just keep the env var set in your shell rc
or systemd unit's `Environment=` line.

## `LLM_API_KEY` works but keyring doesn't

Your panel session doesn't have the libsecret `$DBUS_SESSION_BUS_ADDRESS`
reachable by the daemon process. Common causes:

- The tray was started by `sudo` from a different session.
- The systemd user unit runs in a session that doesn't see
  `$DBUS_SESSION_BUS_ADDRESS` (rare; `After=graphical-session.target`
  usually fixes this).
- The keyring daemon is running but on a different bus address
  (e.g. the tray's session bus vs the user's).

**Fix**: set `LLM_API_KEY` in the systemd unit's `Environment=`
line, or run the tray from the same session that owns the
keyring daemon.

## The chip stays red

`Throttled` is set when the window is exhausted (`remaining_pct <= 0`).
If the window shouldn't be exhausted:

1. **The parser isn't reading the right fields** — `field_prefix`
   typo, wrong `entries_path`, etc. Check:

   ```sh
   RUST_LOG=debug ~/.local/bin/llm-quota-tray
   # → "refresh" logs show window remaining_pct
   ```

2. **The window has 0/0 counts but the API actually returns non-
   zero**: Coding Plan style, where the API returns
   `*_remaining_percent` only. Confirm with `curl` against the
   endpoint.

3. **The endpoint is returning an error envelope**: the daemon
   logs `"API error NNN: message"` at `info` level. The menu row
   shows the same.

## The chip shows 0% but I have plenty left

1. **Float → int truncation**: your provider returns cents / tokens
   as floats (`25.5` for $25.50). The parser truncates to integer
   cents (`25`). If `used / total` is sub-dollar, both numerator and
   denominator truncate to 0 → `0%`. Fix: scale in the adapter
   (multiply by 100), or set `count_unit: "cents"` if you've already
   scaled.

2. **Missing `*_remaining_percent` field**: the parser falls back
   to `100 - (100 * used / total)`. If `used == total`, it shows
   0% even though there might be a different cap. The
   `examples/providers/openrouter.json` template documents this
   case and the adapter pattern.

3. **Window length is wrong**: e.g. you set `reset_is_absolute_epoch:
   true` but the provider returns a duration. The reset math is
   off by ~50 years; the window looks "already exhausted" from
   the parser's perspective.

## The burn row doesn't show

Three reasons:

1. **`burn_warning.enabled: false`** in the config — no row at all.
2. **History hasn't accumulated** — the row needs `min_history_ms`
   of polling history (default 10 min). Wait it out.
3. **Pct-only window with rate=0** — the suppression rule:
   `decide_burn_row` returns `None` for pct-only windows that
   haven't ticked recently. Token-counting windows keep the row
   even at rate=0.

## Refresh is too slow / too fast

`refresh_seconds` is the baseline; `scheduler::next_interval`
applies:

- `/2` when remaining% drops below `yellow`.
- `/4` when remaining% drops below `red`.
- Exponential backoff on consecutive errors.

If you want a slower idle cadence, raise `refresh_seconds`. If
you want a tighter urgent cadence, lower `refresh_min_seconds`
(the floor on the adaptive cut).

## The menu "Open dashboard" doesn't open

```sh
RUST_LOG=debug ~/.local/bin/llm-quota-tray
# → look for "OpenURI portal" or "xdg-open" log lines
```

* **No portal daemon**: the daemon falls through to `xdg-open(1)`.
  Verify it's installed: `which xdg-open`.
* **No default browser**: `xdg-open` itself opens a "choose your
  app" dialog. Configure `xdg-settings set default-web-browser
  <browser>.desktop`.
* **Wrong URL**: `dashboard_url` in the config is the URL. Check
  the tray's startup log for the loaded config.

## Two instances collide

`Lock::acquire` returns `Ok(None)` and the daemon prints
`"another instance is already running; exiting"`.

* **Same `--instance=` flag**: only one of you can have it.
* **Stale PID file**: the prior instance crashed without cleanup.
  Check `$XDG_RUNTIME_DIR/llm-quota-tray[-<name>].pid`; if the
  process listed isn't running, `rm` it.

## `another instance is already running` after the daemon exits

Stale PID file. The takeover logic in `lock.rs` checks
`/proc/<pid>` and replaces if the holder is dead. On most
platforms this works; on rare systems without `/proc`, the lock
file persists. Fix:

```sh
rm "$XDG_RUNTIME_DIR/llm-quota-tray.pid"
# (or with --instance=foo: rm "$XDG_RUNTIME_DIR/llm-quota-tray-foo.pid")
```

## "fatal: ..." at startup

```sh
RUST_LOG=trace ~/.local/bin/llm-quota-tray 2>&1 | head -200
```

Common fatal errors:

* **Config file is malformed JSON** — `Config::default()` is
  supposed to fall through, but a panic here means a bug in the
  default constructor. Check `RUST_LOG` for which field failed.
* **Lock file I/O error** — `$XDG_RUNTIME_DIR` is read-only or
  doesn't exist. Fall through is supposed to be permissive.
* **Bus connection failure** — the daemon needs the session D-Bus
  for SNI. If `echo $DBUS_SESSION_BUS_ADDRESS` is empty, fix
  that first.

## High CPU / high RSS

* **High RSS** (>20 MB): the `tests/integration.rs::rss_under_target`
  test guards against accidental re-introduction of a heavy
  library. If you've added one, you'll see this fail. The
  integration test is `#[ignore]`'d by default; run it explicitly
  to catch the regression.

* **High CPU**: `RUST_LOG=trace` shows the per-poll work. If the
  network or HTTP is slow, `reqwest::blocking::Client` is on a
  `spawn_blocking` thread, so the tokio reactor isn't blocked —
  but other tasks can be. Most CPU regressions are from a busy
  loop in `network::run_watcher` or a re-fetch storm after a
  backoff reset.

## Diagnostic checklist (when nothing else helps)

```sh
# 1. Confirm the binary runs at all
~/.local/bin/llm-quota-tray --set-key
# (should prompt for a key and exit 0)

# 2. Run with full logging
RUST_LOG=trace ~/.local/bin/llm-quota-tray 2>&1 | tee /tmp/tray.log
# (let it run for a minute, then Ctrl-C)

# 3. Check the config that loaded
grep -E 'endpoint|shape|auth' ~/.config/llm-quota-tray/config.json

# 4. Test the endpoint directly
curl -v -H "Authorization: Bearer $LLM_API_KEY" "<endpoint>"
# (verify the response shape matches the parser's contract)

# 5. Watch D-Bus traffic
dbus-monitor --session | tee /tmp/dbus.log
# (should see org.kde.StatusNotifierItem signals + com.canonical.dbusmenu traffic)

# 6. Reset everything (last resort)
systemctl --user stop llm-quota-tray.service
rm -f "$XDG_RUNTIME_DIR/llm-quota-tray.pid"
rm -rf ~/.config/llm-quota-tray
secret-tool clear application llm-quota-tray
cargo build --release
./install.sh
```

If the daemon still misbehaves after step 6, file an issue with
the log output from steps 2 + 5 attached.
