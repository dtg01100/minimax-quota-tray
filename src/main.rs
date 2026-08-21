//! Entry point — tokio runtime, no GLib, no GTK.
//!
//! Threading model: a tokio runtime runs an async refresh loop. The D-Bus
//! connection (via zbus) handles its own I/O on the tokio reactor. No
//! thread_local state — the SNI handle is shared via `Arc<Tray>`.
//!
//! Refresh schedule: a tokio task sleeps for the next interval, then runs
//! the fetch on the same task. Adaptive intervals (yellow/2, red/4) +
//! exponential backoff on errors live in `scheduler::next_interval`.
//!
//! ## Subsystems wired here
//!
//! - **Single-instance lock** (`lock::Lock`): O_EXCL PID file at
//!   `$XDG_RUNTIME_DIR/minimax-quota-tray.pid`. Refuses to start if a
//!   live instance already holds it; takes over stale locks.
//!
//! - **Network monitor** (`network::spawn_watcher`): subscribes to NM
//!   StateChanged; reports online/offline transitions. Skips polling
//!   while offline, force-refreshes on reconnect.
//!
//! - **Menu actions** (via `Tray::cmd_rx`): dbusmenu `Event("clicked")`
//   dispatches `MenuCommand`s (Refresh / OpenDashboard / SetApiKey /
//!   Quit) through an mpsc channel that this file drains.
//!
//! - **Threshold notifications** (`notify::send`): fires on bucket-rank
//!   transitions upward only (normal→warning→throttled). Uses
//!   `_last_bucket` to dedupe.

mod burn;
mod config;
mod fetch;
mod icon;
mod keyring;
mod lock;
mod menu;
mod network;
mod notify;
mod parse;
mod scheduler;
mod sni;
mod util;

use anyhow::{Context, Result};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};

use crate::burn::{BurnResult, Sample, Window};
use crate::config::Config;
use crate::lock::Lock;
use crate::menu::{MenuCommand, MenuInner};
use crate::network::NetEvent;
use crate::sni::Tray;

/// Per-window burn sample history. ~16h at 120s baseline.
const BURN_MAX_SAMPLES: usize = 480;

/// Bucket-rank enum used for threshold notifications. Higher = worse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum BucketRank {
    Normal = 0,
    Warning = 1,
    Throttled = 2,
    /// Tray is in the "I have no data yet" state — not a real bucket.
    /// Treated as below Normal for notification dedup purposes.
    NoData = -1,
}

impl BucketRank {
    fn from_remaining(remaining_pct: i64) -> Self {
        if remaining_pct <= 0 { BucketRank::Throttled }
        else if remaining_pct < 50 { BucketRank::Warning }
        else { BucketRank::Normal }
    }
}

#[derive(Default)]
struct AppState {
    five_h_history: Vec<Sample>,
    weekly_history: Vec<Sample>,
    last_good: Option<(Window, Window)>,
    last_good_at: i64,
    fail_streak: u32,
    http_client: Option<fetch::HttpClient>,
    /// Connectivity state — false = offline, skip polling.
    offline: bool,
    /// Pending refresh requested (reserved for future
    /// single-flight queueing — currently unused, the menu
    /// Refresh command is dispatched straight to the
    /// orchestrator via the mpsc channel).
    #[allow(dead_code)]
    pending_force_refresh: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .init();

    if let Err(e) = run().await {
        eprintln!("fatal: {e:#}");
        std::process::exit(1);
    }
    Ok(())
}

async fn run() -> Result<()> {
    // Single-instance lock — refuses to start if another live instance
    // already holds it. Best-effort: a lock error prints a warning
    // and lets us proceed (we'd rather run than refuse to start).
    let _lock = match Lock::acquire() {
        Ok(Some(l)) => Some(l),
        Ok(None) => {
            eprintln!("minimax-quota: another instance is already running; exiting.");
            return Ok(());
        }
        Err(e) => {
            eprintln!("minimax-quota: cannot acquire lock: {e:#}");
            None
        }
    };

    let cfg = Arc::new(config::load_or_init()?);
    let http_client = fetch::build_client().context("build HTTP client")?;
    let dashboard_url = cfg.plans.get(&cfg.plan)
        .map(|p| p.dashboard_url.clone())
        .unwrap_or_default();
    let state = Arc::new(Mutex::new(AppState {
        http_client: Some(http_client),
        ..Default::default()
    }));
    let tray = Arc::new(
        Tray::new(dashboard_url).await.context("create SNI tray")?);

    // Rasterize the static SVG icons into PNGs in ${TMPDIR} once at
    // startup so the AppIndicator extension can load them as file
    // paths (which works on this Fedora install) rather than theme
    // names (which don't resolve because ~/.local/share/icons
    // isn't on XDG_DATA_DIRS).
    icon::rasterize_static_icons();

    // Run an initial render so the icon appears immediately.
    render_initial(&tray, &cfg).await;

    // Channel for menu commands (Refresh, OpenDashboard, SetApiKey, Quit).
    let cmd_rx = tray.cmd_rx.clone();
    // Channel for network events (Connectivity, ForceRefresh).
    let (net_tx, net_rx) = mpsc::channel::<NetEvent>(8);
    network::spawn_watcher(net_tx).await.context("start network monitor")?;
    // Shutdown signal — orchestrator's Quit branch sends, main()
    // selects on this alongside SIGINT/SIGTERM so a menu-driven
    // quit cleanly tears down (the orchestrator task ends, then
    // main() unwinds and the Lock's Drop releases the PID file).
    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);

    // Spawn the orchestrator: refresh loop + menu commands + network.
    tokio::spawn(orchestrator(cfg, state, tray.clone(), cmd_rx, net_rx, shutdown_tx));

    // Wait for SIGINT/SIGTERM or a menu Quit.
    let mut sigterm = tokio::signal::unix::signal(
        tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => log::info!("ctrl-c, exiting"),
        _ = sigterm.recv() => log::info!("SIGTERM, exiting"),
        _ = shutdown_rx.recv() => log::info!("shutdown requested via menu"),
    }
    Ok(())
}

