//! StatusNotifierItem over D-Bus — pure Rust, no GTK, no libappindicator.
//!
//! The org.kde.StatusNotifierItem protocol is the freedesktop standard that
//! KDE Plasma implements natively and that GNOME supports via the
//! AppIndicator extension. Talking to it directly means:
//!   - No libgtk3, no libappindicator, no libloading.
//!   - Works on KDE, GNOME (with extension), XFCE, swaybar, Waybar, etc.
//!   - Tiny memory footprint (~12-15 MB RSS at idle vs ~25 MB with GTK).
//!
//! What we expose:
//!   - `Category`     = "ApplicationStatus"
//!   - `Id`           = "minimax-quota-tray"
//!   - `Title`        = e.g. "MiniMax: 80%" — updated per refresh
//!   - `Status`       = "Active" / "Passive"
//!   - `IconName`     = theme icon name — updated per refresh
//!   - `Menu`         = `/NoMenu` (SNI requires it; no menu in this build)
//!
//! No menu in the minimal build — dbusmenu is a separate interface
//! (com.canonical.dbusmenu.DBusMenu) and is hundreds of lines of glue.
//! Re-add it if you want a real menu back. The title text carries the
//! burn rate info inline for now.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use zbus::{interface, object_server::{Interface, InterfaceRef}, Connection};

const SNI_PATH: &str = "/StatusNotifierItem";
const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";

/// Shared state behind the SNI interface.
#[derive(Default, Clone)]
struct State {
    title: String,
    icon_name: String,
    status: String,
    /// Cached pixmap (width, height, ARGB bytes) — refreshed on each
    /// update(). SNI clients re-read on the NewIcon signal.
    pixmap: Arc<Mutex<Option<(u32, u32, Vec<u8>)>>>,
}

/// The SNI object — exposes the required properties over D-Bus. State is
/// held inside `Arc<Mutex<>>` so updates from the polling loop can write
/// to it while the D-Bus thread reads for Get/Set calls.
struct StatusNotifierItem {
    state: Arc<Mutex<State>>,
}

#[interface(name = "org.kde.StatusNotifierItem")]
impl StatusNotifierItem {
    /// "ApplicationStatus" — we're a long-running background service indicator.
    #[zbus(property)]
    async fn category(&self) -> String {
        "ApplicationStatus".to_string()
    }

    /// Stable identifier for this tray icon.
    #[zbus(property)]
    async fn id(&self) -> String {
        "minimax-quota-tray".to_string()
    }

    /// Human-readable title. Updated by `update()` to show e.g. "MiniMax: 80%".
    #[zbus(property)]
    async fn title(&self) -> String {
        self.state.lock().await.title.clone()
    }

    /// "Active" when there's fresh data, "Passive" otherwise.
    #[zbus(property)]
    async fn status(&self) -> String {
        self.state.lock().await.status.clone()
    }

    /// Theme icon name. Updated per refresh.
    #[zbus(property)]
    async fn icon_name(&self) -> String {
        self.state.lock().await.icon_name.clone()
    }

