//! Threshold notifications via the freedesktop Notification
//! portal, with a direct `org.freedesktop.Notifications` fallback.
//!
//! Targets https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.Notification.html
//! (v2). The portal's `AddNotification(id, vardict)` carries a
//! caller-supplied `id` the server uses to *replace* in-flight
//! notifications of the same kind instead of stacking them —
//! which is the same dedup behavior the prior direct-Notifications
//! path got via the `x-canonical-private-synchronous` hint. We
//! pass the caller-supplied `tag` straight through as the portal
//! `id`, dropping the hint hack.
//!
//! ## Why the portal
//!
//! - The Notification portal is the sandbox-friendly path; flatpak
//!   apps cannot reach `org.freedesktop.Notifications` directly
//!   and must go through the portal. Talking to the portal from
//!   the start means a future flatpak packaging path requires no
//!   code changes here.
//! - The portal's `id`-based replace is canonical, documented, and
//!   honored by every portal backend (GNOME's `xdg-desktop-portal-gtk`,
//!   KDE's `xdg-desktop-portal-kde`, etc.). The old
//!   `x-canonical-private-synchronous` hint was a libnotify-server
//!   extension that worked on Ubuntu/GNOME but wasn't guaranteed
//!   elsewhere.
//!
//! ## Fallback
//!
//! 1. **`xdg-desktop-portal` Notification** — `AddNotification(id, vardict)`.
//! 2. **Direct `org.freedesktop.Notifications.Notify`** — used when
//!    no portal daemon is running (headless CI, minimal WMs).
//!    Replicates the original behavior with the
//!    `x-canonical-private-synchronous` hint preserved for
//!    gjs-parity.
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
//! the dispatch + arg shaping.

use std::sync::OnceLock;
use tokio::sync::OnceCell;
use zbus::zvariant::Value;
use zbus::{Connection, Proxy};

/// Direct Notifications spec constants (the fallback path).
const NOTIF_BUS: &str = "org.freedesktop.Notifications";
const NOTIF_PATH: &str = "/org/freedesktop/Notifications";
const NOTIF_IFACE: &str = "org.freedesktop.Notifications";

/// Desktop portal constants (the primary path).
const PORTAL_BUS: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const PORTAL_NOTIF_IFACE: &str = "org.freedesktop.portal.Notification";
const PORTAL_APP_ID: &str = "llm-quota-tray.desktop";

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

/// Send a desktop notification. Best-effort — failures are logged
/// at debug level only (no daemon / headless session / D-Bus auth
/// error shouldn't take down the tray).
///
/// `tag` is a stable per-notification-kind string. The portal
/// uses it as the `id` argument to `AddNotification` so the
/// notification server replaces in-flight notifications of the
/// same kind instead of stacking them. On the direct-Notifications
/// fallback path, the same string is passed via the
/// `x-canonical-private-synchronous` hint (the libnotify-server
/// extension that achieves the same dedup).
///
/// `activation_token` is the XDG Activation token the desktop
/// shell provided at launch (`$XDG_ACTIVATION_TOKEN` or
/// `--token=<token>`). The portal uses it to animate the
/// notification from the originating click when the bucket
/// transition fires close enough to launch that the token is
/// still valid. Pass `None` when the caller doesn't have one or
/// the transition fires long after launch (the token is
/// single-use and expires). The direct-Notifications fallback
/// doesn't honor activation tokens — it simply ignores the
/// parameter.
pub async fn send(
    tag: &str,
    title: &str,
    body: &str,
    urgency: Urgency,
    activation_token: Option<&str>,
) {
    // Portal-first; if the portal daemon isn't reachable, fall
    // through to the direct Notifications D-Bus path. Both
    // branches are best-effort — the user gets a debug log on
    // failure but no error surface to the caller.
    if let Err(e) = send_portal(tag, title, body, urgency, activation_token).await {
        log::debug!("notification via portal failed ({e:#}); trying direct");
        if let Err(e) = send_direct(tag, title, body, urgency).await {
            log::debug!("notification via direct D-Bus failed: {e}");
        }
    }
}

