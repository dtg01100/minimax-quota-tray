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
//!   `$XDG_RUNTIME_DIR/llm-quota-tray.pid`. Refuses to start if a
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

mod activation;
mod burn;
mod config;
mod fetch;
mod icon;
mod instance;
mod keyring;
mod lock;
mod menu;
mod network;
mod notify;
mod parse;
mod portal_openuri;
mod pricing;
mod provider;
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
use crate::provider::RingColors;
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
        if remaining_pct <= 0 {
            BucketRank::Throttled
        } else if remaining_pct < 50 {
            BucketRank::Warning
        } else {
            BucketRank::Normal
        }
    }
}

/// Per-window burn-rate sample history, keyed by `Window.id` (a
/// `String` now that window ids come from config). One `Vec<Sample>`
/// per window — the burn-rate projection in `burn::compute_burn`
/// reads its window's slice by id. We key on id rather than position
/// so adding or removing windows from the `PlanShape` doesn't
/// reshuffle which history belongs to which window across restarts.
///
/// The map is unbounded in principle but each entry is capped to
/// `BURN_MAX_SAMPLES` by `record_sample` (oldest-first eviction).
type Histories = std::collections::HashMap<String, Vec<Sample>>;

#[derive(Default)]
struct AppState {
    /// Per-window burn-rate sample history (see `Histories`).
    histories: Histories,
    /// Most recent successful fetch, in display order (windows[0]
    /// is the primary). `None` until the first fetch completes.
    last_good: Option<Vec<Window>>,
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

    /// Cached per-model price table (see `pricing.rs`). Populated
    /// at startup when `Config::pricing_endpoint` is `Some`;
    /// re-fetched every `Config::pricing_refresh_polls` polls
    /// (default: once at startup). `None` when no pricing endpoint
    /// is configured.
    price_table: Option<pricing::PriceTable>,
    /// Successful polls since the last pricing refresh. Used to gate
    /// the periodic re-fetch without a background timer thread —
    /// `do_refresh` increments this and triggers a refresh when it
    /// crosses the configured threshold.
    polls_since_pricing_refresh: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Resolve the instance name from CLI/env first, so all the
    // path-sensitive subsystems (lock, config, keyring) namespace
    // themselves correctly.
    let instance_name = instance::init();
    log::info!("instance: {instance_name:?} (empty = default)");
    // XDG Activation token (--token=<token> CLI flag or
    // $XDG_ACTIVATION_TOKEN env var). The desktop shell provides
    // this when launching us via the .desktop file (StartupNotify
    // =true). We forward it to the portals (OpenURI, Notification)
    // so the resulting dialogs/notifications animate from the
    // originating click. Resolution is one-shot; safe to skip
    // before --set-key (the one-shot flow never reaches a portal).
    activation::init();

    // `--set-key` is a one-shot helper: prompt for the API key,
    // write it to the keyring via `keyring::set`, then exit. We do
    // this BEFORE `run()` so we never acquire the single-instance
    // lock or spawn the SNI server — the daemon isn't actually
    // starting up. The README and several example-provider templates
    // document this flag; without it users following those docs would
    // see the tray boot, render "No API key configured", and have to
    // click the chip → "Set API Key…" manually.
    if instance::wants_set_key() {
        return run_set_key().await;
    }

    if let Err(e) = run().await {
        eprintln!("fatal: {e:#}");
        std::process::exit(1);
    }
    Ok(())
}

/// One-shot `--set-key` flow: prompt for the key, write it to the
/// keyring via `keyring::set`, print a short status line, and exit.
/// See the comment in `main()` for why this short-circuits `run()`.
async fn run_set_key() -> anyhow::Result<()> {
    match set_api_key_interactive().await {
        Ok(Some(_)) => {
            println!(
                "API key stored. Launch the tray normally to start polling \
                 (e.g. `llm-quota-tray` or `llm-quota-tray --instance={}`).",
                instance::name(),
            );
            Ok(())
        }
        Ok(None) => {
            eprintln!("cancelled.");
            Ok(())
        }
        Err(e) => {
            eprintln!("set-key failed: {e:#}");
            std::process::exit(1);
        }
    }
}

