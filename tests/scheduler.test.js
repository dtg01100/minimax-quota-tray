// tests/scheduler.test.js — unit harness for the minimax-quota-tray poll
// scheduler.
//
// Run from the repo root:
//   ./tests/run.sh
//   (or manually: MINIMAX_QUOTA_TEST=1 gjs -m tests/scheduler.test.js)
//
// MINIMAX_QUOTA_TEST=1 makes the app module (when imported) skip main(),
// so the harness can drive the scheduler without booting a tray. All side
// effects (network fetch, tray chip, menu, notifications) are replaced with
// fakes via the app's __test seams.
//
// The focus is the single-flight invariant: at most one poll timeout is ever
// armed, and an explicit refresh() arriving while a fetch is in flight is
// queued — not dropped, and not multiplied. Before the fix, every manual
// refresh that landed while idle spawned a second, permanently
// self-rescheduling polling chain.
//
// Driving async: GJS runs promise continuations as main-context jobs, but a
// promise resolved inside an `await` continuation is not dispatched by a
// synchronous MainContext.iteration() call. So the tests are async and
// `await flush()`, which yields to the loop via a 0ms timeout — the fetch
// chain's jobs (queued first) drain before the flush continuation runs.

import GLib from 'gi://GLib';
import * as app from '../minimax-quota-tray.js';

// ---------------------------------------------------------------------------
// Tiny test framework (zero dependencies)
// ---------------------------------------------------------------------------

let passed = 0;
let failed = 0;

function assert(cond, msg) {
  if (!cond) throw new Error(msg);
}

async function test(name, fn) {
  try {
    await fn();
    passed++;
    print(`ok   - ${name}`);
  } catch (e) {
    failed++;
    print(`FAIL - ${name}: ${e.message}`);
  }
}

// ---------------------------------------------------------------------------
// Fakes for the app's hooks
// ---------------------------------------------------------------------------

const TEST_CONFIG = {
  plan: 'coding_plan',
  refresh_seconds: 1,           // 1s polls (1000ms floor) keep tests fast
  refresh_min_seconds: 1,
  refresh_max_backoff_seconds: 2,
  plans: {
    coding_plan: {
      endpoint: 'https://example.invalid/',
      dashboard_url: 'https://example.invalid/',
      label: 'Test Plan',
    },
  },
  thresholds: { yellow: 60, red: 85 },
  burn_warning: { enabled: true, min_history_ms: 1, lookback_ms: 3600e3, use_epoch_average: true },
};

// Payload with explicit interval usage (for burn-rate tests). startTime is
// the epoch start in ms; remainsMs the ms until the window resets. The
// weekly knobs let a test drive weekly usage independent of the 5h window
// (both are in the same payload and both get a sample recorded).
function burnPayload({
  used, total = 500, remainsMs = 5 * 3600e3, startTime = 0,
  // Weekly defaults mirror the LIVE Coding Plan API shape: token count
  // fields are 0/0, consumption is tracked via remaining_percent only
  // (verified 2026-08-18 against api.minimax.io general entry). Token-based
  // weekly tests must opt in by passing weeklyTotal / weeklyUsed explicitly.
  weeklyUsed = 0, weeklyTotal = 0, weeklyRemainsMs = 6 * 86400e3, weeklyStartTime = 0,
}) {
  const remainingPct = Math.max(0, Math.round(100 * (total - used) / total));
  const weeklyRemainingPct = Math.max(0, Math.round(100 * (weeklyTotal - weeklyUsed) / weeklyTotal));
  return {
    model_remains: [{
      model_name: 'general',
      current_interval_total_count: total,
      current_interval_usage_count: used,
      current_interval_remaining_percent: remainingPct,
      start_time: startTime,
      remains_time: remainsMs,
      current_interval_status: 1,
      current_weekly_total_count: weeklyTotal,
      current_weekly_usage_count: weeklyUsed,
      current_weekly_remaining_percent: weeklyRemainingPct,
      weekly_start_time: weeklyStartTime,
      weekly_remains_time: weeklyRemainsMs,
      current_weekly_status: 1,
    }],
  };
}

let fetchMode = 'manual';      // 'manual' | 'auto' | 'reject'
const pendingResolvers = [];   // one resolver per in-flight fetch
const calls = { fetch: [], setChip: [], updateMenu: [], notify: [] };

function okPayload(remainingPct) {
  return {
    model_remains: [{
      model_name: 'general',
      current_interval_total_count: 500,
      current_interval_usage_count: Math.round(500 * (100 - remainingPct) / 100),
      current_interval_remaining_percent: remainingPct,
      remains_time: 3600000,
      current_interval_status: 1,
      current_weekly_total_count: 0,
      current_weekly_usage_count: 0,
      current_weekly_remaining_percent: 100,
      weekly_remains_time: 561600000,
      current_weekly_status: 1,
    }],
  };
}

app.__test.setConfig(TEST_CONFIG);
app.__test.setApiKey('test-key');
app.__test.setHooks({
  fetchQuota: (key, ep) => {
    calls.fetch.push({ key, ep, at: Date.now() });
    if (fetchMode === 'auto') return Promise.resolve(okPayload(100));
    if (fetchMode === 'reject') return Promise.reject(new Error('simulated failure'));
    return new Promise((res) => pendingResolvers.push(res));
  },
  setChip: (a) => calls.setChip.push(a),
  updateMenu: (a) => calls.updateMenu.push(a),
  notify: (title, body) => calls.notify.push({ title, body }),
});

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function reset() {
  app.__test.resetState();
  app.__test.setConfig(TEST_CONFIG);   // restore burn_warning knobs for each test
  calls.fetch.length = 0;
  calls.setChip.length = 0;
  calls.updateMenu.length = 0;
  calls.notify.length = 0;
  pendingResolvers.length = 0;
  fetchMode = 'manual';
  app.__test.setApiKey('test-key');
  app.__test.setOffline(false);
}

function resolveNext(payload) {
  assert(pendingResolvers.length > 0, 'expected an in-flight fetch to resolve');
  pendingResolvers.shift()(payload);
}

// Yield to the main loop once so queued promise jobs (the fetch .then /
// .finally chain) get dispatched. The 0ms timeout fires first; the chain's
// jobs were queued before this promise's continuation, so they drain first.
function flush() {
  return new Promise((resolve) => {
    GLib.timeout_add(GLib.PRIORITY_DEFAULT, 0, () => {
      resolve();
      return GLib.SOURCE_REMOVE;
    });
  });
}

