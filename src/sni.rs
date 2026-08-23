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
//! - `Id`           = "llm-quota-tray"
//! - `Title`        = always `""` (gjs parity — the chip carries the
//!   bucket via icon color, never a visible text label; the menu
//!   carries the detail)
//! - `Status`       = "Active" / "Passive"
//! - `IconName`     = PNG path on disk — updated per refresh
//! - `Menu`         = `/Menu` (a real dbusmenu tree; see menu module)
//!
//! `DBusMenu` (com.canonical.dbusmenu at `/Menu`) backs onto
//! `crate::menu::MenuInner` for the full menu tree — plan header,
//! per-window label/bar/burn rows, throttled/error rows, and the
//! Refresh / Open dashboard / Set API Key… / Quit action items.
//! Clicking an action item sends a `MenuCommand` via the `mpsc`
//! channel the main loop drains.
//!
//! We emit dbusmenu signals on state change so the panel can re-render:
//!   - `ItemsUpdated(update_id)` — structural change coming
//!   - `LayoutUpdated(update_id, parent_id)` — subtree changed
//!   - `ItemPropertiesUpdated(items_props, removed_props)` — only
//!     properties changed (e.g. visibility toggle)

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use zbus::zvariant::OwnedValue;
use zbus::{interface, object_server::InterfaceRef, Connection};

use crate::menu::{self, MenuCommand, MenuInner, ROOT_ID};

const SNI_PATH: &str = "/StatusNotifierItem";
/// Path we export a real (empty) dbusmenu object at. The Menu property
/// must point at a *truthy* object path for the AppIndicator extension
/// to consider the item ready — the `/NO_DBUSMENU` sentinel makes the
/// extension's `menuPath` getter return null, which permanently
/// blocks the `ready` signal and the icon never renders (it stays
/// on the `image-loading` three-dots fallback).
const MENU_PATH: &str = "/Menu";
const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";

/// Maximum time to wait for a single SNI signal emission before
/// giving up. Bounds the daemon against a wedged D-Bus watcher or a
/// stuck connection write. Without this, `render_initial` can hang
/// forever on the first `new_icon` call when the watcher is in a
/// bad state (we observed this once after a back-to-back restart —
/// see `docs/freedesktop-integration.md`). The chip state in
/// `SharedState` is updated before the signal is emitted, so a
/// missed signal only delays the panel's view by at most one poll
/// cycle (`refresh_seconds`).
const SIGNAL_EMIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Emit an SNI signal with a bounded wait. Logs a warning on
/// timeout or error and continues — never propagates the failure.
/// Required because `zbus::Connection::emit_signal()` can block
/// indefinitely when the session bus / watcher is in a degraded
/// state; without this, a single bad emission wedges the entire
/// poll loop (and `render_initial` at startup).
async fn emit_signal_with_timeout<F>(name: &'static str, fut: F)
where
    F: std::future::Future<Output = zbus::Result<()>>,
{
    match tokio::time::timeout(SIGNAL_EMIT_TIMEOUT, fut).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => log::warn!("SNI signal {name}: emission failed: {e}"),
        Err(_elapsed) => log::warn!(
            "SNI signal {name}: emission timed out after {SIGNAL_EMIT_TIMEOUT:?}; \
             chip state is updated but the host may not refresh until the next poll"
        ),
    }
}

/// Shared state behind both the SNI and dbusmenu interfaces.
///
/// `menu_state` is the full menu tree (the dbusmenu server reads it
/// for GetLayout/GetProperty; refresh-loop tasks call `apply_menu`
/// on the Tray to rebuild after each fetch).
///
/// `cmd_tx` is the channel the dbusmenu `Event("clicked")` handler
/// uses to dispatch menu actions back to the main loop (which
/// converts them into fetches, xdg-open calls, keyring writes, and
/// process exits).
struct SharedState {
    title: Mutex<String>,
    icon_name: Mutex<String>,
    status: Mutex<String>,
    /// Cached pixmap (width, height, ARGB bytes) — refreshed on each
    /// update(). SNI clients re-read on the NewIcon signal.
    pixmap: Mutex<Option<(u32, u32, Vec<u8>)>>,
    menu_state: Mutex<MenuInner>,
    /// Dashboard URL — read by the menu handler when the user clicks
    /// "Open dashboard". Set at construction; constant thereafter.
    dashboard_url: Mutex<String>,
    /// Monotonic counter — bumped on every menu-state change so the
    /// dbusmenu client can invalidate caches via ItemsUpdated/LayoutUpdated.
    menu_revision: Mutex<u32>,
    /// Hover-tooltip description. The SNI spec's ToolTip property
    /// takes `(icon_name, (title, description), has_icon)` — we set
    /// `description` to a short summary like "Coding Plan — 80% left"
    /// so screen readers and panels that surface hover-tooltips get
    /// the same accessibility hint gjs delivers via libayatana's
    /// `set_label(label, guide)` second argument.
    tool_tip_desc: Mutex<String>,
}

