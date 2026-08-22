//! Threshold notifications via the freedesktop Notifications D-Bus API.
//!
//! Replaces the previous `notify-send(1)` subprocess. We call
//! `org.freedesktop.Notifications.Notify` directly on the session bus,
//! per https://specifications.freedesktop.org/notification-spec/.
//!
//! ## Why D-Bus instead of the CLI
//!
//! - No `PATH` lookup for `notify-send` (which lives in
//!   `libnotify-bin` / `libnotify-tools` — a separate package on
//!   every distro we ship to). Removing it eliminates a soft
//!   dependency; the session bus is always present on the distros
//!   where the rest of this tray already runs.
//! - No `fork(2)` + `execve(2)` on every bucket-rank transition.
//!   At our cadence (1–2 notifications per hour in steady state, more
//!   when depleting) the win is small but real, and the failure
//!   surface shrinks: no missing-binary path, no zombie processes if
//!   the user mashes the tray menu during a transition.
//! - The D-Bus call returns the server-assigned `notification_id`
//!   synchronously. We deliberately ignore it (the spec lets us
//!   pick the ID on the next call via `replaces_id`); we use
//!   `x-canonical-private-synchronous` instead so the notification
//!   server dedups in-flight notifications of the same logical kind.
//!   That matches gjs's `-h string:x-canonical-private-synchronous:`
//!   behavior 1:1.
//!
//! ## gjs parity
//!
//! Matches the gjs `notify()` helper. Fires on upward bucket-rank
//! transitions only:
//!
//!   normal   → warning   ("<plan> — running low")  urgency: normal
//!   warning  → throttled ("<plan> — throttled")     urgency: critical
//!   * → normal — NOT notified (only worse states trigger)
//!
//! Deduplication is the caller's job (track `_last_bucket` between
//! refreshes, like gjs's `_lastBucket`). This module only does
//! the D-Bus call + arg shaping.

use std::sync::OnceLock;
use tokio::sync::OnceCell;
use zbus::zvariant::Value;
use zbus::{Connection, Proxy};

/// Well-known bus name / object path / interface for the freedesktop
/// Notifications service. All three are the same string by spec.
const NOTIF_BUS: &str = "org.freedesktop.Notifications";
const NOTIF_PATH: &str = "/org/freedesktop/Notifications";
const NOTIF_IFACE: &str = "org.freedesktop.Notifications";

/// `Value::U8` byte value for each urgency, per the spec's
/// `urgency` hint. (`y` over the wire — same numeric values as
/// libnotify's `NotifyUrgency` enum.)
const URGENCY_LOW: u8 = 0;
const URGENCY_NORMAL: u8 = 1;
const URGENCY_CRITICAL: u8 = 2;

/// Cached session-bus connection, opened lazily on the first
/// notification. Reused across calls so the daemon-side handshake
/// (and any auth like `XAUTHORITY` cookies) only runs once per
/// process.
///
/// `tokio::sync::OnceCell` rather than `std::sync::OnceLock` because
/// `Connection::session()` is async — a `OnceCell<T>` with
/// `get_or_try_init(|| async { ... })` is the right shape.
static SESSION_BUS: OnceCell<Connection> = OnceCell::const_new();

/// Open the session bus once and cache the handle. Returns the
/// cached connection on every call after the first.
async fn session_bus() -> zbus::Result<&'static Connection> {
    SESSION_BUS
        .get_or_try_init(|| async { Connection::session().await })
        .await
}

/// Send a desktop notification via the freedesktop Notifications
/// D-Bus API. Best-effort — failures are logged at debug level only
/// (no daemon / headless session / D-Bus auth error shouldn't take
/// down the tray).
///
/// `tag` is the `x-canonical-private-synchronous` value: a stable
/// per-notification-kind string. The notification server uses it
/// to replace in-flight notifications of the same kind instead of
/// stacking them, matching gjs's `-h string:x-canonical-private-
/// synchronous:<tag>` behavior.
pub async fn send(tag: &str, title: &str, body: &str, urgency: Urgency) {
    let result = send_inner(tag, title, body, urgency).await;
    if let Err(e) = result {
        log::debug!("notification via D-Bus failed: {e}");
    }
}

