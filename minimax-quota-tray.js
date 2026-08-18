#!/usr/bin/gjs -m
// minimax-quota-tray.js — standalone GNOME tray indicator for MiniMax quota.
// Supports both the Coding Plan and the Token Plan via the user's API key.
//
// Tray: libayatana-appindicator (AyatanaAppIndicator3) over the freedesktop
// StatusNotifierItem protocol, with a Gtk menu. An earlier revision spoke
// the SNI spec by hand over Gio.DBus + Dbusmenu.Server; that raced the GNOME
// Shell AppIndicator extension's proxy init (its handlers dereference
// this._cancellable before init completes) and the shell logged repeated
// uncaught JS errors, destabilizing the whole tray. The library paces
// registration and signal emission correctly, so we use it instead.
//
// Note: the library logs a one-line "libayatana-appindicator is deprecated"
// warning at startup. That's cosmetic — the suggested GTK-4 fork
// (libayatana-appindicator-glib) has no GJS typelib, so this is the binding
// the AppIndicator extension is designed to consume. Don't "fix" it by
// hand-rolling SNI again; the races above are why we're here.


import GLib from 'gi://GLib';
import Gio from 'gi://Gio';
import Soup from 'gi://Soup';
import Gtk from 'gi://Gtk?version=3.0';
import AyatanaAppIndicator3 from 'gi://AyatanaAppIndicator3?version=0.1';
import Secret from 'gi://Secret?version=1';

// ---------------------------------------------------------------------------
// Secret schema for the API key
// ---------------------------------------------------------------------------

const KEY_SCHEMA = new Secret.Schema(
  'org.dlafreniere.minimax-quota',
  // DONT_MATCH_NAME: secret-tool writes under the generic schema name
  // ('org.freedesktop.Secret.Generic'); without this flag, our lookup
  // would never match regardless of attributes.
  Secret.SchemaFlags.DONT_MATCH_NAME,
  { application: Secret.SchemaAttributeType.STRING }
);
const KEY_ATTRIBUTES = { application: 'minimax-quota' };
const KEY_LABEL = 'MiniMax API Key';

// ---------------------------------------------------------------------------
// Paths and config
// ---------------------------------------------------------------------------

const HOME = GLib.get_home_dir();
const CONFIG_DIR = `${HOME}/.config/minimax-quota`;
const CONFIG_PATH = `${CONFIG_DIR}/config.json`;
const KEY_PATH = `${CONFIG_DIR}/key`;  // legacy — auto-migrated to keyring on first run

// Burn-rate projection tuning. The quota windows are fixed-length epochs
// (the API exposes start_time / end_time / remains_time), so `used` grows
// monotonically inside an epoch and resets at the boundary — a burn rate
// from successive poll samples is meaningful. These knobs control how much
// history the projection needs before it speaks and how the rate is formed.
const DEFAULT_BURN_WARNING = {
  enabled: true,
  // Watching time (ms) before we project a burn rate. With 120s polls this
  // is ~5 samples; any less and a single burst would scream alarm.
  min_history_ms: 10 * 60 * 1000,
  // Recent-slope window (ms): the least-squares slope over samples within
  // this span is the "right now" burn rate.
  lookback_ms: 60 * 60 * 1000,
  // Floor the rate with the whole-epoch average (used / elapsed since
  // epoch start), so a short recent dip can't hide a fast epoch overall.
  use_epoch_average: true,
};

const DEFAULT_CONFIG = {
  plan: 'coding_plan',
  // Polling cadence (adaptive; see nextIntervalSeconds below).
  // 120s baseline matches peer usage indicators (was 60s, too aggressive).
  refresh_seconds: 120,
  refresh_min_seconds: 15,          // floor for fast/urgent polls
  refresh_max_backoff_seconds: 600, // cap on exponential error backoff
  plans: {
    coding_plan: {
      endpoint: 'https://api.minimax.io/v1/api/openplatform/coding_plan/remains',
      dashboard_url: 'https://platform.minimax.io/console/plan',
      label: 'Coding Plan',
    },
    token_plan: {
      endpoint: 'https://api.minimax.io/v1/token_plan/remains',
      dashboard_url: 'https://platform.minimax.io/console/plan',
      label: 'Token Plan',
    },
  },
  thresholds: { yellow: 60, red: 85 },
  burn_warning: { ...DEFAULT_BURN_WARNING },
};

function loadConfig() {
  try {
    const file = Gio.File.new_for_path(CONFIG_PATH);
    if (file.query_exists(null)) {
      const [, contents] = file.load_contents(null);
      const text = new TextDecoder().decode(contents);
      const merged = Object.assign({}, DEFAULT_CONFIG, JSON.parse(text));
      merged.plans = Object.assign({}, DEFAULT_CONFIG.plans, merged.plans || {});
      merged.thresholds = Object.assign({}, DEFAULT_CONFIG.thresholds, merged.thresholds || {});
      merged.burn_warning = Object.assign({}, DEFAULT_BURN_WARNING, merged.burn_warning || {});
      return merged;
    }
  } catch (e) {
    printerr(`minimax-quota: config error: ${e.message}`);
  }
  try {
    GLib.mkdir_with_parents(CONFIG_DIR, 0o700);
    const f = Gio.File.new_for_path(CONFIG_PATH);
    const bytes = new TextEncoder().encode(JSON.stringify(DEFAULT_CONFIG, null, 2));
    f.replace_contents(
      bytes,
      null, false, Gio.FileCreateFlags.REPLACE_DESTINATION, null
    );
    // Force 0600 explicitly — replace_contents() inherits the process umask
    // (typically 0644), and install.sh already writes the same file at 0600.
    // A read-only mode flip keeps first-run installs consistent with later runs.
    try {
      f.set_attribute_uint32('unix::mode', 0o600, Gio.FileSetAttributeFlags.NONE);
    } catch (_) {}
  } catch (e) {
    printerr(`minimax-quota: cannot create default config: ${e.message}`);
  }
  return JSON.parse(JSON.stringify(DEFAULT_CONFIG));
}

// ---------------------------------------------------------------------------
// API key: GNOME Keyring (primary) → legacy file (auto-migrate) → env var
// ---------------------------------------------------------------------------

// Write via `secret-tool` CLI using Gio.Subprocess — direct stdin pipe, no
// shell, no temp file, no argv quoting. Reads stay in-gjs
// (Secret.password_lookup_sync works at 3 args).
function saveKeyToKeyring(value) {
  try {
    const proc = new Gio.Subprocess({
      argv: ['secret-tool', 'store', '--label=MiniMax API Key',
             'application', 'minimax-quota'],
      flags: Gio.SubprocessFlags.STDIN_PIPE
            | Gio.SubprocessFlags.STDOUT_SILENCE
            | Gio.SubprocessFlags.STDERR_PIPE,
    });
    proc.init(null);
    const stdin = new TextEncoder().encode(value);
    const [ok, , errBytes] = proc.communicate(stdin, null);
    if (!ok) {
      const err = errBytes ? new TextDecoder().decode(errBytes).trim() : '(no stderr)';
      printerr(`minimax-quota: keyring save failed: ${err}`);
      return false;
    }
    return true;
  } catch (e) {
    printerr(`minimax-quota: keyring save failed: ${e.message}`);
    return false;
  }
}