// Await-based sleep. Unlike a synchronous MainContext.iteration() loop, this
// lets the loop dispatch real timers AND the promise jobs they queue.
function sleep(ms) {
  return new Promise((resolve) => {
    GLib.timeout_add(GLib.PRIORITY_DEFAULT, ms, () => {
      resolve();
      return GLib.SOURCE_REMOVE;
    });
  });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

await test('T1: overlapping forced refreshes queue exactly one follow-up fetch', async () => {
  reset();
  app.__test.refresh(true);                 // fetch #1 starts
  assert(app.__test.getState().isFetching, 'fetch in flight');
  assert(calls.fetch.length === 1, 'one fetch started');

  app.__test.refresh(true);                 // queued…
  app.__test.refresh(true);                 // …and re-queued (must not pile up)
  app.__test.refresh(true);
  assert(app.__test.getState().pendingRefresh === true, 'refresh queued while fetching');
  assert(calls.fetch.length === 1, 'no extra fetch started while in flight');

  resolveNext(okPayload(100));
  await flush();                            // .then + .finally run; queued refresh fires
  assert(calls.fetch.length === 2, 'exactly one follow-up fetch, not three');
  assert(app.__test.getState().pendingRefresh === false, 'queue drained');

  resolveNext(okPayload(100));
  await flush();
  assert(calls.fetch.length === 2, 'no runaway chain after drain');
  assert(!app.__test.getState().isFetching, 'idle again');
});

await test('T2: repeated idle refreshes never stack poll timeouts', async () => {
  reset();
  app.__test.refresh(true);
  resolveNext(okPayload(100));
  await flush();
  assert(app.__test.getState().pollTimeoutId > 0, 'first poll armed');

  for (let i = 0; i < 5; i++) {
    app.__test.refresh(true);
    assert(calls.fetch.length === i + 2, `manual refresh #${i + 1} starts a fetch`);
    resolveNext(okPayload(100));
    await flush();
  }
  assert(calls.fetch.length === 6, 'six manual fetches total');
  assert(app.__test.getState().pollTimeoutId > 0, 'still exactly one armed poll');

  // Firing the armed timeout once must produce exactly one refresh…
  assert(app.__test.firePollTimeout() === true, 'armed poll fires');
  assert(calls.fetch.length === 7, 'timeout drove exactly one fetch');
  assert(app.__test.getState().pollTimeoutId === 0, 'poll disarmed after firing');
  // …and a second fire must be a no-op: no second chain exists.
  assert(app.__test.firePollTimeout() === false, 'no second chain to fire');

  resolveNext(okPayload(100));
  await flush();
  assert(app.__test.getState().pollTimeoutId > 0, 'loop re-armed, still single');
});

await test('T3: poll firing mid-fetch is absorbed without losing the poll', async () => {
  reset();
  app.__test.refresh(true);
  resolveNext(okPayload(100));
  await flush();
  assert(app.__test.getState().pollTimeoutId > 0, 'poll armed');

  app.__test.refresh(true);                 // fetch #2 in flight while poll armed
  assert(calls.fetch.length === 2, 'manual refresh started a fetch');

  assert(app.__test.firePollTimeout() === true, 'scheduled poll fires mid-fetch');
  assert(calls.fetch.length === 2, 'mid-fetch poll does not start a new fetch');
  assert(!app.__test.getState().pendingRefresh, 'no-force poll queues nothing');
  assert(app.__test.getState().pollTimeoutId === 0, 'fired poll disarmed');

  resolveNext(okPayload(100));              // in-flight fetch completes…
  await flush();
  const s = app.__test.getState();
  assert(s.pollTimeoutId > 0, '…and re-arms exactly one poll');
  assert(!s.isFetching, 'idle');
});

await test('T4: real timers — repeated manual refreshes do not multiply the poll rate', async () => {
  reset();
  fetchMode = 'auto';                       // fetches complete instantly
  const origRandom = Math.random;
  Math.random = () => 0;                    // kill jitter: poll = exactly 1000ms
  try {
    app.__test.refresh(true);               // seed the chain
    for (let i = 0; i < 5; i++) {
      await flush();                        // let the auto-resolved fetch settle
      app.__test.refresh(true);             // idle now → starts a new fetch
    }
    await flush();
    assert(calls.fetch.length === 6, 'six seed fetches');

    const starts = [];
    let last = calls.fetch.length;
    const watchEnd = Date.now() + 3400;   // ≥2 full intervals at 1s
    while (Date.now() < watchEnd) {
      await sleep(50);
      if (calls.fetch.length > last) {
        for (let i = last; i < calls.fetch.length; i++) starts.push(calls.fetch[i].at);
        last = calls.fetch.length;
      }
    }
    // One chain, 1s interval → ~2–3 timeout-driven fetches in 3.4s, ≥1s
    // apart. Pre-fix, the 5 extra chains armed 5 additional 1s timeouts
    // that all fired together — a burst of 5 fetches within milliseconds.
    const n = starts.length;
    assert(n >= 2 && n <= 4, `expected 2–4 timeout-driven fetches, got ${n}`);
    for (let i = 1; i < starts.length; i++) {
      const gap = starts[i] - starts[i - 1];
      assert(gap >= 800, `burst detected: two fetches only ${gap}ms apart`);
    }
    print(`      (observed ${n} timeout-driven fetches ≥1s apart — no burst)`);
  } finally {
    Math.random = origRandom;
  }
});

await test('T5: offline skips polling and surfaces the offline state', async () => {
  reset();
  app.__test.setOffline(true);
  app.__test.refresh(true);
  assert(calls.fetch.length === 0, 'no fetch while offline');
  assert(app.__test.getState().pollTimeoutId === 0, 'no poll armed while offline');
  assert(!app.__test.getState().isFetching, 'not stuck in fetching');
  assert(calls.setChip.some((c) => c.offline === true), 'offline chip rendered');
  assert(calls.updateMenu.some((m) => m.offline === true), 'offline menu row shown');

  app.__test.setOffline(false);             // reconnect
  app.__test.refresh(true);
  assert(calls.fetch.length === 1, 'back online → fetch resumes');
  resolveNext(okPayload(100));
  await flush();
  assert(app.__test.getState().pollTimeoutId > 0, 'loop re-armed after reconnect');
});

await test('T6: missing API key shows the error state and never fetches', async () => {
  reset();
  app.__test.setApiKey('');
  app.__test.refresh(true);
  assert(calls.fetch.length === 0, 'no fetch without a key');
  assert(app.__test.getState().pollTimeoutId === 0, 'no poll armed without a key');
  assert(calls.setChip.some((c) => c.error === 'No API key — open menu → Set API Key…'), 'no-key chip');
  assert(calls.updateMenu.some((m) => m.error === 'No API key configured'), 'no-key menu row');
});

await test('T7: threshold notifications fire only on state regression', async () => {
  reset();
  app.__test.refresh(true);  resolveNext(okPayload(100)); await flush();
  assert(calls.notify.length === 0, 'no notification on first refresh (no prior state)');

  app.__test.refresh(true);  resolveNext(okPayload(30)); await flush();   // normal → warning
  assert(calls.notify.length === 1, 'warning notified once');
  assert(calls.notify[0].title.includes('running low'), 'warning notification title');

  app.__test.refresh(true);  resolveNext(okPayload(25)); await flush();   // warning → warning
  assert(calls.notify.length === 1, 'no re-notification for the same bucket');

  app.__test.refresh(true);  resolveNext(okPayload(0)); await flush();    // warning → throttled
  assert(calls.notify.length === 2, 'throttled notified');
  assert(calls.notify[1].title.includes('throttled'), 'throttled notification title');

  app.__test.refresh(true);  resolveNext(okPayload(100)); await flush();  // throttled → normal
  assert(calls.notify.length === 2, 'improvement is not notified');
});

await test('T9: scheduleNext cancels any previously armed poll before re-arming', async () => {
  reset();
  app.__test.scheduleNext(100);
  const id1 = app.__test.getState().pollTimeoutId;
  assert(id1 > 0, 'first poll armed');
  app.__test.scheduleNext(100);
  const id2 = app.__test.getState().pollTimeoutId;
  assert(id2 > 0 && id2 !== id1, 'second poll re-armed');
  assert(GLib.MainContext.default().find_source_by_id(id1) === null,
    'first source is cancelled (single-flight)');
  assert(GLib.MainContext.default().find_source_by_id(id2) !== null,
    'second source is live');
  assert(app.__test.firePollTimeout() === true, 'armed poll fires once');
  assert(app.__test.firePollTimeout() === false, 'no second source to fire');
  resolveNext(okPayload(100));               // fetch started by firePollTimeout
  await flush();
  assert(app.__test.getState().pollTimeoutId > 0, 'loop re-armed, still single');
});

// The queue-drain invariant: .finally() must clear pendingRefresh BEFORE
// re-firing the queued refresh. If it fired first and cleared after, every
// follow-up fetch would see a queued refresh again and re-queue itself — a
// self-perpetuating cascade (infinite loop). T10 pins the ordering: during
// the follow-up fetch the queue must already be empty, and completing the
// follow-up must not start anything further.
await test('T10: queued refresh drains before re-firing — no infinite loop', async () => {
  reset();
  app.__test.refresh(true);                 // fetch A in flight
  app.__test.refresh(true);                 // queue…
  app.__test.refresh(true);                 // …multiple refreshes
  assert(app.__test.getState().pendingRefresh === true, 'refresh queued while fetching');
  assert(calls.fetch.length === 1, 'one fetch in flight');

  resolveNext(okPayload(100));              // A completes → queued refresh fires
  await flush();
  const mid = app.__test.getState();
  assert(calls.fetch.length === 2, 'exactly one follow-up fetch started');
  assert(mid.isFetching === true, 'follow-up fetch in flight');
  assert(mid.pendingRefresh === false,
    'queue drained BEFORE the follow-up fired — a clear-after-fire bug would re-queue here');

  resolveNext(okPayload(100));              // B completes — must NOT cascade
  await flush();
  const done = app.__test.getState();
  assert(calls.fetch.length === 2, 'no cascade: the follow-up did not re-queue itself');
  assert(done.pendingRefresh === false, 'queue empty at rest');
  assert(done.isFetching === false, 'idle — loop terminated');
});

// ---------------------------------------------------------------------------
// Burn-rate projection
// ---------------------------------------------------------------------------
// The projection needs a controlled clock: samples are taken at refresh()
// time, so the recent-slope math must be deterministic. The app exposes a
// clock seam (__test.setNow) for exactly this — do NOT stub the global
// Date.now, which gjs doesn't reliably honor for imported modules. The seam
// also keeps parseWindow's resetAt and the epoch arithmetic consistent.

function withClock(startMs, fn) {
  // Substitute the app's clock seam (not the global Date.now — gjs doesn't
  // reliably honor global reassignment for imported modules). resetState()
  // restores Date.now on the next reset(); we also restore here for safety.
  let now = startMs;
  app.__test.setNow(() => now);
  const p = fn({ now: () => now, advance: (ms) => { now += ms; } });
  // Restore the real clock when the callback's promise settles. A plain
  // `try { return fn() } finally { … }` would restore synchronously, while
  // the async body is still awaiting — the stub would be dead before the
  // first refresh's .then ever ran (this bit us: samples got real times).
  return p.finally(() => app.__test.setNow(null));
}

await test('T11: burn projection warns when the usage trend exhausts before reset', async () => {
  reset();
  await withClock(1700000000000, async ({ now, advance }) => {
    const epochStart = now() - 30 * 60e3;   // epoch began 30 min ago
    app.__test.refresh(true);
    resolveNext(burnPayload({ used: 40, startTime: epochStart, remainsMs: 3 * 3600e3 }));
    await flush();
    assert(app.__test.getBurnHistory('5h').length === 1, 'first burn sample recorded');

    advance(10 * 60e3);                     // 10 minutes of steady burning
    app.__test.refresh(true);
    resolveNext(burnPayload({ used: 240, startTime: epochStart, remainsMs: 3 * 3600e3 }));
    await flush();
    assert(app.__test.getBurnHistory('5h').length === 2, 'second burn sample recorded');

    const w = { id: '5h', total: 500, used: 240, startAt: epochStart, resetAt: now() + 3 * 3600e3, remaining_pct: 52 };
    const burn = app.__test.computeBurn(w, app.__test.getBurnHistory(w.id));
    assert(burn !== null, 'burn projection computed');
    // 200 tokens in 10 min = 1200/h; 260 left → ~13 min to exhaust < 3h reset.
    assert(Math.round(burn.ratePerHour) === 1200, `recent slope rate is 1200/h, got ${burn.ratePerHour}`);
    assert(burn.exhaustBeforeReset === true, 'projects exhaustion before the reset');
    assert(app.__test.bucketForChip(w, burn) === 'warning', 'chip flips to warning');
    const warnLabel = app.__test.burnRowLabel(burn);
    assert(warnLabel.includes('exhausts ~13m before reset'), `warning row label, got: ${warnLabel}`);
    assert(!warnLabel.includes('on pace'), 'warning label is not the informational variant');
  });
});

await test('T12: low burn rate projects no exhaustion warning', async () => {
  reset();
  await withClock(1700000000000, async ({ now, advance }) => {
    const epochStart = now() - 30 * 60e3;
    app.__test.refresh(true);
    resolveNext(burnPayload({ used: 40, startTime: epochStart, remainsMs: 3 * 3600e3 }));
    await flush();
    advance(10 * 60e3);
    app.__test.refresh(true);
    resolveNext(burnPayload({ used: 45, startTime: epochStart, remainsMs: 3 * 3600e3 }));
    await flush();

    const w = { id: '5h', total: 500, used: 45, startAt: epochStart, resetAt: now() + 3 * 3600e3, remaining_pct: 91 };
    const burn = app.__test.computeBurn(w, app.__test.getBurnHistory(w.id));
    assert(burn !== null, 'projection computed (a rate exists)');
    // 5 tok/10 min = 30/h; epoch avg 45/40 min = 67.5/h → 455/67.5 ≈ 6.7h > 3h reset.
    assert(burn.exhaustBeforeReset === false, 'does not warn: exhausts long after the reset');
    assert(app.__test.bucketForChip(w, burn) === 'normal', 'chip stays normal');
    // Even healthy, the informational row shows the projected % at reset:
    // used 45 + 67.5/h × 3h = 247.5 → 252.5/500 = 50.5% → rounds to 51.
    assert(burn.projectedPctLeft === 51, `projected ~51% left at reset, got ${burn.projectedPctLeft}`);
    const infoLabel = app.__test.burnRowLabel(burn);
    assert(infoLabel.includes('on pace to have ~51% left at reset'), `informational row label, got: ${infoLabel}`);
    assert(!infoLabel.includes('⚠'), 'informational label is not the warning variant');
  });
});

await test('T13: epoch rollover clears the burn history', async () => {
  reset();
  await withClock(1700000000000, async ({ now, advance }) => {
    const epoch1 = now() - 30 * 60e3;
    app.__test.refresh(true);
    resolveNext(burnPayload({ used: 40, startTime: epoch1, remainsMs: 10 * 60e3 }));
    await flush();
    advance(10 * 60e3);
    app.__test.refresh(true);
    resolveNext(burnPayload({ used: 200, startTime: epoch1, remainsMs: 60e3 }));
    await flush();
    assert(app.__test.getBurnHistory('5h').length === 2, 'history spans the epoch');

    advance(5 * 60e3);                      // window resets: fresh epoch, usage drops
    app.__test.refresh(true);
    resolveNext(burnPayload({ used: 5, startTime: now(), remainsMs: 5 * 3600e3 }));
    await flush();
    assert(app.__test.getBurnHistory('5h').length === 1, 'history cleared on rollover');

    const w = { id: '5h', total: 500, used: 5, startAt: now(), resetAt: now() + 5 * 3600e3, remaining_pct: 99 };
    assert(app.__test.computeBurn(w, app.__test.getBurnHistory(w.id)) === null, 'no projection with a single fresh sample');
  });
});

await test('T15: enabled:false disables the projection; use_epoch_average:false drops the floor', async () => {
  // disabled: even a steep trend must produce nothing
  reset();
  app.__test.setConfig({
    ...TEST_CONFIG,
    burn_warning: { enabled: false, min_history_ms: 1, lookback_ms: 3600e3, use_epoch_average: true },
  });
  await withClock(1700000000000, async ({ now, advance }) => {
    const epochStart = now() - 30 * 60e3;
    app.__test.refresh(true);
    resolveNext(burnPayload({ used: 40, startTime: epochStart, remainsMs: 3 * 3600e3 }));
    await flush();
    advance(10 * 60e3);
    app.__test.refresh(true);
    resolveNext(burnPayload({ used: 240, startTime: epochStart, remainsMs: 3 * 3600e3 }));
    await flush();
    const w = { id: '5h', total: 500, used: 240, startAt: epochStart, resetAt: now() + 3 * 3600e3, remaining_pct: 52 };
    assert(app.__test.computeBurn(w, app.__test.getBurnHistory(w.id)) === null, 'disabled: no projection despite a steep trend');
  });

  // no epoch-average floor: a flat recent trend returns an informational
  // projection (rate=0) instead of null — so the row still shows "0 tok/h"
  // for an idle user — but it does not warn (exhaustBeforeReset is false
  // at rate=0 because Infinity isn't less than any finite remainingMs).
  reset();
  app.__test.setConfig({
    ...TEST_CONFIG,
    burn_warning: { enabled: true, min_history_ms: 1, lookback_ms: 3600e3, use_epoch_average: false },
  });
  await withClock(1700000000000, async ({ now, advance }) => {
    const epochStart = now() - 60 * 60e3;   // heavy early burn, then flat
    app.__test.refresh(true);
    resolveNext(burnPayload({ used: 200, startTime: epochStart, remainsMs: 3 * 3600e3 }));
    await flush();
    advance(10 * 60e3);
    app.__test.refresh(true);
    resolveNext(burnPayload({ used: 200, startTime: epochStart, remainsMs: 3 * 3600e3 }));
    await flush();
    const w = { id: '5h', total: 500, used: 200, startAt: epochStart, resetAt: now() + 3 * 3600e3, remaining_pct: 60 };
    const burn = app.__test.computeBurn(w, app.__test.getBurnHistory(w.id));
    assert(burn !== null, 'idle user still gets an informational row');
    assert(burn.ratePerHour === 0, `idle user has rate 0, got ${burn.ratePerHour}`);
    assert(burn.exhaustBeforeReset === false, 'idle user does not warn (rate=0 → Infinity exhaustMs)');
    assert(!app.__test.burnRowLabel(burn).includes('⚠'), 'idle user label is informational, not the warning variant');
  });

  // sanity: the same flat trend WITH the floor does warn (epoch avg 200/70min ≈ 171/h → exhaust ~1.75h < 3h)
  reset();
  app.__test.setConfig({
    ...TEST_CONFIG,
    burn_warning: { enabled: true, min_history_ms: 1, lookback_ms: 3600e3, use_epoch_average: true },
  });
  await withClock(1700000000000, async ({ now, advance }) => {
    const epochStart = now() - 60 * 60e3;
    app.__test.refresh(true);
    resolveNext(burnPayload({ used: 200, startTime: epochStart, remainsMs: 3 * 3600e3 }));
    await flush();
    advance(10 * 60e3);
    app.__test.refresh(true);
    resolveNext(burnPayload({ used: 200, startTime: epochStart, remainsMs: 3 * 3600e3 }));
    await flush();
    const w = { id: '5h', total: 500, used: 200, startAt: epochStart, resetAt: now() + 3 * 3600e3, remaining_pct: 60 };
    const burn = app.__test.computeBurn(w, app.__test.getBurnHistory(w.id));
    assert(burn !== null && burn.exhaustBeforeReset === true,
      'with floor: whole-epoch average still projects exhaustion before reset');
  });
});

await test('T14: min_history_ms gate suppresses premature projections', async () => {
  reset();
  app.__test.setConfig({
    ...TEST_CONFIG,
    burn_warning: { enabled: true, min_history_ms: 3600e3, lookback_ms: 3600e3, use_epoch_average: true },
  });
  await withClock(1700000000000, async ({ now, advance }) => {
    const epochStart = now() - 30 * 60e3;
    app.__test.refresh(true);
    resolveNext(burnPayload({ used: 40, startTime: epochStart, remainsMs: 3 * 3600e3 }));
    await flush();
    advance(10 * 60e3);
    app.__test.refresh(true);
    resolveNext(burnPayload({ used: 240, startTime: epochStart, remainsMs: 3 * 3600e3 }));
    await flush();

    const w = { id: '5h', total: 500, used: 240, startAt: epochStart, resetAt: now() + 3 * 3600e3, remaining_pct: 52 };
    assert(app.__test.computeBurn(w, app.__test.getBurnHistory(w.id)) === null,
      'history only spans 10 min < 1h gate — the steep slope must not fire yet');
  });
});

// Regression: live MiniMax API doesn't return start_time, so the epoch-average
// floor is skipped. With an idle user (no token usage), the recent slope is
// also 0. Before this fix, computeBurn() returned null and the menu never
// showed the projected burn rate — even after a long uptime. The fix: when
// the rate is 0, return an informational projection (rate=0) instead of
// null. exhaustBeforeReset is naturally false at rate=0, so we never warn;
// the row just reads "on pace to have ~100% left at reset (0 tok/h)".
await test('T16: idle user with live API shape (no start_time) still gets an informational row', async () => {
  reset();
  app.__test.setConfig({
    ...TEST_CONFIG,
    burn_warning: { enabled: true, min_history_ms: 1, lookback_ms: 3600e3, use_epoch_average: true },
  });
  await withClock(1700000000000, async ({ now, advance }) => {
    // 5 polls spaced 2 min apart, used unchanged (idle user).
    // start_time omitted from every payload → startAt = 0 in parsed window.
    for (let i = 0; i < 5; i++) {
      const used = 100;                          // constant — no growth
      const total = 5000;
      const remainsMs = 5 * 3600e3 - i * 2 * 60e3;
      const payload = {
        model_remains: [{
          model_name: 'general',
          current_interval_total_count: total,
          current_interval_usage_count: used,
          current_interval_remaining_percent: Math.round(100 * (total - used) / total),
          // start_time intentionally omitted
          remains_time: remainsMs,
          current_interval_status: 1,
          current_weekly_total_count: 0,
          current_weekly_usage_count: 0,
          current_weekly_remaining_percent: 80,
          weekly_remains_time: 6 * 86400e3,
          current_weekly_status: 1,
        }],
      };
      app.__test.refresh(true);
      // Use the test's resolveNext helper, which expects burnPayload shape,
      // so build the resolve inline by pushing the payload into the queue.
      assert(pendingResolvers.length > 0, `expected pending fetch at iteration ${i}`);
      pendingResolvers.shift()(payload);
      await flush();
      if (i < 4) advance(2 * 60e3);
    }

    const hist = app.__test.getBurnHistory('5h');
    assert(hist.length === 5, `5 samples recorded, got ${hist.length}`);
    assert(hist[0].startAt === 0, `startAt=0 (live shape), got ${hist[0].startAt}`);

    const last = hist[hist.length - 1];
    const w = {
      id: '5h', label: '5h', total: last.total, used: last.used,
      remaining_pct: last.remainingPct, resetAt: last.resetAt, startAt: last.startAt,
      throttled: false,
    };
    const burn = app.__test.computeBurn(w, app.__test.getBurnHistory(w.id));
    assert(burn !== null, 'idle user with no start_time still gets an informational projection');
    assert(burn.ratePerHour === 0, `idle user rate is 0, got ${burn.ratePerHour}`);
    assert(burn.exhaustBeforeReset === false, 'idle user does not warn');
    assert(burn.projectedPctLeft === 98, `idle user with used=100/5000 projects 98% left at reset, got ${burn.projectedPctLeft}`);
    const label = app.__test.burnRowLabel(burn);
    assert(label.includes('on pace to have ~98% left at reset'), `informational label, got: ${label}`);
    assert(label.includes('0 tok/h'), `shows 0 tok/h, got: ${label}`);
    assert(!label.includes('⚠'), 'no warning glyph for an idle user');
  });
});

// Live Coding Plan shape: `current_interval_total_count` and
// `current_interval_usage_count` are 0; only `current_interval_remaining_percent`
// carries consumption signal. Verify computeBurn fits a pct-based rate and
// the row renders the pct/h variant.
await test('T17: live Coding Plan shape — used=0, remaining_pct drops → pct-based rate', async () => {
  reset();
  app.__test.setConfig({
    ...TEST_CONFIG,
    burn_warning: { enabled: true, min_history_ms: 1, lookback_ms: 3600e3, use_epoch_average: true },
  });
  await withClock(1700000000000, async ({ now, advance }) => {
    // 5 polls spaced 2 min apart; used=0 always; remaining_pct drops 0.5 per poll.
    // 0.5%/2min = 15%/h — well under the 72%/4h = 18%/h that would trigger a
    // warning, so we exercise the informational variant.
    const startTime = now() - 30 * 60e3;     // epoch started 30 min ago
    for (let i = 0; i < 5; i++) {
      const remainingPct = Math.round((80 - i * 0.5) * 10) / 10;  // 80, 79.5, 79, 78.5, 78
      const remainsMs = 5 * 3600e3 - 30 * 60e3 - i * 2 * 60e3;
      const payload = {
        model_remains: [{
          model_name: 'general',
          current_interval_total_count: 0,    // <-- live shape: 0
          current_interval_usage_count: 0,    // <-- live shape: 0
          current_interval_remaining_percent: remainingPct,
          start_time: startTime,
          remains_time: remainsMs,
          current_interval_status: 1,
          current_weekly_total_count: 0,
          current_weekly_usage_count: 0,
          current_weekly_remaining_percent: 80,
          weekly_remains_time: 6 * 86400e3,
          current_weekly_status: 1,
        }],
      };
      app.__test.refresh(true);
      assert(pendingResolvers.length > 0, `expected pending fetch at iteration ${i}`);
      pendingResolvers.shift()(payload);
      await flush();
      if (i < 4) advance(2 * 60e3);
    }

    const hist = app.__test.getBurnHistory('5h');
    assert(hist.length === 5, `5 samples recorded, got ${hist.length}`);
    assert(hist.every((s) => s.used === 0), 'all samples have used=0 (live shape)');

    const last = hist[hist.length - 1];
    const w = {
      id: '5h', label: '5h', total: last.total, used: last.used,
      remaining_pct: last.remainingPct, resetAt: last.resetAt, startAt: last.startAt,
      throttled: false,
    };
    const burn = app.__test.computeBurn(w, app.__test.getBurnHistory(w.id));
    assert(burn !== null, 'live Coding Plan shape must produce a projection');
    assert(burn.mode === 'pct', `pct mode, got ${burn.mode}`);
    // 0.5 pct per 2 min = 15 pct/h
    assert(Math.round(burn.ratePerHour) === 15, `rate ~15%/h, got ${burn.ratePerHour}`);
    // ~78%/15%h ≈ 5.2h to exhaust vs ~4h remaining → does NOT warn
    assert(burn.exhaustBeforeReset === false, '15%/h does not warn (5.2h to exhaust > 4h remainingMs)');
    const label = app.__test.burnRowLabel(burn);
    assert(label.includes('on pace to have'), `informational label, got: ${label}`);
    assert(label.includes('%/h'), `pct unit in label, got: ${label}`);
    assert(!label.includes('tok/h'), `no token unit in pct mode, got: ${label}`);
    assert(!label.includes('⚠'), 'no warning glyph for a healthy 15%/h trend');
  });
});

await test('T18: weekly burn rate is tracked independently of the 5h window (live Coding Plan shape)', async () => {
  // Verified 2026-08-18: the live Coding Plan returns 0/0 for the weekly
  // token count fields, same as the 5h window. Both windows compute in
  // pct mode (%/h), driven by remaining_percent dropping. The point of
  // this test is the INDEPENDENCE property — a quiet 5h does not pollute
  // the weekly rate, and the 5h rollover does not clear the weekly
  // history — not the rate unit (covered by T17 for the 5h side).
  reset();
  app.__test.setConfig({
    ...TEST_CONFIG,
    burn_warning: { enabled: true, min_history_ms: 1, lookback_ms: 3600e3, use_epoch_average: true },
  });
  await withClock(1700000000000, async ({ now, advance }) => {
    const weeklyEpoch = now() - 2 * 86400e3;    // 2 days into the week
    const weeklyResetMs = 5 * 86400e3;          // 5 days left in the week
    const fiveHrStart = now() - 30 * 60e3;      // epoch started 30 min ago
    // 5h: very slow burn (1 pct per 2 min → 30 pct/h). weekly: faster
    // burn (2 pct per 2 min → 60 pct/h). The two rates are independent.
    // start_time is hoisted out of the loop so the rollover detector in
    // recordBurnSample doesn't fire on every iteration (it would see the
    // epoch "advance" by 2 min each poll and clear the history).
    for (let i = 0; i < 5; i++) {
      const fiveHrRemaining = 80 - i * 1;        // 80, 79, 78, 77, 76
      const weeklyRemaining = 90 - i * 2;        // 90, 88, 86, 84, 82
      const fiveHrRemains = 4 * 3600e3 - i * 2 * 60e3;
      const weeklyRemains = weeklyResetMs - i * 2 * 60e3;
      app.__test.refresh(true);
      // Live Coding Plan shape: total/used = 0 for both windows.
      const payload = {
        model_remains: [{
          model_name: 'general',
          current_interval_total_count: 0,
          current_interval_usage_count: 0,
          current_interval_remaining_percent: fiveHrRemaining,
          start_time: fiveHrStart,
          remains_time: fiveHrRemains,
          current_interval_status: 1,
          current_weekly_total_count: 0,
          current_weekly_usage_count: 0,
          current_weekly_remaining_percent: weeklyRemaining,
          weekly_start_time: weeklyEpoch,
          weekly_remains_time: weeklyRemains,
          current_weekly_status: 1,
        }],
      };
      assert(pendingResolvers.length > 0, `expected pending fetch at iteration ${i}`);
      pendingResolvers.shift()(payload);
      await flush();
      if (i < 4) advance(2 * 60e3);
    }

    // 5h: 1 pct / 2 min = 30 pct/h, pct mode (matches T17's live shape).
    const fiveHrHist = app.__test.getBurnHistory('5h');
    assert(fiveHrHist.length === 5, `5h: 5 samples, got ${fiveHrHist.length}`);
    const burn5h = app.__test.computeBurn(
      { id: '5h', total: 0, used: 0,
        startAt: now() - 50 * 60e3, resetAt: now() + 4 * 3600e3 - 8 * 60e3,
        remaining_pct: 76 },
      fiveHrHist,
    );
    assert(burn5h !== null && burn5h.mode === 'pct', `5h: pct mode, got ${burn5h && burn5h.mode}`);
    assert(Math.round(burn5h.ratePerHour) === 30, `5h rate ~30%/h, got ${burn5h.ratePerHour}`);

    // Weekly: 2 pct / 2 min = 60 pct/h, pct mode — independent of 5h.
    const weeklyHist = app.__test.getBurnHistory('weekly');
    assert(weeklyHist.length === 5, `weekly: 5 samples, got ${weeklyHist.length}`);
    const burnWeekly = app.__test.computeBurn(
      { id: 'weekly', total: 0, used: 0,
        startAt: weeklyEpoch, resetAt: now() + weeklyResetMs - 8 * 60e3,
        remaining_pct: 82 },
      weeklyHist,
    );
    assert(burnWeekly !== null && burnWeekly.mode === 'pct', `weekly: pct mode, got ${burnWeekly && burnWeekly.mode}`);
    assert(Math.round(burnWeekly.ratePerHour) === 60, `weekly rate ~60%/h, got ${burnWeekly.ratePerHour}`);
    const label = app.__test.burnRowLabel(burnWeekly);
    assert(label.includes('60%/h'), `weekly row shows %/h rate, got: ${label}`);
    assert(!label.includes('tok/h'), `no token unit in pct mode, got: ${label}`);
  });
});

await test('T19: per-window histories are independent — 5h rollover does not clear weekly', async () => {
  // The 5h window resets every 5h; the weekly window resets every 7d. When
  // the 5h window rolls over, the weekly history must survive. (Per-window
  // Map keyed by window.id is what preserves this.)
  reset();
  await withClock(1700000000000, async ({ now, advance }) => {
    const weeklyEpoch = now() - 1 * 86400e3;
    // Two 5h samples, two weekly samples — both spans min_history_ms.
    app.__test.refresh(true);
    resolveNext(burnPayload({
      used: 100, startTime: now() - 30 * 60e3, remainsMs: 4 * 3600e3,
      weeklyUsed: 800, weeklyStartTime: weeklyEpoch,
    }));
    await flush();
    advance(10 * 60e3);
    app.__test.refresh(true);
    resolveNext(burnPayload({
      used: 200, startTime: now() - 40 * 60e3, remainsMs: 3 * 3600e3,
      weeklyUsed: 900, weeklyStartTime: weeklyEpoch,
    }));
    await flush();
    assert(app.__test.getBurnHistory('5h').length === 2, '5h: 2 samples');
    assert(app.__test.getBurnHistory('weekly').length === 2, 'weekly: 2 samples');

    // 5h window rolls over (fresh epoch = new startAt, used drops to 5).
    advance(5 * 60e3);
    app.__test.refresh(true);
    resolveNext(burnPayload({
      used: 5, startTime: now(), remainsMs: 5 * 3600e3,
      weeklyUsed: 950, weeklyStartTime: weeklyEpoch,
    }));
    await flush();
    assert(app.__test.getBurnHistory('5h').length === 1, '5h: cleared on rollover');
    assert(app.__test.getBurnHistory('weekly').length === 3, 'weekly: preserved across 5h rollover');
  });
});

await test('T20: idle Coding-Plan user (total=0) labels the row with %/h, not tok/h', async () => {
  // Regression for the live-tray bug: an idle user on the Coding Plan
  // (total=0, used=0, no remaining_pct slope) used to show "(0 tok/h)"
  // because the unit was inferred from mode===idle. The Coding Plan
  // has no tokens to count — the unit should be %/h whenever total=0.
  reset();
  app.__test.setConfig({
    ...TEST_CONFIG,
    burn_warning: { enabled: true, min_history_ms: 1, lookback_ms: 3600e3, use_epoch_average: false },
  });
  await withClock(1700000000000, async ({ now, advance }) => {
    // 3 polls, remaining_pct flat at 100, used=0, total=0. Idle.
    for (let i = 0; i < 3; i++) {
      app.__test.refresh(true);
      const payload = {
        model_remains: [{
          model_name: 'general',
          current_interval_total_count: 0,
          current_interval_usage_count: 0,
          current_interval_remaining_percent: 100,
          start_time: 0,            // no epoch start — projection uses slope only
          remains_time: 5 * 3600e3 - i * 2 * 60e3,
          current_interval_status: 1,
          current_weekly_total_count: 0,
          current_weekly_usage_count: 0,
          current_weekly_remaining_percent: 100,
          weekly_remains_time: 6 * 86400e3,
          current_weekly_status: 1,
        }],
      };
      assert(pendingResolvers.length > 0, `expected pending fetch at iteration ${i}`);
      pendingResolvers.shift()(payload);
      await flush();
      if (i < 2) advance(2 * 60e3);
    }

    const w = { id: '5h', total: 0, used: 0, startAt: 0,
                resetAt: now() + 5 * 3600e3 - 4 * 60e3, remaining_pct: 100 };
    const burn = app.__test.computeBurn(w, app.__test.getBurnHistory(w.id));
    assert(burn !== null, 'idle Coding-Plan user still gets an informational row');
    assert(burn.ratePerHour === 0, `idle rate is 0, got ${burn.ratePerHour}`);
    assert(burn.unit === 'pct', `idle Coding-Plan unit is pct, got ${burn.unit}`);
    const label = app.__test.burnRowLabel(burn);
    assert(label.includes('0%/h'), `idle Coding-Plan row shows 0%/h, got: ${label}`);
    assert(!label.includes('tok/h'), `idle Coding-Plan row does not mention tokens, got: ${label}`);
  });
});

// Pct-only providers (Coding Plan, anything returning 0/0 for token counts)
// suppress the burn row when ratePerHour === 0. At 120s polls, a weekly
// user's per-poll drop is ~0.01–0.05%, well below 1% precision — the percent
// barely ticks and the slope is meaningless. Token-counting providers
// compute their rate from real count deltas, so the suppression only fires
// for pct-only providers, never for token plans. Verifies the menu-level
// suppression rule via decideBurnRow() (computeBurn() still returns an
// object; updateMenu() filters it out for pct-only idle windows).
await test('T21: pct-only burn row suppression — weekly + idle 5h hidden, active 5h + token plans shown', async () => {
  reset();
  app.__test.setConfig({
    ...TEST_CONFIG,
    burn_warning: { enabled: true, min_history_ms: 1, lookback_ms: 3600e3, use_epoch_average: true },
  });
  await withClock(1700000000000, async ({ now, advance }) => {
    // Build up history with live Coding Plan shape (total/used = 0 for
    // both windows, remaining_pct dropping on 5h, flat on weekly).
    // start_time and weekly_start_time are hoisted out of the loop so the
    // rollover detector in recordBurnSample doesn't fire on every iteration
    // (it would see the epoch "advance" by 2 min each poll and clear the
    // history). The real API returns the same start_time for the whole epoch.
    const fiveHrStart = now() - 30 * 60e3;
    const weeklyEpoch = now() - 2 * 86400e3;
    for (let i = 0; i < 5; i++) {
      const fiveHrRemaining = 80 - i * 1;        // 5h ticks down 1%/2min
      const weeklyRemaining = 90;                // weekly flat — integer precision kills it
      app.__test.refresh(true);
      const payload = {
        model_remains: [{
          model_name: 'general',
          current_interval_total_count: 0,
          current_interval_usage_count: 0,
          current_interval_remaining_percent: fiveHrRemaining,
          start_time: fiveHrStart,
          remains_time: 4 * 3600e3 - i * 2 * 60e3,
          current_interval_status: 1,
          current_weekly_total_count: 0,
          current_weekly_usage_count: 0,
          current_weekly_remaining_percent: weeklyRemaining,
          weekly_start_time: weeklyEpoch,
          weekly_remains_time: 5 * 86400e3 - i * 2 * 60e3,
          current_weekly_status: 1,
        }],
      };
      assert(pendingResolvers.length > 0, `expected pending fetch at iteration ${i}`);
      pendingResolvers.shift()(payload);
      await flush();
      if (i < 4) advance(2 * 60e3);
    }
    // 5h is now non-zero rate (pct ticks down each poll); weekly is flat.
    assert(app.__test.getBurnHistory('5h').length === 5, '5h: 5 samples recorded');
    assert(app.__test.getBurnHistory('weekly').length === 5, 'weekly: 5 samples recorded');
    const fiveH = { id: '5h', total: 0, used: 0,
                    startAt: fiveHrStart, resetAt: now() + 4 * 3600e3 - 8 * 60e3,
                    remaining_pct: 76 };
    const week = { id: 'weekly', total: 0, used: 0,
                   startAt: weeklyEpoch, resetAt: now() + 5 * 86400e3 - 8 * 60e3,
                   remaining_pct: 90 };
    assert(app.__test.burnRowVisible(fiveH) === true,
      'pct-only 5h with active ticks: burn row visible');
    assert(app.__test.burnRowVisible(week) === false,
      'pct-only weekly (flat at integer): burn row suppressed even with history');
  });

  // Token Plan: 5h active. Rate is non-zero (used grows). Row shown.
  reset();
  await withClock(1700000000000, async ({ now, advance }) => {
    const epochStart = now() - 30 * 60e3;
    for (let i = 0; i < 3; i++) {
      app.__test.refresh(true);
      resolveNext(burnPayload({ used: 100 + i * 20, startTime: epochStart, remainsMs: 3 * 3600e3 }));
      await flush();
      if (i < 2) advance(10 * 60e3);
    }
    assert(app.__test.getBurnHistory('5h').length === 3, 'token-plan 5h: 3 samples');
    const last = app.__test.getBurnHistory('5h').slice(-1)[0];
    const w = { id: '5h', total: last.total, used: last.used,
                startAt: last.startAt, resetAt: last.resetAt, remaining_pct: last.remainingPct };
    assert(app.__test.burnRowVisible(w) === true, 'token-plan 5h active: burn row visible');
  });

  // Token Plan: 5h idle (used unchanged, rate=0, unit='token'). Row still
  // shown — the suppression is for pct-only idle, not token idle.
  reset();
  await withClock(1700000000000, async ({ now, advance }) => {
    const epochStart = now() - 30 * 60e3;
    for (let i = 0; i < 3; i++) {
      app.__test.refresh(true);
      resolveNext(burnPayload({ used: 100, startTime: epochStart, remainsMs: 3 * 3600e3 }));
      await flush();
      if (i < 2) advance(10 * 60e3);
    }
    assert(app.__test.getBurnHistory('5h').length === 3, 'token-plan 5h idle: 3 samples');
    const last = app.__test.getBurnHistory('5h').slice(-1)[0];
    const w = { id: '5h', total: last.total, used: last.used,
                startAt: last.startAt, resetAt: last.resetAt, remaining_pct: last.remainingPct };
    assert(app.__test.burnRowVisible(w) === true, 'token-plan 5h idle: row still shown (unit=token)');
  });

  // Coding Plan: 5h idle (pct flat, rate=0, unit='pct'). Row suppressed.
  reset();
  await withClock(1700000000000, async ({ now, advance }) => {
    const fiveHrStart = now() - 30 * 60e3;
    for (let i = 0; i < 3; i++) {
      app.__test.refresh(true);
      const payload = {
        model_remains: [{
          model_name: 'general',
          current_interval_total_count: 0,
          current_interval_usage_count: 0,
          current_interval_remaining_percent: 100,   // flat — idle
          start_time: fiveHrStart,
          remains_time: 5 * 3600e3 - i * 2 * 60e3,
          current_interval_status: 1,
          current_weekly_total_count: 0,
          current_weekly_usage_count: 0,
          current_weekly_remaining_percent: 100,
          weekly_remains_time: 6 * 86400e3,
          current_weekly_status: 1,
        }],
      };
      assert(pendingResolvers.length > 0, `expected pending fetch at iteration ${i}`);
      pendingResolvers.shift()(payload);
      await flush();
      if (i < 2) advance(2 * 60e3);
    }
    assert(app.__test.getBurnHistory('5h').length === 3, 'pct-only 5h idle: 3 samples');
    const last = app.__test.getBurnHistory('5h').slice(-1)[0];
    const w = { id: '5h', total: last.total, used: last.used,
                startAt: last.startAt, resetAt: last.resetAt, remaining_pct: last.remainingPct };
    assert(app.__test.burnRowVisible(w) === false,
      'pct-only 5h idle (rate=0): row suppressed — no signal is no info');
  });
});

await test('T8: failed fetch shows the error row, backs off, stays single-flight', async () => {
  reset();
  fetchMode = 'reject';
  app.__test.refresh(true);
  await flush();                            // flush the rejection
  const s = app.__test.getState();
  assert(s.consecutiveFailures === 1, 'failure counted');
  assert(s.pollTimeoutId > 0, 'backoff poll armed');
  assert(!s.isFetching, 'not stuck fetching');
  assert(calls.setChip.some((c) => c.error === 'simulated failure'), 'error chip');
  assert(calls.updateMenu.some((m) => m.error === 'simulated failure'), 'error menu row');

  fetchMode = 'manual';
  assert(app.__test.firePollTimeout() === true, 'backoff poll fires');
  assert(app.__test.getState().pollTimeoutId === 0, 'disarmed after firing');
  resolveNext(okPayload(100));               // recovery succeeds
  await flush();
  assert(app.__test.getState().consecutiveFailures === 0, 'success resets failures');
});

// ---------------------------------------------------------------------------
// Summary
// ---------------------------------------------------------------------------

print('');
print(`passed: ${passed}  failed: ${failed}`);
if (failed > 0) imports.system.exit(1);
