//! Screen-lock monitoring via the freedesktop ScreenSaver interface.
//!
//! Watches `org.gnome.ScreenSaver` on the session bus. GNOME, KDE,
//! XFCE, Cinnamon, MATE, and Sway all implement this same interface
//! (the GNOME-flavored name is the canonical D-Bus name; the spec is
//! freedesktop). On lock/unlock transitions we emit
//! `ScreenLock(bool)` to the main loop; on unlock we additionally emit
//! `ForceRefresh` so the cadence resumes immediately and the next
//! refresh isn't held off by an in-progress backoff interval.
//!
//! Behavior:
//!   - On startup: query current `Active`; if locked, send
//!     `ScreenLock(true)` immediately so the tray skips the first
//!     fetch. If the ScreenSaver isn't on the bus (headless CI,
//!     Wayland sessions without a lock daemon, etc.) we degrade
//!     gracefully and never send events — `state.screen_locked`
//!     stays at its `Default` value of `false`.
//!   - On transition to locked: send `ScreenLock(true)`.
//!   - On transition to unlocked: send `ScreenLock(false)` AND
//!     `ForceRefresh` (matches the gjs `refresh(true)` semantics
//!     used by `network::spawn_watcher` on reconnect).
//!
//! We deliberately do NOT use `org.freedesktop.login1.Manager`'s
//! `PrepareForSleep` signal — that fires on suspend/resume, not on
//! screen-lock. Locking the screen without suspending is the common
//! case (laptop lid open, monitor on, user walks away) and we want to
//! catch it.

use anyhow::{Context, Result};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use zbus::{connection::Builder as ConnectionBuilder, Proxy};

/// Screen-lock events delivered to the main loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockEvent {
    /// Screen-lock state changed. `locked = true` → skip polling;
    /// `locked = false` → resume polling. Mirrors `NetEvent::Connectivity`
    /// in shape so the orchestrator can treat both uniformly.
    ScreenLock(bool),
    /// Force a refresh right now. Mirrors `NetEvent::ForceRefresh` and
    /// reuses the orchestrator's existing arm — that arm already clears
    /// `state.offline` and `state.fail_streak`, so this also gets the
    /// offline-override behavior for free.
    ForceRefresh,
}

/// Spawn a tokio task that watches ScreenSaver and feeds events to
/// `tx`. The task lives for the duration of the program; if the
/// ScreenSaver interface isn't reachable (no session bus, no lock
/// daemon) the task exits silently and `tx` is never used. This is
/// the same graceful-degradation shape as `network::spawn_watcher`.
///
/// # Errors
///
/// Returns `Err` if connecting to the **session** D-Bus fails. The
/// session bus is intentional here (lock state is per-session, not
/// per-system), unlike `network::spawn_watcher` which uses the system
/// bus for NetworkManager.
pub async fn spawn_watcher(tx: mpsc::Sender<LockEvent>) -> Result<()> {
    let conn = match ConnectionBuilder::session()
        .context("connecting to session D-Bus")?
        .build()
        .await
    {
        Ok(c) => c,
        Err(e) => {
            log::debug!("screenlock monitor: no session bus ({e}); lock detection disabled");
            return Ok(());
        }
    };
    tokio::spawn(async move {
        if let Err(e) = run_watcher(conn, tx).await {
            log::debug!("screenlock monitor: watcher exited: {e:#}");
        }
    });
    Ok(())
}

/// Inner watcher loop: probes the initial `Active`, then subscribes
/// to `ActiveChanged` and forwards transitions.
async fn run_watcher(conn: zbus::Connection, tx: mpsc::Sender<LockEvent>) -> Result<()> {
    let proxy = Proxy::new(
        &conn,
        "org.gnome.ScreenSaver",
        "/org/gnome/ScreenSaver",
        "org.gnome.ScreenSaver",
    )
    .await
    .context("create ScreenSaver proxy")?;

    // `GetActive` returns bool. Default to `false` (unlocked) if the
    // property/method is missing — matches the conservative "skip the
    // feature, don't disable the tray" stance in network.rs.
    let initial_locked: bool = proxy.get_property("Active").await.unwrap_or(false);

    if initial_locked {
        let _ = tx.send(LockEvent::ScreenLock(true)).await;
    }

    // Subscribe to ActiveChanged. If the signal isn't available
    // (screensaver daemon without it, headless compositor) we exit
    // silently — `state.screen_locked` stays at its default.
    let mut stream = match proxy.receive_signal("ActiveChanged").await {
        Ok(s) => s,
        Err(e) => {
            log::debug!("screenlock monitor: ActiveChanged subscribe failed: {e}");
            return Ok(());
        }
    };

    let mut last_locked = initial_locked;
    while let Some(msg) = stream.next().await {
        // ActiveChanged signal payload is `(b: active)` per the spec.
        let new_locked: bool = match msg.body().deserialize() {
            Ok(v) => v,
            Err(_) => continue,
        };
        if new_locked == last_locked {
            continue;
        }
        last_locked = new_locked;
        let _ = tx.send(LockEvent::ScreenLock(new_locked)).await;
        if !new_locked {
            // Unlock — same pattern as the network monitor on
            // reconnect: emit ForceRefresh so the orchestrator
            // picks up where it left off rather than waiting out
            // the backoff.
            let _ = tx.send(LockEvent::ForceRefresh).await;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_event_eq() {
        assert_eq!(LockEvent::ScreenLock(true), LockEvent::ScreenLock(true));
        assert_ne!(LockEvent::ScreenLock(true), LockEvent::ScreenLock(false));
        assert_eq!(LockEvent::ForceRefresh, LockEvent::ForceRefresh);
    }

    #[test]
    fn lock_event_variants_are_distinct() {
        // Same rationale as `network::NetEvent::net_event_variants_are_distinct`:
        // if any two variants compare equal the orchestrator's dedupe
        // (if added later) would silently drop events.
        assert_ne!(LockEvent::ScreenLock(true), LockEvent::ForceRefresh);
        assert_ne!(LockEvent::ScreenLock(false), LockEvent::ForceRefresh);
        assert_ne!(LockEvent::ScreenLock(true), LockEvent::ScreenLock(false));
    }

    #[test]
    fn lock_event_clone_preserves_variant() {
        let a = LockEvent::ScreenLock(true);
        let b = a.clone();
        assert_eq!(a, b);

        let c = LockEvent::ForceRefresh;
        let d = c.clone();
        assert_eq!(c, d);
    }

    #[test]
    fn lock_event_debug_is_human_readable() {
        assert_eq!(format!("{:?}", LockEvent::ScreenLock(true)), "ScreenLock(true)");
        assert_eq!(format!("{:?}", LockEvent::ScreenLock(false)), "ScreenLock(false)");
        assert_eq!(format!("{:?}", LockEvent::ForceRefresh), "ForceRefresh");
    }

    #[test]
    fn lock_event_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<LockEvent>();
    }
}