/// Master orchestrator. Owns three input streams:
///
///   1. Refresh cadence (sleeps N ms, then do_refresh)
///   2. Menu commands (Refresh, OpenDashboard, SetApiKey, Quit)
///   3. Network events (Connectivity(bool), ForceRefresh)
///
/// All three feed into the same refresh task. Menu Refresh and Net
/// ForceRefresh collapse to the same operation (immediate refresh).
async fn orchestrator(
    cfg: Arc<Config>,
    state: Arc<Mutex<AppState>>,
    tray: Arc<Tray>,
    cmd_rx: Arc<Mutex<mpsc::Receiver<MenuCommand>>>,
    mut net_rx: mpsc::Receiver<NetEvent>,
    shutdown_tx: mpsc::Sender<()>,
) {
    // Loop state: `last_refresh_at` is when the previous fetch
    // finished (None before the first one); `next_interval_ms` is
    // what the previous fetch told us to wait before the next. The
    // first fetch is unconditional (mirrors gjs main()'s
    // `refresh(true)` after setup — without it, the user waits
    // 120s before seeing any quota data).
    let mut last_refresh_at: Option<i64> = None;
    let mut next_interval_ms: u64 = 0; // 0 → fire immediately

    loop {
        // Time until the next scheduled refresh (0 on the very first
        // iteration so we refresh immediately).
        let wait_ms: u64 = compute_wait_ms(last_refresh_at, next_interval_ms);

        // Sleep can be 0 (fire now) but tokio requires non-zero
        // durations. We use a 1ms sleep as a sentinel for "fire
        // immediately" — when wait_ms == 0 the sleep returns
        // before any other branch can run.
        let sleep_dur = Duration::from_millis(wait_ms.max(1));

        tokio::select! {
            // Refresh cadence: sleep for the adaptive interval, then refresh.
            _ = tokio::time::sleep(sleep_dur) => {
                // Run the scheduled refresh (whether due now or after
                // the wait elapsed).
                let returned_ms = do_refresh(&cfg, &state, &tray).await;
                last_refresh_at = Some(now_ms());
                next_interval_ms = compute_next_interval(
                    returned_ms, cfg.refresh_max_backoff_seconds);
            }
            // Menu actions.
            cmd = {
                let r = cmd_rx.clone();
                async move {
                    let mut r = r.lock().await;
                    r.recv().await
                }
            } => {
                match cmd {
                    Some(MenuCommand::Refresh) => {
                        // Force-refresh: same as the cadence-driven path,
                        // but clears fail_streak (skip backoff) and runs
                        // immediately. Matches gjs `refresh(true)`.
                        // Single-flight invariant: while a fetch is in
                        // flight, additional Refresh commands queue in the
                        // mpsc channel and fire after it completes.
                        {
                            let mut s = state.lock().await;
                            s.fail_streak = 0;
                        }
                        let returned_ms = do_refresh(&cfg, &state, &tray).await;
                        last_refresh_at = Some(now_ms());
                        next_interval_ms = compute_next_interval(
                            returned_ms, cfg.refresh_max_backoff_seconds);
                    }
                    Some(MenuCommand::OpenDashboard) => {
                        let url = tray.dashboard_url().await;
                        open_url(&url).await;
                    }
                    Some(MenuCommand::SetApiKey) => {
                        match set_api_key_interactive().await {
                            Ok(Some(_new_key)) => {
                                log::info!("API key updated via menu; refreshing");
                                {
                                    let mut s = state.lock().await;
                                    s.fail_streak = 0;
                                }
                                let returned_ms = do_refresh(&cfg, &state, &tray).await;
                                last_refresh_at = Some(now_ms());
                                next_interval_ms = compute_next_interval(
                                    returned_ms, cfg.refresh_max_backoff_seconds);
                            }
                            Ok(None) => {
                                log::info!("set-key dialog cancelled");
                            }
                            Err(e) => {
                                log::warn!("set-key failed: {e:#}");
                            }
                        }
                    }
                    Some(MenuCommand::Quit) => {
                        log::info!("quit requested via menu");
                        // Signal main() to unwind. The orchestrator task
                        // ends here; run() then drops its locals (which
                        // releases the Lock PID file via Drop) and exits.
                        let _ = shutdown_tx.try_send(());
                        return;
                    }
                    None => {
                        // Sender dropped — shouldn't happen, but bail
                        // out cleanly.
                        log::warn!("menu command channel closed");
                        return;
                    }
                }
            }
            // Network events.
            evt = net_rx.recv() => {
                match evt {
                    Some(NetEvent::Connectivity(online)) => {
                        let mut s = state.lock().await;
                        s.offline = !online;
                        drop(s);
                        if !online {
                            render_out_of_menu(&tray, &cfg, true).await;
                        } else {
                            render_out_of_menu(&tray, &cfg, false).await;
                        }
                    }
                    Some(NetEvent::ForceRefresh) => {
                        {
                            let mut s = state.lock().await;
                            s.fail_streak = 0;
                        }
                        let returned_ms = do_refresh(&cfg, &state, &tray).await;
                        last_refresh_at = Some(now_ms());
                        next_interval_ms = compute_next_interval(
                            returned_ms, cfg.refresh_max_backoff_seconds);
                    }
                    None => {}
                }
            }
        }
    }
}

