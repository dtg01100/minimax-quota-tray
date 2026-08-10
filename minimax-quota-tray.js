#!/usr/bin/gjs -m
// minimax-quota-tray.js — standalone GNOME tray indicator for MiniMax quota.
// Supports both the Coding Plan and the Token Plan via the user's API key.
// Talks directly to the MiniMax API (no Hermes dependency).

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
  icon_name: 'appointment-soon-symbolic',
  warning_icon: 'dialog-warning-symbolic',
  thresholds: { yellow: 60, red: 85 },
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
      return merged;
    }
  } catch (e) {
    printerr(`minimax-quota: config error: ${e.message}`);
  }
  try {
    GLib.mkdir_with_parents(CONFIG_DIR, 0o700);
    const f = Gio.File.new_for_path(CONFIG_PATH);
    f.replace_contents(
      JSON.stringify(DEFAULT_CONFIG, null, 2),
      null, false, Gio.FileCreateFlags.REPLACE_DESTINATION, null
    );
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
      flags: Gio.SubprocessFlags.STDIN_PIPE,
    });
    proc.init(null);
    return proc.communicate(null, null)[0];
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
            const snippet = new TextDecoder().decode(bytes.get_data()).slice(0, 200);
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
  const statusKey = weekly ? 'current_weekly_status' : 'current_interval_status';
  return {
    id: weekly ? 'weekly' : '5h',
    label: weekly ? 'weekly' : '5h',
    total, used,
    remaining_pct,
    resetAt: Date.now() + resetMs,
    throttled: Number(entry[statusKey]) === 1,
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

function barColor(remainingPct) {
  const used = 100 - remainingPct;
  if (used >= config.thresholds.red)    return '#e01b24';
  if (used >= config.thresholds.yellow) return '#f6d32d';
  return '#3584e4';
}

function barMarkup(fractionPct) {
  const W = 22;
  const fraction = Math.max(0, Math.min(1, fractionPct / 100));
  const filled = Math.round(fraction * W);
  const empty = W - filled;
  const color = barColor(fractionPct);
  return `  <tt><span foreground="${color}">${'█'.repeat(filled)}</span><span foreground="#555555">${'░'.repeat(empty)}</span></tt>`;
}

// Gtk.MenuItem.set_markup doesn't exist in gjs — markup must go on the
// child GtkLabel via get_child(). This wrapper handles both forms.
function setItemMarkup(item, markup) {
  const child = item.get_child();
  if (child && typeof child.set_markup === 'function') {
    child.set_markup(markup);
  } else {
    item.set_label(markup.replace(/<[^>]+>/g, ''));
  }
}

// ---------------------------------------------------------------------------
// Tray
// ---------------------------------------------------------------------------

let config, apiKey, indicator;
let isFetching = false;
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
  const ms = Math.max(1000, Math.floor(nextIntervalSeconds(remainingPct) * 1000));
  GLib.timeout_add(GLib.PRIORITY_DEFAULT, ms, () => {
    refresh();
    return GLib.SOURCE_REMOVE;  // refresh() re-schedules itself
  });
}

function setChip({ windows, error, fetching, offline }) {
  const planLabel = (config.plans[config.plan] && config.plans[config.plan].label) || 'MiniMax';
  if (offline) {
    // Show last cached % (if any) with the network-offline icon and a · suffix.
    const cur = lastGoodWindows ? lastGoodWindows.find((w) => w.id === '5h') : null;
    const pct = cur ? cur.remaining_pct : 0;
    indicator.set_icon_full('network-offline-symbolic', '');
    indicator.set_label(`${planLabel} ${pct} ·`, `${planLabel} 0%`);
    return;
  }
  if (error) {
    // Keep showing the last known % (with warning icon + " !" suffix) when
    // we have cached data, so a transient API error doesn't blind the user.
    // Fall back to the bare "Plan !" only when there's no cached data.
    if (lastGoodWindows) {
      const cur = lastGoodWindows.find((w) => w.id === '5h');
      const pct = cur ? cur.remaining_pct : 0;
      indicator.set_icon_full(config.warning_icon, '');
      indicator.set_label(`${planLabel} ${pct} !`, `${planLabel} 0%`);
    } else {
      indicator.set_icon_full(config.warning_icon, '');
      indicator.set_label(`${planLabel} !`, `${planLabel} 0%`);
    }
    return;
  }
  const cur = windows?.find((w) => w.id === '5h');
  const remainingPct = cur ? cur.remaining_pct : 0;
  const iconName =
    remainingPct <= 100 - config.thresholds.red          ? config.warning_icon :
    remainingPct <= 100 - config.thresholds.yellow       ? config.warning_icon :
                                                            config.icon_name;
  const label = fetching
    ? planLabel
    : `${planLabel} ${remainingPct}%`;
  indicator.set_icon_full(iconName, '');
  indicator.set_label(label, `${planLabel} 0%`);
}

