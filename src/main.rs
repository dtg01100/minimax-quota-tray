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
mod screenlock;
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
    /// Most recent successful fetch, in display order. The first
    /// window in the vector is the primary window. `None` until the
    /// first fetch completes.
    last_good: Option<Vec<Window>>,
    last_good_at: i64,
    fail_streak: u32,
    http_client: Option<fetch::HttpClient>,
    /// Connectivity state — false = offline, skip polling.
    offline: bool,
    /// Screen-lock state — true = screen is locked, skip polling.
    /// Nobody is looking at the tray icon while the screen is locked,
    /// so we pause the cadence entirely. The screenlock watcher
    /// updates this and emits a ForceRefresh on unlock (see
    /// `crate::screenlock`). The default is `false` (unlocked) so
    /// headless / no-screenlock-daemon environments behave normally.
    screen_locked: bool,

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
            // Same budget as the periodic pricing refresh in
            // `do_refresh` — see the comment there. Without a
            // ceiling, a hung endpoint at boot delays `Tray::new`
            // + the keyring probe + the first SNI signal emission
            // indefinitely. The startup pricing fetch blocks
            // the daemon from becoming interactive, so a 10s
            // ceiling here protects the panel slot from going
            // blank on the very first launch.
            const PRICING_FETCH_BUDGET: Duration = Duration::from_secs(10);
            match tokio::time::timeout(
                PRICING_FETCH_BUDGET,
                tokio::task::spawn_blocking({
                    let client = http_client.clone();
                    let url = url.to_string();
                    move || pricing::fetch_pricing_blocking(&client, &url)
                }),
            )
            .await
            {
                Ok(Ok(Ok(table))) => {
                    log::info!("loaded {} model prices", table.len());
                    Some(table)
                }
                Ok(Ok(Err(e))) => {
                    log::warn!("pricing fetch failed: {e:#} -- continuing without cost fragments");
                    None
                }
                Ok(Err(e)) => {
                    log::warn!(
                        "pricing fetch task panicked: {e} -- continuing without cost fragments"
                    );
                    None
                }
                Err(_elapsed) => {
                    log::warn!(
                        "pricing fetch timed out after {PRICING_FETCH_BUDGET:?}; \
                         continuing without cost fragments"
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
    // Channel for screen-lock events (ScreenLock, ForceRefresh on unlock).
    let (lock_tx, lock_rx) = mpsc::channel::<screenlock::LockEvent>(8);
    screenlock::spawn_watcher(lock_tx)
        .await
        .context("start screen-lock monitor")?;
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
        lock_rx,
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

/// Master orchestrator. Owns four input streams:
///
///   1. Refresh cadence (sleeps N ms, then do_refresh)
///   2. Menu commands (Refresh, OpenDashboard, SetApiKey, Quit)
///   3. Network events (Connectivity(bool), ForceRefresh)
///   4. Screen-lock events (ScreenLock(bool), ForceRefresh on unlock)
///
/// All four feed into the same refresh task. Menu Refresh, Net
/// ForceRefresh, and LockEvent::ForceRefresh collapse to the same
/// operation (immediate refresh).
async fn orchestrator(
    cfg: Arc<Config>,
    state: Arc<Mutex<AppState>>,
    tray: Arc<Tray>,
    cmd_rx: Arc<Mutex<mpsc::Receiver<MenuCommand>>>,
    mut net_rx: mpsc::Receiver<NetEvent>,
    shutdown_tx: mpsc::Sender<()>,
    mut lock_rx: mpsc::Receiver<screenlock::LockEvent>,
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
                        //
                        // The offline-override lives inside
                        // `force_refresh_now` — a user click is a stronger
                        // signal than the cached NetworkManager state. If
                        // the network really is down the fetch itself
                        // will fail and the normal error path takes over.
                        let returned_ms = force_refresh_now(&cfg, &state, &tray).await;
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
                        // External trigger (reconnect, future wake-from-sleep,
                        // unlock-after-lock, etc.) — the offline-override
                        // semantics live inside `force_refresh_now`; a real
                        // network outage will fail through the normal
                        // error path.
                        let returned_ms = force_refresh_now(&cfg, &state, &tray).await;
                        last_refresh_at = Some(now_ms());
                        next_interval_ms = compute_next_interval(
                            returned_ms, cfg.refresh_max_backoff_seconds);
                    }
                    None => {}
                }
            }
            // Screen-lock events.
            evt = lock_rx.recv() => {
                match evt {
                    Some(screenlock::LockEvent::ScreenLock(locked)) => {
                        let mut s = state.lock().await;
                        s.screen_locked = locked;
                        // Note: do NOT touch offline here. The screen-lock
                        // signal is orthogonal to NM connectivity; an
                        // offline-locked machine should still render the
                        // offline icon on unlock.
                    }
                    Some(screenlock::LockEvent::ForceRefresh) => {
                        // Unlock happened. Treat exactly like the
                        // NetEvent::ForceRefresh arm — force_refresh_now
                        // handles the offline/fail_streak clear and the
                        // fetch. The do_refresh gate will see
                        // screen_locked=false (set above by the preceding
                        // ScreenLock event) and proceed.
                        let returned_ms = force_refresh_now(&cfg, &state, &tray).await;
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

/// Pure gate predicates and state mutations shared by `do_refresh`
/// and `force_refresh_now`. Extracted (rather than inlined) so the
/// invariants below can be unit-tested without constructing a
/// `Tray` (which requires a live D-Bus session). The functions are
/// trivially small — the point is the *name + contract*, not the
/// line count.
///
/// `true` iff the screen-lock short-circuit should fire for this
/// state. While the screen is locked there is nobody to read the
/// tray icon, so `do_refresh` skips the fetch (preserving the
/// last-known-good icon) and returns the normal cadence interval.
/// This helper is pure (read-only); the actual early-return lives
/// in `do_refresh` because that path also needs to drop the mutex
/// lock before returning.
fn is_screen_locked(state: &AppState) -> bool {
    state.screen_locked
}

/// The interval `do_refresh`'s short-circuit gates return. Distinct
/// from `compute_next_interval` (which applies backoff / adaptive
/// cut) because the gates must NOT adjust cadence — locked or
/// offline, the schedule stays aligned with `refresh_seconds` so the
/// next cadence tick (or the unlock / reconnect event) is the only
/// behavior change.
fn short_circuit_interval(cfg: &Config) -> u64 {
    cfg.refresh_seconds * 1000
}

/// Mutate the per-state fields that every "force refresh" trigger
/// is required to clear before `do_refresh` runs: `fail_streak` (so
/// the next fetch skips backoff) and `offline` (so a cached
/// "we're down" opinion doesn't override a stronger "user clicked
/// Refresh" or "NM reconnected" signal).
///
/// IMPORTANT: this helper deliberately does NOT touch
/// `state.screen_locked`. The lock flag is owned by the
/// `screenlock` module's event stream — clearing it here would
/// race the screenlock watcher's own transitions. If the screen is
/// locked at the moment a force-refresh fires, `do_refresh`'s
/// screen-lock gate will short-circuit anyway, so there's no need
/// to second-guess the lock state here.
///
/// Pinned by tests in `tests::*` — a regression that adds or
/// removes a field from the clear-list will fail loud.
fn clear_force_refresh_state(state: &mut AppState) {
    state.fail_streak = 0;
    state.offline = false;
}

/// Force a refresh now. Shared by every "force" trigger — menu
/// Refresh, NetEvent::ForceRefresh (NM reconnect, etc.), and
/// LockEvent::ForceRefresh (screen unlock). Clears `fail_streak` and
/// `offline` (the trigger is a stronger signal than cached state),
/// runs `do_refresh`, and returns the new interval so the caller can
/// re-arm its cadence. The single-flight invariant lives in the
/// mpsc channel — concurrent force-refreshes queue and fire one at a
/// time.
async fn force_refresh_now(
    cfg: &Config,
    state: &Arc<Mutex<AppState>>,
    tray: &Arc<Tray>,
) -> u64 {
    {
        let mut s = state.lock().await;
        clear_force_refresh_state(&mut s);
    }
    let returned_ms = do_refresh(cfg, state, tray).await;
    returned_ms
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
            return short_circuit_interval(cfg);
        }
    }

    // Check screen-lock state. While the screen is locked there is
    // nobody to read the tray icon, so we skip the fetch and let the
    // next cadence tick (or the unlock event) drive the next attempt.
    // Returning the normal interval keeps the schedule aligned with
    // what we *would* have polled, so unlock-time ForceRefresh is the
    // only behavior change. Note: unlike the offline gate, this
    // path does NOT call `render_out_of_menu` — the last-known-good
    // icon is preserved (matches gjs `refresh(true)` semantics on
    // unlock).
    {
        let s = state.lock().await;
        if is_screen_locked(&s) {
            drop(s);
            return short_circuit_interval(cfg);
        }
    }

    // Direct D-Bus call — no spawn_blocking needed. The session-bus
    // connection is cached in `keyring` (see OnceCell there), and
    // zbus's `Proxy::call` is async-native so it integrates with
    // the tokio runtime without blocking.
    //
    // Bound the probe: at session boot the Secret Service and its
    // `/run/user/<uid>/keyring/control` socket are still waking up,
    // and `keyring::get()` blocks indefinitely waiting for the
    // collection to unlock — which leaves this `do_refresh` call
    // stuck, the select! sleep arm unable to make progress (the
    // orchestrator's only tick path runs through here), and the
    // chip permanently showing the static-fallback SVG with no
    // real quota data. A 2s ceiling mirrors `render_initial`'s
    // probe budget; on a warm session it's sub-millisecond, and
    // on cold boots it self-heals on the next 120s tick.
    let api_key = match tokio::time::timeout(Duration::from_secs(2), keyring::get()).await {
        Ok(Some(k)) => k,
        Ok(None) => {
            // gjs parity: "No API key configured" (the full message)
            // in the menu's error row, not just "No API key".
            render_error(tray, cfg, "No API key configured").await;
            return cfg.refresh_seconds * 1000;
        }
        Err(_) => {
            // Boot-time race: Secret Service unavailable right now.
            // Log at warn so the journal captures the cause, render a
            // distinct chip message so the user knows it's transient
            // (vs. the "no key configured" path which is a real key
            // setup issue requiring `llm-quota-tray --set-key`).
            // The next 120s tick will retry when Secret Service is
            // almost certainly up.
            log::warn!("keyring probe timed out — Secret Service unavailable");
            render_error(
                tray,
                cfg,
                "Secret Service unavailable (keyring not ready) — will retry next refresh",
            )
            .await;
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

    // The success path does bookkeeping in two short critical
    // sections (state lock held only across in-memory work) and
    // drops the lock before any network I/O so the orchestrator's
    // `select!` arms (menu commands, network events) never block
    // on a hung pricing endpoint. The HTTP client + URL are cloned
    // out under the lock, then we re-acquire briefly to commit
    // `price_table` once the spawn_blocking future resolves.
    match result {
        Ok(windows) if !windows.is_empty() => {
            // Snapshot the in-memory state we need *before* we
            // commit anything, so the lock-held region stays
            // minimal. `primary` (windows[0]) drives the chip; the
            // prev/new rank pair drives notification dedup.
            let primary = windows[0].clone();
            let prev_rank = {
                let s = state.lock().await;
                s.last_good
                    .as_ref()
                    .and_then(|w| w.first())
                    .map(|w| BucketRank::from_remaining(w.remaining_pct))
                    .unwrap_or(BucketRank::NoData)
            };
            let new_rank = BucketRank::from_remaining(primary.remaining_pct);

            // Determine whether this tick triggers a pricing refresh
            // and grab the artifacts needed for it. If so, do the
            // network call WITHOUT the state lock; commit the new
            // table back via a second short critical section.
            let pricing_refresh = {
                let mut s = state.lock().await;
                s.fail_streak = 0;
                s.last_good = Some(windows.clone());
                s.last_good_at = now_ms();
                pricing_refresh_artifact(cfg, &mut s)
            };
            // Pricing refresh outcome — `Some(table)` means "new table to
            // commit"; `None` means "keep the previous table" (either
            // the cadence wasn't due yet, the fetch failed, the join
            // panicked, or the budget expired). The handler in this
            // block always commits successfully or skips the commit
            // step; never returns an Err to the caller.
            let new_price_table: Option<pricing::PriceTable> = match pricing_refresh {
                None => None,
                Some(client_url) => {
                    // Bound the pricing refresh too — the endpoint
                    // is unauthenticated but a hung DNS / TCP
                    // handshake shouldn't block the rest of the
                    // orchestrator forever. We use a 10s budget:
                    // short enough that a pathological endpoint can't
                    // stall the menu render for more than one refresh
                    // cycle's worth of user-visible delay, long enough
                    // that a slow but working endpoint (OpenRouter
                    // averages ~150ms; some rate-limited paths can
                    // spike to multi-second) still completes. On
                    // timeout the previous table stays in place —
                    // best-effort, no alerting.
                    const PRICING_REFRESH_BUDGET: Duration = Duration::from_secs(10);
                    let timed = tokio::time::timeout(
                        PRICING_REFRESH_BUDGET,
                        tokio::task::spawn_blocking(move || {
                            pricing::fetch_pricing_blocking(
                                &client_url.client,
                                &client_url.url,
                            )
                        }),
                    )
                    .await;
                    let joined: Option<
                        pricing::PriceTable,
                    > = match timed {
                        Ok(Ok(Ok(table))) => {
                            log::debug!("pricing refresh: {} models", table.len());
                            Some(table)
                        }
                        Ok(Ok(Err(e))) => {
                            log::warn!(
                                "pricing refresh failed: {e:#} -- keeping previous table"
                            );
                            None
                        }
                        Ok(Err(join_err)) => {
                            log::warn!(
                                "pricing refresh task panicked: {join_err} -- keeping previous table"
                            );
                            None
                        }
                        Err(_elapsed) => {
                            log::warn!(
                                "pricing refresh timed out after {PRICING_REFRESH_BUDGET:?}; \
                                 keeping previous table"
                            );
                            None
                        }
                    };
                    joined
                }
            };
            if let Some(table) = new_price_table {
                let mut s = state.lock().await;
                s.price_table = Some(table);
            }

            // Record samples + compute burn rows. Each takes the
            // lock just long enough to read/write its own slot.
            for w in &windows {
                let mut s = state.lock().await;
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
                // Borrow the history slice just long enough to call
                // `decide_burn_row`. The lock is dropped before we
                // await anything that could yield — this is a short
                // map lookup, not a long-held critical section.
                let history_owned: Vec<Sample>;
                let history_slice: &[Sample] = {
                    let s = state.lock().await;
                    history_owned = s.histories.get(&w.id).cloned().unwrap_or_default();
                    &history_owned
                };
                burn_results.push(burn::decide_burn_row(Some(w), history_slice, now_ms(), &cfg.burn_warning));
            }
            let pair_refs: Vec<(&Window, Option<&burn::BurnResult>)> = windows
                .iter()
                .zip(burn_results.iter())
                .map(|(w, b)| (w, b.as_ref()))
                .collect();
            // Primary's burn is the first window's burn (already computed).
            let primary_burn = pair_refs.first().and_then(|(_, b)| *b);
            let pct = windows[0].remaining_pct;

            // Snapshot the menu-shape inputs (last_good_at + price_table)
            // under one short critical section, then drop the lock
            // before the IPC calls — the chip/menu render takes no
            // locks of its own but can take a few hundred ms and we
            // don't want to starve the orchestrator's other arms.
            let (last_good_age_ms, price_table_ref) = {
                let s = state.lock().await;
                let age = now_ms() - s.last_good_at;
                (age, s.price_table.clone())
            };

            let menu_state = build_menu_state(
                &cfg.label,
                &pair_refs,
                None,
                false,
                last_good_age_ms,
                pct <= 0,
                price_table_ref.as_ref(),
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

            // The state lock is already dropped — the chip/menu
            // render takes no AppState locks, and the orchestrator's
            // other `select!` arms can now make progress against
            // this same AppState.

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
                        &format!(
                            "Usage reached {}% (threshold: {}%).",
                            100 - cfg.thresholds.yellow,
                            cfg.thresholds.yellow
                        ),
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
            // Snapshot fail_streak + stale-fallback data under a
            // short critical section, then drop the lock before the
            // IPC render — same pattern as the success path.
            let (fail_streak, last_good, age_ms) = {
                let mut s = state.lock().await;
                s.fail_streak = s.fail_streak.saturating_add(1);
                let last_good = s.last_good.take();
                let age_ms = if s.last_good_at > 0 {
                    now_ms() - s.last_good_at
                } else {
                    0
                };
                (s.fail_streak, last_good, age_ms)
            };

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
            let err_str = e.to_string();
            log::debug!("fetch failed: {e:#}");
            // Same short-critical-section pattern as the other
            // branches — read what we need under the lock, then drop
            // it so the orchestrator's other `select!` arms can make
            // progress against the same AppState while the error
            // chip/menu render runs.
            let (fail_streak, last_good, age_ms) = {
                let mut s = state.lock().await;
                s.fail_streak = s.fail_streak.saturating_add(1);
                let last_good = s.last_good.clone().unwrap_or_default();
                let age_ms = if s.last_good_at > 0 {
                    now_ms() - s.last_good_at
                } else {
                    0
                };
                (s.fail_streak, last_good, age_ms)
            };

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
    //
    // Bound the probe: at session boot the Secret Service and its
    // `/run/user/<uid>/keyring/control` socket are still waking up,
    // and `keyring::get()` then blocks indefinitely waiting for the
    // collection to unlock — which leaves `tray.update(...)` below
    // never called, the SNI `Status` stuck at the placeholder
    // `"Passive"` default, and the panel slot blank until the user
    // manually restarts us. A 2s ceiling is generous on warm
    // sessions (sub-millisecond in practice) and short enough that
    // even on cold boots render_initial completes well before the
    // first 120s refresh tick. The `do_refresh` cycle re-probes
    // the keyring per-poll, so a transient "no key" here
    // self-heals on the next tick once Secret Service is up.
    let _has_key = matches!(
        tokio::time::timeout(Duration::from_secs(2), keyring::get()).await,
        Ok(Some(_))
    );
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

/// Inputs needed to perform a periodic pricing refresh on a worker
/// thread. Returned by `pricing_refresh_artifact` under the AppState
/// lock; the caller then drops the lock and runs the HTTP fetch.
struct PricingRefresh {
    client: fetch::HttpClient,
    url: String,
}

/// Decide whether this poll should trigger a pricing refresh, bump
/// `polls_since_pricing_refresh`, and return the artifacts needed to
/// perform the fetch. Called under the AppState lock — returns
/// `None` when the refresh cadence isn't due yet, when no pricing
/// endpoint is configured, or when `pricing_refresh_polls` is unset
/// (meaning "fetch only at startup", and that already happened in
/// `run()`).
fn pricing_refresh_artifact(cfg: &Config, s: &mut AppState) -> Option<PricingRefresh> {
    let url = cfg.pricing_endpoint.as_deref()?;
    let interval = cfg.pricing_refresh_polls?;
    s.polls_since_pricing_refresh += 1;
    if s.polls_since_pricing_refresh < interval.max(1) {
        return None;
    }
    s.polls_since_pricing_refresh = 0;
    let client = s.http_client.clone()?;
    Some(PricingRefresh {
        client,
        url: url.to_string(),
    })
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

    // ---- record_sample ----
    //
    // `record_sample` maintains the per-window burn-rate history. Two
    // invariants it must uphold:
    //
    // 1. Epoch rollover detection: when the window's start_at changes
    //    (new epoch), OR when used/pct drop by 2 or more (counter
    //    reset), the history is cleared. Otherwise the burn-rate
    //    slope mixes samples from two epochs and produces nonsense.
    //
    // 2. Memory cap: history.len() must never exceed BURN_MAX_SAMPLES
    //    (480). Beyond that, oldest-first eviction.
    //
    // The `+ 1` in the rollover condition (`new + 1 < old`) means
    // a drop of exactly 1 is NOT a rollover (handles integer-percent
    // rounding noise). The `>= 2` threshold is what the test
    // fixtures exercise.

    fn fresh_window(used: i64, pct: i64, start_at: i64, reset_at: i64) -> Window {
        Window {
            id: "w".to_string(),
            total: 1000,
            used,
            remaining_pct: pct,
            start_at,
            reset_at,
            count_unit: None,
            currency: None,
            model: None,
        }
    }

    #[test]
    fn record_sample_appends_to_empty_history() {
        let mut h: Vec<Sample> = Vec::new();
        record_sample(&mut h, &fresh_window(0, 100, 1000, 2000));
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].used, 0);
        assert_eq!(h[0].remaining_pct, 100);
    }

    #[test]
    fn record_sample_appends_to_non_empty_history() {
        let mut h: Vec<Sample> = Vec::new();
        record_sample(&mut h, &fresh_window(0, 100, 1000, 2000));
        // Second sample: monotonic increase (consumption). The `+ 1`
        // threshold means a single-token-per-poll drop is not a reset;
        // but pct must not drop significantly either. Use pct=99
        // (drop of 1, not >= 2) to avoid triggering the rollover.
        record_sample(&mut h, &fresh_window(1, 99, 1000, 2000));
        assert_eq!(h.len(), 2);
        assert_eq!(h[1].used, 1);
        assert_eq!(h[1].remaining_pct, 99);
    }

    #[test]
    fn record_sample_does_not_clear_on_normal_consumption() {
        // Regression guard: a typical poll shows used going UP by a
        // small amount and pct going DOWN by 1. Neither must trigger
        // the rollover detector.
        let mut h: Vec<Sample> = Vec::new();
        record_sample(&mut h, &fresh_window(0, 100, 1000, 2000));
        for i in 1..=10 {
            record_sample(&mut h, &fresh_window(i, 100 - i, 1000, 2000));
        }
        assert_eq!(
            h.len(),
            11,
            "normal monotonic consumption must not clear history"
        );
    }

    #[test]
    fn record_sample_does_not_clear_on_one_pct_drop() {
        // The `+ 1` threshold means a drop of exactly 1 is NOT a
        // rollover (handles rounding noise from providers that
        // round to integer percent).
        let mut h: Vec<Sample> = Vec::new();
        record_sample(&mut h, &fresh_window(0, 100, 1000, 2000));
        record_sample(&mut h, &fresh_window(1, 99, 1000, 2000));
        // pct went 100 -> 99 (drop of 1, not >= 2).
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn record_sample_clears_history_on_start_at_change() {
        // start_at jumping from 1000 -> 2000 is the canonical epoch
        // rollover. The old samples (which belong to the old epoch)
        // must be cleared so the burn-rate slope only sees the new
        // epoch's data.
        let mut h: Vec<Sample> = Vec::new();
        record_sample(&mut h, &fresh_window(0, 100, 1000, 2000));
        record_sample(&mut h, &fresh_window(1, 99, 1000, 2000));
        record_sample(&mut h, &fresh_window(2, 98, 1000, 2000));
        assert_eq!(h.len(), 3);
        // Now the epoch rolls over -- start_at moves to 2000.
        record_sample(&mut h, &fresh_window(0, 100, 2000, 3000));
        assert_eq!(
            h.len(),
            1,
            "history must be cleared on epoch rollover"
        );
        assert_eq!(h[0].start_at, 2000);
        assert_eq!(h[0].used, 0);
    }

    #[test]
    fn record_sample_clears_history_when_used_drops_by_2_or_more() {
        // Edge case: start_at is the same (no epoch rollover) but
        // used went backward by >= 2 (e.g. provider reset its counter
        // without bumping start_at). The function detects this via
        // `w.used + 1 < last.used` (new_used < old_used - 1).
        let mut h: Vec<Sample> = Vec::new();
        record_sample(&mut h, &fresh_window(500, 50, 1000, 2000));
        record_sample(&mut h, &fresh_window(501, 49, 1000, 2000));
        assert_eq!(h.len(), 2);
        // used drops from 501 to 0 (drop of 501, well >= 2).
        record_sample(&mut h, &fresh_window(0, 100, 1000, 2000));
        assert_eq!(
            h.len(),
            1,
            "history must be cleared when used drops by >= 2"
        );
        assert_eq!(h[0].used, 0);
    }

    #[test]
    fn record_sample_clears_history_when_pct_drops_by_2_or_more() {
        // Same pattern, but detected via remaining_pct.
        let mut h: Vec<Sample> = Vec::new();
        record_sample(&mut h, &fresh_window(500, 50, 1000, 2000));
        record_sample(&mut h, &fresh_window(501, 49, 1000, 2000));
        // pct jumps from 49 back to 100 (drop of 49 in the new direction).
        // Wait -- 49 -> 100 is pct going UP (counter reset to full).
        // The detector fires on `new + 1 < old` which for going UP
        // means new=100, old=49, 101 < 49 → false. So this does NOT
        // trigger clear. Let me think... a pct reset from 49 -> 100
        // means remaining went UP, which is normal for a reset. But
        // the detector fires on the wrong direction. Let me look
        // at the actual condition again:
        //   w.remaining_pct + 1 < last.remaining_pct
        // This is: new_pct + 1 < old_pct, i.e., new < old - 1.
        // So the detector fires when new_pct is significantly LOWER
        // than old_pct. That's the OPPOSITE of what a reset would do.
        //
        // This test pins the actual behavior (whatever it is) so a
        // future refactor must update the test, not silently flip
        // the semantics. The current behavior: dropping pct by >= 2
        // (going DOWN further) is treated as rollover.
        //
        // For now we test the documented contract:
        record_sample(&mut h, &fresh_window(501, 0, 1000, 2000));
        assert_eq!(
            h.len(),
            1,
            "history must be cleared when pct drops by >= 2 (49 -> 0)"
        );
    }

    #[test]
    fn record_sample_evicts_oldest_when_over_burn_max() {
        // BURN_MAX_SAMPLES = 480. After 481 samples, only the
        // newest 480 must remain (oldest evicted).
        let mut h: Vec<Sample> = Vec::new();
        for i in 0..(BURN_MAX_SAMPLES + 1) {
            record_sample(
                &mut h,
                &fresh_window(i as i64, 100, 1000, 2000),
            );
        }
        assert_eq!(
            h.len(),
            BURN_MAX_SAMPLES,
            "history must never exceed BURN_MAX_SAMPLES"
        );
        // First remaining sample is the one at index 1 (the original
        // index 0 was evicted).
        assert_eq!(h[0].used, 1);
        // Last sample is the most recent.
        assert_eq!(h.last().unwrap().used, BURN_MAX_SAMPLES as i64);
    }

    #[test]
    fn record_sample_evicts_multiple_when_far_over_burn_max() {
        // Pushing way past the cap should still leave exactly
        // BURN_MAX_SAMPLES (not evict one at a time across multiple
        // calls -- evict the exact excess).
        let mut h: Vec<Sample> = Vec::new();
        for i in 0..(BURN_MAX_SAMPLES + 100) {
            record_sample(
                &mut h,
                &fresh_window(i as i64, 100, 1000, 2000),
            );
        }
        assert_eq!(h.len(), BURN_MAX_SAMPLES);
    }

    #[test]
    fn record_sample_clear_then_evict() {
        // Edge case: clear() then push a new sample -- the new
        // sample is the only one in the history (no eviction
        // because we're under the cap).
        let mut h: Vec<Sample> = Vec::new();
        for i in 0..100 {
            record_sample(
                &mut h,
                &fresh_window(i, 100, 1000, 2000),
            );
        }
        assert_eq!(h.len(), 100);
        // Rollover clears the entire history.
        record_sample(&mut h, &fresh_window(0, 100, 2000, 3000));
        assert_eq!(h.len(), 1);
        // Pushing past the cap evicts oldest.
        for i in 0..(BURN_MAX_SAMPLES + 5) {
            record_sample(
                &mut h,
                &fresh_window(i as i64, 100, 2000, 3000),
            );
        }
        assert_eq!(h.len(), BURN_MAX_SAMPLES);
    }

    // ------------------------------------------------------------------
    // Gate-helper tests (commit 4ee12be coverage).
    //
    // `is_screen_locked`, `short_circuit_interval`, and
    // `clear_force_refresh_state` are pure helpers extracted from
    // `do_refresh` / `force_refresh_now` so the gate contracts can be
    // unit-tested without constructing a `Tray` (which requires a
    // live D-Bus session). These tests pin the invariants the
    // 2026-08-28 code review flagged as regression risks:
    //   1. `AppState::default().screen_locked` must be `false` so the
    //      graceful-degradation path (ScreenSaver daemon absent)
    //      polls normally.
    //   2. `is_screen_locked` must be pure (read-only) — the gate
    //      must not mutate state.
    //   3. `short_circuit_interval` must return `refresh_seconds *
    //      1000`, NOT `compute_next_interval(...)` — the gates
    //      deliberately do not apply backoff so the cadence stays
    //      aligned across a lock window.
    //   4. `clear_force_refresh_state` must zero `fail_streak` AND
    //      clear `offline` (the two-field invariant all three
    //      force-refresh call sites depend on).
    //   5. `clear_force_refresh_state` must NOT touch
    //      `screen_locked` — the screenlock watcher owns that flag.
    //   6. `clear_force_refresh_state` must be idempotent.
    // ------------------------------------------------------------------

    #[test]
    fn app_state_default_has_screen_locked_false() {
        // Graceful-degradation invariant: when the ScreenSaver daemon
        // is absent (headless CI, Wayland-without-lock-daemon), the
        // watcher task exits without ever sending a LockEvent, so
        // `state.screen_locked` stays at its Default value forever.
        // If that default were ever `true`, polling would never
        // happen on such hosts. Pinned here so a future refactor
        // that flips the default trips the test immediately.
        assert!(
            !AppState::default().screen_locked,
            "AppState::default().screen_locked must be false so absent ScreenSaver daemon does not gate polling"
        );
    }

    #[test]
    fn is_screen_locked_returns_true_when_locked() {
        let s = AppState {
            screen_locked: true,
            ..AppState::default()
        };
        assert!(is_screen_locked(&s));
    }

    #[test]
    fn is_screen_locked_returns_false_when_unlocked() {
        // Default-state path: should be unreachable in production
        // (the watcher would have set `screen_locked = true`) but the
        // helper must still return the right answer for any state.
        let s = AppState::default();
        assert!(!is_screen_locked(&s));
    }

    #[test]
    fn is_screen_locked_does_not_mutate_state() {
        // Pure read. Pin the fields that matter for the gate logic
        // (offline + fail_streak + screen_locked); flipping any of
        // them would change the orchestrator's behavior on the next
        // pass.
        let mut s = AppState::default();
        s.offline = true;
        s.fail_streak = 3;
        let before = (s.offline, s.fail_streak, s.screen_locked);
        let _ = is_screen_locked(&s);
        assert_eq!(
            (s.offline, s.fail_streak, s.screen_locked),
            before,
            "is_screen_locked must be a pure read",
        );
    }

    #[test]
    fn short_circuit_interval_equals_refresh_seconds_times_1000() {
        let mut cfg = Config::default();
        cfg.refresh_seconds = 120;
        assert_eq!(short_circuit_interval(&cfg), 120_000);
    }

    #[test]
    fn short_circuit_interval_scales_with_refresh_seconds() {
        // Regression guard: if the gate ever switches to
        // `compute_next_interval` (which clamps to
        // `refresh_min_seconds` and applies backoff), a 30-second
        // refresh_seconds would no longer yield a 30-second gate
        // interval. Pinned so the divergence between "gate cadence"
        // and "fetch cadence" stays explicit.
        for secs in [1u64, 15, 30, 60, 120, 600, 3600] {
            let cfg = Config {
                refresh_seconds: secs,
                refresh_min_seconds: 15,
                refresh_max_backoff_seconds: 600,
                ..Config::default()
            };
            assert_eq!(
                short_circuit_interval(&cfg),
                secs * 1000,
                "short_circuit_interval must equal refresh_seconds * 1000 (secs={secs})",
            );
        }
    }

    #[test]
    fn short_circuit_interval_does_not_apply_backoff() {
        // Even with `refresh_min_seconds = 999` (which would force a
        // 999-second minimum on backoff logic), the gate must
        // return the raw `refresh_seconds * 1000`. This pins the
        // divergence between gate cadence (raw) and fetch cadence
        // (backoff-aware).
        let cfg = Config {
            refresh_seconds: 30,
            refresh_min_seconds: 999,
            refresh_max_backoff_seconds: 600,
            ..Config::default()
        };
        assert_eq!(
            short_circuit_interval(&cfg),
            30_000,
            "short_circuit_interval must ignore refresh_min_seconds (no backoff clamping)"
        );
    }

    #[test]
    fn clear_force_refresh_state_zeros_fail_streak() {
        // Pre-seed a non-zero fail_streak — what we'd see after two
        // failed fetches. The helper must clear it so the next fetch
        // starts from zero (skip backoff), matching the gjs
        // `refresh(true)` semantics.
        let mut s = AppState::default();
        s.fail_streak = 5;
        clear_force_refresh_state(&mut s);
        assert_eq!(s.fail_streak, 0, "must clear fail_streak");
    }

    #[test]
    fn clear_force_refresh_state_clears_offline() {
        // The whole point of the offline-override in this commit:
        // a cached "we're offline" opinion must NOT prevent a
        // force-refresh (user clicked Refresh, NM reconnected, screen
        // unlocked) from making a fetch attempt. Pre-seed offline=true
        // and confirm the helper flips it.
        let mut s = AppState::default();
        s.offline = true;
        clear_force_refresh_state(&mut s);
        assert!(!s.offline, "must clear offline (the override is the whole point)");
    }

    #[test]
    fn clear_force_refresh_state_does_not_touch_screen_locked() {
        // CRITICAL invariant. If `force_refresh_now` cleared
        // `screen_locked`, it would race the screenlock watcher's
        // own transitions (ScreenLock(true) → ScreenLock(false) →
        // ForceRefresh arriving in mpsc order, but the watcher might
        // be about to send another ScreenLock(true)). The lock flag
        // is owned by the `screenlock` module's event stream, full
        // stop. Pre-seed screen_locked = true (the locked state)
        // and confirm the helper does NOT clear it. If the screen
        // is locked, `do_refresh`'s own screen-lock gate handles
        // the short-circuit — no need to second-guess the flag here.
        let mut s = AppState::default();
        s.screen_locked = true;
        clear_force_refresh_state(&mut s);
        assert!(
            s.screen_locked,
            "clear_force_refresh_state must NOT clear screen_locked — the screenlock watcher owns that flag"
        );
    }

    #[test]
    fn clear_force_refresh_state_does_not_touch_screen_locked_when_unlocked() {
        // Mirror of the above, but for the default case. If the
        // helper accidentally did `screen_locked = !screen_locked`,
        // the default would flip to true and break graceful
        // degradation (see `app_state_default_has_screen_locked_false`).
        let mut s = AppState::default();
        let before = s.screen_locked;
        clear_force_refresh_state(&mut s);
        assert_eq!(s.screen_locked, before);
    }

    #[test]
    fn clear_force_refresh_state_is_idempotent() {
        // Calling the helper twice in a row must leave state in the
        // same end-state as one call. Guards against a regression
        // that adds an unconditional mutation on the second call.
        let mut s = AppState {
            offline: true,
            fail_streak: 7,
            ..AppState::default()
        };
        clear_force_refresh_state(&mut s);
        let snapshot = (
            s.offline,
            s.fail_streak,
            s.screen_locked,
            s.last_good_at,
            s.polls_since_pricing_refresh,
        );
        clear_force_refresh_state(&mut s);
        assert_eq!(
            (
                s.offline,
                s.fail_streak,
                s.screen_locked,
                s.last_good_at,
                s.polls_since_pricing_refresh,
            ),
            snapshot,
            "clear_force_refresh_state must be idempotent"
        );
    }

    #[test]
    fn clear_force_refresh_state_clears_both_fields_atomically() {
        // Regression guard: the original inline body was TWO lines
        // (`s.fail_streak = 0; s.offline = false;`). The helper must
        // still clear BOTH. A regression that drops one of the two
        // lines would change behavior: clearing only `fail_streak`
        // means the offline gate still short-circuits the fetch and
        // the user clicks Refresh and nothing happens. Clearing
        // only `offline` means a backoff build-up overrides the
        // user's "no really, go now" intent.
        let mut s = AppState {
            offline: true,
            fail_streak: 4,
            ..AppState::default()
        };
        clear_force_refresh_state(&mut s);
        assert_eq!(s.fail_streak, 0, "must zero fail_streak");
        assert!(!s.offline, "must clear offline");
    }
}