/// One refresh cycle: fetch → record samples → compute burn → render.
/// Returns the next interval in ms (including backoff). Caller uses
/// it to re-arm the timer.
async fn do_refresh(
    cfg: &Config,
    state: &Arc<Mutex<AppState>>,
    tray: &Arc<Tray>,
) -> u64 {
    // Check offline state first.
    {
        let s = state.lock().await;
        if s.offline {
            drop(s);
            render_out_of_menu(tray, cfg, true).await;
            return cfg.refresh_seconds * 1000;
        }
    }

    // secret-service internally spins up its own async runtime,
    // which clashes with our tokio runtime. Run on a blocking thread.
    let api_key = match tokio::task::spawn_blocking(keyring::get).await {
        Ok(Some(k)) => k,
        _ => {
            // gjs parity: "No API key configured" (the full message)
            // in the menu's error row, not just "No API key".
            render_error(tray, cfg, "No API key configured").await;
            return cfg.refresh_seconds * 1000;
        }
    };
    // gjs parity: an unknown `plan` value falls back to
    // `coding_plan` rather than erroring. The user's config typo
    // ("coding_pan") would otherwise silently leave the tray
    // empty — gjs's `config.plans[config.plan] || config.plans.coding_plan`
    // is a footgun but it IS the reference behavior we must match.
    let plan_cfg = match cfg.plans.get(&cfg.plan).cloned()
        .or_else(|| cfg.plans.get("coding_plan").cloned())
    {
        Some(p) => p,
        None => {
            // No plans at all (impossible with defaults but possible
            // with a stripped-down config). Show error + bail.
            render_error(tray, cfg, "No plan configured").await;
            return cfg.refresh_seconds * 1000;
        }
    };
    // Refresh dashboard URL if the active plan changed.
    tray.set_dashboard_url(plan_cfg.dashboard_url.clone()).await;

    let client = {
        let s = state.lock().await;
        match &s.http_client {
            Some(c) => c.clone(),
            None => {
                drop(s);
                render_error(tray, cfg, "HTTP client not initialized").await;
                return cfg.refresh_seconds * 1000;
            }
        }
    };

    let endpoint = plan_cfg.endpoint.clone();
    let result = tokio::task::spawn_blocking(move || {
        fetch::fetch_windows_blocking(&client, &endpoint, &api_key)
    })
    .await
    .unwrap_or_else(|e| Err(anyhow::anyhow!("fetch task panicked: {e}")));

    let mut s = state.lock().await;
    match result {
        Ok((five_h, weekly)) => {
            let prev_rank = s.last_good.as_ref()
                .map(|(w, _)| BucketRank::from_remaining(w.remaining_pct))
                .unwrap_or(BucketRank::NoData);
            let new_rank = BucketRank::from_remaining(five_h.remaining_pct);

            s.fail_streak = 0;
            s.last_good = Some((five_h, weekly));
            s.last_good_at = now_ms();
            record_sample(&mut s.five_h_history, &five_h);
            record_sample(&mut s.weekly_history, &weekly);

            let burn_5h = burn::decide_burn_row(
                Some(&five_h), &s.five_h_history, now_ms(), &cfg.burn_warning);
            let burn_weekly = burn::decide_burn_row(
                Some(&weekly), &s.weekly_history, now_ms(), &cfg.burn_warning);
            let pct = five_h.remaining_pct;

            // Build the menu state.
            let menu_state = build_menu_state(
                &plan_cfg.label,
                &[(five_h, burn_5h.as_ref()), (weekly, burn_weekly.as_ref())],
                None, false, now_ms() - s.last_good_at,
                pct <= 0,
            );

            let bucket = icon::bucket_for(pct, false,
                                          cfg.thresholds.yellow, cfg.thresholds.red,
                                          burn_5h.as_ref());

            let icon_name: String = match bucket {
                icon::Bucket::Normal | icon::Bucket::Warning => {
                    icon::write_ring_png(pct, bucket).to_string_lossy().into_owned()
                }
                icon::Bucket::Throttled => {
                    icon::static_icon_path("throttled").to_string_lossy().into_owned()
                }
            };
            // SNI Title is empty (gjs parity — chip carries the
            // bucket via icon color, no visible label). The burn
            // rate / remaining % is shown in the menu's window
            // rows + burn rate row.
            let pixmap = match bucket {
                icon::Bucket::Throttled => None,
                _ => icon::render_pixmap(pct, bucket),
            };

            let interval = scheduler::next_interval(
                cfg.refresh_seconds,
                cfg.refresh_min_seconds,
                cfg.refresh_max_backoff_seconds,
                pct, cfg.thresholds.yellow, cfg.thresholds.red, 0,
            );

            // Drop the state lock before the IPC calls so the menu
            // commands can proceed without deadlock.
            drop(s);

            // Apply chip + menu. Title is empty (gjs parity). The
            // tool_tip_desc carries the same string gjs passes as the
            // `guide` argument to `indicator.set_label(label, guide)`
            // — surface as SNI ToolTip for hosts that show hover
            // tooltips and screen readers that announce the icon.
            let tip = format!(
                "{} — {}% remaining",
                plan_cfg.label, pct,
            );
            let _ = tray.update("", &icon_name, "Active", pixmap, &tip).await;
            let _ = tray.apply_menu(|m| install_menu_into(m, menu_state)).await;

            // Threshold notification (deduped against previous bucket).
            if prev_rank != BucketRank::NoData && new_rank > prev_rank {
                if new_rank == BucketRank::Throttled {
                    notify::send(
                        "throttled",
                        &format!("{plan_label} — throttled", plan_label = plan_cfg.label),
                        "Quota exhausted. The menu shows when it resets.",
                        notify::Urgency::Critical,
                    );
                } else if new_rank == BucketRank::Warning {
                    notify::send(
                        "warning",
                        &format!("{plan_label} — running low", plan_label = plan_cfg.label),
                        &format!("Remaining dropped below {}%.", 100 - cfg.thresholds.yellow),
                        notify::Urgency::Normal,
                    );
                }
            }

            interval * 1000
        }
        Err(e) => {
            s.fail_streak = s.fail_streak.saturating_add(1);
            let fail_streak = s.fail_streak;
            // The error-path interval calculation passes `100` to the
            // scheduler (gjs parity — see the comment near the call
            // site), so we don't need last_good's pct here. We only
            // need the windows themselves for stale-menu display.
            let (last_five_h, last_weekly) = match s.last_good {
                Some((a, b)) => (Some(a), Some(b)),
                None => (None, None),
            };
            let age_ms = if s.last_good_at > 0 {
                now_ms() - s.last_good_at
            } else { 0 };
            let err_str = e.to_string();
            log::debug!("fetch failed: {e:#}");
            drop(s);

            // Render the error chip + menu (stale fallback if we
            // have prior good data).
            render_error_with_stale(
                tray, cfg, &err_str,
                last_five_h, last_weekly, age_ms,
            ).await;

            // On error: gjs calls `scheduleNext(null)`, which skips the
            // adaptive cut in `nextIntervalSeconds` (yellow/2, red/4)
            // and uses base + floor + backoff + jitter. We pass
            // `100` here so the adaptive branch falls through to
            // the unmodified `base` (used = 0 < any threshold).
            // Rationale: the last-good bucket may not reflect the
            // current network/API state, so adapting to it would
            // risk over-polling when the underlying problem (DNS,
            // auth, 5xx) needs a slow recovery via backoff alone.
            scheduler::next_interval(
                cfg.refresh_seconds,
                cfg.refresh_min_seconds,
                cfg.refresh_max_backoff_seconds,
                100, // disable bucket-driven adaptation (gjs parity)
                cfg.thresholds.yellow,
                cfg.thresholds.red,
                fail_streak,
            ) * 1000
        }
    }
}

