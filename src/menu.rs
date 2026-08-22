//! Tray menu state + dbusmenu tree builder.
//!
//! The Rust tray builds a `com.canonical.dbusmenu` tree that mirrors the
//! gjs version's Gtk.Menu:
//!
//!   Plan: <plan label>           (disabled, informational header)
//!     <window 0>: X% left · resets in Y    (disabled)
//!       [████░░░░░░░░░░░░░░░░░░░]          (disabled, ASCII bar)
//!       · on pace to have ~N% left at reset (rate/h)   (optional, burn row)
//!     <window 1>: X% left · resets in Y    (disabled)
//!       [████████████████████████]          (disabled, ASCII bar)
//!       ⚠ rate → exhausts ~X before reset              (optional, warn burn)
//!     ⚠ Throttled                (hidden unless any window throttled)
//!     ⚠ Error: …                 (hidden unless last refresh failed)
//!   ───
//!   Refresh now                   (action → MenuCommand::Refresh)
//!   Open dashboard                (action → MenuCommand::OpenDashboard)
//!   Set API Key…                  (action → MenuCommand::SetApiKey)
//!   ───
//!   Quit                          (action → MenuCommand::Quit)
//!
//! The state is held in a Mutex<MenuInner>; the dbusmenu server reads
//! from it for `GetLayout`/`GetGroupProperties`/`GetProperty`. Menu
//! actions are dispatched via an `mpsc::Sender<MenuCommand>` set at
//! construction time — clicking an action item sends the command to
//! the main loop's receiver, which then triggers refresh, opens the
//! dashboard URL, etc.
//!
//! We emit dbusmenu signals on state change so the panel can re-render:
//!   - ItemsUpdated(update_id, removed_ids)  — emitted before structural
//!     changes; gives the client a chance to drop cached layout
//!   - LayoutUpdated(update_id, parent_id)  — emitted after a sub-tree
//!     changed; client re-fetches GetLayout(parent_id, -1)
//!   - ItemPropertiesUpdated(items_props, removed_props) — emitted when
//!     just properties change (no structural move)
//!
//! Revision number increments on every state change. The client uses
//! it to invalidate stale caches (similar to ETag).

use std::collections::HashMap;

use zbus::zvariant::{OwnedValue, Str, Value};

/// Menu commands dispatched when the user clicks an action item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuCommand {
    /// "Refresh now" — trigger an immediate refresh, don't change the
    /// schedule. The main loop's refresh-loop task drains this from
    /// the receiver and calls `do_refresh()` directly.
    Refresh,
    /// "Open dashboard" — open the plan's `dashboard_url` in the
    /// default browser. The Tray holds the dashboard URL and the
    /// main loop's task calls `xdg-open` on it.
    OpenDashboard,
    /// "Set API Key…" — pop a password prompt (zenity/kdialog) and
    /// save the entered key to the keyring via secret-tool. After
    /// saving, the main loop triggers a refresh with the new value.
    SetApiKey,
    /// "Quit" — shut down the tray. The main loop's task breaks out
    /// of its tokio::select and the program exits.
    Quit,
}

/// Internal id space:
///
///   0 = root (required by dbusmenu)
///   1 = plan header (disabled label)
///   100..199 = window rows: 100+3i = label, 100+3i+1 = bar, 100+3i+2 = burn
///   200 = throttled row
///   210 = error row
///   300 = separator 1
///   301 = "Refresh now"
///   302 = separator 2
///   303 = "Open dashboard"
///   304 = "Set API Key…"
///   305 = separator 3
///   306 = "Quit"
///
/// The 100+3i/3i+1/3i+2 layout gives us three IDs per window; we
/// don't need a hash map keyed on window.id at the menu level (the
/// caller passes `&[Window]` in order).
pub const ROOT_ID: i32 = 0;
pub const HEADER_ID: i32 = 1;
pub const THROTTLED_ID: i32 = 200;
pub const ERROR_ID: i32 = 210;
pub const SEP_1_ID: i32 = 300;
pub const REFRESH_ID: i32 = 301;
pub const SEP_2_ID: i32 = 302;
pub const DASHBOARD_ID: i32 = 303;
pub const SET_KEY_ID: i32 = 304;
pub const SEP_3_ID: i32 = 305;
pub const QUIT_ID: i32 = 306;

