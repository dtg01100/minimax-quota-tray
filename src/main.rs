//! Entry point — tokio runtime, no GLib, no GTK.
//!
//! Threading model: a tokio runtime runs an async refresh loop. The D-Bus
//! connection (via zbus) handles its own I/O on the tokio reactor. No
//! thread_local state — the SNI handle is shared via `Arc<Tray>`.
//!
//! Refresh schedule: a tokio task sleeps for the next interval, then runs
//! the fetch on the same task. Adaptive intervals (yellow/2, red/4) +
//! exponential backoff on errors live in `scheduler::next_interval`.

mod burn;
mod config;
mod fetch;
mod icon;
mod keyring;
mod parse;
mod scheduler;
mod sni;
mod util;

use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::burn::{Sample, Window};
use crate::config::Config;
use crate::sni::Tray;

/// Per-window burn sample history. ~16h at 120s baseline.
const BURN_MAX_SAMPLES: usize = 480;

#[derive(Default)]
struct AppState {
    five_h_history: Vec<Sample>,
    weekly_history: Vec<Sample>,
    last_good: Option<(Window, Window)>,
    last_good_at: i64,
    fail_streak: u32,
    http_client: Option<fetch::HttpClient>,
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
    let cfg = Arc::new(config::load_or_init()?);
    let http_client = fetch::build_client().context("build HTTP client")?;
    let state = Arc::new(Mutex::new(AppState {
        http_client: Some(http_client),
        ..Default::default()
    }));
    let tray = Arc::new(Tray::new().await.context("create SNI tray")?);

    // Run an initial render so the icon appears immediately.
    render_initial(&tray, &cfg).await;

    // Spawn the refresh loop.
    tokio::spawn(refresh_loop(cfg, state, tray));

    // Wait for SIGINT/SIGTERM. tokio::signal handles both on Unix.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => log::info!("ctrl-c, exiting"),
        _ = sigterm.recv() => log::info!("SIGTERM, exiting"),
    }
    Ok(())
}

/// Run the periodic refresh loop. Sleeps for the adaptive interval
/// between fetches. Errors don't kill the loop — they just bump the
/// backoff counter.
async fn refresh_loop(
    cfg: Arc<Config>,
    state: Arc<Mutex<AppState>>,
    tray: Arc<Tray>,
) {
    loop {
        let interval_ms = do_refresh(&cfg, &state, &tray).await;
        tokio::time::sleep(std::time::Duration::from_millis(interval_ms)).await;
    }
}

