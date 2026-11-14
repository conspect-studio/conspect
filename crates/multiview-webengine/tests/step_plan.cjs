'use strict';
// Run: node --test crates/multiview-webengine/tests/step_plan.cjs (driven by tests/step_plan.rs)
const test = require('node:test');
const assert = require('node:assert');
const path = require('node:path');
const sp = require(path.join(__dirname, '..', 'src', 'step_plan.cjs'));

const opts = (o = {}) => ({timing: 'equal', frame_ms: 600, end_hold_ms: 1000, max_steps: 12, ...o});
const MIN = 60000;

test('options are bounded like the Rust contract', () => {
  assert.doesNotThrow(() => sp.validateOptions(opts(), 10));
  for (const bad of [{frame_ms: 99}, {frame_ms: 5001}, {end_hold_ms: -1}, {end_hold_ms: 10001},
    {max_steps: 1}, {max_steps: 49}, {timing: 'real'}, {frame_ms: 600.5}, {max_steps: '12'}])
    assert.throws(() => sp.validateOptions(opts(bad), 10), /invalid capture limits/, JSON.stringify(bad));
  assert.throws(() => sp.validateOptions(opts(), 0));
  assert.throws(() => sp.validateOptions(opts(), 31));
  // 48 steps x 5000 ms at 30 fps = 7200 frames: over the 600-frame cap.
  assert.throws(() => sp.validateOptions(opts({max_steps: 48, frame_ms: 5000}), 30), /invalid capture limits/);
});

test('selects the most recent distinct steps in ascending order', () => {
  const ts = [5, 1, 3, 3, 2, 4, -1, 1.5, 6];
  assert.deepStrictEqual(sp.selectSteps(ts, 3), [4, 5, 6]);
  assert.deepStrictEqual(sp.selectSteps(ts, 48), [1, 2, 3, 4, 5, 6]);
  assert.throws(() => sp.selectSteps([7], 12), /radar timeline unavailable/);
  assert.throws(() => sp.selectSteps(null, 12), /radar timeline unavailable/);
  assert.throws(() => sp.selectSteps([1, 2], 1), /radar timeline unavailable/);
});

test('drops a step identical to the previous kept step only', () => {
  const s = (t, hash) => ({t, hash});
  const {kept, dropped} = sp.dedupe([s(1, 'a'), s(2, 'a'), s(3, 'b'), s(4, 'b'), s(5, 'a')]);
  assert.deepStrictEqual(kept.map(k => k.t), [1, 3, 5]);
  assert.strictEqual(dropped, 2);
  assert.throws(() => sp.dedupe([s(1, 'x'), s(2, 'x'), s(3, 'x')]), /fewer than 2 distinct frames/);
});

test('equal timing: integer frame counts and the hold on the last step', () => {
  const p = sp.planFrames({timestamps: [1, 2, 3], timing: 'equal', frameMs: 600, endHoldMs: 1000, fps: 10});
  assert.deepStrictEqual(p.counts, [6, 6, 16]);
  assert.strictEqual(p.holdFrames, 10);
  assert.strictEqual(p.total, 28);
  // 25 fps x 600 ms = 15 frames exactly, hold 0.
  const q = sp.planFrames({timestamps: [1, 2], timing: 'equal', frameMs: 600, endHoldMs: 0, fps: 25});
  assert.deepStrictEqual(q.counts, [15, 15]);
});

test('cumulative rounding keeps the clip length exact and every step >= 1 frame', () => {
  // 100 ms at 1 fps would round to 0 per step; each still gets >= 1.
  const p = sp.planFrames({timestamps: [1, 2, 3], timing: 'equal', frameMs: 100, endHoldMs: 0, fps: 1});
  assert.ok(p.counts.every(c => c >= 1));
  // 333 ms at 3 fps: cumulative 1,2,3,4 frames, never drifting.
  const q = sp.planFrames({timestamps: [1, 2, 3, 4], timing: 'equal', frameMs: 333, endHoldMs: 0, fps: 3});
  assert.deepStrictEqual(q.counts, [1, 1, 1, 1]);
  const r = sp.planFrames({timestamps: [1, 2, 3, 4, 5, 6], timing: 'equal', frameMs: 250, endHoldMs: 0, fps: 6});
  assert.strictEqual(r.total, sp.framesAt(1500, 6));
});