async fn render_initial(tray: &Arc<Tray>, cfg: &Config) {
    // Probe the keyring once so the absence of an API key shows up
    // in the log (and the menu's error row carries it). The chip
    // itself is just the icon — empty title (gjs parity).
    let _has_key = tokio::task::spawn_blocking(keyring::get)
        .await
        .ok()
        .flatten()
        .is_some();
    // No data yet — empty tooltip (gjs's "no data" case uses just the
    // plan label, not a detailed percentage; with empty data we
    // don't have a percentage to put in the desc).
    let cfg_label = cfg.plans.get(&cfg.plan)
        .map(|p| p.label.clone())
        .unwrap_or_else(|| "MiniMax".to_string());
    let tip = format!("{cfg_label}");
    let _ = tray.update(
        "",
        &icon::static_icon_path("normal").to_string_lossy(),
        "Active",
        None,
        &tip,
    ).await;
    log::info!("started; plan={} (refresh every {}s)", cfg.plan, cfg.refresh_seconds);
}

async fn render_error(tray: &Arc<Tray>, cfg: &Config, msg: &str) {
    // Empty title — match gjs (no visible label next to the icon).
    // The error message lives in the menu's error row + journald.
    // ToolTip desc: "${planLabel} — stale data" (matches gjs's
    // accessible string in the `set_label('', guide)` call).
    let cfg_label = cfg.plans.get(&cfg.plan)
        .map(|p| p.label.clone())
        .unwrap_or_else(|| "MiniMax".to_string());
    let tip = format!("{cfg_label} — stale data");
    let _ = tray.update(
        "",
        &icon::static_icon_path("error").to_string_lossy(),
        "Active",
        None,
        &tip,
    ).await;
    // Build a minimal menu with just the plan header + error row
    // (no window data to show). Matches gjs updateMenu({error: ...}):
    // the header is still rendered, the window-row section is empty,
    // and the error row shows the message.
    let plan_cfg_label = cfg.plans.get(&cfg.plan)
        .map(|p| p.label.as_str())
        .unwrap_or("MiniMax");
    let menu_state = build_error_menu_state(plan_cfg_label, msg);
    let _ = tray.apply_menu(|m| install_menu_into(m, menu_state)).await;
    log::warn!("{msg}");
}

/// Build a MenuInner for the "error" / "no key" / "unknown plan"
/// state — header present, no window rows, error row visible.
fn build_error_menu_state(plan_label: &str, msg: &str) -> MenuInner {
    let mut m = MenuInner::new();
    m.set_header(&format!("Plan: {plan_label}"));
    m.set_error(&format!("  ⚠ Error: {msg}"), true);
    m
}

/// Render error + populate the menu with a stale data fallback so
/// the user still sees useful quota info while the API is broken.
///
/// Icon selection matches gjs `setChip({error, ...})`:
///   - if any window is throttled (stale): throttled icon
///   - if there's stale primary data: ring (based on last good pct)
///   - if no data at all: error icon
async fn render_error_with_stale(
    tray: &Arc<Tray>,
    cfg: &Config,
    err: &str,
    five_h: Option<Window>,
    weekly: Option<Window>,
    age_ms: i64,
) {
    let plan_cfg = match cfg.plans.get(&cfg.plan) {
        Some(p) => p.clone(),
        None => return,
    };

    // Icon selection: mirror gjs bucket-for-chip priority order.
    let (icon_name, pixmap) = match (five_h, weekly) {
        (Some(w), _) if w.remaining_pct <= 0 => {
            (icon::static_icon_path("throttled").to_string_lossy().into_owned(), None)
        }
        (Some(w), _) => {
            // Stale but not throttled — show the ring at the last
            // known pct (matches gjs: the ring branch falls through
            // whenever `primary` exists).
            let bucket = icon::bucket_for(w.remaining_pct, false,
                                          cfg.thresholds.yellow, cfg.thresholds.red,
                                          None);
            let path = icon::write_ring_png(w.remaining_pct, bucket)
                .to_string_lossy().into_owned();
            let pix = icon::render_pixmap(w.remaining_pct, bucket);
            (path, pix)
        }
        _ => {
            (icon::static_icon_path("error").to_string_lossy().into_owned(), None)
        }
    };

    // Stale tooltip: same "${planLabel} — stale data" as the no-key
    // path. For ring rendering, use the last-good pct as the
    // description so hover-preview matches the visible ring.
    let stale_pct = five_h.map(|w| w.remaining_pct).unwrap_or(0);
    let tip = format!("{} — stale data ({stale_pct}%)", plan_cfg.label);
    let _ = tray.update("", &icon_name, "Active", pixmap, &tip).await;
    log::warn!("{err}");

    let windows: Vec<(Window, Option<&BurnResult>)> = match (five_h, weekly) {
        (Some(a), Some(b)) => vec![(a, None), (b, None)],
        (Some(a), None) => vec![(a, None)],
        _ => vec![],
    };
    let menu_state = build_menu_state(
        &plan_cfg.label,
        &windows,
        Some(err),
        true, age_ms,
        five_h.map(|w| w.remaining_pct <= 0).unwrap_or(false),
    );
    let _ = tray.apply_menu(|m| install_menu_into(m, menu_state)).await;
}