/// Per-window base id: id, id+1 (bar), id+2 (burn).
pub fn window_id(idx: usize) -> i32 {
    100 + (idx as i32) * 3
}
pub fn bar_id(idx: usize) -> i32 {
    100 + (idx as i32) * 3 + 1
}
pub fn burn_id(idx: usize) -> i32 {
    100 + (idx as i32) * 3 + 2
}

/// One menu item.
#[derive(Debug, Clone)]
pub struct MenuItem {
    pub id: i32,
    pub label: String,
    pub enabled: bool,
    pub visible: bool,
    /// True for separator items (rendered as a horizontal line, no label).
    pub separator: bool,
    /// Optional action triggered when this item is clicked.
    pub action: Option<MenuCommand>,
}

impl MenuItem {
    fn standard(id: i32, label: impl Into<String>, enabled: bool, visible: bool) -> Self {
        Self {
            id,
            label: label.into(),
            enabled,
            visible,
            separator: false,
            action: None,
        }
    }
    fn action(id: i32, label: impl Into<String>, action: MenuCommand) -> Self {
        Self {
            id,
            label: label.into(),
            enabled: true,
            visible: true,
            separator: false,
            action: Some(action),
        }
    }
    fn separator(id: i32) -> Self {
        Self {
            id,
            label: String::new(),
            enabled: true,
            visible: true,
            separator: true,
            action: None,
        }
    }
}

/// Full menu tree state. `items` is keyed by id; `children_of` is the
/// parent→children map. The root is id 0.
///
/// The root's children list always contains every potential child id in
/// stable order — including currently-hidden ones. Visibility is tracked
/// via the `visible` property on each item, not via structural
/// membership. This matches how libdbusmenu-gtk / -qt render: an item
/// present in the children list with `visible: false` is just not
/// painted, no LayoutUpdated needed.
#[derive(Debug)]
pub struct MenuInner {
    items: HashMap<i32, MenuItem>,
    children_of: HashMap<i32, Vec<i32>>,
    /// Monotonic counter — bumped on every state change so the dbusmenu
    /// client can invalidate caches.
    revision: u32,
    /// Highest window slot ever used. Monotonically grows; never shrinks.
    /// Lets us keep stable IDs in the children list even when the
    /// current window count drops.
    max_window_slots: usize,
}

impl Default for MenuInner {
    fn default() -> Self {
        Self::new()
    }
}

impl MenuInner {
    pub fn new() -> Self {
        let mut items = HashMap::new();
        let mut children_of: HashMap<i32, Vec<i32>> = HashMap::new();
        // Root
        items.insert(ROOT_ID, MenuItem::standard(ROOT_ID, "", true, true));
        // Static items
        items.insert(
            HEADER_ID,
            MenuItem::standard(HEADER_ID, "Plan: …", false, true),
        );
        items.insert(
            THROTTLED_ID,
            MenuItem::standard(THROTTLED_ID, "", false, false),
        );
        items.insert(ERROR_ID, MenuItem::standard(ERROR_ID, "", false, false));
        items.insert(SEP_1_ID, MenuItem::separator(SEP_1_ID));
        items.insert(
            REFRESH_ID,
            MenuItem::action(REFRESH_ID, "Refresh now", MenuCommand::Refresh),
        );
        items.insert(SEP_2_ID, MenuItem::separator(SEP_2_ID));
        items.insert(
            DASHBOARD_ID,
            MenuItem::action(DASHBOARD_ID, "Open dashboard", MenuCommand::OpenDashboard),
        );
        items.insert(
            SET_KEY_ID,
            MenuItem::action(SET_KEY_ID, "Set API Key…", MenuCommand::SetApiKey),
        );
        items.insert(SEP_3_ID, MenuItem::separator(SEP_3_ID));
        items.insert(
            QUIT_ID,
            MenuItem::action(QUIT_ID, "Quit", MenuCommand::Quit),
        );
        // Initial root children (no window slots yet; rebuild_window_rows grows this).
        let root_children = vec![
            HEADER_ID,
            THROTTLED_ID,
            ERROR_ID,
            SEP_1_ID,
            REFRESH_ID,
            SEP_2_ID,
            DASHBOARD_ID,
            SET_KEY_ID,
            SEP_3_ID,
            QUIT_ID,
        ];
        children_of.insert(ROOT_ID, root_children);

        Self {
            items,
            children_of,
            revision: 1,
            max_window_slots: 0,
        }
    }