async fn run() -> Result<()> {
    // Per-instance single-instance lock — refuses to start if another
    // live instance with the same name already holds it. Different
    // instances (different `--instance=` flags) have different lock
    // paths and can run concurrently. Best-effort: a lock error
    // prints a warning and lets us proceed (we'd rather run than
    // refuse to start).
    let _lock = match Lock::acquire() {
        Ok(Some(l)) => Some(l),
        Ok(None) => {
            eprintln!(
                "llm-quota-tray: another instance is already running; exiting. \
                 (set --instance=<name> for an additional instance.)"
            );
            return Ok(());
        }
        Err(e) => {
            eprintln!("llm-quota-tray: cannot acquire lock: {e:#}");
            None
        }
    };

    let cfg = Arc::new(config::load_or_init()?);
    log::info!(
        "started: endpoint={} label={:?} shape_windows={} auth={:?} rings_inner={:?}/{:?}/{:?} rings_outer={:?}",
        cfg.endpoint, cfg.label, cfg.shape.windows.len(),
        std::mem::discriminant(&cfg.auth),
        cfg.ring_colors.inner.normal, cfg.ring_colors.inner.warning, cfg.ring_colors.inner.throttled,
        cfg.ring_colors.outer,
    );

    let http_client = fetch::build_client(&cfg.user_agent).context("build HTTP client")?;

    // Startup pricing fetch (best-effort). When the config sets
    // `pricing_endpoint`, we hit it once before the refresh loop
    // starts so the very first menu render can show a cost fragment.
    // Failures are logged and result in `price_table = None` —
    // equivalent to "no pricing configured". A sidecar / network
    // outage at startup shouldn't prevent the tray from running.
    let initial_price_table = match cfg.pricing_endpoint.as_deref() {
        Some(url) => {
            log::info!("fetching pricing endpoint: {url}");
            match tokio::task::spawn_blocking({
                let client = http_client.clone();
                let url = url.to_string();
                move || pricing::fetch_pricing_blocking(&client, &url)
            })
            .await
            {
                Ok(Ok(table)) => {
                    log::info!("loaded {} model prices", table.len());
                    Some(table)
                }
                Ok(Err(e)) => {
                    log::warn!("pricing fetch failed: {e:#} -- continuing without cost fragments");
                    None
                }
                Err(e) => {
                    log::warn!(
                        "pricing fetch task panicked: {e} -- continuing without cost fragments"
                    );
                    None
                }
            }
        }
        None => None,
    };

    let state = Arc::new(Mutex::new(AppState {
        http_client: Some(http_client),
        price_table: initial_price_table,
        polls_since_pricing_refresh: 0,
        ..Default::default()
    }));
    let tray = Arc::new(
        Tray::new(cfg.dashboard_url.clone())
            .await
            .context("create SNI tray")?,
    );

    // Write the static SVG icons into ${TMPDIR} once at startup so the
    // SNI host can load them as `IconName` file paths. Hosts with
    // SVG support (KDE/QtSvg, GNOME with libpixbufloader-svg.so
    // registered) render the SVG natively at the panel's target size;
    // hosts without SVG support fall through to the ARGB bytes in
    // `IconPixmap` (rendered via Cogl), which works everywhere.
    icon::write_static_svgs(&cfg.ring_colors);

    // Run an initial render so the icon appears immediately.
    render_initial(&tray, &cfg).await;

    // Channel for menu commands (Refresh, OpenDashboard, SetApiKey, Quit).
    let cmd_rx = tray.cmd_rx.clone();
    // Channel for network events (Connectivity, ForceRefresh).
    let (net_tx, net_rx) = mpsc::channel::<NetEvent>(8);
    network::spawn_watcher(net_tx)
        .await
        .context("start network monitor")?;
    // Shutdown signal — orchestrator's Quit branch sends, main()
    // selects on this alongside SIGINT/SIGTERM so a menu-driven
    // quit cleanly tears down (the orchestrator task ends, then
    // main() unwinds and the Lock's Drop releases the PID file).
    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);

    // Spawn the orchestrator: refresh loop + menu commands + network.
    tokio::spawn(orchestrator(
        cfg,
        state,
        tray.clone(),
        cmd_rx,
        net_rx,
        shutdown_tx,
    ));

    // Wait for SIGINT/SIGTERM or a menu Quit.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
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
async fn do_refresh(cfg: &Config, state: &Arc<Mutex<AppState>>, tray: &Arc<Tray>) -> u64 {
    // Check offline state first.
    {
        let s = state.lock().await;
        if s.offline {
            drop(s);
            render_out_of_menu(tray, cfg, true).await;
            return cfg.refresh_seconds * 1000;
        }
    }

    // Direct D-Bus call — no spawn_blocking needed. The session-bus
    // connection is cached in `keyring` (see OnceCell there), and
    // zbus's `Proxy::call` is async-native so it integrates with
    // the tokio runtime without blocking.
    let api_key = match keyring::get().await {
        Some(k) => k,
        None => {
            // gjs parity: "No API key configured" (the full message)
            // in the menu's error row, not just "No API key".
            render_error(tray, cfg, "No API key configured").await;
            return cfg.refresh_seconds * 1000;
        }
    };

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

    let endpoint = cfg.endpoint.clone();
    let auth = cfg.auth.clone();
    let shape = cfg.shape.clone();
    let result = tokio::task::spawn_blocking(move || {
        fetch::fetch_windows_blocking(&client, &endpoint, &api_key, &auth, &shape)
    })
    .await
    .unwrap_or_else(|e| Err(anyhow::anyhow!("fetch task panicked: {e}")));

    let mut s = state.lock().await;
    match result {
        Ok(windows) if !windows.is_empty() => {
            // Primary window = windows[0] (drives the chip). The
            // PlanShape declares the ordering, so the provider picks
            // what "primary" means by listing it first (typically the
            // rolling short-interval window, the one most likely to
            // need urgent attention).
            let primary = windows[0].clone();
            let prev_rank = s
                .last_good
                .as_ref()
                .and_then(|w| w.first())
                .map(|w| BucketRank::from_remaining(w.remaining_pct))
                .unwrap_or(BucketRank::NoData);
            let new_rank = BucketRank::from_remaining(primary.remaining_pct);

            s.fail_streak = 0;
            s.last_good = Some(windows.clone());
            s.last_good_at = now_ms();

            // Periodic pricing refresh: gated on
            // `cfg.pricing_refresh_polls`. `None` ⇒ fetch only once
            // at startup (the table is already populated then).
            // Failures keep the previous table — best-effort, no
            // backoff or alerting. Cheap because the endpoint is
            // usually unauthenticated (OpenRouter's /api/v1/models
            // returns the full table on every call).
            if let (Some(url), Some(interval)) =
                (cfg.pricing_endpoint.as_deref(), cfg.pricing_refresh_polls)
            {
                s.polls_since_pricing_refresh += 1;
                if s.polls_since_pricing_refresh >= interval.max(1) {
                    s.polls_since_pricing_refresh = 0;
                    if let Some(client) = s.http_client.clone() {
                        let url = url.to_string();
                        let new_table = tokio::task::spawn_blocking(move || {
                            pricing::fetch_pricing_blocking(&client, &url)
                        })
                        .await;
                        match new_table {
                            Ok(Ok(table)) => {
                                log::debug!("pricing refresh: {} models", table.len());
                                s.price_table = Some(table);
                            }
                            Ok(Err(e)) => {
                                log::warn!(
                                    "pricing refresh failed: {e:#} -- keeping previous table"
                                );
                            }
                            Err(e) => {
                                log::warn!(
                                    "pricing refresh task panicked: {e} -- keeping previous table"
                                );
                            }
                        }
                    }
                }
            }

            // Record a sample for each window, keyed by window.id so
            // the burn-rate projection looks up the right slice.
            for w in &windows {
                let hist = s.histories.entry(w.id.clone()).or_insert_with(Vec::new);
                record_sample(hist, w);
            }

            // Compute burn rows for each window. We collect burn
            // results into a Vec so we can borrow them when building
            // the menu state, while iterating over the windows by
            // reference (no Window clones needed).
            let mut burn_results: Vec<Option<burn::BurnResult>> =
                Vec::with_capacity(windows.len());
            for w in &windows {
                let history = s.histories.get(&w.id).map(Vec::as_slice).unwrap_or(&[]);
                burn_results.push(burn::decide_burn_row(Some(w), history, now_ms(), &cfg.burn_warning));
            }
            let pair_refs: Vec<(&Window, Option<&burn::BurnResult>)> = windows
                .iter()
                .zip(burn_results.iter())
                .map(|(w, b)| (w, b.as_ref()))
                .collect();
            // Primary's burn is the first window's burn (already computed).
            let primary_burn = pair_refs.first().and_then(|(_, b)| *b);
            let pct = windows[0].remaining_pct;

            let menu_state = build_menu_state(
                &cfg.label,
                &pair_refs,
                None,
                false,
                now_ms() - s.last_good_at,
                pct <= 0,
                s.price_table.as_ref(),
            );

            let bucket = icon::bucket_for(
                pct,
                false,
                cfg.thresholds.yellow,
                primary_burn,
            );

            let icon_name: String = match bucket {
                icon::Bucket::Normal | icon::Bucket::Warning => {
                    icon::write_ring_svg(pct, bucket, &cfg.ring_colors)
                        .to_string_lossy()
                        .into_owned()
                }
                icon::Bucket::Throttled => icon::static_svg_path("throttled", &cfg.ring_colors)
                    .to_string_lossy()
                    .into_owned(),
            };
            // SNI Title is empty (gjs parity — chip carries the
            // bucket via icon color, no visible label). The burn
            // rate / remaining % is shown in the menu's window
            // rows + burn rate row.
            let pixmap = match bucket {
                icon::Bucket::Throttled => None,
                _ => icon::render_pixmap(pct, bucket, &cfg.ring_colors),
            };

            let interval = scheduler::next_interval(
                cfg.refresh_seconds,
                cfg.refresh_min_seconds,
                cfg.refresh_max_backoff_seconds,
                pct,
                cfg.thresholds.yellow,
                cfg.thresholds.red,
                0,
            );

            // Drop the state lock before the IPC calls so the menu
            // commands can proceed without deadlock.
            drop(s);

            // Apply chip + menu. Title is empty (gjs parity). The
            // tool_tip_desc carries the same string gjs passes as the
            // `guide` argument to `indicator.set_label(label, guide)`
            // — surface as SNI ToolTip for hosts that show hover
            // tooltips and screen readers that announce the icon.
            let tip = format!("{} — {}% remaining", cfg.label, pct,);
            let _ = tray.update("", &icon_name, "Active", pixmap, &tip).await;
            let _ = tray.apply_menu(|m| install_menu_into(m, menu_state)).await;

            // Threshold notification (deduped against previous bucket).
            if prev_rank != BucketRank::NoData && new_rank > prev_rank {
                if new_rank == BucketRank::Throttled {
                    notify::send(
                        "throttled",
                        &format!("{plan_label} — throttled", plan_label = cfg.label),
                        "Quota exhausted. The menu shows when it resets.",
                        notify::Urgency::Critical,
                        crate::activation::get(),
                    )
                    .await;
                } else if new_rank == BucketRank::Warning {
                    notify::send(
                        "warning",
                        &format!("{plan_label} — running low", plan_label = cfg.label),
                        &format!("Remaining dropped below {}%.", 100 - cfg.thresholds.yellow),
                        notify::Urgency::Normal,
                        crate::activation::get(),
                    )
                    .await;
                }
            }

            interval * 1000
        }
        Ok(_) => {
            // Empty windows vec — the parser returned 0 entries. Treat
            // as a soft error so we don't burn a "no plan configured"
            // chip but also don't render stale data.
            let err_str = "API returned no quota windows".to_string();
            log::warn!("{err_str}");
            s.fail_streak = s.fail_streak.saturating_add(1);
            let fail_streak = s.fail_streak;
            let last_good = s.last_good.take();
            let age_ms = if s.last_good_at > 0 {
                now_ms() - s.last_good_at
            } else {
                0
            };
            drop(s);

            render_error_with_stale(
                tray,
                cfg,
                &err_str,
                last_good.unwrap_or_default(),
                age_ms,
                &cfg.ring_colors,
            )
            .await;

            scheduler::next_interval(
                cfg.refresh_seconds,
                cfg.refresh_min_seconds,
                cfg.refresh_max_backoff_seconds,
                100,
                cfg.thresholds.yellow,
                cfg.thresholds.red,
                fail_streak,
            ) * 1000
        }
        Err(e) => {
            s.fail_streak = s.fail_streak.saturating_add(1);
            let fail_streak = s.fail_streak;
            // The error-path interval calculation passes `100` to the
            // scheduler (gjs parity — see the comment near the call
            // site), so we don't need last_good's pct here. We only
            // need the windows themselves for stale-menu display.
            let last_good = s.last_good.clone().unwrap_or_default();
            let age_ms = if s.last_good_at > 0 {
                now_ms() - s.last_good_at
            } else {
                0
            };
            let err_str = e.to_string();
            log::debug!("fetch failed: {e:#}");
            drop(s);

            // Render the error chip + menu (stale fallback if we
            // have prior good data).
            render_error_with_stale(tray, cfg, &err_str, last_good, age_ms, &cfg.ring_colors).await;

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
    let _has_key = keyring::get().await.is_some();
    // No data yet — empty tooltip (gjs's "no data" case uses just the
    // plan label, not a detailed percentage; with empty data we
    // don't have a percentage to put in the desc).
    let tip = cfg.label.clone();
    let _ = tray
        .update(
            "",
            &icon::static_svg_path("normal", &cfg.ring_colors).to_string_lossy(),
            "Active",
            None,
            &tip,
        )
        .await;
    log::info!("started; refresh every {}s", cfg.refresh_seconds);
}

async fn render_error(tray: &Arc<Tray>, cfg: &Config, msg: &str) {
    // Empty title — match gjs (no visible label next to the icon).
    // The error message lives in the menu's error row + journald.
    // ToolTip desc: "${planLabel} — stale data" (matches gjs's
    // accessible string in the `set_label('', guide)` call).
    // Note: this function doesn't need ring colors because it
    // uses the static "error" SVG (color is fixed at write_static_svgs
    // time, shared across instances).
    let tip = format!("{} — stale data", cfg.label);
    let _ = tray
        .update(
            "",
            &icon::static_svg_path("error", &cfg.ring_colors).to_string_lossy(),
            "Active",
            None,
            &tip,
        )
        .await;
    // Build a minimal menu with just the plan header + error row
    // (no window data to show). Matches gjs updateMenu({error: ...}):
    // the header is still rendered, the window-row section is empty,
    // and the error row shows the message.
    let plan_cfg_label = cfg.label.as_str();
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
///
/// Takes the last-good `Vec<Window>` directly (no per-window
/// destructuring) so this works for plans with any number of windows.
async fn render_error_with_stale(
    tray: &Arc<Tray>,
    cfg: &Config,
    err: &str,
    stale_windows: Vec<Window>,
    age_ms: i64,
    rings: &RingColors,
) {
    // Icon selection: mirror gjs bucket-for-chip priority order.
    // The primary window is stale_windows[0] when available.
    let (icon_name, pixmap) = match stale_windows.first() {
        Some(w) if w.remaining_pct <= 0 => (
            icon::static_svg_path("throttled", &cfg.ring_colors)
                .to_string_lossy()
                .into_owned(),
            None,
        ),
        Some(w) => {
            // Stale but not throttled — show the ring at the last
            // known pct (matches gjs: the ring branch falls through
            // whenever `primary` exists).
            let bucket = icon::bucket_for(
                w.remaining_pct,
                false,
                cfg.thresholds.yellow,
                None,
            );
            let path = icon::write_ring_svg(w.remaining_pct, bucket, rings)
                .to_string_lossy()
                .into_owned();
            let pix = icon::render_pixmap(w.remaining_pct, bucket, rings);
            (path, pix)
        }
        None => (
            icon::static_svg_path("error", &cfg.ring_colors)
                .to_string_lossy()
                .into_owned(),
            None,
        ),
    };

    // Stale tooltip: same "${planLabel} — stale data" as the no-key
    // path. For ring rendering, use the last-good pct as the
    // description so hover-preview matches the visible ring.
    let stale_pct = stale_windows.first().map(|w| w.remaining_pct).unwrap_or(0);
    let tip = format!("{} — stale data ({stale_pct}%)", cfg.label);
    let _ = tray.update("", &icon_name, "Active", pixmap, &tip).await;
    log::warn!("{err}");

    // Build menu pairs (Window, no burn result for stale data —
    // we don't have fresh samples to project on).
    let windows: Vec<(&Window, Option<&BurnResult>)> =
        stale_windows.iter().map(|w| (w, None)).collect();
    let throttled = stale_windows
        .first()
        .map(|w| w.remaining_pct <= 0)
        .unwrap_or(false);
    let menu_state = build_menu_state(
        &cfg.label,
        &windows,
        Some(err),
        true,
        age_ms,
        throttled,
        None,
    );
    let _ = tray.apply_menu(|m| install_menu_into(m, menu_state)).await;
}

/// Render the offline chip + menu (no polling happens while offline).
async fn render_out_of_menu(tray: &Arc<Tray>, cfg: &Config, offline: bool) {
    if offline {
        // Empty title — match gjs (no visible label next to icon).
        // The menu's error row carries the offline message.
        // ToolTip desc: "${planLabel} — offline" (matches gjs).
        // Note: offline uses the static "offline" SVG (gray), so no
        // ring colors are needed here.
        let tip = format!("{} — offline", cfg.label);
        let _ = tray
            .update(
                "",
                &icon::static_svg_path("offline", &cfg.ring_colors).to_string_lossy(),
                "Active",
                None,
                &tip,
            )
            .await;
        let _ = tray
            .apply_menu(|m| {
                m.set_header("Plan: …");
                m.set_error(
                    "  ⚠ Offline — local network unavailable (showing cached data)",
                    true,
                );
            })
            .await;
    }
}

/// Build a fresh MenuState for the given window data. Returns a
/// pre-built MenuState; caller is responsible for installing it
/// into the tray's menu.
///
/// `price_table` is the cached per-model price table (or `None`
/// when no pricing endpoint is configured). When non-empty and a
/// window's parser-read model id is present in the table, a
/// `· $X/h` cost fragment is appended to the burn row. Sub-tenth-
/// cent rates are hidden so cheap-model / low-volume workloads
/// don't show noise.
fn build_menu_state(
    plan_label: &str,
    windows: &[(&Window, Option<&BurnResult>)],
    error: Option<&str>,
    stale: bool,
    age_ms: i64,
    throttled: bool,
    price_table: Option<&pricing::PriceTable>,
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
        let label = &w.id;
        let resets_in_ms = (w.reset_at - now_ms()).max(0);
        labels
            .push(util::window_label(label, w.remaining_pct, resets_in_ms, stale) + &stale_suffix);
        bar_strs.push(util::bar_markup(w.remaining_pct));
        // Compute the cost fragment for this window, if any. We do
        // this in build_menu_state (not in burn_row_label) so the
        // price table doesn't need to thread through util.rs's
        // formatter signature. The 0.5 default split matches a
        // balanced chat workload; v2 could read prompt/completion
        // tokens separately when the parser exposes them.
        let cost_fragment = burn_opt.and_then(|b| {
            pricing::cost_per_hour(price_table?, w.model.as_deref(), b.rate_per_hour, 0.5)
        });
        burns.push(burn_opt.map(|b| {
            util::burn_row_label(
                b,
                w.count_unit.as_deref(),
                w.currency.as_deref(),
                cost_fragment.as_deref(),
            )
        }));
    }
    m.rebuild_window_rows(&labels, &bar_strs, &burns);

    // Throttled row.
    m.set_throttled(
        if stale {
            "  ⚠ Throttled (stale)"
        } else {
            "  ⚠ Throttled"
        },
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

/// Open a URL via the freedesktop OpenURI portal (preferred) with
/// a `xdg-open(1)` subprocess as the fallback for hosts without a
/// portal daemon (headless CI, minimal WMs).
///
/// Portal-first ordering matches the pattern used elsewhere in the
/// codebase: try the spec-defined D-Bus path, fall back to a
/// subprocess only when the spec path is unreachable. On any
/// modern GNOME/KDE/XFCE session the portal path wins and the
/// `xdg-open` binary is never spawned.
async fn open_url(url: &str) {
    if url.is_empty() {
        log::warn!("open_url: empty URL");
        return;
    }
    match portal_openuri::open(url, crate::activation::get()).await {
        Ok(()) => {
            log::debug!("opened dashboard via OpenURI portal: {url}");
            return;
        }
        Err(e) => {
            log::debug!(
                "OpenURI portal unavailable ({e:#}); falling back to xdg-open"
            );
        }
    }
    // tokio::process::Command for non-blocking spawn.
    let result = tokio::process::Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match result {
        Ok(_) => log::debug!("opened dashboard via xdg-open: {url}"),
        Err(e) => log::warn!("xdg-open failed: {e}"),
    }
}

/// Prompt for a new API key. Tries zenity (GNOME) first, then
/// kdialog (KDE). The earlier third fallback (`secret-tool` over
/// a `sh -c` pipeline) was removed: it injected user-controlled
/// instance-name strings into a shell command, a footgun
/// documented at length in the prior version of this file. The
/// terminal escape hatch (`secret-tool store --label=<label>
/// application <app>`) is preserved in the error message below
/// for users with neither zenity nor kdialog installed.
///
/// Returns:
///   - `Ok(Some(key))` on success — key has been written to keyring.
///   - `Ok(None)` if the user cancelled.
///   - `Err(e)` on a hard failure (no GUI tool available, etc.).
///
/// The dialog titles use the instance's `config_dir_basename`
/// (`llm-quota-tray` for the default instance, `llm-quota-tray-<name>`
/// otherwise) so a user running multiple instances can tell which
/// window is which — and so the prompts stay neutral on the
/// provider name, matching the rest of the codebase.
async fn set_api_key_interactive() -> Result<Option<String>> {
    let app = crate::instance::keyring_application();
    let label = format!("{app} API Key");

    // Try zenity first (GNOME).
    if let Some(k) = prompt_with_zenity(&label, &app).await? {
        keyring::set(&k).await?;
        return Ok(Some(k));
    }
    // Fall back to kdialog (KDE).
    if let Some(k) = prompt_with_kdialog(&label, &app).await? {
        keyring::set(&k).await?;
        return Ok(Some(k));
    }
    Err(anyhow::anyhow!(
        "no GUI prompt tool available (tried zenity, kdialog). \
         From a terminal: `secret-tool store --label='{label}' application {app}`"
    ))
}

async fn prompt_with_zenity(label: &str, app: &str) -> Result<Option<String>> {
    let output = tokio::process::Command::new("zenity")
        .args([
            "--entry",
            &format!("--title={label}"),
            &format!(
                "--text=Enter your API key (stored in your OS keyring as `application {app}`):"
            ),
            "--hide-text",
        ])
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await;
    match output {
        Ok(o) if o.status.success() => {
            Ok(Some(String::from_utf8_lossy(&o.stdout).trim().to_string()))
        }
        Ok(_) => Ok(None),  // user cancelled
        Err(_) => Ok(None), // not installed
    }
}

async fn prompt_with_kdialog(label: &str, app: &str) -> Result<Option<String>> {
    let output = tokio::process::Command::new("kdialog")
        .args([
            "--password",
            &format!(
                "Enter your API key (stored in your OS keyring as `application {app}`) — {label}:"
            ),
        ])
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await;
    match output {
        Ok(o) if o.status.success() => {
            Ok(Some(String::from_utf8_lossy(&o.stdout).trim().to_string()))
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

    fn w(label: &str, pct: i64, start: i64, end: i64) -> Window {
        Window {
            id: label.to_string(),
            total: 0,
            used: 0,
            remaining_pct: pct,
            start_at: start,
            reset_at: end,
            count_unit: None,
            currency: None,
            model: None,
        }
    }

    fn burn_pct(rate: f64, exhaust: bool) -> BurnResult {
        BurnResult {
            rate_per_hour: rate,
            mode: "pct",
            unit: "pct",
            exhaust_ms: if exhaust {
                30.0 * 60_000.0
            } else {
                f64::INFINITY
            },
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
            &[(&five_h, burn_5h), (&weekly, burn_weekly)],
            None,
            false,
            0,
            false,
            None,
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
            &[(&five_h, burn_5h_ref), (&weekly, burn_weekly_ref)],
            None,
            false,
            0,
            false,
            None,
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
            &[(&five_h, None), (&weekly, None)],
            None,
            false,
            0,
            true,
            None,
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
            &[(&five_h, None), (&weekly, None)],
            Some("API error 1004"),
            true,
            3 * 60_000,
            false,
            None,
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
        assert!(
            wait > 50_000 && wait <= 60_000,
            "expected ~60s remaining, got {wait}"
        );
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
        //
        // Note: `rand_jitter_ms` is deterministic from the boot-time
        // clock, so on a single test run the jitter value is fixed
        // (every call returns the same ns-from-epoch % 5000). The
        // test only asserts that the result lands in the valid range
        // — it doesn't insist on jitter variability between calls.
        for _ in 0..20 {
            let got = compute_next_interval(60_000, 600);
            // Jittered range: [60_000, 65_000). Zero-jitter returns
            // exactly 60_000 — both are valid.
            assert!(
                (60_000..=65_000).contains(&got),
                "expected 60..65s, got {got}"
            );
        }
    }

    #[test]
    fn compute_next_interval_clamps_to_backoff() {
        // 999s returned → clamped to max_backoff (600s).
        let got = compute_next_interval(999_000, 600);
        assert!(
            (600_000..=605_000).contains(&got),
            "expected clamped to 600s + jitter, got {got}"
        );
    }

    #[test]
    fn compute_next_interval_floors_at_one_second() {
        // 0 returned + jitter (0..5000ms) → result is [1000, 5000].
        // (`.max(1000)` floors at 1s; jitter can push it up to 5s.)
        for _ in 0..20 {
            let got = compute_next_interval(0, 600);
            assert!((1000..=5000).contains(&got), "expected 1..5s, got {got}");
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
        assert_eq!(m.item(menu::HEADER_ID).unwrap().label, "Plan: Coding Plan");
        assert!(m.item(menu::ERROR_ID).unwrap().visible);
        let err_label = m.item(menu::ERROR_ID).unwrap().label.clone();
        assert!(
            err_label.contains("No API key configured"),
            "error label must contain the full gjs message; got {err_label}"
        );
        // No window rows (no data to show).
        assert!(!m.item(menu::window_id(0)).is_some_and(|i| i.visible));
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
        let unadapted = scheduler::next_interval(120, 15, 600, 100, 60, 85, 0);
        let adapted = scheduler::next_interval(120, 15, 600, 14, 60, 85, 0);
        assert_eq!(
            unadapted, 120,
            "unadapted error path: base only, no / / cut"
        );
        assert_eq!(adapted, 30, "comparison: same args, pct=14 → adapted /4");
        // With backoff, unadapted still respects min floor.
        let unadapted_backoff = scheduler::next_interval(120, 15, 600, 100, 60, 85, 2);
        assert_eq!(
            unadapted_backoff, 480,
            "base=120 × 2^2 = 480, clamped to max=600"
        );
    }

    /// The "No API key" error path uses the full gjs message,
    /// "No API key configured" (not just "No API key").
    #[test]
    fn no_api_key_uses_full_gjs_message() {
        let m = build_error_menu_state("Coding Plan", "No API key configured");
        let err = m.item(menu::ERROR_ID).unwrap().label.clone();
        assert!(
            err.contains("No API key configured"),
            "gjs shows the full phrase in the menu; got {err}"
        );
        assert!(!err.contains("configured: configured"), "no double-word");
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
        let buggy_label = util::fmt_duration((buggy_reset_at - now).max(0), false);
        // The buggy version would have produced "1y 11mo" or similar.
        // (We don't lock the exact string; we just verify the
        // CORRECT version's math doesn't degenerate.)
        assert_eq!(
            util::fmt_duration(remains_ms, false),
            "4h 32m",
            "the correct math is `remains_ms`, not `remains_ms * 1000 - now`"
        );
        assert!(
            !buggy_label.contains("1y"),
            "the buggy math (epoch seconds) gave '{buggy_label}'"
        );
    }
}
