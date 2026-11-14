'use strict';
// Pure step-loop planning for radar "steps" capture. No I/O, no browser: the
// host feeds it timestamps and screenshot hashes; tests run it under node --test.
// All duration -> frame-count arithmetic is integer (never float fps).
const LIMITS = Object.freeze({
  frameMs: [100, 5000], endHoldMs: [0, 10000], maxSteps: [2, 48], fps: [1, 30], maxFrames: 600,
});
const TIMINGS = ['equal', 'proportional'];
const inRange = (v, [lo, hi]) => Number.isSafeInteger(v) && v >= lo && v <= hi;
// Frames for a cumulative duration: round(ms * fps / 1000), half up, integers only.
const framesAt = (ms, fps) => Math.floor((ms * fps + 500) / 1000);

function validateOptions(o, fps) {
  if (!o || typeof o !== 'object' || !TIMINGS.includes(o.timing)
      || !inRange(o.frame_ms, LIMITS.frameMs) || !inRange(o.end_hold_ms, LIMITS.endHoldMs)
      || !inRange(o.max_steps, LIMITS.maxSteps) || !inRange(fps, LIMITS.fps))
    throw new Error('invalid capture limits');
  // Nominal (equal-timing) size must fit the frame cap; the host re-checks the actual plan.
  const nominal = o.max_steps * Math.max(1, framesAt(o.frame_ms, fps)) + framesAt(o.end_hold_ms, fps);
  if (nominal > LIMITS.maxFrames) throw new Error('invalid capture limits');
  return o;
}

// Most recent `maxSteps` distinct, positive, integer timestamps in ascending order.
function selectSteps(timestamps, maxSteps) {
  if (!Array.isArray(timestamps) || !inRange(maxSteps, LIMITS.maxSteps))
    throw new Error('radar timeline unavailable');
  const valid = [...new Set(timestamps.filter(t => Number.isSafeInteger(t) && t > 0))].sort((a, b) => a - b);
  if (valid.length < 2) throw new Error('radar timeline unavailable');
  return valid.slice(-maxSteps);
}

// Drop a step whose hash equals the previous KEPT step's hash.
function dedupe(steps) {
  const kept = [];
  let dropped = 0;
  for (const s of steps) {
    if (kept.length && kept[kept.length - 1].hash === s.hash) { dropped++; continue; }
    kept.push(s);
  }
  if (kept.length < 2) throw new Error('radar loop has fewer than 2 distinct frames');
  return {kept, dropped};
}

// Per-step duration in integer ms. `proportional`: each step's share follows its
// gap to the next kept timestamp (the last step uses the mean gap), normalised so
// the mean is frameMs, then clamped to [ceil(frameMs/4), frameMs*4].
function durations(timestamps, timing, frameMs) {
  const n = timestamps.length;
  if (timing === 'equal' || n < 2) return timestamps.map(() => frameMs);
  const gaps = [];
  for (let i = 0; i + 1 < n; i++) gaps.push(timestamps[i + 1] - timestamps[i]);
  const span = gaps.reduce((a, b) => a + b, 0);
  gaps.push(Math.round(span / (n - 1)));
  const total = gaps.reduce((a, b) => a + b, 0);
  if (!(total > 0)) return timestamps.map(() => frameMs);
  const lo = Math.ceil(frameMs / 4), hi = frameMs * 4;
  return gaps.map(g => Math.min(hi, Math.max(lo, Math.round(frameMs * n * g / total))));
}

// Repeat counts for each kept step at constant `fps` (cumulative rounding, each >= 1),
// plus the end hold folded onto the last step.
function planFrames({timestamps, timing, frameMs, endHoldMs, fps}) {
  const ms = durations(timestamps, timing, frameMs);
  const counts = [];
  let cum = 0;
  for (const d of ms) {
    const before = framesAt(cum, fps);
    cum += d;
    counts.push(Math.max(1, framesAt(cum, fps) - before));
  }
  const holdFrames = framesAt(endHoldMs, fps);
  counts[counts.length - 1] += holdFrames;
  const total = counts.reduce((a, b) => a + b, 0);
  if (total < 2 || total > LIMITS.maxFrames) throw new Error('step loop exceeds frame cap');
  return {durations: ms, counts, holdFrames, total};
}

module.exports = {LIMITS, validateOptions, selectSteps, dedupe, durations, planFrames, framesAt};