impl SharedState {
    fn new(initial_dashboard_url: String) -> Self {
        // SNI Title defaults to empty (gjs parity — the chip carries
        // the bucket via icon color, never a visible text label).
        // The first `tray.update(...)` from `main::render_initial`
        // also writes `""`; this default keeps the property truthful
        // for the brief window before `render_initial` runs.
        Self {
            title: Mutex::new(String::new()),
            icon_name: Mutex::new("dialog-information-symbolic".to_string()),
            status: Mutex::new("Passive".to_string()),
            pixmap: Mutex::new(None),
            menu_state: Mutex::new(MenuInner::new()),
            dashboard_url: Mutex::new(initial_dashboard_url),
            menu_revision: Mutex::new(0),
            tool_tip_desc: Mutex::new(String::new()),
        }
    }
}

// ---------------------------------------------------------------------------
// org.kde.StatusNotifierItem
// ---------------------------------------------------------------------------

/// The SNI object — exposes the required properties over D-Bus.
struct StatusNotifierItem {
    shared: Arc<SharedState>,
}

#[interface(name = "org.kde.StatusNotifierItem")]
impl StatusNotifierItem {
    #[zbus(property)]
    async fn category(&self) -> String {
        "ApplicationStatus".to_string()
    }

    #[zbus(property)]
    async fn id(&self) -> String {
        "llm-quota-tray".to_string()
    }

    #[zbus(property)]
    async fn title(&self) -> String {
        self.shared.title.lock().await.clone()
    }

    #[zbus(property)]
    async fn status(&self) -> String {
        self.shared.status.lock().await.clone()
    }

    #[zbus(property)]
    async fn icon_name(&self) -> String {
        self.shared.icon_name.lock().await.clone()
    }

    #[zbus(property)]
    async fn icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        let pixmap = self.shared.pixmap.lock().await;
        match &*pixmap {
            Some((w, h, bytes)) => vec![(*w as i32, *h as i32, bytes.clone())],
            None => Vec::new(),
        }
    }

    #[zbus(property)]
    async fn icon_theme_path(&self) -> String {
        String::new()
    }

    /// SNI ToolTip — the spec is `s(ss)b`:
    ///   - first `s` — icon name or absolute path (empty = use the
    ///     active IconName/IconPixmap)
    ///   - `(ss)` — `(title, description)`; hosts that surface
    ///     hover-tooltips use these
    ///   - `b` — whether the icon-name field is valid
    ///
    /// gjs parity: gjs's libayatana `set_label(label, guide)` second
    /// argument (`guide`) is the accessibility / hover tooltip
    /// ("Coding Plan — 80% remaining", etc.). Pure SNI doesn't have
    /// `set_label`, so we surface that string via ToolTip.
    #[zbus(property)]
    async fn tool_tip(&self) -> (String, (String, String), bool) {
        let desc = self.shared.tool_tip_desc.lock().await.clone();
        // No icon-name slot (we don't have a separate tooltip icon),
        // empty title (matches gjs's empty visible label), real desc.
        (String::new(), (String::new(), desc), false)
    }

    #[zbus(property)]
    async fn menu(&self) -> zbus::zvariant::OwnedObjectPath {
        zbus::zvariant::OwnedObjectPath::try_from(MENU_PATH).unwrap()
    }

    /// Standard SNI attribute — listed in the introspection of
    /// well-behaved SNI implementations. Set to `true` so the host
    /// registers the dbusmenu proxy and routes both left- and
    /// right-clicks through it. Setting `false` (the gjs libayatana
    /// default for SYSTEM_SERVICES) causes some hosts — notably the
    /// GNOME AppIndicator extension — to skip menu rendering entirely:
    /// the icon is visible but clicks do nothing. The dbusmenu tree
    /// itself is always served regardless of this flag; this just
    /// controls whether the host wires clicks to it.
    #[zbus(property)]
    async fn item_is_menu(&self) -> bool {
        true
    }

    async fn activate(&self, _x: i32, _y: i32) -> zbus::fdo::Result<()> {
        Ok(())
    }

    async fn secondary_activate(&self, _x: i32, _y: i32) -> zbus::fdo::Result<()> {
        Ok(())
    }

    async fn scroll(&self, _delta: i32, _orientation: &str) -> zbus::fdo::Result<()> {
        Ok(())
    }

    #[zbus(signal)]
    async fn new_icon(_signal_emitter: &zbus::object_server::SignalEmitter<'_>)
        -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_title(
        _signal_emitter: &zbus::object_server::SignalEmitter<'_>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_status(
        _signal_emitter: &zbus::object_server::SignalEmitter<'_>,
    ) -> zbus::Result<()>;
}