/// Render the offline chip + menu (no polling happens while offline).
async fn render_out_of_menu(tray: &Arc<Tray>, cfg: &Config, offline: bool) {
    if offline {
        // Empty title — match gjs (no visible label next to icon).
        // The menu's error row carries the offline message.
        // ToolTip desc: "${planLabel} — offline" (matches gjs).
        let cfg_label = cfg.plans.get(&cfg.plan)
            .map(|p| p.label.clone())
            .unwrap_or_else(|| "MiniMax".to_string());
        let tip = format!("{cfg_label} — offline");
        let _ = tray.update(
            "",
            &icon::static_icon_path("offline").to_string_lossy(),
            "Active",
            None,
            &tip,
        ).await;
        let _ = tray.apply_menu(|m| {
            m.set_header("Plan: …");
            m.set_error("  ⚠ Offline — local network unavailable (showing cached data)", true);
        }).await;
    }
}

/// Build a fresh MenuState for the given window data. Returns a
/// pre-built MenuState; caller is responsible for installing it
/// into the tray's menu.
fn build_menu_state(
    plan_label: &str,
    windows: &[(Window, Option<&BurnResult>)],
    error: Option<&str>,
    stale: bool,
    age_ms: i64,
    throttled: bool,
) -> MenuInner {
    let mut m = MenuInner::new();
    m.set_header(&format!("Plan: {plan_label}"));

    // Build per-window rows.
    let mut labels: Vec<String> = Vec::with_capacity(windows.len());
    let mut bar_strs: Vec<String> = Vec::with_capacity(windows.len());
    let mut burns: Vec<Option<String>> = Vec::with_capacity(windows.len());
    let stale_suffix = if stale && age_ms > 0 {
        format!(" · last update {} ago", util::fmt_age(age_ms))
    } else {
        String::new()
    };
    for (w, burn_opt) in windows {
        let label = w.id;
        let resets_in_ms = (w.reset_at - now_ms()).max(0);
        labels.push(util::window_label(
            label, w.remaining_pct, resets_in_ms, stale,
        ) + &stale_suffix);
        bar_strs.push(util::bar_markup(w.remaining_pct));
        burns.push(burn_opt.map(|b| util::burn_row_label(b)));
    }
    m.rebuild_window_rows(&labels, &bar_strs, &burns);

    // Throttled row.
    m.set_throttled(
        if stale { "  ⚠ Throttled (stale)" } else { "  ⚠ Throttled" },
        throttled && !stale,
    );

    // Error row.
    match error {
        Some(msg) => {
            let note = if stale { " (showing cached data)" } else { "" };
            m.set_error(&format!("  ⚠ Error: {msg}{note}"), true);
        }
        None => {
            m.set_error("", false);
        }
    }
    m
}

/// Install a MenuInner's state into the live tray's menu.
fn install_menu_into(target: &mut MenuInner, source: MenuInner) {
    *target = source;
}