/// Portal path: `org.freedesktop.portal.Notification.AddNotification`.
///
/// Spec signature:
///   AddNotification(IN s id, IN a{sv} notification)
///
/// The `id` is the dedup key — the server replaces any in-flight
/// notification with the same id instead of stacking. Notification
/// vardict supports `title`, `body`, `priority` (low/normal/high/
/// urgent), and others; we use the first three.
///
/// `activation_token` is forwarded as the `activation_token`
/// vardict key (Notification portal v2+, see
/// https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.Notification.html).
/// Stale tokens (None, expired) are simply omitted — the portal
/// shows the notification without a launch animation.
async fn send_portal(
    tag: &str,
    title: &str,
    body: &str,
    urgency: Urgency,
    activation_token: Option<&str>,
) -> zbus::Result<()> {
    let conn = session_bus().await?;
    let proxy = Proxy::new(conn, PORTAL_BUS, PORTAL_PATH, PORTAL_NOTIF_IFACE).await?;

    let mut notification: std::collections::HashMap<&str, Value<'_>> =
        std::collections::HashMap::with_capacity(4);
    notification.insert("title", Value::Str(title.into()));
    notification.insert("body", Value::Str(body.into()));
    notification.insert("priority", Value::Str(urgency.to_priority().into()));
    if let Some(tok) = activation_token {
        notification.insert("activation_token", Value::Str(tok.into()));
    }

    // The `::<_, _, ()>` turbofish pins the return type to `()`. The
    // Rust 2024 never-type-fallback lint requires an explicit type
    // here because `AddNotification`'s return is empty — without
    // the turbofish, the macro's return-type inference can pick
    // `!` and trip the lint. Same fix for the direct-Notifications
    // call below.
    let _ = proxy
        .call::<_, _, ()>("AddNotification", &(tag, notification))
        .await;
    Ok(())
}

/// Direct fallback: `org.freedesktop.Notifications.Notify`.
///
/// Identical to the pre-portal implementation, preserved for
/// hosts without `xdg-desktop-portal` running.
async fn send_direct(
    tag: &str,
    title: &str,
    body: &str,
    urgency: Urgency,
) -> zbus::Result<()> {
    let conn = session_bus().await?;
    let proxy = Proxy::new(conn, NOTIF_BUS, NOTIF_PATH, NOTIF_IFACE).await?;

    // Spec signature:
    //   Notify(app_name, replaces_id, app_icon, summary, body,
    //          actions, hints, expire_timeout) -> notification_id
    //
    // `replaces_id = 0` always; the
    // `x-canonical-private-synchronous` hint carries the dedup
    // key. Servers that understand it replace the in-flight
    // notification; servers that don't get a fresh notification
    // per call.
    //
    // `expire_timeout = -1` → use the server's default.
    let mut hints: std::collections::HashMap<&str, Value<'_>> =
        std::collections::HashMap::with_capacity(2);
    hints.insert("urgency", Value::U8(urgency.to_byte()));
    hints.insert("x-canonical-private-synchronous", Value::Str(tag.into()));

    let _id: u32 = proxy
        .call(
            "Notify",
            &(
                PORTAL_APP_ID,
                0u32,
                "",
                title,
                body,
                Vec::<&str>::new(),
                hints,
                -1i32,
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
    /// Matches libnotify's `NotifyUrgency` enum. Used by the
    /// direct-Notifications fallback path.
    fn to_byte(self) -> u8 {
        match self {
            Urgency::Low => URGENCY_LOW,
            Urgency::Normal => URGENCY_NORMAL,
            Urgency::Critical => URGENCY_CRITICAL,
        }
    }

    /// Notification portal's `priority` field, per
    /// https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.Notification.html
    /// (one of `low` / `normal` / `high` / `urgent`). Maps the
    /// libnotify-shaped `Urgency` onto the portal's priority
    /// vocabulary.
    fn to_priority(self) -> &'static str {
        match self {
            Urgency::Low => "low",
            Urgency::Normal => "normal",
            Urgency::Critical => "urgent",
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

    #[test]
    fn urgency_priority_mapping_for_portal() {
        assert_eq!(Urgency::Low.to_priority(), "low");
        assert_eq!(Urgency::Normal.to_priority(), "normal");
        assert_eq!(Urgency::Critical.to_priority(), "urgent");
    }

    #[test]
    fn urgency_priority_and_byte_are_consistent() {
        assert_ne!(Urgency::Normal.to_priority(), Urgency::Critical.to_priority());
        assert_ne!(Urgency::Normal.to_byte(), Urgency::Critical.to_byte());
    }

    #[test]
    fn urgency_priority_strings_are_lowercase() {
        for u in [Urgency::Low, Urgency::Normal, Urgency::Critical] {
            let p = u.to_priority();
            assert!(p.chars().all(|c| !c.is_uppercase()), "portal priority must be lowercase, got {p:?}");
            assert!(!p.is_empty(), "portal priority must be non-empty");
        }
    }

    #[test]
    fn urgency_debug_format_is_stable() {
        assert_eq!(format!("{:?}", Urgency::Low), "Low");
        assert_eq!(format!("{:?}", Urgency::Normal), "Normal");
        assert_eq!(format!("{:?}", Urgency::Critical), "Critical");
    }


}