function clearKeyFromKeyring() {
  try {
    const proc = new Gio.Subprocess({
      argv: ['secret-tool', 'clear', 'application', 'minimax-quota'],
      flags: Gio.SubprocessFlags.STDOUT_SILENCE
            | Gio.SubprocessFlags.STDERR_PIPE,
    });
    proc.init(null);
    const [ok, , errBytes] = proc.communicate(null, null);
    if (!ok) {
      const err = errBytes ? new TextDecoder().decode(errBytes).trim() : '(no stderr)';
      printerr(`minimax-quota: keyring clear failed: ${err}`);
      return false;
    }
    return true;
  } catch (e) {
    printerr(`minimax-quota: keyring clear failed: ${e.message}`);
    return false;
  }
}

function loadApiKey() {
  // 1. Try keyring
  try {
    const k = Secret.password_lookup_sync(KEY_SCHEMA, KEY_ATTRIBUTES, null);
    if (k && k.length > 0) return k;
  } catch (e) {
    printerr(`minimax-quota: keyring lookup failed: ${e.message}`);
  }
  // 2. Auto-migrate from legacy file
  try {
    const f = Gio.File.new_for_path(KEY_PATH);
    if (f.query_exists(null)) {
      const [, contents] = f.load_contents(null);
      const text = new TextDecoder().decode(contents).trim();
      if (text) {
        if (saveKeyToKeyring(text)) {
          printerr('minimax-quota: migrated API key from file → GNOME Keyring');
          // Best-effort: clear the legacy file. Don't fail if unlink errors.
          try { f.delete(null); } catch (_) {}
        }
        return text;
      }
    }
  } catch (e) {}
  // 3. Env var fallback (handy for systemd unit overrides)
  return (GLib.getenv('MINIMAX_API_KEY') || '').trim();
}

// ---------------------------------------------------------------------------
// Soup-3.0 client
// ---------------------------------------------------------------------------

const session = Soup.Session.new();
session.user_agent = 'minimax-quota-tray/1.0';
// Hard ceiling on a single request. Without this, a stalled TCP open
// (firewall blackhole, dead API host) sits in send_and_read_async forever
// and blocks the scheduler from re-arming — the next poll never fires.
session.timeout = 30;

function fetchQuota(apiKey, endpoint) {
  return new Promise((resolve, reject) => {
    const message = Soup.Message.new('GET', endpoint);
    if (!message) { reject(new Error('invalid URL')); return; }
    message.request_headers.append('Authorization', `Bearer ${apiKey}`);
    message.request_headers.append('Accept', 'application/json');
    session.send_and_read_async(
      message,
      GLib.PRIORITY_DEFAULT,
      null,
      (sess, res) => {
        try {
          const bytes = sess.send_and_read_finish(res);
          const status = message.status_code;
          if (status !== 200) {
            // Defensive: some providers echo the bearer token in error bodies.
            // Truncate hard and strip anything that looks like an Authorization
            // header before this lands in the menu / journald.
            const raw = new TextDecoder().decode(bytes.get_data()).slice(0, 80);
            const snippet = raw.replace(/(?:Bearer|Authorization|api[-_]?key)\s*[:=]?\s*[A-Za-z0-9._\-+/=]+/gi, '[redacted]');
            reject(new Error(`HTTP ${status}: ${snippet}`));
            return;
          }
          const text = new TextDecoder().decode(bytes.get_data());
          resolve(JSON.parse(text));
        } catch (e) { reject(e); }
      }
    );
  });
}

// ---------------------------------------------------------------------------
// Response parsing
// ---------------------------------------------------------------------------

function parseWindow(entry, weekly) {
  const suffix = weekly ? 'weekly' : 'interval';
  const total = Math.max(0, Number(entry[`current_${suffix}_total_count`]) || 0);
  const used  = Math.max(0, Number(entry[`current_${suffix}_usage_count`])  || 0);
  const remaining_pct = Math.max(
    0,
    Math.min(100, Number(entry[`current_${suffix}_remaining_percent`]) || 0)
  );
  const resetMs = Math.max(0, Number(entry[weekly ? 'weekly_remains_time' : 'remains_time']) || 0);
  // Throttle = the window has been fully consumed. We deliberately do NOT
  // read current_interval_status / current_weekly_status: per the official
  // MiniMax-AI/cli source (src/output/quota-table.ts) that field's enum is
  // 1=normal, 2=exhausted, 3=unlimited — so a `=== 1` check would (mis)flag
  // every healthy window as throttled. Other community parsers (minimax-status,
  // openclaw, openchamber) likewise ignore the status field and infer state
  // from remaining_pct. Stay consistent with that.
  return {
    id: weekly ? 'weekly' : '5h',
    label: weekly ? 'weekly' : '5h',
    total, used,
    remaining_pct,
    resetAt: nowFn() + resetMs,
    // Epoch start (absolute ms). The burn-rate projection uses it to floor
    // the rate with the whole-epoch average; it doubles as the rollover
    // detector (a changed startAt means a fresh epoch). Porters whose
    // provider lacks a start time can omit it (0) — the projection then
    // relies on the recent slope and the used-drop rollover check alone.
    startAt: Math.max(0, Number(entry[weekly ? 'weekly_start_time' : 'start_time']) || 0),
    throttled: remaining_pct <= 0,
  };
}

