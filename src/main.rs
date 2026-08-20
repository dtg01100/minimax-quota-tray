//! Entry point. Wires together config, keyring, fetch, tray, scheduler.
//!
//! Threading: GLib main thread runs the Gtk main loop. HTTP runs on a
//! background thread (reqwest blocking) and bounces results back via
//! `glib::idle_add_once`. Tray is main-thread-only (gtk widgets aren't
//! Send) and lives in a `thread_local!` slot accessed only from main-thread
//! callbacks. AppState is shared between threads via `Arc<Mutex<>>`.

mod burn;
mod config;
mod fetch;
mod icon;
mod indicator;
mod keyring;
mod notify;
mod parse;
mod scheduler;
mod tray;
mod util;

use gio;
use glib;
use gtk::prelude::*;
use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use crate::burn::{BurnResult, Sample, Window};
use crate::config::Config;
use crate::tray::{Tray, TrayState};

/// Per-window burn sample history. ~16h at 120s baseline. Cleared on rollover.
const BURN_MAX_SAMPLES: usize = 480;

#[derive(Default)]
struct AppState {
    five_h_history: Vec<Sample>,
    weekly_history: Vec<Sample>,
    last_good: Option<(Window, Window)>,
    last_good_at: i64,
    fail_streak: u32,
    poll_source: Option<glib::SourceId>,
    http_client: Option<fetch::HttpClient>,
}

/// Main-thread-only slot holding the Tray. Gtk widgets are !Send, so we
/// access Tray via this thread_local from main-thread callbacks.
thread_local! {
    static TRAY_SLOT: RefCell<Option<Tray>> = const { RefCell::new(None) };
}