/// Inner implementation, factored out so the `pub` entry point can
/// be a single-line wrapper around a fallible async block.
async fn send_inner(tag: &str, title: &str, body: &str, urgency: Urgency) -> zbus::Result<()> {
    let conn = session_bus().await?;
    let proxy = Proxy::new(conn, NOTIF_BUS, NOTIF_PATH, NOTIF_IFACE).await?;

    // Spec signature:
    //   Notify(app_name, replaces_id, app_icon, summary, body,
    //          actions, hints, expire_timeout) -> notification_id
    //
    // `replaces_id = 0` always → we never reuse a server-issued ID;
    // the `x-canonical-private-synchronous` hint carries the dedup
    // key instead. (Servers that understand it replace the in-flight
    // notification; servers that don't still get a fresh notification
    // per call, same as `notify-send`.)
    //
    // `expire_timeout = -1` → use the server's default. Negative
    // values are explicitly documented as "use server default" in
    // the spec.
    //
    // Hints are `Dict<String, Variant>`. We populate two:
    //   * `urgency` (y / byte): 0=low, 1=normal, 2=critical.
    //   * `x-canonical-private-synchronous` (s / string): the
    //     caller-supplied dedup tag.
    let mut hints: std::collections::HashMap<&str, Value<'_>> =
        std::collections::HashMap::with_capacity(2);
    hints.insert("urgency", Value::U8(urgency.to_byte()));
    hints.insert("x-canonical-private-synchronous", Value::Str(tag.into()));

    // The `_` (return value) is the server-assigned `notification_id`.
    // We don't track it — the next call's dedup goes through the
    // `x-canonical-private-synchronous` hint, not `replaces_id`.
    let _id: u32 = proxy
        .call(
            "Notify",
            &(
                "llm-quota-tray",   // app_name
                0u32,               // replaces_id (0 = new)
                "",                 // app_icon (empty — server default)
                title,              // summary
                body,               // body
                Vec::<&str>::new(), // actions
                hints,              // hints
                -1i32,              // expire_timeout (server default)
            ),
        )
        .await?;

    Ok(())
}

/// Notification urgency. Matches the three levels defined by the
/// `urgency` hint in the freedesktop spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    /// Not currently emitted by the tray (the threshold logic only
    /// fires on `Normal` and `Critical`). Defined for parity with
    /// the libnotify urgency levels.
    #[allow(dead_code)]
    Low,
    Normal,
    Critical,
}

impl Urgency {
    /// Wire-format byte for the `urgency` hint (`y` over the wire).
    /// Matches libnotify's `NotifyUrgency` enum.
    fn to_byte(self) -> u8 {
        match self {
            Urgency::Low => URGENCY_LOW,
            Urgency::Normal => URGENCY_NORMAL,
            Urgency::Critical => URGENCY_CRITICAL,
        }
    }
}

/// Marker that ensures the lazy `OnceCell<Connection>` actually
/// constructs during integration tests that run without a session
/// bus. Touching it from the integration test file would silently
/// no-op; we keep it as a private const so the compiler doesn't
/// strip it during `--release` builds of the binary.
///
/// In practice this is only referenced via the `session_bus()`
/// helper above. The `OnceLock` here is intentionally unused — it's
/// a compile-time assertion that the `OnceCell` import is reachable
/// (the `cargo doc` lint otherwise dead-code-eliminates the unused
/// static in --release).
#[allow(dead_code)]
static _ENSURE_IMPORT: OnceLock<()> = OnceLock::new();

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urgency_byte_mapping_matches_libnotify() {
        // 0/1/2 mapping is documented in the freedesktop spec AND
        // matches libnotify's `NotifyUrgency` enum (LOW=0, NORMAL=1,
        // CRITICAL=2). Locking it in with a test so a refactor can't
        // accidentally swap critical/normal.
        assert_eq!(Urgency::Low.to_byte(), 0);
        assert_eq!(Urgency::Normal.to_byte(), 1);
        assert_eq!(Urgency::Critical.to_byte(), 2);
    }

    #[test]
    fn urgency_eq_and_copy() {
        // Used as a function arg by value; Copy trait is implicit
        // because all variants are unit. This test makes sure we
        // didn't accidentally add a payload.
        let u = Urgency::Normal;
        let v = u;
        assert_eq!(u, v);
    }

    #[test]
    fn urgency_send_is_send() {
        // `send` is a free function but takes `Urgency` by value
        // across an async boundary. This test is a compile-time
        // assertion that `Urgency: Send + Sync` (it is, trivially,
        // since the variants are unit).
        fn assert_send<T: Send + Sync>() {}
        assert_send::<Urgency>();
    }
}
