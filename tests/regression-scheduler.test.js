// tests/regression-scheduler.test.js — executable documentation of the poll
// scheduler bug this project's harness guards against.
//
// Run from the repo root (included automatically by tests/run.sh):
//   MINIMAX_QUOTA_TEST=1 gjs -m tests/regression-scheduler.test.js
//
// The bug: before the single-flight fix, `scheduleNext()` armed a fresh
// GLib.timeout_add() source on EVERY fetch completion and never cancelled
// the previous one. `refresh()`'s `isFetching` guard prevented two
// *simultaneous* fetches, but it did not prevent the creation of additional
// independent polling chains — every manual "Refresh now" (or new API key)
// that landed while idle spawned a second, permanently self-rescheduling
// chain. N chains → N× the request rate, duplicate threshold notifications,
// and erratic exponential backoff.
//
// The pre-fix file (commit d4d07cd) cannot be imported by the harness: it
// has no test seams and boots the tray on import. So this test transcribes
// the exact pre-fix scheduling algorithm in isolation (PreFixScheduler
// below — scheduleNext/refresh from minimax-quota-tray.js lines 589–595 and
// 873–923 at d4d07cd) and runs the same overlapping-refresh scenario against
// BOTH implementations:
//
//   Part A (PreFixScheduler replica) — must reproduce the bug: N idle
//     refreshes leave N pending poll sources, the stacked chains fire a
//     burst of fetches, and an explicit refresh during a fetch is silently
//     dropped (no queue — the T10 invariant cannot even be satisfied). If
//     these assertions fail, the replica no longer documents the bug it was
//     extracted to document.
//   Part B (the real, fixed module) — must satisfy the single-flight
//     invariant: exactly one pending poll, fetches ≥1s apart. If these
//     assertions fail, the fix has regressed.
//
// So the test is red-on-pre-fix, green-on-fixed: run it against the replica
// to see the bug, against the app to see it gone.

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
// The pre-fix algorithm, extracted verbatim from d4d07cd
// ---------------------------------------------------------------------------
//
//   function scheduleNext(remainingPct) {                    // line 589
//     const ms = Math.max(1000, Math.floor(nextIntervalSeconds(remainingPct) * 1000));
//     GLib.timeout_add(GLib.PRIORITY_DEFAULT, ms, () => {
//       refresh();
//       return GLib.SOURCE_REMOVE;  // refresh() re-schedules itself
//     });
//   }
//
//   function refresh() {                                      // line 873
//     if (isFetching) return;
//     ...
//     isFetching = true;
//     fetchQuota(apiKey, planCfg.endpoint)
//       .then((payload) => { ...; scheduleNext(...); })       // line 915
//       .catch((err) => { ...; scheduleNext(null); })         // line 921
//       .finally(() => { isFetching = false; });              // line 923
//   }
//
// Only the scheduling-relevant parts are kept (UI updates, parsing, and
// notifications are omitted — they don't influence the bug). The one thing
// that matters is faithfully reproduced: scheduleNext() appends a source
// without cancelling the previous one.

class PreFixScheduler {
  constructor({ fetchImpl, intervalMs = 1000 }) {
    this._fetchImpl = fetchImpl;
    this._intervalMs = intervalMs;
    this.isFetching = false;
    this.fetches = [];          // timestamps of every fetch started
    this._sources = new Set();  // every armed poll source (never pruned here)
  }

  scheduleNext() {
    // PRE-FIX: append, never cancel. Each call arms an independent chain.
    const id = GLib.timeout_add(GLib.PRIORITY_DEFAULT, this._intervalMs, () => {
      this._sources.delete(id);
      this.refresh();
      return GLib.SOURCE_REMOVE;  // refresh() re-schedules itself
    });
    this._sources.add(id);
    return id;
  }

  refresh() {
    // PRE-FIX: isFetching guard only — no queuing, no cancel, and a skipped
    // call re-arms nothing (the in-flight fetch re-arms via .then/.catch).
    if (this.isFetching) return;
    this.isFetching = true;
    this.fetches.push(Date.now());
    this._fetchImpl()
      .then(() => this.scheduleNext())   // pre-fix .then re-arms
      .catch(() => this.scheduleNext())  // pre-fix .catch re-arms (backoff)
      .finally(() => { this.isFetching = false; });
  }

  pendingPollCount() {
    let n = 0;
    for (const id of this._sources) {
      if (GLib.MainContext.default().find_source_by_id(id) !== null) n++;
    }
    return n;
  }

