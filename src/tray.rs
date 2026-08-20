//! Gtk3 menu + libayatana-appindicator3 tray icon. Mirrors the gjs UI:
//!   Plan: <label>
//!     5h: <pct>% left · resets in <fmt>
//!     ████░░░░
//!     weekly: ...
//!     ████░░░░
//!     ───
//!     Refresh now
//!     Open dashboard
//!     Set API Key…
//!     Clear API Key
//!     ───
//!     Quit
//!
//! `update()` is called from the main thread whenever fresh data arrives.
//! All menu strings and the tray icon are rebuilt based on the new state.

use gtk::prelude::*;
use gtk::{Menu, MenuItem, SeparatorMenuItem};

use crate::burn::BurnResult;
use crate::icon::{bucket_for, ring_icon_path};
use crate::indicator::AppIndicator;
use crate::{config, notify};

/// Menu items we need to mutate per refresh — stored in a struct so
/// `update()` can find them by name. Separators are appended directly
/// to the menu (no state, no need to track).
struct MenuItems {
    header: MenuItem,
    window_5h_label: MenuItem,
    window_5h_bar: MenuItem,
    window_weekly_label: MenuItem,
    window_weekly_bar: MenuItem,
    burn_5h: MenuItem,
    burn_weekly: MenuItem,
    error: MenuItem,
    refresh: MenuItem,
    dashboard: MenuItem,
    set_key: MenuItem,
    clear_key: MenuItem,
    quit: MenuItem,
}

pub struct Tray {
    pub indicator: AppIndicator,
    pub menu: Menu,
    items: MenuItems,
    pub plan_label: String,
}

fn fmt_reset(ms: i64) -> String {
    if ms <= 0 {
        return "now".to_string();
    }
    let mins = ms / 60_000;
    if mins < 60 {
        return format!("{mins}m");
    }
    let h = mins / 60;
    let m = mins % 60;
    if h < 24 {
        return if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h {m}m")
        };
    }
    let d = h / 24;
    format!("{d}d {}h", h % 24)
}

fn bar_markup(filled_pct: i64) -> String {
    // 22-cell bar.
    let filled = ((filled_pct.max(0).min(100)) as f64 / 100.0 * 22.0).round() as usize;
    let empty = 22 - filled;
    format!("  {}{}", "█".repeat(filled), "░".repeat(empty))
}

fn burn_label(b: &BurnResult) -> String {
    if b.unit == "pct" {
        format!("· on pace to have ~{:.0}% left at reset ({:.0}%/h)",
                b.projected_pct_left, b.rate_per_hour)
    } else {
        let rate = if b.rate_per_hour >= 1000.0 {
            format!("{:.1}k", b.rate_per_hour / 1000.0)
        } else {
            format!("{:.0}", b.rate_per_hour)
        };
        format!("· on pace to have ~{:.0}% left at reset ({rate} tok/h)",
                b.projected_pct_left)
    }
}

fn burn_warning_label(b: &BurnResult) -> String {
    let exhaust_secs = ((b.exhaust_ms / 1000.0).max(0.0)) as i64;
    let rate = if b.rate_per_hour >= 1000.0 {
        format!("{:.1}k", b.rate_per_hour / 1000.0)
    } else {
        format!("{:.0}", b.rate_per_hour)
    };
    if b.unit == "pct" {
        format!("⚠ {rate}%/h → exhausts ~{} before reset", fmt_reset(exhaust_secs * 1000))
    } else {
        format!("⚠ {rate} tok/h → exhausts ~{} before reset", fmt_reset(exhaust_secs * 1000))
    }
}

fn append_separator(menu: &Menu) {
    menu.append(&SeparatorMenuItem::new());
}

fn make_menu_item(menu: &Menu, label: &str, sensitive: bool) -> MenuItem {
    let item = MenuItem::with_label(label);
    item.set_sensitive(sensitive);
    menu.append(&item);
    item
}