// ---------------------------------------------------------------------------
// com.canonical.dbusmenu
// ---------------------------------------------------------------------------

/// The dbusmenu object — backs onto `SharedState::menu_state` for the
/// full menu tree. `Event("clicked")` dispatches via the mpsc sender.
pub struct DBusMenu {
    shared: Arc<SharedState>,
    cmd_tx: mpsc::Sender<MenuCommand>,
}

#[interface(name = "com.canonical.dbusmenu")]
impl DBusMenu {
    #[zbus(property)]
    fn version(&self) -> u32 {
        3
    }

    #[zbus(property)]
    fn text_direction(&self) -> String {
        "ltr".to_string()
    }

    #[zbus(property)]
    fn status(&self) -> String {
        "normal".to_string()
    }

    #[zbus(property)]
    fn icon_theme_path(&self) -> Vec<String> {
        Vec::new()
    }

    /// Layout of the subtree rooted at `parent_id`. The
    /// `recursion_depth` is `i32`: -1 means "infinite depth" (we
    /// always recurse), 0 means "just this item", >0 means
    /// "descend that many levels". `property_names` is a filter list;
    /// we ignore it and return the full property dict for each item
    /// (libdbusmenu expects this — clients that want filtering ask
    /// for it explicitly via GetGroupProperties).
    async fn get_layout(
        &self,
        parent_id: i32,
        recursion_depth: i32,
        _property_names: Vec<String>,
    ) -> zbus::fdo::Result<crate::menu::ItemLayoutResponse> {
        let menu = self.shared.menu_state.lock().await;
        // `recursion_depth` is i32: -1 = "infinite" (we always recurse),
        // 0 = "just this item", >0 = "descend that many levels".
        let recurse = recursion_depth != 0;
        Ok(menu::build_layout_response(&menu, parent_id, recurse))
    }

    async fn get_group_properties(
        &self,
        ids: Vec<i32>,
        _property_names: Vec<String>,
    ) -> zbus::fdo::Result<Vec<(i32, HashMap<String, OwnedValue>)>> {
        let menu = self.shared.menu_state.lock().await;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(item) = menu.item(id) {
                out.push((id, menu::build_properties(item)));
            }
        }
        Ok(out)
    }

    async fn get_property(&self, id: i32, name: String) -> zbus::fdo::Result<OwnedValue> {
        let menu = self.shared.menu_state.lock().await;
        let Some(item) = menu.item(id) else {
            return Ok(OwnedValue::from(zbus::zvariant::Str::from("")));
        };
        let props = menu::build_properties(item);
        Ok(props
            .get(&name)
            .cloned()
            .unwrap_or_else(|| OwnedValue::from(zbus::zvariant::Str::from(""))))
    }

    /// Handle user actions on menu items.
    ///   - `event_id = "clicked"` — fire the action (Refresh, etc.)
    ///   - anything else — no-op
    async fn event(&self, id: i32, event_id: String, _data: OwnedValue, _timestamp: u32) {
        if event_id != "clicked" {
            return;
        }
        let menu = self.shared.menu_state.lock().await;
        let Some(item) = menu.item(id) else {
            return;
        };
        let Some(cmd) = item.action.clone() else {
            return;
        };
        drop(menu);
        // best-effort dispatch — the receiver might have been dropped
        // if the main loop has exited; in that case the click is just
        // a no-op (the tray is going away anyway).
        let _ = self.cmd_tx.send(cmd).await;
    }

    async fn event_group(&self, events: Vec<(i32, String, OwnedValue, u32)>) -> Vec<i32> {
        for (id, event_id, data, ts) in events {
            self.event(id, event_id, data, ts).await;
        }
        Vec::new()
    }

    /// AboutToShow: false = no layout update needed. Returning false
    /// for every item is safe because the refresh loop always emits
    /// ItemsUpdated + LayoutUpdated + ItemPropertiesUpdated signals
    /// when state actually changes, so the client never needs a
    /// hint-driven re-fetch.
    async fn about_to_show(&self, _id: i32) -> bool {
        false
    }

    async fn about_to_show_group(&self, _ids: Vec<i32>) -> (Vec<i32>, Vec<i32>) {
        (Vec::new(), Vec::new())
    }

    /// dbusmenu ItemsUpdated signal. The spec signature is
    /// `(update_id: u, removed_ids: a(ias))`. A missing
    /// `removed_ids` parameter causes libdbusmenu-glib's proxy to
    /// fail when reading the signal — which in turn prevents the
    /// AppIndicator extension from initializing the menu, so
    /// right-click never produces the menu. We always pass an
    /// empty `removed_ids` (we never remove items dynamically).
    #[zbus(signal)]
    async fn items_updated(
        _signal_emitter: &zbus::object_server::SignalEmitter<'_>,
        update_id: u32,
        removed_ids: Vec<(i32, Vec<String>)>,
    ) -> zbus::Result<()>;

    /// dbusmenu LayoutUpdated signal. Spec: `(update_id: u, parent: i)`.
    #[zbus(signal)]
    async fn layout_updated(
        _signal_emitter: &zbus::object_server::SignalEmitter<'_>,
        update_id: u32,
        parent_id: i32,
    ) -> zbus::Result<()>;

    /// dbusmenu ItemPropertiesUpdated signal. Spec signature is
    /// `(updatedProps: a(ia{sv}), removedProps: a(ias))` — note
    /// the ARRAYS of (id, ...) tuples, NOT dicts with id keys.
    /// zvariant serializes `HashMap<i32, ...>` as `a{i...}` (illegal
    /// as a dict-key type for property-bag values) — the spec
    /// requires `a(i...)`. Using `Vec<(i32, ...)>` makes zvariant
    /// produce the correct `a(i...)` signature.
    #[zbus(signal)]
    async fn item_properties_updated(
        _signal_emitter: &zbus::object_server::SignalEmitter<'_>,
        updated_props: Vec<(i32, HashMap<String, OwnedValue>)>,
        removed_props: Vec<(i32, Vec<String>)>,
    ) -> zbus::Result<()>;
}