  dispose() {
    for (const id of this._sources) {
      if (GLib.MainContext.default().find_source_by_id(id) !== null)
        GLib.source_remove(id);
    }
    this._sources.clear();
  }
}

// ---------------------------------------------------------------------------
// Fakes for the (fixed) app under test
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
};

let fetchMode = 'manual';      // 'manual' | 'auto'
const pendingResolvers = [];
// Only fetch timing matters here; UI/notification calls are no-ops.
const calls = { fetch: [] };

function okPayload(remainingPct) {
  return {
    model_remains: [{
      model_name: 'general',
      current_interval_total_count: 500,
      current_interval_usage_count: Math.round(500 * (100 - remainingPct) / 100),
      current_interval_remaining_percent: remainingPct,
      remains_time: 3600000,
      current_interval_status: 1,
      current_weekly_total_count: 5000,
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
    return new Promise((res) => pendingResolvers.push(res));
  },
  setChip: () => {},
  updateMenu: () => {},
  notify: () => {},
});

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function resetApp() {
  app.__test.resetState();
  calls.fetch.length = 0;
  pendingResolvers.length = 0;
  fetchMode = 'manual';
  app.__test.setApiKey('test-key');
  app.__test.setOffline(false);
}

function resolveNext(payload) {
  assert(pendingResolvers.length > 0, 'expected an in-flight fetch to resolve');
  pendingResolvers.shift()(payload);
}

// Yield to the main loop once so queued promise jobs get dispatched (see the
// notes in scheduler.test.js: GJS defers jobs queued inside an await
// continuation to the next loop dispatch).
function flush() {
  return new Promise((resolve) => {
    GLib.timeout_add(GLib.PRIORITY_DEFAULT, 0, () => {
      resolve();
      return GLib.SOURCE_REMOVE;
    });
  });
}

function sleep(ms) {
  return new Promise((resolve) => {
    GLib.timeout_add(GLib.PRIORITY_DEFAULT, ms, () => {
      resolve();
      return GLib.SOURCE_REMOVE;
    });
  });
}

// Sample a growing list of fetch timestamps for `watchMs` of wall time.
async function collectTimestamps(count, at, watchMs) {
  const starts = [];
  let last = count();
  const end = Date.now() + watchMs;
  while (Date.now() < end) {
    await sleep(50);
    if (count() > last) {
      for (let i = last; i < count(); i++) starts.push(at(i));
      last = count();
    }
  }
  return starts;
}

// Largest number of timestamps within any `windowMs` span (burstiness).
function maxBurst(starts, windowMs) {
  let best = 0;
  for (const t of starts) {
    let c = 0;
    for (const u of starts) if (u >= t && u <= t + windowMs) c++;
    best = Math.max(best, c);
  }
  return best;
}

// ---------------------------------------------------------------------------
// Part A — reproduce the bug against the pre-fix algorithm
// ---------------------------------------------------------------------------

await test('A1: PRE-FIX — idle refreshes stack independent poll chains', async () => {
  const bug = new PreFixScheduler({ fetchImpl: () => Promise.resolve({}) });
  try {
    bug.refresh();
    await flush();
    assert(bug.pendingPollCount() === 1, 'one poll after the first refresh');

    for (let i = 0; i < 5; i++) {
      bug.refresh();            // idle → a fresh fetch, a fresh chain
      await flush();
    }
    const pending = bug.pendingPollCount();
    print(`      PRE-FIX: ${pending} pending poll sources after 6 idle refreshes (single-flight wants 1)`);
    // The documented bug: every completion armed a new source, none cancelled.
    // A single-flight result (1) means the replica stopped reproducing the
    // bug; any other drift from the 6 stacked sources shows up in the count.
    assert(pending === 6, `expected the 6 stacked sources of the documented bug, got ${pending}`);
  } finally {
    bug.dispose();
  }
});

await test('A2: PRE-FIX — the stacked chains fire a burst of fetches', async () => {
  const bug = new PreFixScheduler({ fetchImpl: () => Promise.resolve({}) });
  try {
    bug.refresh();
    await flush();
    for (let i = 0; i < 5; i++) {
      bug.refresh();
      await flush();
    }
    bug.fetches.length = 0;     // ignore the seed fetches
    await sleep(1600);          // all 6 stacked 1s polls are due by now
    const starts = bug.fetches;
    const burst = maxBurst(starts, 300);
    print(`      PRE-FIX: ${starts.length} fetch(es) fired, worst 300ms burst = ${burst}`);
    assert(starts.length >= 3, `expected a burst of fetches from 6 stacked chains, got ${starts.length}`);
    assert(burst >= 3, `expected ≥3 fetches within 300ms (rate multiplication), got ${burst}`);
  } finally {
    bug.dispose();
  }
});

// A3 completes the T10 story against the pre-fix code. The current scheduler
// queues an explicit refresh() that arrives mid-fetch and replays it exactly
// once (see scheduler.test.js T10). The pre-fix refresh() had no queue at
// all: its `if (isFetching) return;` (d4d07cd line 874) silently dropped the
// request — a manual "Refresh now", a newly saved API key, or a reconnect
// poll never happened until the next scheduled interval.
await test('A3: PRE-FIX — refresh() during a fetch is silently dropped (no queue)', async () => {
  const bug = new PreFixScheduler({ fetchImpl: () => Promise.resolve({}) });
  try {
    bug.refresh();                            // fetch A in flight
    assert(bug.isFetching === true, 'fetch in flight');
    assert(bug.fetches.length === 1, 'one fetch started');

    bug.refresh();                            // explicit requests while fetching…
    bug.refresh();                            // …pre-fix: both silently dropped
    assert(bug.fetches.length === 1, 'no fetch started for the dropped requests');
    print('      PRE-FIX: 2 explicit refresh(es) during a fetch were silently dropped (no queue)');

    await flush();                            // A completes
    assert(bug.isFetching === false, 'fetch completed');
    assert(bug.fetches.length === 1, 'nothing replayed — the dropped requests stay dropped');
  } finally {
    bug.dispose();
  }
});

// ---------------------------------------------------------------------------
// Part B — the same scenario against the fixed app must NOT exhibit the bug
// ---------------------------------------------------------------------------

await test('B1: FIXED — the same refreshes leave exactly one pending poll', async () => {
  resetApp();
  app.__test.refresh(true);
  resolveNext(okPayload(100));
  await flush();
  assert(app.__test.getState().pollTimeoutId > 0, 'one poll armed');

  for (let i = 0; i < 5; i++) {
    app.__test.refresh(true);
    resolveNext(okPayload(100));
    await flush();
  }
  const s = app.__test.getState();
  assert(s.pollTimeoutId > 0, 'still exactly one armed poll');
  // The single source fires exactly once…
  assert(app.__test.firePollTimeout() === true, 'the one poll fires');
  assert(app.__test.firePollTimeout() === false, 'no second chain to fire');
  resolveNext(okPayload(100));
  await flush();
  assert(app.__test.getState().pollTimeoutId > 0, 'loop re-armed, still single');
});

// B2 is the decisive regression detector: B1 only holds the single handle
// to the *last* armed source, so it stays green even if cancel-before-arm
// is removed (verified: with the fix reverted, B2 sees 13 fetches in 2.1s
// instead of ≤3, while B1 still passes). Keep this real-timer test.
await test('B2: FIXED — real timers show no burst (fetches ≥1s apart)', async () => {
  resetApp();
  fetchMode = 'auto';
  const origRandom = Math.random;
  Math.random = () => 0;        // kill jitter: poll = exactly 1000ms
  try {
    app.__test.refresh(true);
    for (let i = 0; i < 5; i++) {
      await flush();
      app.__test.refresh(true);
    }
    await flush();
    const starts = await collectTimestamps(
      () => calls.fetch.length,
      (i) => calls.fetch[i].at,
      2100,
    );
    const n = starts.length;
    assert(n >= 1 && n <= 3, `expected 1–3 timeout-driven fetches, got ${n}`);
    for (let i = 1; i < starts.length; i++) {
      const gap = starts[i] - starts[i - 1];
      assert(gap >= 800, `burst detected: two fetches only ${gap}ms apart`);
    }
    print(`      FIXED: ${n} timeout-driven fetch(es), ≥1s apart — no burst`);
  } finally {
    Math.random = origRandom;
  }
});

// ---------------------------------------------------------------------------
// Summary
// ---------------------------------------------------------------------------

print('');
print('Part A (pre-fix replica) demonstrates the bug — stacked chains, burst, dropped refreshes;');
print('Part B (fixed app) proves the single chain and no burst (queued-and-replayed exactly once is');
print('covered by scheduler.test.js T10).');
print(`passed: ${passed}  failed: ${failed}`);
if (failed > 0) imports.system.exit(1);