impl Tray {
    pub fn new(cfg: &config::Config) -> Self {
        let plan_label = cfg
            .plans
            .get(&cfg.plan)
            .map(|p| p.label.clone())
            .unwrap_or_else(|| cfg.plan.clone());

        // Indicator.
        let mut indicator = AppIndicator::new("minimax-quota", "dialog-information-symbolic")
            .expect("create indicator");
        indicator.set_status_active().expect("set status active");
        indicator.set_title("MiniMax").expect("set title");

        // Menu.
        let mut menu = Menu::new();

        let items = MenuItems {
            header:            make_menu_item(&menu, &format!("Plan: {plan_label}"), false),
            window_5h_label:   make_menu_item(&menu, "", false),
            window_5h_bar:     make_menu_item(&menu, "", false),
            window_weekly_label: make_menu_item(&menu, "", false),
            window_weekly_bar: make_menu_item(&menu, "", false),
            burn_5h:           make_menu_item(&menu, "", false),
            burn_weekly:       make_menu_item(&menu, "", false),
            error:             make_menu_item(&menu, "", false),
            // Separator 1: between data rows and action rows.
            refresh:           make_menu_item(&menu, "Refresh now", true),
            dashboard:         make_menu_item(&menu, "Open dashboard", true),
            set_key:           make_menu_item(&menu, "Set API Key…", true),
            clear_key:         make_menu_item(&menu, "Clear API Key", true),
            // Separator 2: between actions and Quit.
            quit:              make_menu_item(&menu, "Quit", true),
        };
        append_separator(&menu);
        append_separator(&menu);
        menu.show_all();

        indicator.set_menu(&menu).expect("set menu");

        Self { indicator, menu, items, plan_label }
    }

    /// Rebuild the menu from a fresh poll result. Called on the main thread.
    pub fn update(&mut self, state: TrayState<'_>) {
        // Header
        self.items.header.set_label(&format!("Plan: {}", self.plan_label));

        // 5h row
        if let Some(w) = state.five_h {
            self.items.window_5h_label
                .set_label(&format!("  5h: {}% left · resets in {}",
                                   w.remaining_pct, fmt_reset(w.reset_at - state.now_ms)));
            self.items.window_5h_bar.set_label(&bar_markup(w.remaining_pct));
            self.items.window_5h_label.show();
            self.items.window_5h_bar.show();
        } else {
            self.items.window_5h_label.set_label("");
            self.items.window_5h_bar.set_label("");
        }

        // Weekly row
        if let Some(w) = state.weekly {
            self.items.window_weekly_label
                .set_label(&format!("  weekly: {}% left · resets in {}",
                                   w.remaining_pct, fmt_reset(w.reset_at - state.now_ms)));
            self.items.window_weekly_bar.set_label(&bar_markup(w.remaining_pct));
            self.items.window_weekly_label.show();
            self.items.window_weekly_bar.show();
        } else {
            self.items.window_weekly_label.set_label("");
            self.items.window_weekly_bar.set_label("");
        }

        // Burn rows
        if let Some(b) = state.burn_5h {
            let label = if b.exhaust_before_reset {
                burn_warning_label(&b)
            } else {
                burn_label(&b)
            };
            self.items.burn_5h.set_label(&format!("  {label}"));
            self.items.burn_5h.show();
        } else {
            self.items.burn_5h.set_label("");
        }
        if let Some(b) = state.burn_weekly {
            let label = if b.exhaust_before_reset {
                burn_warning_label(&b)
            } else {
                burn_label(&b)
            };
            self.items.burn_weekly.set_label(&format!("  {label}"));
            self.items.burn_weekly.show();
        } else {
            self.items.burn_weekly.set_label("");
        }

        // Error row
        if let Some(err) = state.error {
            self.items.error.set_label(&format!("  ⚠ Error: {err}"));
            self.items.error.show();
        } else {
            self.items.error.set_label("");
        }

        // Tray icon
        if let Some(w) = state.five_h {
            let bucket = bucket_for(w.remaining_pct, state.throttled,
                                    state.cfg.thresholds.yellow, state.cfg.thresholds.red);
            let icon_name = ring_icon_path(w.remaining_pct, bucket)
                .and_then(|p| p.to_str().map(String::from))
                .unwrap_or_else(|| "dialog-information-symbolic".to_string());
            let _ = self.indicator.set_icon_full(&icon_name, "MiniMax");
            notify::maybe_notify(bucket, &self.plan_label, w.remaining_pct);
        } else {
            let _ = self.indicator.set_icon_full("dialog-information-symbolic", "MiniMax");
        }
    }