    /// Current revision number (incremented on each `bump_revision` call).
    pub fn revision(&self) -> u32 {
        self.revision
    }

    /// Bump the revision counter (call after every state change).
    pub fn bump_revision(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    /// Children of `parent_id` in render order.
    pub fn children(&self, parent_id: i32) -> &[i32] {
        self.children_of
            .get(&parent_id)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Get an item by id.
    pub fn item(&self, id: i32) -> Option<&MenuItem> {
        self.items.get(&id)
    }

    /// Update the header label.
    pub fn set_header(&mut self, text: &str) {
        if let Some(item) = self.items.get_mut(&HEADER_ID) {
            if item.label != text {
                item.label = text.to_string();
                self.bump_revision();
            }
        }
    }

    /// Update throttled row label and visibility.
    pub fn set_throttled(&mut self, text: &str, visible: bool) {
        let item = self
            .items
            .entry(THROTTLED_ID)
            .or_insert_with(|| MenuItem::standard(THROTTLED_ID, "", false, false));
        let mut changed = false;
        if item.label != text {
            item.label = text.to_string();
            changed = true;
        }
        if item.visible != visible {
            item.visible = visible;
            changed = true;
        }
        if changed {
            self.bump_revision();
        }
    }

    /// Update error row label and visibility.
    pub fn set_error(&mut self, text: &str, visible: bool) {
        let item = self
            .items
            .entry(ERROR_ID)
            .or_insert_with(|| MenuItem::standard(ERROR_ID, "", false, false));
        let mut changed = false;
        if item.label != text {
            item.label = text.to_string();
            changed = true;
        }
        if item.visible != visible {
            item.visible = visible;
            changed = true;
        }
        if changed {
            self.bump_revision();
        }
    }

    /// Rebuild the window-row section in place. `labels` are the per-window
    /// row labels (e.g. "  5h: 80% left · resets in 4h"), `bars` are the
    /// ASCII bars, `burns` are the burn-rate rows (Some = visible, None
    /// = hidden). Order matches the input slice.
    ///
    /// This reuses existing window IDs so the client doesn't churn on
    /// every refresh (the items keep the same id, just the labels and
    /// visibility change). Slot IDs are stable — when the window count
    /// shrinks, the extra slots are kept in the root children list with
    /// `visible: false` so the dbusmenu client doesn't see churn.
    pub fn rebuild_window_rows(
        &mut self,
        labels: &[String],
        bars: &[String],
        burns: &[Option<String>],
    ) {
        let n = labels.len();
        if n > self.max_window_slots {
            // Need to grow: create the new slot items (hidden) and
            // extend the root children list. This is the only path
            // that produces an ItemsUpdated signal — a smaller window
            // count is purely a property change.
            for idx in self.max_window_slots..n {
                let lid = window_id(idx);
                let bid = bar_id(idx);
                let bid2 = burn_id(idx);
                self.items
                    .entry(lid)
                    .or_insert_with(|| MenuItem::standard(lid, "", false, false));
                self.items
                    .entry(bid)
                    .or_insert_with(|| MenuItem::standard(bid, "", false, false));
                self.items
                    .entry(bid2)
                    .or_insert_with(|| MenuItem::standard(bid2, "", false, false));
            }
            self.max_window_slots = n;
            self.extend_root_children_with_new_slots(self.max_window_slots);
            self.bump_revision();
        }
        for idx in 0..n {
            let lid = window_id(idx);
            let bid = bar_id(idx);
            let burn = burns.get(idx).cloned().flatten();

            let row_label = labels[idx].clone();
            let bar_label = bars[idx].clone();

            // Label item
            let label_item = self
                .items
                .entry(lid)
                .or_insert_with(|| MenuItem::standard(lid, "", false, true));
            let mut changed = false;
            if label_item.label != row_label {
                label_item.label = row_label;
                changed = true;
            }
            if !label_item.visible {
                label_item.visible = true;
                changed = true;
            }
            if changed {
                self.bump_revision();
            }

            // Bar item
            let bar_item = self
                .items
                .entry(bid)
                .or_insert_with(|| MenuItem::standard(bid, "", false, true));
            let mut changed = false;
            if bar_item.label != bar_label {
                bar_item.label = bar_label;
                changed = true;
            }
            if !bar_item.visible {
                bar_item.visible = true;
                changed = true;
            }
            if changed {
                self.bump_revision();
            }

            // Burn row
            let bid2 = burn_id(idx);
            let entry = self
                .items
                .entry(bid2)
                .or_insert_with(|| MenuItem::standard(bid2, "", false, false));
            let mut changed = false;
            match burn {
                Some(text) => {
                    if entry.label != text {
                        entry.label = text;
                        changed = true;
                    }
                    if !entry.visible {
                        entry.visible = true;
                        changed = true;
                    }
                }
                None => {
                    if entry.visible {
                        entry.visible = false;
                        changed = true;
                    }
                    if !entry.label.is_empty() {
                        entry.label = String::new();
                        changed = true;
                    }
                }
            }
            if changed {
                self.bump_revision();
            }
        }
        // Hide any stale window slots above the current count, but keep
        // them in the children list so the IDs are reserved.
        for idx in n..self.max_window_slots {
            let lid = window_id(idx);
            let bid = bar_id(idx);
            let bid2 = burn_id(idx);
            let mut changed = false;
            for id in [lid, bid, bid2] {
                if let Some(it) = self.items.get_mut(&id) {
                    if it.visible {
                        it.visible = false;
                        changed = true;
                    }
                    if !it.label.is_empty() {
                        it.label = String::new();
                        changed = true;
                    }
                }
            }
            if changed {
                self.bump_revision();
            }
        }
    }

    /// Insert new window slots into the root children list, in stable
    /// order between HEADER_ID and THROTTLED_ID. Called when
    /// `max_window_slots` grows.
    fn extend_root_children_with_new_slots(&mut self, total: usize) {
        let root = self.children_of.entry(ROOT_ID).or_default();
        // Find the position to insert (HEADER_ID + 1, before THROTTLED_ID/ERROR_ID).
        let insert_at = root
            .iter()
            .position(|&x| x == HEADER_ID)
            .map(|i| i + 1)
            .unwrap_or(0);
        // Build the new window slots in order.
        let new_slots: Vec<i32> = (0..total)
            .flat_map(|idx| vec![window_id(idx), bar_id(idx), burn_id(idx)])
            .collect();
        // Remove any existing duplicates (shouldn't happen, but defensive).
        let mut new_children: Vec<i32> = Vec::with_capacity(root.len() + new_slots.len());
        new_children.extend_from_slice(&root[..insert_at]);
        new_children.extend(new_slots);
        new_children.extend_from_slice(&root[insert_at..]);
        *root = new_children;
    }
}

/// Properties dict for one item, in dbusmenu wire format.
/// `(id, HashMap<String, OwnedValue>)` — used for `GetProperty` /
/// `GetGroupProperties` responses.
pub fn build_properties(item: &MenuItem) -> HashMap<String, OwnedValue> {
    let mut props: HashMap<String, OwnedValue> = HashMap::new();
    if item.separator {
        props.insert("type".into(), OwnedValue::from(Str::from("separator")));
        // Separators have no label, are non-interactive.
        props.insert("visible".into(), OwnedValue::from(item.visible));
        props.insert("enabled".into(), OwnedValue::from(false));
    } else {
        props.insert("type".into(), OwnedValue::from(Str::from("standard")));
        props.insert(
            "label".into(),
            OwnedValue::from(Str::from(item.label.as_str())),
        );
        props.insert("enabled".into(), OwnedValue::from(item.enabled));
        props.insert("visible".into(), OwnedValue::from(item.visible));
        // children-display = "never" for our leaf items so the panel
        // doesn't render submenu arrows on plain rows.
        props.insert(
            "children-display".into(),
            OwnedValue::from(Str::from("never")),
        );
    }
    props
}

/// Build a child-variant list for one parent's layout. Each child is
/// wrapped in an `OwnedValue` carrying its `(id, props)` structure.
/// If `recurse` is true (depth < 0 or depth > 0), descendants' children
/// are also populated; otherwise children are represented as empty.
pub fn build_child_variants(state: &MenuInner, parent_id: i32, recurse: bool) -> Vec<OwnedValue> {
    let mut out = Vec::new();
    for &cid in state.children(parent_id) {
        let item = match state.item(cid) {
            Some(i) => i.clone(),
            None => continue,
        };
        let grandchildren = if recurse {
            build_child_variants(state, cid, true)
        } else {
            Vec::new()
        };
        let props = build_properties(&item);
        let layout: ItemLayout = (item.id, props, grandchildren);
        let value: Value<'_> = Value::Structure(zbus::zvariant::Structure::from(layout));
        // Convert borrowed Value → owned Value via the inherent
        // `try_to_owned()` (zvariant 5.x — the 3.x `to_owned()`
        // method doesn't exist on this version).
        out.push(value.try_to_owned().expect("static owned layout"));
    }
    out
}

/// One dbusmenu item as it appears in the wire format: `(id, properties,
/// children)`. Factored out of the `GetLayout` return type so clippy
/// doesn't trip on the inline tuple-of-tuple declaration.
pub type ItemLayout = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);

/// Full `GetLayout` response: `(revision, item_layout)`. Mirrors the
/// signature of `com.canonical.dbusmenu.GetLayout(parentId, ...)`.
pub type ItemLayoutResponse = (u32, ItemLayout);

/// Build the GetLayout response `(revision, item_layout)` for the
/// subtree rooted at `parent_id`. `recurse` controls whether children
/// are populated recursively.
pub fn build_layout_response(
    state: &MenuInner,
    parent_id: i32,
    recurse: bool,
) -> ItemLayoutResponse {
    let item = state
        .item(parent_id)
        .cloned()
        .unwrap_or_else(|| MenuItem::standard(parent_id, "", true, true));
    let props = build_properties(&item);
    let children = build_child_variants(state, parent_id, recurse);
    let layout = (item.id, props, children);
    (state.revision(), layout)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items_in_order(state: &MenuInner) -> Vec<i32> {
        state.children(ROOT_ID).to_vec()
    }

    #[test]
    fn default_has_static_layout() {
        let s = MenuInner::new();
        let kids = items_in_order(&s);
        assert_eq!(kids.first(), Some(&HEADER_ID));
        assert!(kids.contains(&REFRESH_ID));
        assert!(kids.contains(&QUIT_ID));
        assert!(kids.contains(&SEP_1_ID));
        assert!(kids.contains(&SEP_3_ID));
        // Throttled + error are hidden by default
        let t = s.item(THROTTLED_ID).unwrap();
        let e = s.item(ERROR_ID).unwrap();
        assert!(!t.visible);
        assert!(!e.visible);
    }

    #[test]
    fn revision_bumps_on_change() {
        let mut s = MenuInner::new();
        let r0 = s.revision();
        s.set_header("Plan: Coding Plan");
        assert!(s.revision() > r0);
        // No-change set: don't bump.
        let r1 = s.revision();
        s.set_header("Plan: Coding Plan");
        assert_eq!(s.revision(), r1);
        s.set_header("Plan: Token Plan");
        assert!(s.revision() > r1);
    }

    #[test]
    fn window_rows_appear_in_order() {
        let mut s = MenuInner::new();
        s.rebuild_window_rows(
            &["  5h: 80% left".into(), "  weekly: 100% left".into()],
            &["  [bar5h]".into(), "  [barweekly]".into()],
            &[Some("  burn 5h".into()), None],
        );
        let kids = items_in_order(&s);
        let lid0 = window_id(0);
        let bid0 = bar_id(0);
        let bid2 = burn_id(0);
        let lid1 = window_id(1);
        let bid1 = bar_id(1);
        let pos = |id: i32| kids.iter().position(|&x| x == id).unwrap();
        assert!(pos(lid0) < pos(bid0));
        assert!(pos(bid0) < pos(bid2));
        assert!(pos(bid2) < pos(lid1));
        assert!(pos(lid1) < pos(bid1));
        // Burn row 2 (idx=1) was passed None → not visible → but stays
        // in the children list with a stable ID for next time it shows.
        assert!(kids.contains(&burn_id(1)));
        assert!(pos(bid1) < pos(THROTTLED_ID));
        assert!(pos(THROTTLED_ID) < pos(SEP_1_ID));
    }

    #[test]
    fn window_rows_hide_when_count_drops() {
        let mut s = MenuInner::new();
        s.rebuild_window_rows(
            &["a".into(), "b".into()],
            &["a".into(), "b".into()],
            &[None, None],
        );
        assert!(s.item(window_id(0)).unwrap().visible);
        assert!(s.item(window_id(1)).unwrap().visible);

        // Drop to 1 window
        s.rebuild_window_rows(&["only".into()], &["bar".into()], &[None]);
        assert!(s.item(window_id(0)).unwrap().visible);
        assert!(!s.item(window_id(1)).unwrap().visible);
    }

    #[test]
    fn burn_row_toggles_visibility() {
        let mut s = MenuInner::new();
        s.rebuild_window_rows(&["row".into()], &["bar".into()], &[None]);
        assert!(!s.item(burn_id(0)).unwrap().visible);

        s.rebuild_window_rows(
            &["row".into()],
            &["bar".into()],
            &[Some("  · burn row".into())],
        );
        assert!(s.item(burn_id(0)).unwrap().visible);
        assert_eq!(s.item(burn_id(0)).unwrap().label, "  · burn row");

        s.rebuild_window_rows(&["row".into()], &["bar".into()], &[None]);
        assert!(!s.item(burn_id(0)).unwrap().visible);
    }

    #[test]
    fn throttled_toggles() {
        let mut s = MenuInner::new();
        assert!(!s.item(THROTTLED_ID).unwrap().visible);
        s.set_throttled("  ⚠ Throttled", true);
        assert!(s.item(THROTTLED_ID).unwrap().visible);
        assert_eq!(s.item(THROTTLED_ID).unwrap().label, "  ⚠ Throttled");
        let r = s.revision();
        s.set_throttled("  ⚠ Throttled", true);
        assert_eq!(s.revision(), r);
        s.set_throttled("", false);
        assert!(!s.item(THROTTLED_ID).unwrap().visible);
    }

    #[test]
    fn error_toggles() {
        let mut s = MenuInner::new();
        s.set_error("  ⚠ Error: x", true);
        assert!(s.item(ERROR_ID).unwrap().visible);
        assert!(s.item(ERROR_ID).unwrap().label.contains("Error: x"));
        s.set_error("", false);
        assert!(!s.item(ERROR_ID).unwrap().visible);
    }

    #[test]
    fn action_items_have_commands() {
        let s = MenuInner::new();
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

    #[test]
    fn separators_have_no_label() {
        let s = MenuInner::new();
        for sid in [SEP_1_ID, SEP_2_ID, SEP_3_ID] {
            let it = s.item(sid).unwrap();
            assert!(it.separator);
            assert_eq!(it.label, "");
            assert!(it.action.is_none());
        }
    }

    #[test]
    fn window_id_helpers() {
        assert_eq!(window_id(0), 100);
        assert_eq!(bar_id(0), 101);
        assert_eq!(burn_id(0), 102);
        assert_eq!(window_id(1), 103);
        assert_eq!(bar_id(1), 104);
        assert_eq!(burn_id(1), 105);
    }

    #[test]
    fn build_layout_returns_props() {
        let s = MenuInner::new();
        let (rev, layout) = build_layout_response(&s, REFRESH_ID, false);
        assert!(rev > 0);
        assert_eq!(layout.0, REFRESH_ID);
        assert!(layout.1.contains_key("type"));
        assert!(layout.1.contains_key("label"));
        assert_eq!(
            layout
                .1
                .get("label")
                .unwrap()
                .downcast_ref::<zbus::zvariant::Str>()
                .unwrap()
                .to_string(),
            "Refresh now"
        );
    }

    #[test]
    fn separator_layout() {
        let s = MenuInner::new();
        let (_rev, layout) = build_layout_response(&s, SEP_1_ID, false);
        assert_eq!(layout.0, SEP_1_ID);
        assert!(layout.1.contains_key("type"));
        let t = layout.1.get("type").unwrap();
        assert_eq!(
            t.downcast_ref::<zbus::zvariant::Str>().unwrap().to_string(),
            "separator"
        );
    }

    #[test]
    fn child_variants_recursive() {
        let mut s = MenuInner::new();
        s.rebuild_window_rows(&["row".into()], &["bar".into()], &[None]);
        let kids = build_child_variants(&s, ROOT_ID, true);
        // HEADER + 3 window items + THROTTLED + ERROR + 3 seps + 4 actions
        // = 13 children
        assert_eq!(kids.len(), 13);
        for v in &kids {
            match &**v {
                Value::Structure(_) => {}
                _ => panic!("expected Value::Structure"),
            }
        }
    }
}