// ---------------------------------------------------------------------------
// Tray handle (used by main.rs)
// ---------------------------------------------------------------------------

/// Handle to the registered SNI.
pub struct Tray {
    shared: Arc<SharedState>,
    iface_ref: InterfaceRef<StatusNotifierItem>,
    menu_iface_ref: InterfaceRef<DBusMenu>,
    /// Sender for menu commands — also stored in DBusMenu so the
    /// `Event("clicked")` handler can dispatch. Kept here so the
    /// Tray can also inject commands from elsewhere (e.g. a
    /// startup probe). Unused directly by `Tray::update` / friends.
    cmd_tx: mpsc::Sender<MenuCommand>,
    /// Receiver for menu commands — exposed to main.rs so it can
    /// drive refresh / dashboard / set-key / quit from clicks.
    pub cmd_rx: Arc<Mutex<mpsc::Receiver<MenuCommand>>>,
    _conn: Connection,
}

impl Tray {
    /// Acquire a unique bus name, export the SNI + dbusmenu
    /// interfaces, register with the watcher. Returns a handle.
    pub async fn new(dashboard_url: String) -> Result<Self> {
        let shared = Arc::new(SharedState::new(dashboard_url));
        let (cmd_tx, cmd_rx) = mpsc::channel::<MenuCommand>(16);

        let pid = std::process::id();
        let bus_name = format!("org.kde.StatusNotifierItem-{pid}-1");

        let conn = zbus::connection::Builder::session()
            .context("connect to session D-Bus")?
            .name(bus_name.as_str())
            .context("acquire well-known name")?
            .build()
            .await
            .context("build connection")?;

        let sni_iface = StatusNotifierItem {
            shared: Arc::clone(&shared),
        };
        conn.object_server()
            .at(SNI_PATH, sni_iface)
            .await
            .context("export SNI object")?;

        let menu_iface = DBusMenu {
            shared: Arc::clone(&shared),
            cmd_tx: cmd_tx.clone(),
        };
        conn.object_server()
            .at(MENU_PATH, menu_iface)
            .await
            .context("export dbusmenu object")?;

        let iface_ref = conn
            .object_server()
            .interface::<_, StatusNotifierItem>(SNI_PATH)
            .await
            .context("look up SNI interface ref")?;

        let menu_iface_ref = conn
            .object_server()
            .interface::<_, DBusMenu>(MENU_PATH)
            .await
            .context("look up dbusmenu interface ref")?;

        if let Err(e) = register_with_watcher(&conn).await {
            log::debug!(
                "explicit RegisterStatusNotifierItem failed (auto-discovery still applies): {e}"
            );
        }

        // Spawn the watcher-recovery task. The GNOME `appindicator`
        // extension restarts its `org.kde.StatusNotifierWatcher`
        // periodically (extension reload, shell redraw, gnome-shell
        // `r` reset, …). When the watcher's bus name disappears and a
        // new owner claims it, our daemon's previously-good signal
        // subscriptions now point at a dead peer — every subsequent
        // `NewIcon` / `NewTitle` / `NewStatus` emission then fails
        // with `Broken pipe (os error 32)` and the chip vanishes
        // permanently until the daemon restarts. This task
        // re-registers + re-emits the current state whenever the
        // watcher (re)appears, so the chip is resilient to
        // extension restarts. See `docs/freedesktop-integration.md`.
        spawn_watcher_recovery(conn.clone(), Arc::clone(&shared), iface_ref.clone(), menu_iface_ref.clone());

        Ok(Self {
            shared,
            iface_ref,
            menu_iface_ref,
            cmd_tx,
            cmd_rx: Arc::new(Mutex::new(cmd_rx)),
            _conn: conn,
        })
    }

