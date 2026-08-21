//! Network connectivity monitoring via NetworkManager.
//!
//! gjs parity: matches the gjs `setupNetworkMonitor()` block.
//! Subscribes to `org.freedesktop.NetworkManager` `StateChanged`
//! signals and reports online/offline transitions to the main loop
//! via an mpsc channel.
//!
//! Behavior:
//!   - On startup: query current connectivity; if offline, send
//!     `Connectivity(false)` immediately so the tray skips the
//!     first fetch.
//!   - On transition to offline: send `Connectivity(false)`. The
//!     main loop cancels any pending fetch, shows the offline icon,
//!     and skips scheduling the next poll.
//!   - On transition to online: send `Connectivity(true)` AND
//!     `ForceRefresh` so the loop restarts immediately (skipping
//!     the exponential backoff).
//!
//! If NetworkManager isn't reachable (no NM daemon, headless CI),
//! we just stay online — gjs does the same.

use anyhow::{Context, Result};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use zbus::{connection::Builder as ConnectionBuilder, Proxy};

/// Network events delivered to the main loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetEvent {
    /// Connectivity changed. `online = false` → skip polling,
    /// show offline icon. `online = true` → resume polling.
    Connectivity(bool),
    /// Force a refresh right now (used on reconnect — gjs uses
    /// `refresh(true)` which skips backoff).
    ForceRefresh,
}

/// Spawn a tokio task that watches NetworkManager and feeds
/// events to `tx`. The task lives for the duration of the program;
/// if NM isn't available it exits silently (graceful degradation
/// — gjs does the same).
pub async fn spawn_watcher(tx: mpsc::Sender<NetEvent>) -> Result<()> {
    let conn = match ConnectionBuilder::system()
        .context("connecting to system D-Bus")?
        .build()
        .await
    {
        Ok(c) => c,
        Err(e) => {
            log::debug!("network monitor: no system bus ({e}); offline detection disabled");
            return Ok(());
        }
    };
    tokio::spawn(async move {
        if let Err(e) = run_watcher(conn, tx).await {
            log::debug!("network monitor: watcher exited: {e:#}");
        }
    });
    Ok(())
}

/// Inner watcher loop: probes the initial state, then subscribes
/// to NM's StateChanged signal and forwards transitions.
async fn run_watcher(conn: zbus::Connection, tx: mpsc::Sender<NetEvent>) -> Result<()> {
    let nm_proxy = Proxy::new(
        &conn,
        "org.freedesktop.NetworkManager",
        "/org/freedesktop/NetworkManager",
        "org.freedesktop.NetworkManager",
    )
    .await
    .context("create NM proxy")?;

    // NM_STATE_CONNECTED_GLOBAL (70) = fully online.
    let initial_state: u32 = nm_proxy.get_property("State").await.unwrap_or(70);
    let initially_online = initial_state == 70;
    if !initially_online {
        let _ = tx.send(NetEvent::Connectivity(false)).await;
    }

    // Subscribe to StateChanged. If the call fails (no NM, no
    // permission) we just exit silently.
    let mut stream = match nm_proxy.receive_signal("StateChanged").await {
        Ok(s) => s,
        Err(e) => {
            log::debug!("network monitor: StateChanged subscribe failed: {e}");
            return Ok(());
        }
    };

    let mut last_state = initial_state;
    while let Some(msg) = stream.next().await {
        let new_state: u32 = match msg.body().deserialize() {
            Ok(v) => v,
            Err(_) => continue,
        };
        if new_state == last_state { continue; }
        last_state = new_state;
        let online = new_state == 70;
        let _ = tx.send(NetEvent::Connectivity(online)).await;
        if online {
            let _ = tx.send(NetEvent::ForceRefresh).await;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn net_event_eq() {
        assert_eq!(NetEvent::Connectivity(true), NetEvent::Connectivity(true));
        assert_ne!(NetEvent::Connectivity(true), NetEvent::Connectivity(false));
        assert_eq!(NetEvent::ForceRefresh, NetEvent::ForceRefresh);
    }
}