test('proportional timing follows real gaps, mean frame_ms, clamped', () => {
  // Gaps 10,10,20 min; last step gets the mean gap.
  const ts = [0, 10, 20, 40].map(m => 1e12 + m * MIN);
  const d = sp.durations(ts, 'proportional', 600);
  assert.strictEqual(d[0], d[1]);
  assert.ok(d[2] > d[0] * 1.9 && d[2] < d[0] * 2.1, JSON.stringify(d));
  const mean = d.reduce((a, b) => a + b, 0) / d.length;
  assert.ok(Math.abs(mean - 600) <= 1, String(mean));
  // A huge outage gap clamps to 4x and a tiny gap to 1/4.
  const c = sp.durations([0, 1, 2, 3, 4, 5, 6, 7, 1000].map(m => 1e12 + m * MIN), 'proportional', 600);
  assert.ok(c.every(x => x >= 150 && x <= 2400), JSON.stringify(c));
  assert.strictEqual(Math.max(...c), 2400);
  assert.strictEqual(Math.min(...c), 150);
  assert.deepStrictEqual(sp.durations(ts, 'equal', 600), [600, 600, 600, 600]);
});

test('a plan over the 600-frame cap is refused', () => {
  const ts = Array.from({length: 48}, (_, i) => i + 1);
  assert.throws(() => sp.planFrames({timestamps: ts, timing: 'equal', frameMs: 5000, endHoldMs: 0, fps: 30}),
    /exceeds frame cap/);
});

test('step deadline is the step budget capped by the phase cutoff', () => {
  assert.strictEqual(sp.stepDeadline(1000, 100000, 15000), 16000);
  assert.strictEqual(sp.stepDeadline(1000, 5000, 15000), 5000);
  assert.throws(() => sp.stepDeadline(5000, 5000, 15000), /radar step did not settle/);
  assert.throws(() => sp.stepDeadline(6000, 5000, 15000), /radar step did not settle/);
  assert.doesNotThrow(() => sp.checkDeadline(10, 11));
  assert.throws(() => sp.checkDeadline(11, 11), /radar step did not settle/);
});

test('a stalled awaited call fails with the step error within the bound', async () => {
  // A never-resolving status/timestamp call (a stalled page).
  const start = Date.now();
  await assert.rejects(sp.withinDeadline(new Promise(() => {}), start + 80), /radar step did not settle/);
  const took = Date.now() - start;
  assert.ok(took >= 70 && took < 1000, String(took));
  // Already expired: rejects at once, and a late rejection of the loser is observed.
  await assert.rejects(sp.withinDeadline(Promise.reject(new Error('late')), Date.now() - 1), /did not settle/);
  // Fast calls pass through, including their own failures.
  assert.strictEqual(await sp.withinDeadline(Promise.resolve(7), Date.now() + 1000), 7);
  await assert.rejects(sp.withinDeadline(Promise.reject(new Error('boom')), Date.now() + 1000), /boom/);
});

test('quiescence requires timestamp, tiles, data, overlay and no inflight request', () => {
  const ok = {timestamp: 5, tilesPending: 0, noData: false, overlay: 'radar'};
  assert.ok(sp.stepQuiet(ok, 5, 'radar', 0));
  assert.ok(sp.stepQuiet({...ok, overlay: 'x'}, 5, null, 0));
  assert.ok(!sp.stepQuiet(null, 5, 'radar', 0));
  assert.ok(!sp.stepQuiet({...ok, timestamp: 4}, 5, 'radar', 0));
  assert.ok(!sp.stepQuiet({...ok, tilesPending: 1}, 5, 'radar', 0));
  assert.ok(!sp.stepQuiet({...ok, noData: true}, 5, 'radar', 0));
  assert.ok(!sp.stepQuiet({...ok, overlay: 'wind'}, 5, 'radar', 0));
  assert.ok(!sp.stepQuiet(ok, 5, 'radar', 1));
});

test('settling needs continuous quiescence and restarts on activity', () => {
  const tr = sp.settleTracker(300);
  // Old network activity (before the window) never satisfies settling immediately.
  assert.ok(!tr.observe(1000, true, 0));
  assert.ok(!tr.observe(1200, true, 0));
  // A tile starting mid-window breaks quiescence and restarts the window.
  assert.ok(!tr.observe(1250, false, 0));
  assert.ok(!tr.observe(1300, true, 0));
  assert.ok(!tr.observe(1550, true, 0));
  assert.ok(tr.observe(1600, true, 0));
  // Network activity inside the window pushes it forward.
  const tn = sp.settleTracker(300);
  assert.ok(!tn.observe(0, true, 0));
  assert.ok(!tn.observe(300, true, 200));
  assert.ok(tn.observe(500, true, 200));
  tn.reset();
  assert.ok(!tn.observe(600, true, 200));
});