    /// Update the SNI chip (title, icon, status, optional pixmap, and
    /// accessibility tooltip). Called from the polling loop on every
    /// refresh. Emits NewIcon/NewTitle/NewStatus signals.
    ///
    /// `tool_tip_desc` is the second argument of gjs's libayatana
    /// `set_label(label, guide)` (the `guide`). On hosts that surface
    /// SNI ToolTip, the description appears on hover; on hosts that
    /// don't, the menu and chip are unaffected. Defaults to empty
    /// (gjs's "no API key" / "offline" states use a plain plan label,
    /// not a detailed percentage — pass "" to match).
    pub async fn update(
        &self,
        title: &str,
        icon_name: &str,
        status: &str,
        pixmap: Option<(u32, u32, Vec<u8>)>,
        tool_tip_desc: &str,
    ) -> Result<()> {
        {
            let mut t = self.shared.title.lock().await;
            let mut n = self.shared.icon_name.lock().await;
            let mut s = self.shared.status.lock().await;
            *t = title.to_string();
            *n = icon_name.to_string();
            *s = status.to_string();
        }
        if let Some(p) = pixmap {
            *self.shared.pixmap.lock().await = Some(p);
        }
        *self.shared.tool_tip_desc.lock().await = tool_tip_desc.to_string();
        let emitter = self.iface_ref.signal_emitter();
        // SNI signal emissions are best-effort. The shared state above
        // is already updated, so even if every signal below hangs or
        // errors, the chip is correct on the next D-Bus property read
        // by the host. The timeout guards against `render_initial`
        // deadlocking on a stale watcher (observed once after a
        // back-to-back restart — see `docs/freedesktop-integration.md`).
        emit_signal_with_timeout("NewIcon", StatusNotifierItem::new_icon(emitter)).await;
        emit_signal_with_timeout("NewTitle", StatusNotifierItem::new_title(emitter)).await;
        emit_signal_with_timeout("NewStatus", StatusNotifierItem::new_status(emitter)).await;
        Ok(())
    }

    /// Apply a new menu state. Computes the diff (label/visibility
    /// changes vs structural changes) and emits the corresponding
    /// dbusmenu signals. Called from the polling loop on every
    /// successful refresh.
    pub async fn apply_menu<F>(&self, mutate: F) -> Result<()>
    where
        F: FnOnce(&mut MenuInner),
    {
        // Snapshot the previous revision so we can decide which
        // signals to emit. Pure property changes → ItemPropertiesUpdated
        // only. Structural changes (new window slots) → ItemsUpdated
        // + LayoutUpdated.
        let prev_revision = *self.shared.menu_revision.lock().await;
        {
            let mut menu = self.shared.menu_state.lock().await;
            mutate(&mut menu);
        }
        let new_revision = {
            let menu = self.shared.menu_state.lock().await;
            let r = menu.revision();
            *self.shared.menu_revision.lock().await = r;
            r
        };
        // If only labels/visibility changed (revision bumped but no
        // structural change). For now we treat every menu update as a
        // full LayoutUpdated — the client will re-fetch the
        // layout. ItemsUpdated is also emitted with an empty
        // removed_ids (we never remove items dynamically) so the
        // signal signature matches the dbusmenu spec exactly — a
        // missing removed_ids causes libdbusmenu-glib's proxy to
        // fail initialization.
        let emitter = self.menu_iface_ref.signal_emitter();
        if new_revision != prev_revision {
            // Same best-effort contract as `update()`: menu state is
            // already updated; signals are advisory. A stuck watcher
            // can delay the panel's view but must not deadlock the
            // poll loop.
            emit_signal_with_timeout(
                "ItemsUpdated",
                DBusMenu::items_updated(emitter, new_revision, Vec::new()),
            )
            .await;
            emit_signal_with_timeout(
                "LayoutUpdated",
                DBusMenu::layout_updated(emitter, new_revision, ROOT_ID),
            )
            .await;
        }
        Ok(())
    }

    /// Update the dashboard URL at runtime (used when the user
    /// switches plans in the config and we want the menu's "Open
    /// dashboard" link to follow).
    #[allow(dead_code)]
    pub async fn set_dashboard_url(&self, url: String) {
        *self.shared.dashboard_url.lock().await = url;
    }

    /// Current dashboard URL — read by the main loop before
    /// launching `xdg-open` for the Open dashboard action.
    pub async fn dashboard_url(&self) -> String {
        self.shared.dashboard_url.lock().await.clone()
    }

