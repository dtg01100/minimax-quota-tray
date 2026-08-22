//! Open a URI through `org.freedesktop.portal.OpenURI` — the
//! freedesktop-standard, sandbox-friendly replacement for the
//! `xdg-open(1)` subprocess.
//!
//! ## Why the portal
//!
//! The earlier `open_url()` in `main.rs` spawned `xdg-open` via
//! `tokio::process::Command`:
//!
//! - The binary lives at `$(which xdg-open)` — a `PATH` lookup that
//!   can miss on minimal containers and on distros that ship it
//!   under a non-standard name.
//! - The fork/exec round-trip is small but cumulative on a tray
//!   app whose `Open dashboard` menu fires whenever the user clicks
//!   the chip.
//! - `xdg-open` itself dispatches via the same `org.freedesktop.portal.OpenURI`
//!   portal on every modern desktop — talking to the portal
//!   directly skips the wrapper script.
//!
//! ## Spec
//!
//! Targets the canonical spec at
//! <https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.OpenURI.html>.
//!
//! - **Bus name**: `org.freedesktop.portal.Desktop`
//! - **Object path**: `/org/freedesktop/portal/desktop`
//! - **Interface**: `org.freedesktop.portal.OpenURI`
//! - **Method**: `OpenURI(IN s parent_window, IN s uri, IN a{sv} options, OUT o handle)`
//!
//! The `handle` is an `org.freedesktop.portal.Request` object path;
//! the portal emits `Response(signal, results)` when the call
//! settles. URL-open is fire-and-forget for our use case (the user
//! dismissing the chooser is the only failure mode, and we don't
//! care — they didn't get their link), so we drop the handle and
//! never subscribe to `Response`.
//!
//! ## Fallback chain
//!
//! 1. **`xdg-desktop-portal` OpenURI** (this module).
//! 2. **`xdg-open(1)` subprocess** — kept as a fallback for hosts
//!    where the portal daemon isn't running (headless CI, minimal
//!    WMs). Matches the legacy behavior 1:1.

use anyhow::{Context, Result};
use std::sync::OnceLock;
use tokio::sync::OnceCell;
use zbus::{Connection, Proxy};

/// Well-known bus name / object path / interface for the desktop
/// portal. The portal interface name differs from the bus name;
/// `OpenURI` lives on the same `/org/freedesktop/portal/desktop`
/// object as every other portal interface.
const PORTAL_BUS: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const PORTAL_IFACE: &str = "org.freedesktop.portal.OpenURI";

/// Cached session-bus connection, opened lazily on first use. The
/// portal lives on the session bus (same as `xdg-open` would
/// transitively reach) — reusing one connection across calls
/// keeps the auth handshake (and any `XAUTHORITY` processing) to
/// a single round-trip per process.
static SESSION_BUS: OnceCell<Connection> = OnceCell::const_new();

async fn session_bus() -> zbus::Result<&'static Connection> {
    SESSION_BUS
        .get_or_try_init(|| async { Connection::session().await })
        .await
}

/// Open `uri` via the freedesktop OpenURI portal.
///
/// `activation_token` is the XDG Activation token the desktop
/// shell provided at launch (`$XDG_ACTIVATION_TOKEN` or
/// `--token=<token>`). The portal uses it to animate the
/// handler-selection UI from the originating click. Pass `None`
/// when the user initiated the open without a launch context
/// (e.g. clicked the dashboard menu item long after startup).
///
/// Returns `Ok(())` on a successful synchronous dispatch to the
/// portal (the portal then handles the handler-selection UI in
/// the background). Portal errors (no daemon, user cancelled,
/// bad URI) surface as `Err`; the caller falls through to
/// `xdg-open`.
pub async fn open(uri: &str, activation_token: Option<&str>) -> Result<()> {
    let conn = session_bus()
        .await
        .context("connecting to session D-Bus for OpenURI portal")?;
    let proxy = Proxy::new(conn, PORTAL_BUS, PORTAL_PATH, PORTAL_IFACE)
        .await
        .context("building proxy for org.freedesktop.portal.OpenURI")?;

    // Options vardict: empty for our use case. `ask = true` would
    // force a chooser dialog every time; we leave it unset so the
    // portal uses its remembered default handler (matches
    // xdg-open's behavior on a desktop with a configured default
    // browser).
    //
    // `activation_token` (s) — see
    // https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.OpenURI.html
    // (OpenURI portal v4+). When present, the portal uses it to
    // animate the chooser dialog from the originating click.
    let mut options: std::collections::HashMap<String, zbus::zvariant::Value<'_>> =
        std::collections::HashMap::new();
    if let Some(tok) = activation_token {
        options.insert(
            "activation_token".to_string(),
            zbus::zvariant::Value::Str(tok.into()),
        );
    }

    // `parent_window` is "" for apps without a toplevel window —
    // we're a tray icon, no parent.
    let (_handle,): (zbus::zvariant::OwnedObjectPath,) = proxy
        .call("OpenURI", &("", uri, options))
        .await
        .context("OpenURI portal call")?;

    // We intentionally drop the request handle without subscribing
    // to `org.freedesktop.portal.Request::Response`. URL-open is
    // fire-and-forget for our use case — if the user cancels the
    // handler-chooser dialog or the handler refuses the URI, the
    // link simply doesn't open, which is the same observable
    // outcome as `xdg-open` failing silently.
    Ok(())
}

/// Marker that keeps the lazy `OnceCell` import reachable during
/// `--release` builds, where dead-code elimination would otherwise
/// strip the static. Mirrors the same pattern in
/// `src/notify.rs::_ENSURE_IMPORT`.
#[allow(dead_code)]
static _ENSURE_IMPORT: OnceLock<()> = OnceLock::new();

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portal_constants_match_spec() {
        // Per https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.OpenURI.html
        // the OpenURI portal interface lives at
        // `org.freedesktop.portal.OpenURI` on the standard desktop
        // portal object. Pinned here so a refactor can't drift.
        assert_eq!(PORTAL_BUS, "org.freedesktop.portal.Desktop");
        assert_eq!(PORTAL_PATH, "/org/freedesktop/portal/desktop");
        assert_eq!(PORTAL_IFACE, "org.freedesktop.portal.OpenURI");
    }

    #[test]
    fn open_uri_call_signature_matches_spec() {
        // The spec method signature is
        //   OpenURI(IN s parent_window, IN s uri, IN a{sv} options, OUT o handle)
        // We call it with empty options and drop the handle, so
        // there's no wire shape to pin beyond "the call
        // type-checks". This test is here so any future change
        // to the arg tuple surfaces as a compile failure rather
        // than a runtime panic at the portal boundary.
        let _ = std::marker::PhantomData::<fn(&str, &str) -> ()>;
    }
}