fn with_tray<R>(f: impl FnOnce(&mut Tray) -> R) -> Option<R> {
    TRAY_SLOT.with(|cell| cell.borrow_mut().as_mut().map(f))
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .init();

    if let Err(e) = run() {
        eprintln!("fatal: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    gtk::init()?;

    let cfg = config::load_or_init()?;
    TRAY_SLOT.with(|cell| *cell.borrow_mut() = Some(Tray::new(&cfg)));
    let state = Arc::new(Mutex::new(AppState {
        http_client: Some(fetch::build_client()?),
        ..Default::default()
    }));
    let cfg = Arc::new(cfg);

    // Refresh action — used by both menu and the poll timer.
    let refresh_now = {
        let state = Arc::clone(&state);
        let cfg = Arc::clone(&cfg);
        move || refresh(&state, &cfg)
    };

    let on_dashboard = {
        let cfg = Arc::clone(&cfg);
        move || {
            if let Some(plan) = cfg.plans.get(&cfg.plan) {
                if let Err(e) = gio::AppInfo::launch_default_for_uri(
                    &plan.dashboard_url,
                    None::<&gio::AppLaunchContext>,
                ) {
                    log::warn!("opening dashboard failed: {e}");
                }
            }
        }
    };

    let on_set_key = {
        let refresh_now = refresh_now.clone();
        move || {
            let dialog = gtk::Dialog::new();
            dialog.set_title("Set MiniMax API Key");
            dialog.add_button("Cancel", gtk::ResponseType::Cancel);
            dialog.add_button("Save", gtk::ResponseType::Ok);
            let entry = gtk::Entry::new();
            entry.set_visibility(false);
            entry.set_input_purpose(gtk::InputPurpose::Password);
            let content = dialog.get_content_area();
            content.add(&entry);
            dialog.show_all();
            let resp = dialog.run();
            let text = entry.get_buffer().get_text().to_string();
            dialog.close();
            if resp == gtk::ResponseType::Ok && !text.is_empty() {
                if let Err(e) = keyring::set(&text) {
                    log::warn!("set key failed: {e}");
                }
                refresh_now();
            }
        }
    };

    let on_clear_key = {
        let refresh_now = refresh_now.clone();
        move || {
            if let Err(e) = keyring::clear() {
                log::warn!("clear key failed: {e}");
            }
            refresh_now();
        }
    };

    let on_quit = {
        let state = Arc::clone(&state);
        move || {
            if let Some(src) = state.lock().unwrap().poll_source.take() {
                src.remove();
            }
            gtk::main_quit();
        }
    };

    with_tray(|t| t.connect_signals(
        refresh_now.clone(),
        on_dashboard,
        on_set_key,
        on_clear_key,
        on_quit,
    ));

    refresh_now();
    gtk::main();
    Ok(())
}

/// Run a refresh cycle. The HTTP work happens on a background thread;
/// this function (and its closure) runs on the main thread.
fn refresh(state: &Arc<Mutex<AppState>>, cfg: &Arc<Config>) {
    let cfg_v = (**cfg).clone();
    let api_key = match keyring::get() {
        Some(k) => k,
        None => {
            render_error(&cfg_v, "No API key — choose Set API Key…");
            schedule_next(state, cfg_v.refresh_seconds * 1000);
            return;
        }
    };
    let endpoint = match cfg_v.plans.get(&cfg_v.plan) {
        Some(p) => p.endpoint.clone(),
        None => {
            render_error(&cfg_v, &format!("Unknown plan: {}", cfg_v.plan));
            schedule_next(state, cfg_v.refresh_seconds * 1000);
            return;
        }
    };

    let client = {
        let s = state.lock().unwrap();
        s.http_client.clone()
    };
    let client = match client {
        Some(c) => c,
        None => {
            render_error(&cfg_v, "HTTP client not initialized");
            return;
        }
    };

    let state_bg = Arc::clone(state);
    let cfg_bg = Arc::clone(cfg);

    // Fetch happens off-thread; result lands on main thread via idle_add_once.
    // The closure captures only Send data (Arc<Mutex<AppState>> + Arc<Config>)
    // and never references the Tray directly — that's accessed via with_tray()
    // when render_state() runs (which IS on the main thread).
    fetch::dispatch(client, endpoint, api_key, move |result| {
        let mut s = state_bg.lock().unwrap();
        let cfg_v = (*cfg_bg).clone();
        let render = match result {
            Ok((five_h, weekly)) => {
                s.fail_streak = 0;
                s.last_good = Some((five_h, weekly));
                s.last_good_at = now_ms();

                record_sample(&mut s.five_h_history, &five_h);
                record_sample(&mut s.weekly_history, &weekly);

                let burn_5h = compute_with_history(&five_h, &s.five_h_history, &cfg_v.burn_warning);
                let burn_weekly = compute_with_history(&weekly, &s.weekly_history, &cfg_v.burn_warning);

                let interval = scheduler::next_interval(
                    cfg_v.refresh_seconds,
                    cfg_v.refresh_max_backoff_seconds,
                    s.last_good.map(|(w, _)| w.remaining_pct).unwrap_or(100),
                    cfg_v.thresholds.yellow,
                    cfg_v.thresholds.red,
                    0,
                );
                let state_for_render = TrayState {
                    cfg: &cfg_v,
                    five_h: Some(five_h),
                    weekly: Some(weekly),
                    burn_5h,
                    burn_weekly,
                    error: None,
                    throttled: false,
                    now_ms: now_ms(),
                };
                drop(s);
                (interval, state_for_render)
            }
            Err(e) => {
                s.fail_streak = s.fail_streak.saturating_add(1);
                let fail_streak = s.fail_streak;
                let last_good = s.last_good;
                let five_h_hist = s.five_h_history.clone();
                let weekly_hist = s.weekly_history.clone();
                let last_good_at = s.last_good_at;
                drop(s);

                let (burn_5h, burn_weekly, five_h, weekly) =
                    if let Some((fh, wk)) = last_good {
                        (
                            compute_with_history(&fh, &five_h_hist, &cfg_v.burn_warning),
                            compute_with_history(&wk, &weekly_hist, &cfg_v.burn_warning),
                            Some(fh),
                            Some(wk),
                        )
                    } else {
                        (None, None, None, None)
                    };

                let err_str = e.to_string();
                let age = now_ms() - last_good_at;
                let state_for_render = TrayState {
                    cfg: &cfg_v,
                    five_h,
                    weekly,
                    burn_5h,
                    burn_weekly,
                    error: Some(if last_good.is_some() {
                        format!("{err_str} (last good {} ago)", crate::util::fmt_age(age))
                    } else {
                        err_str
                    }),
                    throttled: false,
                    now_ms: now_ms(),
                };

                let interval = scheduler::next_interval(
                    cfg_v.refresh_seconds,
                    cfg_v.refresh_max_backoff_seconds,
                    last_good.map(|(w, _)| w.remaining_pct).unwrap_or(100),
                    cfg_v.thresholds.yellow,
                    cfg_v.thresholds.red,
                    fail_streak,
                );
                (interval, state_for_render)
            }
        };
        let (interval, state_for_render) = render;
        render_state(state_for_render);
        schedule_next(&state_bg, interval * 1000);
    });
}

fn render_state(state: TrayState<'_>) {
    with_tray(|t| t.update(state));
}

fn render_error(_cfg: &Config, msg: &str) {
    log::warn!("{msg}");
}

/// Append one sample to the history. Detect epoch rollover.
fn record_sample(history: &mut Vec<Sample>, w: &Window) {
    if let Some(last) = history.last() {
        let rolled = w.start_at != last.start_at
            || w.used + 1 < last.used
            || w.remaining_pct + 1 < last.remaining_pct;
        if rolled {
            history.clear();
        }
    }
    history.push(Sample {
        t: now_ms(),
        used: w.used,
        total: w.total,
        remaining_pct: w.remaining_pct,
        start_at: w.start_at,
        reset_at: w.reset_at,
    });
    if history.len() > BURN_MAX_SAMPLES {
        let drop = history.len() - BURN_MAX_SAMPLES;
        history.drain(0..drop);
    }
}

fn compute_with_history(w: &Window, history: &[Sample], cfg: &burn::BurnConfig) -> Option<BurnResult> {
    burn::decide_burn_row(Some(w), history, now_ms(), cfg)
}

fn schedule_next(state: &Arc<Mutex<AppState>>, ms: u64) {
    let mut s = state.lock().unwrap();
    if let Some(prev) = s.poll_source.take() {
        prev.remove();
    }
    let state_wk = Arc::clone(state);
    let id = glib::timeout_add_local(
        std::time::Duration::from_millis(ms),
        move || {
            // Rebuild cfg by reading from disk. Cheap (Config::load() is a
            // small JSON read); avoids holding a long-lived Arc in the closure.
            let cfg = config::load();
            refresh(&state_wk, &Arc::new(cfg));
            glib::ControlFlow::Continue
        },
    );
    s.poll_source = Some(id);
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}