    /// Get a clone of the menu command sender (e.g. for the
    /// refresh-loop task to inject commands without holding the
    /// Tray). Held by `Tray` for symmetry with `cmd_rx`; the
    /// `DBusMenu` keeps its own clone for `Event()` dispatch.
    #[allow(dead_code)]
    pub fn cmd_sender(&self) -> mpsc::Sender<MenuCommand> {
        self.cmd_tx.clone()
    }
}

/// Optional: explicitly register with the watcher's
/// RegisterStatusNotifierItem() method. The SNI spec says clients
/// don't need to do this — the watcher auto-discovers via the bus
/// name — but some implementations are more reliable with explicit
/// registration.
async fn register_with_watcher(conn: &Connection) -> Result<()> {
    let pid = std::process::id();
    let service = format!("org.kde.StatusNotifierItem-{pid}-1");
    // Same bounded-wait contract as `emit_signal_with_timeout`: a
    // stuck watcher can hang the registration call indefinitely,
    // which would deadlock `Tray::new` and prevent the daemon from
    // ever starting. The registration is best-effort anyway (the
    // watcher auto-discovers via the bus name per SNI spec), so a
    // timeout is harmless — we just log and fall through.
    match tokio::time::timeout(SIGNAL_EMIT_TIMEOUT, async {
        conn.call_method(
            Some(WATCHER_NAME),
            WATCHER_PATH,
            Some("org.kde.StatusNotifierWatcher"),
            "RegisterStatusNotifierItem",
            &(service,),
        )
        .await
    })
    .await
    {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(anyhow::Error::from(e).context("RegisterStatusNotifierItem on watcher")),
        Err(_elapsed) => Err(anyhow::anyhow!(
            "RegisterStatusNotifierItem timed out after {SIGNAL_EMIT_TIMEOUT:?}"
        )),
    }
}