    /// Connect the menu item signals. The `on_*` callbacks are invoked from
    /// the main thread when the user activates the item.
    pub fn connect_signals(
        &self,
        on_refresh: impl Fn() + 'static,
        on_dashboard: impl Fn() + 'static,
        on_set_key: impl Fn() + 'static,
        on_clear_key: impl Fn() + 'static,
        on_quit: impl Fn() + 'static,
    ) {
        self.items.refresh.connect_activate(move |_| on_refresh());
        self.items.dashboard.connect_activate(move |_| on_dashboard());
        self.items.set_key.connect_activate(move |_| on_set_key());
        self.items.clear_key.connect_activate(move |_| on_clear_key());
        self.items.quit.connect_activate(move |_| on_quit());
    }
}

/// Snapshot of the latest fetch + burn state, ready to be rendered.
pub struct TrayState<'a> {
    pub cfg: &'a config::Config,
    pub five_h: Option<crate::burn::Window>,
    pub weekly: Option<crate::burn::Window>,
    pub burn_5h: Option<BurnResult>,
    pub burn_weekly: Option<BurnResult>,
    pub error: Option<String>,
    pub throttled: bool,
    pub now_ms: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_reset_units() {
        assert_eq!(fmt_reset(0), "now");
        assert_eq!(fmt_reset(30_000), "0m"); // < 1 minute rounds to 0
        assert_eq!(fmt_reset(60_000), "1m");
        assert_eq!(fmt_reset(3_600_000), "1h");
        assert_eq!(fmt_reset(3_900_000), "1h 5m");
        assert_eq!(fmt_reset(86_400_000), "1d 0h");
        assert_eq!(fmt_reset(90_000_000), "1d 1h");
        assert_eq!(fmt_reset(7 * 86_400_000), "7d 0h");
    }

    #[test]
    fn bar_markup_full_and_empty() {
        assert_eq!(bar_markup(100), format!("  {}", "█".repeat(22)));
        assert_eq!(bar_markup(0),   format!("  {}", "░".repeat(22)));
        assert_eq!(bar_markup(50).chars().filter(|c| *c == '█').count(), 11);
    }

    #[test]
    fn burn_label_pct() {
        let b = BurnResult {
            rate_per_hour: 15.0, mode: "pct", unit: "pct",
            exhaust_ms: f64::INFINITY, remaining_ms: 14_400_000,
            exhaust_before_reset: false, projected_pct_left: 62.0,
        };
        let s = burn_label(&b);
        assert!(s.contains("62%"));
        assert!(s.contains("15%/h"));
        assert!(!s.contains("tok/h"));
    }

    #[test]
    fn burn_label_token_thousands() {
        let b = BurnResult {
            rate_per_hour: 1234.0, mode: "token", unit: "token",
            exhaust_ms: f64::INFINITY, remaining_ms: 14_400_000,
            exhaust_before_reset: false, projected_pct_left: 48.0,
        };
        let s = burn_label(&b);
        assert!(s.contains("48%"));
        assert!(s.contains("1.2k"));
    }

    #[test]
    fn burn_warning_label_format() {
        let b = BurnResult {
            rate_per_hour: 60.0, mode: "pct", unit: "pct",
            exhaust_ms: 1_320_000.0, remaining_ms: 3_600_000,
            exhaust_before_reset: true, projected_pct_left: 0.0,
        };
        let s = burn_warning_label(&b);
        assert!(s.starts_with("⚠"));
        assert!(s.contains("60%/h"));
        assert!(s.contains("22m"));
    }
}