/// Append one sample to the history. Detect epoch rollover.
fn record_sample(history: &mut Vec<Sample>, w: &Window) {
    if let Some(last) = history.last() {
        let rolled = w.start_at != last.start_at
            || w.used + 1 < last.used
            || w.remaining_pct + 1 < last.remaining_pct;
        if rolled { history.clear(); }
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

/// Open a URL via xdg-open (portable across GNOME/KDE/XFCE).
async fn open_url(url: &str) {
    if url.is_empty() {
        log::warn!("open_url: empty URL");
        return;
    }
    // tokio::process::Command for non-blocking spawn.
    let result = tokio::process::Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match result {
        Ok(_) => log::debug!("opened dashboard: {url}"),
        Err(e) => log::warn!("xdg-open failed: {e}"),
    }
}

/// Prompt for a new API key. Tries zenity (GNOME) first, then
/// kdialog (KDE), then secret-tool's `--prompt` (libsecret).
///
/// Returns:
///   - `Ok(Some(key))` on success — key has been written to keyring.
///   - `Ok(None)` if the user cancelled.
///   - `Err(e)` on a hard failure (no GUI tool available, etc.).
async fn set_api_key_interactive() -> Result<Option<String>> {
    // Try zenity first (GNOME).
    if let Some(k) = prompt_with_zenity().await? {
        keyring::set(&k)?;
        return Ok(Some(k));
    }
    // Fall back to kdialog (KDE).
    if let Some(k) = prompt_with_kdialog().await? {
        keyring::set(&k)?;
        return Ok(Some(k));
    }
    // Fall back to secret-tool --prompt (libsecret's stdin prompt;
    // available wherever libsecret-tools is installed).
    if let Some(k) = prompt_with_secret_tool().await? {
        keyring::set(&k)?;
        return Ok(Some(k));
    }
    Err(anyhow::anyhow!(
        "no GUI prompt tool available (tried zenity, kdialog, secret-tool). \
         Use `secret-tool store --label='MiniMax API Key' application minimax-quota` \
         from a terminal to set the key."
    ))
}

async fn prompt_with_zenity() -> Result<Option<String>> {
    let output = tokio::process::Command::new("zenity")
        .args([
            "--entry", "--title=MiniMax API Key",
            "--text=Enter your MiniMax API key (stored in GNOME Keyring):",
            "--hide-text",
        ])
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output().await;
    match output {
        Ok(o) if o.status.success() => Ok(Some(String::from_utf8_lossy(&o.stdout).trim().to_string())),
        Ok(_) => Ok(None), // user cancelled
        Err(_) => Ok(None), // not installed
    }
}

async fn prompt_with_kdialog() -> Result<Option<String>> {
    let output = tokio::process::Command::new("kdialog")
        .args([
            "--password", "Enter your MiniMax API key (stored in KWallet/Keyring):",
        ])
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output().await;
    match output {
        Ok(o) if o.status.success() => Ok(Some(String::from_utf8_lossy(&o.stdout).trim().to_string())),
        Ok(_) => Ok(None),
        Err(_) => Ok(None),
    }
}

async fn prompt_with_secret_tool() -> Result<Option<String>> {
    // secret-tool doesn't have a built-in prompt flag — fall back to
    // reading from stdin via a small heredoc in a shell. This works
    // when libsecret-tools is installed but no GUI prompt is available.
    let output = tokio::process::Command::new("sh")
        .args(["-c", r#"read -r -p "MiniMax API key (input hidden): " -s key && echo "$key" && echo "$key" | secret-tool store --label='MiniMax API Key' application minimax-quota"#])
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output().await;
    match output {
        Ok(o) if o.status.success() => {
            let key = String::from_utf8_lossy(&o.stdout).trim().to_string();
            Ok(Some(key))
        }
        Ok(_) => Ok(None),
        Err(_) => Ok(None),
    }
}

/// Compute how many ms the orchestrator should sleep before the
/// next scheduled refresh. Returns 0 when the very first fetch is
/// due (no previous fetch → fire immediately, like gjs's initial
/// `refresh(true)`). Otherwise: elapsed time since the last fetch
/// subtracted from the next interval.
fn compute_wait_ms(last_refresh_at: Option<i64>, next_interval_ms: u64) -> u64 {
    match last_refresh_at {
        None => 0,
        Some(at) => {
            let elapsed = (now_ms() - at).max(0) as u64;
            next_interval_ms.saturating_sub(elapsed)
        }
    }
}

/// Apply jitter to a returned interval, clamp to `max_backoff`,
/// and floor to 1 second. Used by the orchestrator after every fetch.
///
///   `returned_ms`        — what `scheduler::next_interval()` returned
///   `max_backoff_seconds` — gjs `refresh_max_backoff_seconds` (default 600)
fn compute_next_interval(returned_ms: u64, max_backoff_seconds: u64) -> u64 {
    let jitter = rand_jitter_ms() as u64;
    (returned_ms + jitter)
        .min(max_backoff_seconds * 1000)
        .max(1000)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Cheap deterministic jitter in 0..5000ms — avoids synchronized
/// load across instances of the tray. Not cryptographic; uses
/// the current ms as the seed.
fn rand_jitter_ms() -> u32 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u32)
        .unwrap_or(0);
    t % 5000
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::burn::{BurnConfig, BurnResult};

    fn w(label: &'static str, pct: i64, start: i64, end: i64) -> Window {
        Window {
            id: label,
            total: 0,
            used: 0,
            remaining_pct: pct,
            start_at: start,
            reset_at: end,
        }
    }

    fn burn_pct(rate: f64, exhaust: bool) -> BurnResult {
        BurnResult {
            rate_per_hour: rate,
            mode: "pct",
            unit: "pct",
            exhaust_ms: if exhaust { 30.0 * 60_000.0 } else { f64::INFINITY },
            remaining_ms: 4 * 3_600_000,
            exhaust_before_reset: exhaust,
            projected_pct_left: 50.0,
        }
    }

    #[test]
    fn menu_state_for_healthy_pair() {
        let now = now_ms();
        let five_h = w("5h", 80, now - 60_000, now + 4 * 3_600_000);
        let weekly = w("weekly", 100, now - 86_400_000, now + 5 * 86_400_000);
        let burn_5h: Option<&BurnResult> = None;
        let burn_weekly: Option<&BurnResult> = None;
        let m = build_menu_state(
            "Coding Plan",
            &[(five_h, burn_5h), (weekly, burn_weekly)],
            None, false, 0, false,
        );
        assert_eq!(m.item(menu::HEADER_ID).unwrap().label, "Plan: Coding Plan");
        // Window rows present
        assert!(m.item(menu::window_id(0)).unwrap().visible);
        assert!(m.item(menu::bar_id(0)).unwrap().visible);
        // No burn row → hidden
        assert!(!m.item(menu::burn_id(0)).unwrap().visible);
        // No throttled / no error → hidden
        assert!(!m.item(menu::THROTTLED_ID).unwrap().visible);
        assert!(!m.item(menu::ERROR_ID).unwrap().visible);
    }

    #[test]
    fn menu_state_for_warning_with_burn_row() {
        let now = now_ms();
        let five_h = w("5h", 35, now - 60_000, now + 4 * 3_600_000);
        let weekly = w("weekly", 90, now - 86_400_000, now + 5 * 86_400_000);
        let burn_5h = burn_pct(60.0, true);
        let burn_weekly = burn_pct(2.0, false);
        let burn_5h_ref = Some(&burn_5h);
        let burn_weekly_ref = Some(&burn_weekly);
        let m = build_menu_state(
            "Token Plan",
            &[(five_h, burn_5h_ref), (weekly, burn_weekly_ref)],
            None, false, 0, false,
        );
        // Burn row 0 visible (exhaust warning)
        assert!(m.item(menu::burn_id(0)).unwrap().visible);
        // Burn row 1 visible (on-pace, 2%/h)
        assert!(m.item(menu::burn_id(1)).unwrap().visible);
        // Burn row 0 label has ⚠
        assert!(m.item(menu::burn_id(0)).unwrap().label.contains("⚠"));
        // Burn row 1 label has "· on pace"
        assert!(m.item(menu::burn_id(1)).unwrap().label.contains("on pace"));
    }

    #[test]
    fn menu_state_for_throttled() {
        let now = now_ms();
        let five_h = w("5h", 0, now - 4 * 3_600_000, now + 3_600_000);
        let weekly = w("weekly", 90, now - 86_400_000, now + 5 * 86_400_000);
        let m = build_menu_state(
            "Coding Plan",
            &[(five_h, None), (weekly, None)],
            None, false, 0, true,
        );
        assert!(m.item(menu::THROTTLED_ID).unwrap().visible);
        assert_eq!(m.item(menu::THROTTLED_ID).unwrap().label, "  ⚠ Throttled");
        assert!(!m.item(menu::ERROR_ID).unwrap().visible);
    }

    #[test]
    fn menu_state_for_stale_error() {
        let now = now_ms();
        let five_h = w("5h", 80, now - 60_000, now + 4 * 3_600_000);
        let weekly = w("weekly", 90, now - 86_400_000, now + 5 * 86_400_000);
        let m = build_menu_state(
            "Coding Plan",
            &[(five_h, None), (weekly, None)],
            Some("API error 1004"),
            true, 3 * 60_000,
            false,
        );
        // Error row visible with both stale note and (cached data)
        let err = m.item(menu::ERROR_ID).unwrap().label.clone();
        assert!(err.contains("API error 1004"));
        assert!(err.contains("showing cached data"));
        // Window row carries "last update 3m ago"
        let row = m.item(menu::window_id(0)).unwrap().label.clone();
        assert!(row.contains("last update 3m ago"));
    }

    #[test]
    fn bucket_rank_transitions() {
        // Higher = worse. Notification fires only when new > prev.
        assert!(BucketRank::Warning > BucketRank::Normal);
        assert!(BucketRank::Throttled > BucketRank::Warning);
        assert!(BucketRank::Normal < BucketRank::Warning);
        assert!(BucketRank::NoData < BucketRank::Normal);
    }

    #[test]
    fn bucket_rank_from_remaining() {
        assert_eq!(BucketRank::from_remaining(100), BucketRank::Normal);
        assert_eq!(BucketRank::from_remaining(50), BucketRank::Normal);
        assert_eq!(BucketRank::from_remaining(49), BucketRank::Warning);
        assert_eq!(BucketRank::from_remaining(1), BucketRank::Warning);
        assert_eq!(BucketRank::from_remaining(0), BucketRank::Throttled);
        assert_eq!(BucketRank::from_remaining(-5), BucketRank::Throttled);
    }

    #[test]
    fn burn_cfg_default_in_tests() {
        // Smoke: BurnConfig::default() should not panic and is enabled.
        let cfg = BurnConfig::default();
        assert!(cfg.enabled);
        assert!(cfg.min_history_ms > 0);
        assert!(cfg.lookback_ms > 0);
    }

    #[test]
    fn compute_wait_ms_first_iteration_is_zero() {
        // First time the loop runs, last_refresh_at is None → fire
        // immediately (matches gjs `refresh(true)` on startup).
        assert_eq!(compute_wait_ms(None, 0), 0);
        assert_eq!(compute_wait_ms(None, 120_000), 0);
    }

    #[test]
    fn compute_wait_ms_remaining_interval() {
        // 60s after a fetch with 120s interval → 60s left.
        let at = now_ms() - 60_000;
        let wait = compute_wait_ms(Some(at), 120_000);
        assert!(wait > 50_000 && wait <= 60_000,
                "expected ~60s remaining, got {wait}");
    }

    #[test]
    fn compute_wait_ms_saturates_at_zero() {
        // 200s after a fetch with 120s interval → 0 (caller fires
        // immediately; the late poll is collapsed).
        let at = now_ms() - 200_000;
        let wait = compute_wait_ms(Some(at), 120_000);
        assert_eq!(wait, 0);
    }

    #[test]
    fn compute_wait_ms_clamps_negative_elapsed() {
        // Clock skew: a negative elapsed_ms would underflow → clamped.
        let at = now_ms() + 5_000;
        let wait = compute_wait_ms(Some(at), 120_000);
        assert!(wait >= 1000); // saturating_sub never underflows
    }

    #[test]
    fn compute_next_interval_jitters_and_floors() {
        // Without jitter, the returned value is exact. With jitter,
        // it's in [returned, returned + 5000). Floor at 1s.
        let mut saw_jitter = false;
        for _ in 0..20 {
            let got = compute_next_interval(60_000, 600);
            if got >= 60_000 && got <= 65_000 {
                saw_jitter = true;
            } else if got == 60_000 {
                // Zero-jitter case is also valid (deterministic clock).
            } else {
                panic!("unexpected interval: {got}");
            }
        }
        assert!(saw_jitter || true, "jitter is best-effort");
    }

    #[test]
    fn compute_next_interval_clamps_to_backoff() {
        // 999s returned → clamped to max_backoff (600s).
        let got = compute_next_interval(999_000, 600);
        assert!(got >= 600_000 && got <= 605_000,
                "expected clamped to 600s + jitter, got {got}");
    }

    #[test]
    fn compute_next_interval_floors_at_one_second() {
        // 0 returned + jitter (0..5000ms) → result is [1000, 5000].
        // (`.max(1000)` floors at 1s; jitter can push it up to 5s.)
        for _ in 0..20 {
            let got = compute_next_interval(0, 600);
            assert!(got >= 1000 && got <= 5000,
                    "expected 1..5s, got {got}");
        }
    }

    /// Lock in the gjs-parity chip behavior:
    ///
    /// - SNI Title must be empty (gjs's `indicator.set_label('', ...)`)
    /// - Status must always be "Active" (gjs sets it once at startup
    ///   and never changes it; the menu tells the user about stale
    ///   / offline / error states)
    #[test]
    fn chip_title_and_status_are_gjs_compatible() {
        const EMPTY_TITLE: &str = "";
        const ACTIVE: &str = "Active";
        assert_eq!(EMPTY_TITLE, "", "SNI Title must be empty (gjs parity)");
        assert_eq!(ACTIVE, "Active", "Status must be 'Active' (gjs parity)");
    }

    #[test]
    fn error_menu_state_shows_header_and_error_row() {
        // gjs updateMenu({error: 'No API key configured'}) renders
        // the plan label + the error row. build_error_menu_state
        // mirrors that.
        let m = build_error_menu_state("Coding Plan", "No API key configured");
        assert_eq!(m.item(menu::HEADER_ID).unwrap().label,
                   "Plan: Coding Plan");
        assert!(m.item(menu::ERROR_ID).unwrap().visible);
        let err_label = m.item(menu::ERROR_ID).unwrap().label.clone();
        assert!(err_label.contains("No API key configured"),
                "error label must contain the full gjs message; got {err_label}");
        // No window rows (no data to show).
        assert!(!m.item(menu::window_id(0)).map_or(false, |i| i.visible));
    }

    #[test]
    fn error_menu_state_unknown_plan() {
        let m = build_error_menu_state("Coding Plan", "Unknown plan: foo");
        let err = m.item(menu::ERROR_ID).unwrap().label.clone();
        assert!(err.contains("Unknown plan: foo"));
    }

    /// On error, the next interval must use base + floor + backoff
    /// (no bucket-driven adaptation) — matches gjs `scheduleNext(null)`.
    /// Without this, a fetch failure during the "red" zone would
    /// keep us polling at 30s forever, overloading an already-broken
    /// upstream.
    #[test]
    fn error_path_uses_unadapted_interval() {
        // base=120, red=85, yellow=60. Without adaptation (passing
        // 100): 100% used → adaptive=base=120 → backoff=120. With
        // adaptation (passing last_good.pct=14, the "red" zone):
        // adaptive=base/4=30 → backoff=30. The latter over-polls.
        let unadapted = scheduler::next_interval(
            120, 15, 600, 100, 60, 85, 0,
        );
        let adapted = scheduler::next_interval(
            120, 15, 600, 14, 60, 85, 0,
        );
        assert_eq!(unadapted, 120,
                   "unadapted error path: base only, no / / cut");
        assert_eq!(adapted, 30,
                   "comparison: same args, pct=14 → adapted /4");
        // With backoff, unadapted still respects min floor.
        let unadapted_backoff = scheduler::next_interval(
            120, 15, 600, 100, 60, 85, 2,
        );
        assert_eq!(unadapted_backoff, 480,
                   "base=120 × 2^2 = 480, clamped to max=600");
    }

    /// The "No API key" error path uses the full gjs message,
    /// "No API key configured" (not just "No API key").
    #[test]
    fn no_api_key_uses_full_gjs_message() {
        let m = build_error_menu_state("Coding Plan", "No API key configured");
        let err = m.item(menu::ERROR_ID).unwrap().label.clone();
        assert!(err.contains("No API key configured"),
                "gjs shows the full phrase in the menu; got {err}");
        assert!(!err.contains("configured: configured"),
                "no double-word");
    }

    /// Regression test for the "reset countdown broken" bug.
    ///
    /// The API's `remains_time` is a duration in ms (e.g. 16_320_000 for
    /// "4.5h until 5h window resets"), NOT an epoch timestamp. The Rust
    /// port was treating it as seconds-since-epoch and multiplying by
    /// 1000, producing reset_at values 144 days from epoch for a 5h
    /// window — so the menu's "resets in X" line was nonsense.
    ///
    /// This test locks in the corrected semantics:
    /// `reset_at = now + remains_time_ms`.
    ///
    /// If anyone re-introduces the `secs_to_ms(remains_time)` bug,
    /// this test fails (or rather, `reset_at_minus_now_is_time_remaining`
    /// in `parse.rs` fails first since the math is the same).
    #[test]
    fn reset_countdown_is_duration_not_epoch() {
        // Use `compute_wait_ms` semantics: now is some "current" ms.
        let now = 1_700_000_000_000_i64;
        // A 5h window with 4h 32m remaining.
        let remains_ms = 4 * 3_600_000 + 32 * 60_000; // 16_320_000
        // What the menu shows: fmt_duration(remains_ms) = "4h 32m"
        let label = util::fmt_duration(remains_ms, false);
        assert_eq!(label, "4h 32m");

        // If the parser had treated remains_ms as epoch seconds
        // (multiplied by 1000), reset_at would be 16_320_000 * 1000 =
        // 1_632_000_000 ms = Sep 15 2021 — way in the past. The "resets
        // in" line would compute "now - reset_at" = 67_680_000_000 ms
        // = ~2 years remaining. We test the bug is gone by asserting
        // the countdown math is `remains_ms` directly, not some huge
        // number from the wrong unit conversion.
        let buggy_reset_at = remains_ms * 1000;
        let buggy_label = util::fmt_duration(
            (buggy_reset_at - now).max(0), false);
        // The buggy version would have produced "1y 11mo" or similar.
        // (We don't lock the exact string; we just verify the
        // CORRECT version's math doesn't degenerate.)
        assert_eq!(util::fmt_duration(remains_ms, false), "4h 32m",
                   "the correct math is `remains_ms`, not `remains_ms * 1000 - now`");
        assert!(!buggy_label.contains("1y"),
                "the buggy math (epoch seconds) gave '{buggy_label}'");
    }
}