function makeBarMenuItem() {
  const item = new Gtk.MenuItem();
  item.set_sensitive(false);
  return { item };
}

function buildMenu() {
  const menu = new Gtk.Menu();
  const planCfg = config.plans[config.plan] || config.plans.coding_plan;

  _menuItems = {
    header: new Gtk.MenuItem({ label: '' }),
    fiveHLabel: new Gtk.MenuItem({ label: '' }),
    fiveHBar:   makeBarMenuItem(),
    weeklyLabel: new Gtk.MenuItem({ label: '' }),
    weeklyBar:   makeBarMenuItem(),
    throttled: new Gtk.MenuItem({ label: '' }),
    error:     new Gtk.MenuItem({ label: '' }),
  };
  for (const k of ['header', 'fiveHLabel', 'weeklyLabel', 'throttled', 'error']) {
    _menuItems[k].set_sensitive(false);
  }

  _menuItems.header.show();
  _menuItems.fiveHLabel.hide();
  _menuItems.fiveHBar.item.hide();
  _menuItems.weeklyLabel.hide();
  _menuItems.weeklyBar.item.hide();
  _menuItems.throttled.hide();
  _menuItems.error.hide();

  menu.append(_menuItems.header);
  menu.append(_menuItems.fiveHLabel);
  menu.append(_menuItems.fiveHBar.item);
  menu.append(_menuItems.weeklyLabel);
  menu.append(_menuItems.weeklyBar.item);
  menu.append(_menuItems.throttled);
  menu.append(_menuItems.error);

  menu.append(new Gtk.SeparatorMenuItem());

  const refreshItem = new Gtk.MenuItem({ label: 'Refresh now' });
  refreshItem.connect('activate', () => refresh());
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

  const cur = effective?.find((w) => w.id === '5h');
  const wk  = effective?.find((w) => w.id === 'weekly');

  _menuItems.header.set_label(`Plan: ${planCfg.label}`);
  _menuItems.header.show();

  if (cur) {
    _menuItems.fiveHLabel.set_label(
      `  5h: ${cur.remaining_pct}% left · resets in ${fmtReset(cur.resetAt - Date.now())}${staleTag}`
    );
    setItemMarkup(_menuItems.fiveHBar.item, barMarkup(cur.remaining_pct));
    _menuItems.fiveHLabel.show();
    _menuItems.fiveHBar.item.show();
  } else {
    _menuItems.fiveHLabel.hide();
    _menuItems.fiveHBar.item.hide();
  }

  if (wk) {
    _menuItems.weeklyLabel.set_label(
      `  weekly: ${wk.remaining_pct}% left · resets in ${fmtReset(wk.resetAt - Date.now())}${staleTag}`
    );
    setItemMarkup(_menuItems.weeklyBar.item, barMarkup(wk.remaining_pct));
    _menuItems.weeklyLabel.show();
    _menuItems.weeklyBar.item.show();
  } else {
    _menuItems.weeklyLabel.hide();
    _menuItems.weeklyBar.item.hide();
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

function refresh() {
  if (isFetching) return;
  if (isOffline) {
    // Don't hit the API — just update the UI to reflect the offline state.
    setChip({ offline: true });
    updateMenu({ offline: true });
    return;
  }
  isFetching = true;
  setChip({ fetching: true });
  const planCfg = config.plans[config.plan] || config.plans.coding_plan;
  fetchQuota(apiKey, planCfg.endpoint)
    .then((payload) => {
      const windows = parsePayload(payload);
      lastGoodWindows = windows;
      lastGoodAt = Date.now();
      setChip({ windows });
      updateMenu({ windows });
      consecutiveFailures = 0;
      const cur = windows.find((w) => w.id === '5h');
      scheduleNext(cur ? cur.remaining_pct : null);
    })
    .catch((err) => {
      setChip({ error: err.message });
      updateMenu({ error: err.message, lastGood: lastGoodWindows, lastGoodAt });
      consecutiveFailures++;
      scheduleNext(null);
    })
    .finally(() => { isFetching = false; });
}

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
  entry.set_visibility(true);
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
      refresh();  // immediately fetch with the new key
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

  config = loadConfig();
  apiKey = loadApiKey();

  if (!apiKey) {
    printerr(`minimax-quota: no API key found in GNOME Keyring, ${KEY_PATH}, or MINIMAX_API_KEY env var`);
  }

  indicator = AyatanaAppIndicator3.Indicator.new(
    'minimax-quota',
    config.icon_name,
    AyatanaAppIndicator3.IndicatorCategory.SYSTEM_SERVICES,
  );
  indicator.set_status(AyatanaAppIndicator3.IndicatorStatus.ACTIVE);
  indicator.set_menu(buildMenu());

  if (!apiKey) {
    setChip({ error: 'No API key — open menu → Set API Key…' });
    updateMenu({ error: 'No API key configured' });
  } else {
    refresh();
  }

  Gtk.main();
}

main();