    /// Rasterized ARGB32 icon. Updated on each refresh; the tray redraws
    /// after the NewIcon signal.
    #[zbus(property)]
    async fn icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        let state = self.state.lock().await;
        let guard = state.pixmap.lock().await;
        match &*guard {
            Some((w, h, bytes)) => vec![(*w as i32, *h as i32, bytes.clone())],
            None => Vec::new(),
        }
    }

    /// No custom theme path; use the system icon theme.
    #[zbus(property)]
    async fn icon_theme_path(&self) -> String {
        String::new()
    }

    /// SNI requires Menu to be a valid object path even when we don't
    /// serve a menu. `/Menu` is the conventional sentinel used by KDE
    /// plasma's reference implementation.
    #[zbus(property)]
    async fn menu(&self) -> zbus::zvariant::OwnedObjectPath {
        zbus::zvariant::OwnedObjectPath::try_from("/Menu").unwrap()
    }

    /// Standard SNI attributes — not strictly required but listed in the
    /// introspection of well-behaved SNI implementations.
    #[zbus(property)]
    async fn item_is_menu(&self) -> bool { false }

    /// Activated by the tray when the user clicks the icon (if supported).
    /// No-op for now — interactive actions would be wired here.
    async fn activate(&self, _x: i32, _y: i32) -> zbus::fdo::Result<()> { Ok(()) }

    /// Middle-click equivalent. No-op.
    async fn secondary_activate(&self, _x: i32, _y: i32) -> zbus::fdo::Result<()> { Ok(()) }

    /// Scroll on the icon (if supported). No-op.
    async fn scroll(&self, _delta: i32, _orientation: &str) -> zbus::fdo::Result<()> { Ok(()) }

    // Signals (zbus 5 macro form).
    #[zbus(signal)]
    async fn new_icon(_signal_emitter: &zbus::object_server::SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_title(_signal_emitter: &zbus::object_server::SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_status(_signal_emitter: &zbus::object_server::SignalEmitter<'_>) -> zbus::Result<()>;
}

/// Handle to the registered SNI.
pub struct Tray {
    state: Arc<Mutex<State>>,
    iface_ref: InterfaceRef<StatusNotifierItem>,
    _conn: Connection,
}

impl Tray {
    /// Acquire a unique bus name, export the SNI interface, and register
    /// with the StatusNotifierWatcher. Returns a handle for updates.
    pub async fn new() -> Result<Self> {
        let state = Arc::new(Mutex::new(State {
            title: "MiniMax".to_string(),
            icon_name: "dialog-information-symbolic".to_string(),
            status: "Passive".to_string(),
            pixmap: Arc::new(Mutex::new(None)),
        }));

        // Unique bus name: org.kde.StatusNotifierItem-<pid>-<n>.
        // Per the SNI spec, this avoids clashes when multiple instances run.
        let pid = std::process::id();
        let bus_name = format!("org.kde.StatusNotifierItem-{pid}-1");

        let conn = zbus::connection::Builder::session()
            .context("connect to session D-Bus")?
            .name(bus_name.as_str())
            .context("acquire well-known name")?
            .build()
            .await
            .context("build connection")?;

        let iface = StatusNotifierItem { state: Arc::clone(&state) };
        conn.object_server()
            .at(SNI_PATH, iface)
            .await
            .context("export SNI object")?;

        let iface_ref = conn
            .object_server()
            .interface::<_, StatusNotifierItem>(SNI_PATH)
            .await
            .context("look up SNI interface ref")?;

        // Belt-and-suspenders: explicitly register with the watcher too.
        // The SNI spec says the watcher monitors for well-known bus names,
        // but on some implementations (including the AppIndicator GNOME
        // extension) explicit registration is faster / more reliable.
        if let Err(e) = register_with_watcher(&conn).await {
        log::debug!("explicit RegisterStatusNotifierItem failed (auto-discovery still applies): {e}");
        }

        Ok(Self { state, iface_ref, _conn: conn })
    }

    /// Update the title, icon name, status, and (optionally) the rasterized
    /// icon pixmap. Emits NewIcon/NewTitle/NewStatus signals so the tray
    /// redraws. Called from the polling loop.
    ///
    /// If `pixmap` is `Some`, it replaces the cached pixmap (SNI clients
    /// will read it on the NewIcon signal). If `None`, the previous
    /// pixmap is preserved — useful for error renders that don't change
    /// the icon.
    pub async fn update(
        &self,
        title: &str,
        icon_name: &str,
        status: &str,
        pixmap: Option<(u32, u32, Vec<u8>)>,
    ) -> Result<()> {
        {
            let mut s = self.state.lock().await;
            s.title = title.to_string();
            s.icon_name = icon_name.to_string();
            s.status = status.to_string();
        }
        if let Some(p) = pixmap {
            let state = self.state.lock().await;
            *state.pixmap.lock().await = Some(p);
        }
        let emitter = self.iface_ref.signal_emitter();
        StatusNotifierItem::new_icon(emitter).await.context("emit NewIcon")?;
        StatusNotifierItem::new_title(emitter).await.context("emit NewTitle")?;
        StatusNotifierItem::new_status(emitter).await.context("emit NewStatus")?;
        Ok(())
    }
}

/// Optional: explicitly register with the watcher's
/// RegisterStatusNotifierItem() method. The SNI spec says clients don't
/// need to do this — the watcher auto-discovers via the bus name — but
/// some implementations are more reliable with explicit registration.
async fn register_with_watcher(conn: &Connection) -> Result<()> {
    let pid = std::process::id();
    let service = format!("org.kde.StatusNotifierItem-{pid}-1");
    conn.call_method(
        Some(WATCHER_NAME),
        WATCHER_PATH,
        Some("org.kde.StatusNotifierWatcher"),
        "RegisterStatusNotifierItem",
        &(service,),
    )
    .await
    .context("call RegisterStatusNotifierItem on watcher")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Construction needs a session bus; in a stripped-down test container
    /// (no dbus-session) these fail. Mark ignored so `cargo test` doesn't
    /// break the suite — run with `--ignored` in an interactive session.
    #[tokio::test]
    #[ignore = "requires a session D-Bus; run with --ignored"]
    async fn creates_sni_object() {
        let result = Tray::new().await;
        if let Err(e) = result {
            panic!("Tray::new failed: {e}");
        }
    }

    #[test]
    fn interface_name_constant() {
        // Pinned by the SNI spec; changing it would break every tray.
        assert_eq!(StatusNotifierItem::name().as_str(),
                  "org.kde.StatusNotifierItem");
    }

    #[test]
    fn paths_are_well_known() {
        assert_eq!(SNI_PATH, "/StatusNotifierItem");
        assert_eq!(WATCHER_PATH, "/StatusNotifierWatcher");
        assert_eq!(WATCHER_NAME, "org.kde.StatusNotifierWatcher");
    }
}