function parsePayload(payload) {
  const entries = payload.model_remains || [];
  if (entries.length === 0) throw new Error('MiniMax returned no quota windows');
  const entry = entries.find((e) => e.model_name === 'general') || entries[0];
  return [parseWindow(entry, false), parseWindow(entry, true)];
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

function fmtReset(ms) {
  const minutes = Math.max(0, Math.ceil(ms / 60000));
  if (minutes < 60) return `${minutes}m`;
  const h = Math.floor(minutes / 60);
  const m = minutes % 60;
  return m ? `${h}h ${m}m` : `${h}h`;
}

// Compact "Xs / Xm / Xh" since-last-update label for stale-data annotations.
function fmtAge(ms) {
  const s = Math.max(0, Math.floor(ms / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m`;
  return `${Math.floor(m / 60)}h`;
}

function barMarkup(fractionPct) {
  // Plain-text bar — avoids relying on Pango markup support in menu
  // renderers; a plain ASCII bar is universally safe.
  const W = 22;
  const fraction = Math.max(0, Math.min(1, fractionPct / 100));
  const filled = Math.round(fraction * W);
  const empty = W - filled;
  return `  [${'█'.repeat(filled)}${'░'.repeat(empty)}]`;
}

// Compact burn-rate label: "850" or "1.2k" or "12k" tokens/hour.
function fmtRate(tokensPerHour) {
  if (tokensPerHour >= 1000) {
    const k = tokensPerHour / 1000;
    return k >= 100 ? `${Math.round(k)}k` : `${k.toFixed(1).replace(/\.0$/, '')}k`;
  }
  return `${Math.round(tokensPerHour)}`;
}

// ---------------------------------------------------------------------------
// Burn-rate projection
// ---------------------------------------------------------------------------
// The quota windows are fixed-length epochs (start_time / end_time /
// remains_time), so `used` grows monotonically inside an epoch and resets
// at the boundary. recordBurnSample() appends one sample per successful
// refresh and clears the per-window history on epoch rollover; computeBurn()
// forms the rate as max(recent slope, whole-epoch average) and projects
// whether the trend exhausts the window before it resets.
//
// Per-window histories: each plan exposes two windows (e.g. 5h and weekly)
// with very different lengths and reset cadences. Sharing one history
// between them would let a 5h window's samples pollute the weekly rate
// (and vice versa). The window id is its stable label ('5h', 'weekly');

// if a provider renames or reorders them, the projection simply doesn't
// fire for the new label rather than misprojecting.

// Per-window sample history. Keyed by window.id (stable label,
// '5h' / 'weekly'); values are append-only arrays trimmed to
// BURN_MAX_SAMPLES. Each window's samples are independent so a 5h
// window's pacing can't pollute the weekly rate (and vice versa).
const burnHistory = new Map();

// One sample per successful refresh, per window. Tracks both `used`
// (token-count, used by plans that expose it) and `remaining_pct` (the
// universal signal — Coding Plan reports total/used as 0 and tracks
// consumption via remaining_percent only, so a token-only fit is blind
// there). `recordBurnSample` records both; `computeBurn` picks the
// signal that actually moves.
function recordBurnSample(window) {
  if (!window || !window.id) return;
  let history = burnHistory.get(window.id);
  if (!history) {
    history = [];
    burnHistory.set(window.id, history);
  }
  const last = history[history.length - 1];
  if (last) {
    const rolled =
      (window.startAt > 0 && last.startAt > 0 && window.startAt !== last.startAt) ||
      window.used < last.used - 1;  // defensive: usage only grows within an epoch
    if (rolled) history.length = 0;
  }
  history.push({
    t: nowFn(),
    used: window.used,
    total: window.total,
    remainingPct: window.remaining_pct,
    startAt: window.startAt,
    resetAt: window.resetAt,
  });
  if (history.length > BURN_MAX_SAMPLES) history.shift();
}

// Least-squares slope of `key` over the samples within lookback_ms.
// Returns null if fewer than 2 samples or den <= 0.
function slopePerHour(samples, key) {
  if (samples.length < 2) return null;
  const t0 = samples[0].t;
  let sx = 0, sy = 0;
  for (const s of samples) { sx += s.t - t0; sy += s[key]; }
  const mx = sx / samples.length, my = sy / samples.length;
  let num = 0, den = 0;
  for (const s of samples) {
    const dx = s.t - t0 - mx;
    num += dx * (s[key] - my);
    den += dx * dx;
  }
  if (den <= 0) return null;
  return (num / den) * 3.6e6;  // value per ms → per hour
}

// Burn projection for a window, from the recorded sample history. Returns
// null (no warning) until the history spans min_history_ms.
//
// The Coding Plan's primary window reports `current_interval_total_count`
// and `current_interval_usage_count` as 0 — only `current_interval_remaining_percent`
// carries consumption signal. Token-based plans (Token Plan, etc.) expose
// real count fields. We pick whichever signal actually moves:
//   - token-based rate when `used` grows across samples (or the epoch-average
//     floor is positive); expressed in tok/h; projects exhaust by token count
//   - pct-based rate when `remaining_pct` drops across samples (Coding Plan);
//     expressed in %/h; projects exhaust when remaining_pct would hit 0
//   - 0 when neither moves (idle); informational row reads "0 tok/h"
//
// exhaustBeforeReset is naturally false at rate=0 (Infinity is not less
// than any finite remainingMs), so we never warn on an idle user.
// Returns null only when there's not enough history to speak at all.
//
// Per-window: the caller passes the sample history for THIS window (drawn
// from `burnHistory.get(window.id)`). Computing a 5h rate from a weekly
// history would read like a steep burn (weekly usage is heavy) and a
// weekly rate from a 5h history would look idle for most of the week.
function computeBurn(window, history) {
  if (!window) return null;
  if (!history) return null;
  const now = nowFn();
  const cfg = Object.assign({}, DEFAULT_BURN_WARNING, config.burn_warning || {});
  if (!cfg.enabled) return null;
  if (history.length < 2) return null;
  if (now - history[0].t < cfg.min_history_ms) return null;

  // Samples within the recent-slope window, oldest first.
  const recent = [];
  for (let i = history.length - 1; i >= 0; i--) {
    if (now - history[i].t > cfg.lookback_ms) break;
    recent.unshift(history[i]);
  }
  if (recent.length < 2) return null;

  // Try token-based rate first (used > 0 in recent samples, OR the
  // epoch-average floor on `used` is positive). Used-only is the legacy
  // signal and is more meaningful when the API actually exposes counts.
  let tokenRate = null;
  if (recent.some((s) => s.used > 0)) {
    const slopeTok = slopePerHour(recent, 'used');
    if (slopeTok !== null && slopeTok > 0) tokenRate = slopeTok;
  }
  if (cfg.use_epoch_average && window.startAt > 0) {
    const elapsedMs = now - window.startAt;
    if (elapsedMs > 0) {
      const avg = (window.used / elapsedMs) * 3.6e6;
      if (avg > 0 && (tokenRate === null || avg > tokenRate)) tokenRate = avg;
    }
  }

  // Pct-based rate (Coding Plan and any provider whose count fields are 0).
  // remaining_pct DROPS as the window is consumed, so the slope is negative;
  // we store it as a positive "burn" rate (pct-per-hour consumed).
  let pctRate = null;
  const slopePct = slopePerHour(recent, 'remainingPct');
  if (slopePct !== null && slopePct < 0) pctRate = -slopePct;  // negate → positive

  // Pick the mode that has a signal. Token-based wins when both have data
  // (a count-based provider whose pct also moves); otherwise the one with
  // a positive rate. If neither has data, surface an informational row
  // with rate=0 (idle / freshly-started).
  let mode = 'idle';
  let rate = 0;
  if (tokenRate !== null) { mode = 'token'; rate = tokenRate; }
  else if (pctRate !== null) { mode = 'pct'; rate = pctRate; }

  const remainingMs = Math.max(0, window.resetAt - now);
  if (remainingMs <= 0) return null;

  // Projection: where does remaining_pct land at reset, at the current rate?
  // token mode: derive used at reset, then pct = 100 * (total - usedAtReset) / total
  // pct mode:   pct drops linearly at rate pct/h
  // idle:       use the last observed remaining_pct (no change)
  let projectedPctLeft = window.remaining_pct;
  let exhaustMs = Infinity;  // default for rate=0; never < remainingMs
  if (mode === 'pct' && rate > 0) {
    // pct drops at `rate` per hour; hits 0 when remainingMs catches up.
    const hoursToZero = window.remaining_pct / rate;
    exhaustMs = hoursToZero * 3600e3;
    projectedPctLeft = Math.max(0, window.remaining_pct - (rate * remainingMs) / 3600e3);
  } else if (mode === 'token' && rate > 0 && window.total > 0) {
    const usedAtReset = window.used + (rate * remainingMs) / 3600e3;
    projectedPctLeft = Math.max(0, Math.min(100,
      Math.round(100 * (window.total - usedAtReset) / window.total)));
    exhaustMs = ((window.total - window.used) / rate) * 3600e3;
  }
  projectedPctLeft = isNaN(projectedPctLeft) ? window.remaining_pct
                    : Math.round(projectedPctLeft);

  return {
    ratePerHour: rate,
    mode,                                // 'token' | 'pct' | 'idle'
    exhaustMs,
    remainingMs,
    exhaustBeforeReset: exhaustMs < remainingMs,
    // Where the window lands at reset if the current rate holds — the
    // informational row reads from this even when no exhaustion is projected.
    projectedPctLeft,
  };
}

// Label for the burn-rate row. Warning variant when the trend exhausts the
// window before it resets; otherwise an informational projection of how much
// remains at reset. Shown whenever computeBurn() has enough data to speak.
//
// Rate unit depends on the provider shape:
//   - 'token': tok/h (provider exposes counts, e.g. Token Plan)
//   - 'pct':   %/h consumed (Coding Plan — total/used are 0)
//   - 'idle':  no movement; surfaces "(0 tok/h)" for a uniform look
function burnRowLabel(burn) {
  const rateUnit = burn.mode === 'pct'
    ? `${fmtRate(burn.ratePerHour)}%/h`
    : `${fmtRate(burn.ratePerHour)} tok/h`;
  if (burn.exhaustBeforeReset) {
    return `  ⚠ ${rateUnit} → exhausts ~${fmtReset(burn.exhaustMs)} before reset`;
  }
  return `  · on pace to have ~${burn.projectedPctLeft}% left at reset (${rateUnit})`;
}

// ---------------------------------------------------------------------------
// Tray
// ---------------------------------------------------------------------------

let config, apiKey, indicator;
let isFetching = false;
// Set when refresh() is requested while a fetch is in flight (menu click,
// new API key, network reconnect). The in-flight fetch re-runs refresh()
// from its .finally() so the request is never silently dropped.
let pendingRefresh = false;
// Source id of the single scheduled poll timeout. Keeping exactly one
// pending timeout (cancelled/re-armed in scheduleNext) is what makes the
// polling loop single-flight: previously every manual refresh spawned a
// second, permanently self-rescheduling chain that doubled the request
// rate and raced on the shared notification/backoff state.
let pollTimeoutId = 0;
let consecutiveFailures = 0;
// Cache of the last successful windows + when we got it, so a transient
// API error doesn't leave the menu completely empty (we show stale data
// with an annotation instead).
let lastGoodWindows = null;
let lastGoodAt = 0;
// True when Gio.NetworkMonitor reports the system is offline. When true,
// we skip polling entirely (no point hitting the API) and surface the
// state in the menu; we still show the last good data with a stale tag.
let isOffline = false;
let _menuItems = null;
// Tracks the most-pressing state from the previous successful refresh,
// so we can fire a notification when the state gets worse. null on startup
// means "no prior state" — the first refresh won't notify.
let _lastBucket = null;
// Burn-rate projection state: per-window sample histories (`burnHistory`,
// Map<windowId, samples[]> declared at the top of the burn-rate section).
// Each window's samples are taken on every successful refresh, within the
// current window epoch, cleared on epoch rollover. Drives the per-window
// ⚠ burn-rate row and the chip's warning flip when the primary window's
// trend projects exhaustion before reset.
// 480 samples ≈ 16h at the 120s baseline, 2h at the 15s urgent floor —
// always covers the 1h recent-slope lookback and then some.
const BURN_MAX_SAMPLES = 480;
// Clock seam for the burn-rate projection. The projection is time-based
// (sample timestamps, epoch averages, reset countdowns), and the test
// harness substitutes a fake clock so the math is deterministic. Stubbing
// the global Date.now is unreliable under gjs, so the burn path reads time
// exclusively through nowFn(). Production always uses Date.now.
let nowFn = Date.now;

const BUCKET_RANK = { normal: 0, warning: 1, throttled: 2 };
function bucketFor(pct, throttled) {
  if (throttled || pct <= 0) return 'throttled';
  if (pct <= 100 - config.thresholds.red ||
      pct <= 100 - config.thresholds.yellow) return 'warning';
  return 'normal';
}
// Chip bucket for the primary window: throttled > warning (thresholds OR
// burn-rate projection) > normal. The burn flip is what turns the icon
// yellow while remaining % still looks healthy.
function bucketForChip(primary, burn) {
  if (!primary) return 'normal';
  if (primary.throttled || primary.remaining_pct <= 0) return 'throttled';
  if (primary.remaining_pct <= 100 - config.thresholds.red ||
      primary.remaining_pct <= 100 - config.thresholds.yellow ||
      (burn && burn.exhaustBeforeReset)) return 'warning';
  return 'normal';
}
function primaryBucket(windows) {
  if (!windows || windows.length === 0) return null;
  const p = windows[0];
  return bucketFor(p.remaining_pct, p.throttled);
}

function notify(title, body, urgency) {
  try {
    const args = [
      'notify-send',
      '-a', 'minimax-quota-tray',
      '-h', `string:x-canonical-private-synchronous:${title}`,
    ];
    if (urgency) args.push('-u', urgency);
    args.push(title, body);
    const proc = new Gio.Subprocess({ argv: args, flags: Gio.SubprocessFlags.STDOUT_PIPE | Gio.SubprocessFlags.STDERR_PIPE });
    proc.init(null);
    proc.wait_async(null, () => {});
  } catch (e) {
    printerr(`minimax-quota: notify failed: ${e.message}`);
  }
}

// Adaptive polling: faster when remaining is low, slower when high,
// exponential backoff after errors, jitter to avoid synchronized load.
// Cadence: 120s baseline / 60s fast (< 40% remaining) / 30s urgent
// (< 15% remaining) / up to 600s after repeated failures.
function nextIntervalSeconds(remainingPct) {
  let base = config.refresh_seconds;
  if (remainingPct != null) {
    if (remainingPct < 100 - config.thresholds.red) {
      base = config.refresh_seconds / 4;  // urgent
    } else if (remainingPct < 100 - config.thresholds.yellow) {
      base = config.refresh_seconds / 2;  // fast
    }
  }
  base = Math.max(config.refresh_min_seconds ?? 15, base);
  if (consecutiveFailures > 0) {
    base = Math.min(
      config.refresh_max_backoff_seconds ?? 600,
      base * Math.pow(2, consecutiveFailures),
    );
  }
  base += Math.random() * 5;  // 0..5s jitter
  return base;
}

function scheduleNext(remainingPct) {
  // Single-flight: drop any previously-scheduled poll before arming the
  // next one, so at most one timeout (one polling chain) can ever exist.
  if (pollTimeoutId) {
    GLib.source_remove(pollTimeoutId);
    pollTimeoutId = 0;
  }
  const ms = Math.max(1000, Math.floor(nextIntervalSeconds(remainingPct) * 1000));
  pollTimeoutId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, ms, () => {
    pollTimeoutId = 0;  // firing now; the in-flight fetch re-arms on completion
    refresh();
    return GLib.SOURCE_REMOVE;
  });
}

// Status icons. Offline/throttled/error states ship as static SVGs
// (icons/quota-*.svg). For healthy / warning states we draw a progress ring
// around the dot so the user sees the percentage at a glance without
// spending any panel space on text.
const ICON = {
  normal:    'quota-normal',
  warning:   'quota-warning',
  throttled: 'quota-throttled',
  offline:   'quota-offline',
  error:     'quota-error',
};
const RING_COLOR = {
  normal:  '#3a9d4d',
  warning: '#f6d32d',
};
const RING_CIRCUMFERENCE = 2 * Math.PI * 9;  // r=9, matches the SVG ring

// We render the dynamic ring into $TMPDIR and pass that path to
// set_icon_full. AppIndicator accepts absolute paths.
//
// Layout: background ring (full circle, faded) + foreground arc (the
// remaining %, full color, round caps) + inner dot. The faded background
// fills the gap so the foreground cap reads as a rounded terminus instead
// of a flat dasharray cut.
function renderRingSvg(remainingPct, color) {
  const clamped = Math.max(0, Math.min(100, remainingPct));
  const arc = (RING_CIRCUMFERENCE * clamped) / 100;
  const rest = RING_CIRCUMFERENCE - arc;
  // Round caps add ~half a stroke-width to each visible end, so the
  // foreground arc visually overshoots its dasharray length. We shave
  // the dasharray by half a stroke so the round end aligns with the
  // 12-o'clock start point when pct=100, and the tail sits cleanly when
  // pct<100.
  const halfStroke = 1.25;
  const fgArc = Math.max(0, arc - halfStroke);
  const fgRest = RING_CIRCUMFERENCE - fgArc;
  return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 22 22" width="22" height="22">
  <circle cx="11" cy="11" r="9" fill="none" stroke="${color}" stroke-width="2.5" stroke-opacity="0.25"/>
  <circle cx="11" cy="11" r="9" fill="none" stroke="${color}" stroke-width="2.5" stroke-linecap="round"
          stroke-dasharray="${fgArc.toFixed(2)} ${fgRest.toFixed(2)}" transform="rotate(-90 11 11)"/>
  <circle cx="11" cy="11" r="3.5" fill="${color}"/>
</svg>
`;
}

function ringIconPath(remainingPct) {
  // Render to PNG rather than SVG. The GNOME Shell StatusNotifier host
  // rasterizes icons to panel pixel sizes; SVG strokes thinner than ~1px
  // vanish at panel scale, so the ring disappears. PNG renders crisply at
  // any DPI.
  //
  // Unique filename per remaining-% value: the GNOME Shell host caches
  // resolved icons for ~120s and skips the icon update when the cached
  // Gio.Icon object is unchanged. With a constant path the ring would
  // freeze at whatever percentage the host first loaded. Keying the name
  // on the percentage makes every new value a brand-new icon name (cache
  // miss → fresh render), while unchanged values reuse the cached icon
  // (no churn). At most one tiny file per distinct percentage (0-100).
  const pct = Math.max(0, Math.min(100, Math.round(Number(remainingPct) || 0)));
  const dir = GLib.getenv('TMPDIR') || GLib.get_tmp_dir();
  return `${dir}/minimax-quota-ring-${pct}.png`;
}

function setChip({ windows, error, fetching, offline }) {
  const planLabel = (config.plans[config.plan] && config.plans[config.plan].label) || 'MiniMax';
  // The "primary" window — its remaining_pct drives the chip. By convention,
  // this is the first window returned by parsePayload(). Porters can put
  // whichever window represents the most pressing quota (e.g. the rolling
  // 5h interval) first in the array.
  const primary = (windows || lastGoodWindows)?.[0] || null;

  // Pick an icon based on the most pressing state. Order matters:
  // offline > throttled > error > warning > normal.
  // Healthy / warning states get a dynamic progress ring around the dot;
  // offline / throttled / error get a static dot (no percentage to show).
  let iconName;
  let bucket = 'normal';
  if (offline) {
    iconName = ICON.offline;
    bucket = 'offline';
  } else if (primary && primary.throttled) {
    iconName = ICON.throttled;
    bucket = 'throttled';
  } else if (error && !primary) {
    iconName = ICON.error;
    bucket = 'error';
  } else if (primary) {
    // Burn flip only with fresh data — same gate as the menu row, so the
    // chip never warns from a stale payload + current history (an outage
    // spanning a rollover would otherwise flip it on garbage). The chip
    // only reflects the primary window's burn; the weekly window's row
    // (in the menu) can warn independently.
    const burn = (!error && !offline) ? computeBurn(primary, burnHistory.get(primary.id)) : null;
    bucket = bucketForChip(primary, burn);
    const color = RING_COLOR[bucket];
    const remainingPct = primary.remaining_pct;
    const path = ringIconPath(remainingPct);
    // Skip the SVG encode + magick fork when a PNG already exists for this
    // rounded percentage. The steady state (120s polls at 100% remaining for
    // hours) would otherwise fork ImageMagick every poll for nothing. The
    // filename is keyed on the percentage so any new value is still a cache
    // miss and gets a fresh render.
    if (GLib.file_test(path, GLib.FileTest.EXISTS)) {
      iconName = path;
    } else {
      try {
        // Render to PNG (not SVG) — at panel icon size, SVG strokes thinner
        // than ~1px disappear under the host's rasterization. PNG renders
        // crisply at any DPI.
        const bytes = new TextEncoder().encode(renderRingSvg(remainingPct, color));
        const svgPath = path.replace(/\.png$/, '.svg');
        GLib.file_set_contents(svgPath, bytes);
        // -background none MUST precede the input file: ImageMagick paints
        // the SVG onto its canvas while reading it, using whatever
        // -background is set at that instant. Placed after the input it's
        // too late — the ring comes out on an opaque white square.
        const proc = new Gio.Subprocess({
          argv: ['magick', '-background', 'none', '-density', '600', svgPath, path],
          flags: Gio.SubprocessFlags.STDOUT_PIPE | Gio.SubprocessFlags.STDERR_PIPE,
        });
        proc.init(null);
        proc.wait(null);
        if (proc.get_exit_status() === 0)
          iconName = path;
        else
          iconName = bucket === 'warning' ? ICON.warning : ICON.normal;
      } catch (e) {
        // Fall back to the static dot if conversion fails.
        iconName = bucket === 'warning' ? ICON.warning : ICON.normal;
      }
    }
  } else {
    iconName = ICON.normal;
  }

  indicator.set_icon_full(iconName, '');
  // No text label — the menu carries detail. The accessible label still
  // carries the percentage for screen readers and panel tooltips.
  const accessible = error
    ? `${planLabel} — stale data`
    : offline
    ? `${planLabel} — offline`
    : bucket === 'throttled'
    ? `${planLabel} — throttled`
    : primary
    ? `${planLabel} — ${primary.remaining_pct}% remaining`
    : `${planLabel}`;
  indicator.set_label('', accessible);
}

function makeBarMenuItem() {
  const item = new Gtk.MenuItem();
  item.set_sensitive(false);
  return { item };
}

// Pango markup on a menu item's child GtkLabel. Falls back to plain text
// if the child isn't a label.
function setItemMarkup(item, markup) {
  const child = item.get_child();
  if (child && typeof child.set_markup === 'function') {
    child.set_markup(markup);
  } else {
    item.set_label(markup.replace(/<[^>]+>/g, ''));
  }
}

function buildMenu() {
  const menu = new Gtk.Menu();
  const planCfg = config.plans[config.plan] || config.plans.coding_plan;

  _menuItems = {
    header: new Gtk.MenuItem({ label: '' }),
    // Window rows are rebuilt on every updateMenu() — there's one label+bar
    // pair per window in the parser's return array. The menu's static
    // structure (header, separator, action items) stays fixed; only the
    // window rows in the middle change.
    windowRows: [],
    // Burn-rate rows: one per window, keyed by window.id. Recreated lazily
    // the first time we see a window's id (e.g. '5h', 'weekly'). Each row
    // sits directly under its window's bar; the primary window's row
    // additionally drives the chip's warning flip.
    burnRows: new Map(),
    throttled: new Gtk.MenuItem({ label: '' }),
    error:     new Gtk.MenuItem({ label: '' }),
  };
  for (const k of ['header', 'throttled', 'error']) {
    _menuItems[k].set_sensitive(false);
  }

  _menuItems.header.set_label(`Plan: ${planCfg.label}`);
  _menuItems.header.show();
  _menuItems.throttled.hide();
  _menuItems.error.hide();

  menu.append(_menuItems.header);
  // Window rows are inserted between header and throttled at updateMenu() time.
  menu.append(_menuItems.throttled);
  menu.append(_menuItems.error);

  menu.append(new Gtk.SeparatorMenuItem());

  const refreshItem = new Gtk.MenuItem({ label: 'Refresh now' });
  refreshItem.connect('activate', () => refresh(true));
  menu.append(refreshItem);

  const dashItem = new Gtk.MenuItem({ label: 'Open dashboard' });
  dashItem.connect('activate', () => {
    try { Gio.AppInfo.launch_default_for_uri(planCfg.dashboard_url, null); }
    catch (e) { printerr(`minimax-quota: cannot open dashboard: ${e.message}`); }
  });
  menu.append(dashItem);

  const setKeyItem = new Gtk.MenuItem({ label: 'Set API Key…' });
  setKeyItem.connect('activate', () => showKeyEntryDialog());
  menu.append(setKeyItem);

  menu.append(new Gtk.SeparatorMenuItem());

  const quitItem = new Gtk.MenuItem({ label: 'Quit' });
  quitItem.connect('activate', () => Gtk.main_quit());
  menu.append(quitItem);

  menu.show_all();
  return menu;
}

function updateMenu({ windows, error, lastGood, lastGoodAt, offline }) {
  if (!_menuItems) return;
  const planCfg = config.plans[config.plan] || config.plans.coding_plan;

  // On error, fall back to the last successful payload so the menu still
  // shows useful data (with a "stale · Xm ago" annotation).
  const effective = windows ?? lastGood;
  const stale = !!error && !windows && !!lastGood;
  const ageMs = lastGoodAt ? Date.now() - lastGoodAt : 0;
  const staleTag = stale ? ` · last update ${fmtAge(ageMs)} ago` : '';

  _menuItems.header.set_label(`Plan: ${planCfg.label}`);
  _menuItems.header.show();

  // Rebuild the window rows. Remove the old Gtk widgets from the menu first
  // (Gtk.Menu keeps strong refs, so unparented items would leak). The
  // throttled item is our anchor: window rows always sit immediately before it.
  // Burn rows are tracked by window.id so each window can have its own
  // ⚠ / informational row directly under its bar.
  const menu = _menuItems.throttled.get_parent();
  for (const row of _menuItems.windowRows) {
    menu.remove(row.label);
    menu.remove(row.bar.item);
  }
  _menuItems.windowRows = [];
  for (const burnRow of _menuItems.burnRows.values()) {
    if (burnRow.get_parent()) menu.remove(burnRow);
  }

  if (effective && effective.length > 0) {
    const siblings = menu.get_children();
    let throttledIdx = siblings.indexOf(_menuItems.throttled);
    for (const w of effective) {
      const label = new Gtk.MenuItem({ label: '' });
      label.set_sensitive(false);
      label.set_label(
        `  ${w.label}: ${w.remaining_pct}% left · resets in ${fmtReset(w.resetAt - Date.now())}${staleTag}`
      );
      const bar = makeBarMenuItem();
      setItemMarkup(bar.item, barMarkup(w.remaining_pct));
      _menuItems.windowRows.push({ label, bar });
      menu.insert(label, throttledIdx);
      menu.insert(bar.item, throttledIdx + 1);
      throttledIdx += 2;
      // Burn-rate row for this window, sitting directly under its bar.
      // Only with fresh data — never while stale/offline, when the trend
      // would mislead. Each window's row is computed from its own history
      // (different roles: 5h drives the chip flip, weekly is informational).
      const burn = (!stale && !offline) ? computeBurn(w, burnHistory.get(w.id)) : null;
      if (burn) {
        let burnRow = _menuItems.burnRows.get(w.id);
        if (!burnRow) {
          burnRow = new Gtk.MenuItem({ label: '' });
          burnRow.set_sensitive(false);
          _menuItems.burnRows.set(w.id, burnRow);
        }
        burnRow.set_label(burnRowLabel(burn));
        menu.insert(burnRow, throttledIdx);
        throttledIdx += 1;
        burnRow.show();
      }
    }
    menu.show_all();
  }

  if (effective?.some((w) => w.throttled)) {
    _menuItems.throttled.set_label(stale ? `  ⚠ Throttled (stale)` : '  ⚠ Throttled');
    _menuItems.throttled.show();
  } else {
    _menuItems.throttled.hide();
  }

  if (error) {
    const staleNote = stale ? ' (showing cached data)' : '';
    _menuItems.error.set_label(`  ⚠ Error: ${error}${staleNote}`);
    _menuItems.error.show();
  } else if (offline) {
    _menuItems.error.set_label('  ⚠ Offline — local network unavailable (showing cached data)');
    _menuItems.error.show();
  } else {
    _menuItems.error.hide();
  }
}

function refresh(force) {
  if (!apiKey) {
    // Nothing to poll with; keep the "no key" state visible instead of
    // error-spamming the API with an empty key (menu / reconnect refreshes).
    _hooks.setChip({ error: 'No API key — open menu → Set API Key…' });
    _hooks.updateMenu({ error: 'No API key configured' });
    return;
  }
  if (isFetching) {
    // An explicit request (menu, new key, reconnect) must not be lost to
    // the in-flight fetch — queue it and let .finally() re-run us. Plain
    // timeout-triggered polls just skip: the in-flight fetch re-arms the
    // loop itself, so nothing is dropped.
    if (force) pendingRefresh = true;
    return;
  }
  if (isOffline) {
    // Don't hit the API — just update the UI to reflect the offline state.
    // No poll is scheduled here; the NetworkMonitor reconnect handler
    // restarts the loop when connectivity returns.
    _hooks.setChip({ offline: true });
    _hooks.updateMenu({ offline: true });
    return;
  }
  isFetching = true;
  _hooks.setChip({ fetching: true });
  const planCfg = config.plans[config.plan] || config.plans.coding_plan;
  _hooks.fetchQuota(apiKey, planCfg.endpoint)
    .then((payload) => {
      const windows = parsePayload(payload);
      lastGoodWindows = windows;
      lastGoodAt = Date.now();
      // Record a sample per window so each window's burn rate is computed
      // from its own history. A 5h window's pacing must not pollute the
      // weekly rate (and vice versa).
      for (const w of windows) recordBurnSample(w);
      _hooks.setChip({ windows });
      _hooks.updateMenu({ windows });
      consecutiveFailures = 0;
      const cur = windows[0];

      // Threshold notification: only when state gets worse, and not on the
      // first successful refresh (no prior state to compare against).
      const newBucket = primaryBucket(windows);
      if (_lastBucket !== null &&
          BUCKET_RANK[newBucket] > BUCKET_RANK[_lastBucket]) {
        if (newBucket === 'throttled') {
          _hooks.notify(
            `${config.plans[config.plan].label} — throttled`,
            `Quota exhausted. The menu shows when it resets.`,
            'critical',
          );
        } else if (newBucket === 'warning') {
          _hooks.notify(
            `${config.plans[config.plan].label} — running low`,
            `Remaining dropped below ${100 - config.thresholds.yellow}%.`,
            'normal',
          );
        }
      }
      _lastBucket = newBucket;

      scheduleNext(cur ? cur.remaining_pct : null);
    })
    .catch((err) => {
      _hooks.setChip({ error: err.message });
      _hooks.updateMenu({ error: err.message, lastGood: lastGoodWindows, lastGoodAt });
      consecutiveFailures++;
      scheduleNext(null);
    })
    .finally(() => {
      isFetching = false;
      if (pendingRefresh) {
        pendingRefresh = false;
        refresh(true);
      }
    });
}

// ---------------------------------------------------------------------------
// Offline detection via Gio.NetworkMonitor
// ---------------------------------------------------------------------------

function setupNetworkMonitor() {
  try {
    const monitor = Gio.NetworkMonitor.get_default();
    if (!monitor) return;
    isOffline = !monitor.get_network_available();
    monitor.connect('network-changed', (m, available) => {
      const nowOffline = !available;
      if (nowOffline === isOffline) return;
      isOffline = nowOffline;
      if (isOffline) {
        // Network dropped: surface the offline state immediately rather
        // than relying on refresh(), which the isFetching guard may skip
        // (the in-flight fetch's .catch() would otherwise show a
        // misleading API error row). Pending polls no-op against the
        // offline branch and the loop pauses until connectivity returns.
        _hooks.setChip({ offline: true });
        _hooks.updateMenu({ offline: true });
      } else {
        // Back online: skip the exponential backoff and poll right away.
        consecutiveFailures = 0;
        refresh(true);
      }
    });
  } catch (e) {
    printerr(`minimax-quota: NetworkMonitor unavailable (${e.message}); offline detection disabled`);
  }
}

// ---------------------------------------------------------------------------
// Single-instance guard (PID lock in $XDG_RUNTIME_DIR)
// ---------------------------------------------------------------------------

// GLib doesn't expose getpid() directly; read it from /proc/self/stat.
function getSelfPid() {
  try {
    const [, bytes] = GLib.file_get_contents('/proc/self/stat');
    return parseInt(new TextDecoder().decode(bytes).split(' ')[0]);
  } catch (e) {
    return 0;
  }
}

let _lockPath = null;

function acquireSingleInstanceLock() {
  const dir = GLib.getenv('XDG_RUNTIME_DIR') || GLib.get_tmp_dir();
  const path = `${dir}/minimax-quota-tray.pid`;
  try {
    const f = Gio.File.new_for_path(path);
    try {
      // Atomic create (O_EXCL semantics): fails if another instance got
      // here first, closing the check-then-write race.
      const out = f.create(Gio.FileCreateFlags.NONE, null);
      out.write_all(new TextEncoder().encode(String(getSelfPid())), null);
      out.close(null);
      _lockPath = path;
      return true;
    } catch (e) {
      // Lock already exists — only a live owner blocks startup.
      const [, contents] = f.load_contents(null);
      const pid = parseInt(new TextDecoder().decode(contents).trim(), 10);
      if (pid > 0 && GLib.file_test(`/proc/${pid}`, GLib.FileTest.EXISTS)) {
        printerr(`minimax-quota: another instance is already running (pid ${pid}); exiting.`);
        return false;
      }
      // Stale lock (owner is dead): take it over. Best-effort; a recycled
      // PID would block startup until that process exits — rare, acceptable.
      f.replace_contents(
        new TextEncoder().encode(String(getSelfPid())),
        null, false, Gio.FileCreateFlags.REPLACE_DESTINATION, null
      );
      _lockPath = path;
      return true;
    }
  } catch (e) {
    // Locking is best-effort; refusing to run on a lock error is worse
    // than the (rare) duplicate it would allow.
    printerr(`minimax-quota: cannot acquire single-instance lock (${e.message})`);
    return true;
  }
}

function releaseSingleInstanceLock() {
  if (!_lockPath) return;
  try { Gio.File.new_for_path(_lockPath).delete(null); } catch (e) {}
  _lockPath = null;
}

// ---------------------------------------------------------------------------
// Test seams
// ---------------------------------------------------------------------------
// refresh() and the poll scheduler reach the outside world (network, tray
// chip, menu, notifications) only through these hooks. The app uses the
// real implementations below; the unit test harness
// (tests/scheduler.test.js) replaces them with fakes so overlapping
// refresh() calls can be simulated without a network, a tray, or a display.
const _hooks = {
  fetchQuota,
  setChip,
  updateMenu,
  notify,
};

// Test-only API, imported by tests/scheduler.test.js via
//   import * as app from '../minimax-quota-tray.js';
// The module runs main() only when executed directly (see bottom of file),
// so importing it in a test process boots nothing.
export const __test = {
  setConfig(c) { config = c; },
  setApiKey(k) { apiKey = k; },
  setHooks(h) { Object.assign(_hooks, h); },
  setOffline(o) { isOffline = o; },
  resetState() {
    if (pollTimeoutId) {
      GLib.source_remove(pollTimeoutId);
      pollTimeoutId = 0;
    }
    isFetching = false;
    pendingRefresh = false;
    consecutiveFailures = 0;
    lastGoodWindows = null;
    lastGoodAt = 0;
    isOffline = false;
    _lastBucket = null;
    burnHistory.clear();
    nowFn = Date.now;
  },
  getState() {
    return {
      isFetching,
      pendingRefresh,
      pollTimeoutId,
      consecutiveFailures,
      isOffline,
      lastGoodAt,
      lastBucket: _lastBucket,
    };
  },
  refresh,
  scheduleNext,
  computeBurn,
  burnRowLabel,
  bucketForChip,
  getBurnHistory: (id) => {
    // id may be a window object (uses .id) or a string id. Returns a
    // snapshot array of that window's samples, or [] if no history yet.
    if (!id) return [];
    const key = typeof id === 'string' ? id : id.id;
    const h = burnHistory.get(key);
    return h ? h.slice() : [];
  },
  setNow(fn) { nowFn = fn || Date.now; },
  // Fires the armed poll timeout exactly as GLib would (same teardown, same
  // refresh() call), so tests drive the loop deterministically instead of
  // waiting real seconds. Returns false if no poll is armed.
  firePollTimeout() {
    if (!pollTimeoutId) return false;
    const id = pollTimeoutId;
    pollTimeoutId = 0;
    GLib.source_remove(id);
    refresh();
    return true;
  },
};

// ---------------------------------------------------------------------------
// API key entry modal (writes to GNOME Keyring on Save)
// ---------------------------------------------------------------------------

function showKeyEntryDialog() {
  const win = new Gtk.Window();
  win.set_title('Set MiniMax API Key');
  win.set_modal(true);
  win.set_keep_above(true);
  win.set_resizable(false);
  win.set_position(Gtk.WindowPosition.CENTER);
  win.set_default_size(460, -1);
  // Window-icon is optional; skip to avoid Gdk version conflict

  const outer = new Gtk.Box({ orientation: Gtk.Orientation.VERTICAL, spacing: 10 });
  outer.set_margin_top(16); outer.set_margin_bottom(16);
  outer.set_margin_start(16); outer.set_margin_end(16);

  const msg = new Gtk.Label({
    label: 'Enter your MiniMax API key.\nStored in GNOME Keyring (login collection).',
    xalign: 0,
  });

  const entry = new Gtk.Entry();
  entry.set_text(apiKey || '');
  entry.set_visibility(false);  // mask the typed API key
  entry.set_input_purpose(Gtk.InputPurpose.PASSWORD);  // hint to password manager

  const btnBox = new Gtk.Box({ orientation: Gtk.Orientation.HORIZONTAL, spacing: 8 });
  btnBox.set_halign(Gtk.Align.END);

  const cancelBtn = new Gtk.Button({ label: 'Cancel' });
  const saveBtn = new Gtk.Button({ label: 'Save' });
  saveBtn.get_style_context().add_class('suggested-action');

  const doSave = () => {
    const value = entry.get_text().trim();
    if (!value) {
      msg.set_text('API key cannot be empty.');
      return;
    }
    if (saveKeyToKeyring(value)) {
      apiKey = value;
      win.destroy();
      refresh(true);  // fetch with the new key now (queues if one is in flight)
    } else {
      msg.set_text('Failed to save to GNOME Keyring. Is it unlocked?');
    }
  };

  cancelBtn.connect('clicked', () => win.destroy());
  saveBtn.connect('clicked', doSave);
  entry.connect('activate', doSave);  // Enter key

  btnBox.pack_end(saveBtn, false, false, 0);
  btnBox.pack_end(cancelBtn, false, false, 0);

  outer.pack_start(msg, false, false, 0);
  outer.pack_start(entry, false, false, 0);
  outer.pack_end(btnBox, false, false, 0);

  win.add(outer);
  win.show_all();
  entry.grab_focus();
  entry.select_region(0, -1);  // select all so first keystroke replaces
}

function main() {
  Gtk.init(null);

  if (!acquireSingleInstanceLock()) return;

  config = loadConfig();
  apiKey = loadApiKey();

  if (!apiKey) {
    printerr(`minimax-quota: no API key found in GNOME Keyring, ${KEY_PATH}, or MINIMAX_API_KEY env var`);
  }

  indicator = AyatanaAppIndicator3.Indicator.new(
    'minimax-quota',
    ICON.normal,
    AyatanaAppIndicator3.IndicatorCategory.SYSTEM_SERVICES,
  );
  indicator.set_status(AyatanaAppIndicator3.IndicatorStatus.ACTIVE);
  indicator.set_menu(buildMenu());

  setupNetworkMonitor();

  if (!apiKey) {
    _hooks.setChip({ error: 'No API key — open menu → Set API Key…' });
    _hooks.updateMenu({ error: 'No API key configured' });
  } else {
    refresh(true);
  }

  Gtk.main();
  releaseSingleInstanceLock();
}

// Run the app only when executed directly. When imported (e.g. by the unit
// test harness in tests/, which sets MINIMAX_QUOTA_TEST=1), main() is
// skipped so the scheduler can be exercised without booting the tray.
if (!GLib.getenv('MINIMAX_QUOTA_TEST')) {
  main();
}