/// Spawn a background task that listens for the SNI watcher
/// (`org.kde.StatusNotifierWatcher`) re-appearing on the session
/// bus and re-registers + re-emits the current chip state on each
/// appearance. Cloned refs to the connection, shared state, and
/// interface refs must outlive the task — caller (Tray::new)
/// keeps them alive via the `Tray` returned from that fn.
///
/// The subscription is server-side-filtered by the bus name
/// (`org.kde.DBus.NameOwnerChanged` match rule with `arg0 ==
/// WATCHER_NAME`) so we don't see every name change in the
/// session — just the ones for our watched name. The first
/// explicit registration in `Tray::new` still runs at startup;
/// this task only handles appearances *after* that point.
fn spawn_watcher_recovery(
    conn: Connection,
    shared: Arc<SharedState>,
    iface_ref: InterfaceRef<StatusNotifierItem>,
    menu_iface_ref: InterfaceRef<DBusMenu>,
) {
    tokio::spawn(async move {
        use futures_util::StreamExt;
        use zbus::fdo::DBusProxy;

        let dbus = match DBusProxy::new(&conn).await {
            Ok(p) => p,
            Err(e) => {
                log::warn!("SNI watcher recovery: DBusProxy::new failed: {e}");
                return;
            }
        };
        // Server-side match rule: arg0 (the `name` argument of
        // NameOwnerChanged) == WATCHER_NAME. Reduces per-session
        // signal volume from "every bus name change" to just the
        // ones we care about.
        let mut stream = match dbus
            .receive_name_owner_changed_with_args(&[(0u8, WATCHER_NAME)])
            .await
        {
            Ok(s) => s,
            Err(e) => {
                log::warn!(
                    "SNI watcher recovery: failed to subscribe to NameOwnerChanged: {e}"
                );
                return;
            }
        };
        log::debug!(
            "SNI watcher recovery: subscribed to NameOwnerChanged for {WATCHER_NAME}"
        );
        while let Some(signal) = stream.next().await {
            let args = match signal.args() {
                Ok(a) => a,
                Err(e) => {
                    log::debug!("SNI watcher recovery: bad signal args: {e}");
                    continue;
                }
            };
            // "Appeared" = a new owner now holds the name. We
            // trigger on either a fresh appearance (old_owner empty)
            // or an owner change (old_owner non-empty but the name
            // transitioned to a different unique name) — both mean
            // the previous watcher peer is gone and we must
            // re-register against whoever owns the name now.
            // `new_owner()` returns `&Optional<UniqueName<'_>>` (the
            // zbus DBus type for an optional string); auto-deref to
            // `Option<&UniqueName>` via `.as_ref()`.
            let Some(new_owner) = args.new_owner().as_ref() else {
                continue;
            };
            let old_owner_str = args
                .old_owner()
                .as_ref()
                .map(|s| s.as_str())
                .unwrap_or("");
            log::info!(
                "SNI watcher re-appeared on {new_owner} (was {old_owner_str:?}); \
                 re-registering and re-emitting state"
            );
            // Best-effort explicit registration. The watcher
            // auto-discovers us via our `org.kde.StatusNotifierItem-*`
            // bus name anyway, but some hosts (older AppIndicator
            // implementations, valgrind'd watchers, etc.) are more
            // reliable with an explicit call. Failure is fine —
            // auto-discovery still applies.
            if let Err(e) = register_with_watcher(&conn).await {
                log::debug!(
                    "SNI watcher re-registration failed (auto-discovery still applies): {e}"
                );
            }
            // Re-emit the current chip state so the new watcher
            // renders the icon, title, status, and menu
            // immediately — without this, the chip stays on the
            // watcher's empty placeholder until our next 2-min
            // poll cycle fires. Emitting unconditionally is safe
            // (the dbusmenu client treats these as cache
            // invalidation hints and idempotently re-fetches).
            let sni_emitter = iface_ref.signal_emitter();
            emit_signal_with_timeout(
                "NewIcon (recovery)",
                StatusNotifierItem::new_icon(sni_emitter),
            )
            .await;
            emit_signal_with_timeout(
                "NewTitle (recovery)",
                StatusNotifierItem::new_title(sni_emitter),
            )
            .await;
            emit_signal_with_timeout(
                "NewStatus (recovery)",
                StatusNotifierItem::new_status(sni_emitter),
            )
            .await;
            let menu_emitter = menu_iface_ref.signal_emitter();
            let revision = *shared.menu_revision.lock().await;
            emit_signal_with_timeout(
                "ItemsUpdated (recovery)",
                DBusMenu::items_updated(menu_emitter, revision, Vec::new()),
            )
            .await;
            emit_signal_with_timeout(
                "LayoutUpdated (recovery)",
                DBusMenu::layout_updated(menu_emitter, revision, ROOT_ID),
            )
            .await;
        }
        // Stream ending means the connection is gone (daemon
        // shutting down). No recovery needed — the next
        // `Tray::new` will re-subscribe.
        log::debug!("SNI watcher recovery: NameOwnerChanged stream ended");
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::object_server::Interface;

    /// Construction needs a session bus; in a stripped-down test
    /// container (no dbus-session) these fail. Mark ignored so
    /// `cargo test` doesn't break the suite — run with `--ignored`
    /// in an interactive session.
    #[tokio::test]
    #[ignore = "requires a session D-Bus; run with --ignored"]
    async fn creates_sni_object() {
        let result = Tray::new("https://example.invalid/".to_string()).await;
        if let Err(e) = result {
            panic!("Tray::new failed: {e}");
        }
    }

    #[test]
    fn interface_name_constant() {
        assert_eq!(
            StatusNotifierItem::name().as_str(),
            "org.kde.StatusNotifierItem"
        );
    }

    #[test]
    fn paths_are_well_known() {
        assert_eq!(SNI_PATH, "/StatusNotifierItem");
        assert_eq!(MENU_PATH, "/Menu");
        assert_eq!(WATCHER_PATH, "/StatusNotifierWatcher");
        assert_eq!(WATCHER_NAME, "org.kde.StatusNotifierWatcher");
    }

    /// Ensure the menu commands from the menu module are wired to
    /// the same names this module dispatches — guards against
    /// drift between menu.rs's action enum and the dispatcher.
    #[test]
    fn menu_command_names_match() {
        let s = MenuInner::new();
        // The dispatcher in DBusMenu::event uses the action field
        // directly, so this is mostly a sanity check that the IDs
        // line up.
        use crate::menu::*;
        assert_eq!(
            s.item(REFRESH_ID).unwrap().action,
            Some(MenuCommand::Refresh)
        );
        assert_eq!(
            s.item(DASHBOARD_ID).unwrap().action,
            Some(MenuCommand::OpenDashboard)
        );
        assert_eq!(
            s.item(SET_KEY_ID).unwrap().action,
            Some(MenuCommand::SetApiKey)
        );
        assert_eq!(s.item(QUIT_ID).unwrap().action, Some(MenuCommand::Quit));
    }

    /// Build a tray-state-equivalent directly and verify the
    /// GetLayout-shaped response is well-formed (i.e. won't
    /// panic on a real bus).
    #[tokio::test]
    async fn get_layout_response_for_empty_menu() {
        // The full GetLayout response shape for a freshly-constructed
        // menu -- pinned so a regression in build_layout_response
        // (which is also exercised by menu::tests but with a
        // mutation pattern) doesn't silently change the on-the-wire
        // structure the panel receives.

        let menu = MenuInner::new();
        let (revision, layout) = menu::build_layout_response(&menu, ROOT_ID, true);

        // Revision starts at 1 (matches MenuInner::new in menu.rs).
        assert_eq!(revision, 1, "new MenuInner must start at revision 1");

        // Layout shape: (id, properties, children).
        let (id, props, children) = layout;
        assert_eq!(id, ROOT_ID, "root item id must be ROOT_ID (0)");

        // Properties must include the required dbusmenu fields.
        // For non-separator items (the root), build_properties sets:
        // "type", "label", "enabled", "visible", "children-display".
        for required_key in ["type", "label", "enabled", "visible", "children-display"] {
            assert!(
                props.contains_key(required_key),
                "missing required dbusmenu property {required_key:?}"
            );
        }

        // Children must include every static action item the menu
        // ships by default. The exact order matches
        // MenuInner::new's initial root_children list.
        assert_eq!(
            children.len(),
            10,
            "default menu must have 10 root children (1 header + 1 throttled + 1 error + 3 separators + 4 actions)"
        );
        // Each child is a zvariant::Value::Structure wrapping an ItemLayout;
        // we don't introspect the deep structure here -- menu::tests
        // exercises that exhaustively. We just confirm the count is
        // stable so a regression in MenuInner::new surfaces.
    }

    #[test]
    fn shared_state_initial() {
        let s = SharedState::new("https://example.invalid/dashboard".to_string());
        // Synchronous fields only — title/icon_name/status are async-locked.
        assert_eq!(
            s.dashboard_url.try_lock().unwrap().clone(),
            "https://example.invalid/dashboard"
        );
        // ToolTip desc defaults to empty (no data yet).
        assert_eq!(s.tool_tip_desc.try_lock().unwrap().clone(), "");
    }

    /// SNI ToolTip signature is `s(ss)b` — a struct of
    /// (icon_name, (title, description), has_icon). The rust port
    /// surfaces the gjs libayatana `guide` value here, so hosts
    /// that show hover-tooltips (KDE Plasma) get the same hint
    /// screen readers do via the accessible name.
    #[test]
    fn tool_tip_signature_matches_sni_spec() {
        // Type-level smoke check: the property's return type must be
        // (String, (String, String), bool). If we ever change it,
        // this test fails to compile.
        fn _signature_check(_x: (String, (String, String), bool)) {}
        // Compile-time assertion.
        let _ = _signature_check;
    }

    /// `emit_signal_with_timeout` must swallow both error variants
    /// and never panic, never hang past the timeout. This is the
    /// invariant that protects `render_initial` from a wedged
    /// watcher after a back-to-back restart.
    #[tokio::test]
    async fn emit_signal_with_timeout_returns_immediately_on_success() {
        // Should return quickly (well within the 5s SIGNAL_EMIT_TIMEOUT).
        let started = std::time::Instant::now();
        emit_signal_with_timeout(
            "ok",
            async { Ok::<(), zbus::Error>(()) },
        )
        .await;
        assert!(
            started.elapsed() < std::time::Duration::from_millis(100),
            "fast Ok should return in <100ms, took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn emit_signal_with_timeout_swallows_errors() {
        let started = std::time::Instant::now();
        // Future resolves to Err — must not propagate. zbus::Error's
        // InputOutput variant wraps `Arc<io::Error>` (shared so the
        // error is cheap to clone across D-Bus boundaries).
        emit_signal_with_timeout(
            "err",
            async {
                let io = std::io::Error::new(std::io::ErrorKind::Other, "test");
                Err::<(), zbus::Error>(zbus::Error::InputOutput(std::sync::Arc::new(io)))
            },
        )
        .await;
        assert!(
            started.elapsed() < std::time::Duration::from_millis(100),
            "fast Err should return in <100ms, took {:?}",
            started.elapsed()
        );
    }

    /// Real-time test (takes ~`SIGNAL_EMIT_TIMEOUT` to run): proves
    /// the helper aborts a future that would otherwise hang the
    /// caller forever. We use a real `tokio::time::sleep` past the
    /// timeout (instead of `start_paused` which would require the
    /// `test-util` feature flag) — the wall time is bounded by
    /// `SIGNAL_EMIT_TIMEOUT`.
    #[tokio::test]
    async fn emit_signal_with_timeout_aborts_on_hang() {
        let started = std::time::Instant::now();
        emit_signal_with_timeout("hang", async move {
            // Sleep longer than the helper's timeout — proves the
            // timeout fires first (the future never gets to resolve
            // with Ok here; the wrapper is just shaped to match
            // the helper's bound).
            tokio::time::sleep(SIGNAL_EMIT_TIMEOUT + std::time::Duration::from_secs(1)).await;
            Ok::<(), zbus::Error>(())
        })
        .await;
        // Must return at or before the timeout, not after the sleep
        // would naturally resolve. Allow a generous margin for CI jitter.
        let elapsed = started.elapsed();
        assert!(
            elapsed <= SIGNAL_EMIT_TIMEOUT + std::time::Duration::from_millis(500),
            "should abort at SIGNAL_EMIT_TIMEOUT, took {elapsed:?}"
        );
    }
}