/// One refresh cycle: fetch → record samples → compute burn → render.
/// Returns the next interval in milliseconds.
async fn do_refresh(
    cfg: &Config,
    state: &Arc<Mutex<AppState>>,
    tray: &Arc<Tray>,
) -> u64 {
    // secret-service internally spins up its own async runtime, which
    // clashes with our tokio runtime. Run on a blocking thread.
    let api_key = match tokio::task::spawn_blocking(keyring::get).await {
        Ok(Some(k)) => k,
        _ => {
            render_error(tray, cfg, "No API key").await;
            return cfg.refresh_seconds * 1000;
        }
    };
    let endpoint = match cfg.plans.get(&cfg.plan) {
        Some(p) => p.endpoint.clone(),
        None => {
            render_error(tray, cfg, &format!("Unknown plan: {}", cfg.plan)).await;
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

    // Fetch happens on the tokio thread; the reqwest blocking call is
    // wrapped in spawn_blocking so we don't tie up the runtime worker.
    let result = tokio::task::spawn_blocking(move || {
        fetch::fetch_windows_blocking(&client, &endpoint, &api_key)
    })
    .await
    .unwrap_or_else(|e| Err(anyhow::anyhow!("fetch task panicked: {e}")));

    let mut s = state.lock().await;
    match result {
        Ok((five_h, weekly)) => {
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
            // The burn result feeds the bucket_for call so a high
            // remaining% with a burn rate that would exhaust the window
            // before reset flips the chip to yellow (Warning) — matches
            // gjs bucketForChip's `(burn && burn.exhaustBeforeReset)`.
            // Without this the icon stays green even when the title
            // text already shows `⚠ exhausts in Xm`, which is the
            // visible divergence we close here.
            let bucket = icon::bucket_for(pct, false,
                                          cfg.thresholds.yellow, cfg.thresholds.red,
                                          burn_5h.as_ref());
            // Match gjs setChip(): when the window is exhausted (pct <= 0)
            // gjs shows the static `quota-throttled` SVG dot — it does
            // NOT render a red ring. We mirror that by sending no
            // IconPixmap for the Throttled bucket so the tray falls
            // through to the IconName (= `quota-throttled`, installed
            // by install.sh under hicolor/scalable/apps). Renders for
            // Normal/Warning go through IconPixmap (the green/yellow
            // ring with center dot).
            // For Normal/Warning, write the ring PNG to disk and use its
            // path as IconName — matches gjs `set_icon_full(path, ...)` so
            // the AppIndicator extension reads the rendered PNG and shows
            // the ring (with center dot + faded track) instead of the
            // theme-icon fallback (a solid filled circle). See the long
            // comment on `icon::ring_icon_path` for why this is needed.
            // For Throttled (no ring), use the static `quota-throttled`
            // theme icon.
            let icon_name: String = match bucket {
                icon::Bucket::Normal | icon::Bucket::Warning => {
                    icon::write_ring_png(pct, bucket).to_string_lossy().into_owned()
                }
                icon::Bucket::Throttled => bucket_name(bucket).to_string(),
            };
            let title = title_for(&five_h, burn_5h.as_ref());
            let pixmap = match bucket {
                icon::Bucket::Throttled => None,
                _ => icon::render_pixmap(pct, bucket),
            };
            let interval = scheduler::next_interval(
                cfg.refresh_seconds,
                cfg.refresh_max_backoff_seconds,
                pct, cfg.thresholds.yellow, cfg.thresholds.red, 0,
            );
            drop(s);
            let _ = tray.update(&title, &icon_name, "Active", pixmap).await;
            interval * 1000
        }
        Err(e) => {
            s.fail_streak = s.fail_streak.saturating_add(1);
            let fail_streak = s.fail_streak;
            let pct = s.last_good.map(|(w, _)| w.remaining_pct).unwrap_or(100);
            let err_str = e.to_string();
            drop(s);
            render_error(tray, cfg, &err_str).await;
            scheduler::next_interval(
                cfg.refresh_seconds,
                cfg.refresh_max_backoff_seconds,
                pct, cfg.thresholds.yellow, cfg.thresholds.red, fail_streak,
            ) * 1000
        }
    }
}

async fn render_initial(tray: &Arc<Tray>, cfg: &Config) {
    let has_key = tokio::task::spawn_blocking(keyring::get)
        .await
        .ok()
        .flatten()
        .is_some();
    let title = if has_key {
        "MiniMax: connecting…".to_string()
    } else {
        "MiniMax: no API key".to_string()
    };
    // Match gjs setChip() with no primary window: IconName =
    // `quota-normal` (the static green dot), no IconPixmap. Showing a
    // 100%-filled ring at startup would falsely imply "100% remaining"
    // when we have no data yet — gjs shows the static dot in this state.
    let _ = tray.update(&title, "quota-normal", "Passive", None).await;
    log::info!("started; plan={} (refresh every {}s)", cfg.plan, cfg.refresh_seconds);
}

/// Theme icon name for a bucket. Mirrors the gjs `ICON` table — the
/// names must match the SVGs in icons/ that install.sh installs under
/// ~/.local/share/icons/hicolor/scalable/apps/. Some SNI clients
/// (including older AppIndicator extension builds) prefer IconName over
/// IconPixmap when both are present, so the fallback theme name has to
/// be the project's dedicated icon — otherwise a tray that ignores
/// IconPixmap ends up showing a generic Adwaita dialog/info glyph.
fn bucket_name(b: icon::Bucket) -> &'static str {
    use icon::Bucket;
    match b {
        Bucket::Normal => "quota-normal",
        Bucket::Warning => "quota-warning",
        Bucket::Throttled => "quota-throttled",
    }
}

async fn render_error(tray: &Arc<Tray>, _cfg: &Config, msg: &str) {
    let title = format!("MiniMax: {msg}");
    // Match gjs setChip() error branch: IconName = `quota-error`
    // (the static error dot), no IconPixmap. gjs explicitly falls
    // through to the static icon for the error state — it doesn't
    // try to render a ring with bogus data.
    let _ = tray.update(&title, "quota-error", "Active", None).await;
    log::warn!("{msg}");
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

/// Build the title text — what shows next to the icon in the tray.
fn title_for(w: &Window, burn: Option<&burn::BurnResult>) -> String {
    let base = format!("MiniMax: {}%", w.remaining_pct);
    match burn {
        Some(b) if b.rate_per_hour > 0.0 && b.exhaust_before_reset => {
            // Warn about exhaustion.
            let mins = (b.exhaust_ms / 60_000.0).max(0.0) as i64;
            let when = if mins < 60 {
                format!("{}m", mins)
            } else {
                format!("{}h {}m", mins / 60, mins % 60)
            };
            if b.unit == "pct" {
                format!("MiniMax: {}% ⚠ exhausts in {}", w.remaining_pct, when)
            } else {
                let r = if b.rate_per_hour >= 1000.0 {
                    format!("{:.1}k", b.rate_per_hour / 1000.0)
                } else {
                    format!("{:.0}", b.rate_per_hour)
                };
                format!("MiniMax: {}% ⚠ {}/h exhausts in {}", w.remaining_pct, r, when)
            }
        }
        Some(b) if b.rate_per_hour > 0.0 => {
            // Show burn rate inline.
            if b.unit == "pct" {
                format!("MiniMax: {}% ({:.0}%/h)", w.remaining_pct, b.rate_per_hour)
            } else {
                let r = if b.rate_per_hour >= 1000.0 {
                    format!("{:.1}k", b.rate_per_hour / 1000.0)
                } else {
                    format!("{:.0}", b.rate_per_hour)
                };
                format!("MiniMax: {}% ({} tok/h)", w.remaining_pct, r)
            }
        }
        _ => base,
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}