// Unit tests of the receiver's probe.js (ADR-0114 slice 2), the file the engine embeds, loaded in a bare context:
//   node test-probe-unit.mjs
// Covers the mapping tests of decision 2 "Tests" that apply to HLS and the clock: the offset estimate (best of 8,
// error bound), an epoch step between two clock calls discards samples, a clock error above 250 ms makes latency
// unavailable, no requestVideoFrameCallback gives the playhead method, the 0..60 s sanity rule, windows of 5 s
// from 3 s after the first frame; plus the codec parse, the failure memory and the ES5-only rule for every receiver
// script (an unknown syntax form stops the whole script on old Chromecast firmware).
// Exit code = number of failed checks. No browser, no network.
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const dir = path.resolve(here, '..', '..', 'crates', 'multiview-control', 'src', 'cast_receiver');
const require = createRequire(import.meta.url);
let failed = 0; let passed = 0; const failedNames = [];
function check(name, ok, detail = '') {
  if (ok) { passed += 1; } else { failed += 1; failedNames.push(name); }
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${ok ? '' : `  ${detail}`}`);
}

function load(extra = {}) {
  const sandbox = { console: { info() {} }, setInterval, clearInterval, setTimeout, clearTimeout, Date, Math, JSON, ...extra };
  vm.createContext(sandbox);
  vm.runInContext(fs.readFileSync(path.join(dir, 'probe.js'), 'utf8'), sandbox, { filename: 'probe.js' });
  return sandbox.ConspectProbe;
}
const P = load();
check('probe.js defines ConspectProbe and starts nothing on load', P && typeof P.start === 'function' && P.transport() === null);

// ---- clock offset (NTP four timestamps)
{
  const c = new P.Clock();
  // Engine clock 1 000 000 us ahead; one-way 20 ms, engine holds 1 ms: offset 1e6, error 20 ms.
  c.add(0, 1_020_000, 1_021_000, 41_000, 7);
  const u = c.usable();
  check('offset = ((t1-t0)+(t2-t3))/2', u && u.offset_us === 1_000_000, JSON.stringify(u));
  check('error = ((t3-t0)-(t2-t1))/2', u && u.error_us === 20_000, JSON.stringify(u));
  // Asymmetric slow sample: larger error, must not replace the best.
  c.add(100_000, 1_300_000, 1_300_100, 400_000, 7);
  check('best of the samples is the smallest error', c.usable().error_us === 20_000);
  for (let i = 0; i < 8; i++) c.add(i * 1e6, i * 1e6 + 1_200_000, i * 1e6 + 1_200_000, i * 1e6 + 400_000, 7);
  check('only the last 8 samples are kept', c.samples.length === 8 && c.usable().error_us === 200_000, JSON.stringify(c.usable()));
  const bad = new P.Clock();
  bad.add(0, 1_000_000, 1_000_000, 600_000, 1);   // error 300 ms
  check('a best error above 250 ms is unavailable', bad.usable() === null && bad.best().error === 300_000);
  const edge = new P.Clock();
  edge.add(0, 1_000_000, 1_000_000, 500_000, 1);  // error exactly 250 ms
  check('an error of exactly 250 ms is still usable', edge.usable() !== null);
  const step = new P.Clock();
  step.add(0, 10, 10, 20, 7);
  const stepped = step.add(1e6, 1e6 + 10, 1e6 + 10, 1e6 + 20, 8);
  check('an epoch_generation change is reported and drops the old samples', stepped === true && step.samples.length === 1);
  check('the same generation is not a step', step.add(2e6, 2e6 + 10, 2e6 + 10, 2e6 + 20, 8) === false);
}

// ---- latency windows
const clock = { offset_us: 1_000_000, error_us: 4_000 };
function feed(lat, fromUs, toUs, everyUs, latencyMs, c = clock) {
  let out = [];
  for (let t = fromUs; t <= toUs; t += everyUs) {
    // frame shown at receiver time t: engine time t + offset; its engine wall time is latency earlier.
    const r = lat.sample(t, t + (c ? c.offset_us : 0) - latencyMs * 1000, c);
    if (r) out.push(r);
  }
  return out;
}
{
  const lat = new P.Latency('frame');
  lat.reset(10_000_000);
  const early = feed(lat, 10_000_000, 12_999_000, 33_000, 700);
  check('no sample in the 3 s after the first frame', early.length === 0 && lat.windowStartUs === null);
  const windows = feed(lat, 13_000_000, 23_100_000, 33_000, 700);
  check('one window per 5 s', windows.length === 2, windows.length);
  check('median, p95, samples, method, clock error', windows[0].median === 700 && windows[0].p95 === 700 && windows[0].samples > 140 && windows[0].method === 'frame' && windows[0].clock_error_ms === 4, JSON.stringify(windows[0]));
  check('the last window is what the report carries', JSON.stringify(lat.last) === JSON.stringify(windows[1]));
}
{
  const lat = new P.Latency('frame');
  lat.reset(0);
  const w = [];
  for (let i = 0; i < 100; i++) { const r = lat.sample(3_000_000 + i * 51_000, 3_000_000 + i * 51_000 + clock.offset_us - (i + 1) * 1000, clock); if (r) w.push(r); }
  check('percentiles over the window (1..99 ms)', w.length === 1 && w[0].median === 50 && w[0].p95 === 95, JSON.stringify(w));
}
{
  const lat = new P.Latency('frame');
  lat.reset(0);
  for (let i = 0; i < 5; i++) lat.sample(3_000_000 + i * 1000, 3_000_000 + i * 1000 + clock.offset_us + 5_000_000, clock);  // negative latency
  lat.sample(3_010_000, 3_010_000 + clock.offset_us - 70_000_000, clock);   // 70 s: invalid, the 6th
  check('more than 5 invalid samples in a window: unavailable for the rest of the load', lat.method === 'unavailable' && lat.locked && lat.last === null);
  const after = feed(lat, 3_100_000, 20_000_000, 33_000, 700);
  check('no figure after that, even for good samples', after.length === 0 && lat.last === null);
}
{
  const lat = new P.Latency('frame');
  lat.reset(0);
  for (let i = 0; i < 5; i++) lat.sample(3_000_000 + i * 1000, 3_000_000 + clock.offset_us + 1e9, clock);
  const w = feed(lat, 3_100_000, 8_200_000, 33_000, 500);
  check('5 invalid samples are tolerated', lat.method === 'frame' && w.length === 1 && w[0].median === 500);
}
{
  const lat = new P.Latency('frame');
  lat.reset(0);
  const w = feed(lat, 3_000_000, 9_000_000, 33_000, 500, null);
  check('without a usable clock no window carries a figure', w.length === 0 && lat.last === null);
}
{
  const lat = new P.Latency('frame');
  lat.reset(0);
  const w = feed(lat, 3_000_000, 9_000_000, 33_000, 500);
  check('a window was reported before the step', w.length === 1 && lat.last !== null);
  lat.reset(9_000_000);   // the epoch stepped: samples discarded, the warm-up restarts
  check('an epoch step discards the samples, the last figure and restarts the warm-up', lat.values.length === 0 && lat.startUs === 12_000_000 && lat.last === null);
}
check('rVFC present: method frame', P.methodFor(true, true) === 'frame');
check('no requestVideoFrameCallback: method playhead', P.methodFor(false, true) === 'playhead');
check('no absolute-time mapping: method unavailable', P.methodFor(true, false) === 'unavailable');
{
  const pm = { getShakaPlayer: () => ({ getPlayheadTimeAsDate: () => new Date(1794898722866) }) };
  const v = { currentTime: 71.5 };
  check('frame wall time = playhead date - (currentTime - mediaTime)', Math.abs(P.wallUsForMediaTime(pm, v, 71.4) - (1794898722.866 - 0.1) * 1e6) < 1);
  check('no player, no date or no element: no wall time', P.wallUsForMediaTime({}, v, 1) === null && P.wallUsForMediaTime({ getShakaPlayer: () => ({ getPlayheadTimeAsDate: () => null }) }, v, 1) === null && P.wallUsForMediaTime(pm, null, 1) === null && P.wallUsForMediaTime(pm, v, NaN) === null);
}
check('an unavailable method never measures', new P.Latency('unavailable').locked === true);

// ---- transport report when the clock is unavailable
{
  const xhrs = [];
  function FakeXhr() { this.headers = {}; xhrs.push(this); }
  FakeXhr.prototype.open = function (m, u) { this.method = m; this.url = u; };
  FakeXhr.prototype.setRequestHeader = function (k, v) { this.headers[k] = v; };
  FakeXhr.prototype.send = function (b) { this.body = JSON.parse(b); };
  let t = 1000;
  const perf = { timeOrigin: 1.7e12, now: () => t };
  const Q = load({ XMLHttpRequest: FakeXhr, performance: perf, navigator: { userAgent: 'test CrKey' } });
  const video = { paused: false, readyState: 4, currentTime: 0, getVideoPlaybackQuality: () => ({ totalVideoFrames: 1 }) };
  const pm = { getShakaPlayer: () => ({ getPlayheadTimeAsDate: () => new Date(1.8e12) }) };
  const master = '#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1,CODECS="avc1.640028,mp4a.40.2",RESOLUTION=1280x720\nvideo.m3u8\n';
  Q.start({ base: 'https://e.example/cast/TOKEN/', master, video: () => video, playerManager: pm, storage: null, env: { navigator: { userAgent: 'test CrKey' } } });
  const call = xhrs[0];
  check('start() posts the probe to <base>ladder', call && call.method === 'POST' && call.url === 'https://e.example/cast/TOKEN/ladder' && call.headers['Content-Type'] === 'application/json');
  const caps = call && call.body.caps;
  check('a bare environment probes absent/false and the quality counter', caps && caps.wt === false && caps.mse === false && caps.vdec === 'absent' && caps.adec === 'absent' && caps.rtc === false && caps.rvfc === false && caps.frames === 'quality' && caps.srt === null && Array.isArray(caps.rtc_h264), JSON.stringify(caps));
  check('the probe carries v, receiver, need, skip, failed and clock.t0', call && call.body.v === 1 && call.body.receiver.kind === 'web' && call.body.failed === null && typeof call.body.clock.t0 === 'number' && JSON.stringify(call.body.skip) === '{}');
  check('the body fits 2 KiB', call && JSON.stringify(call.body).length < 2048);
  t = 1040;
  call.status = 200;
  call.responseText = JSON.stringify({ v: 1, clock: null, attach: { rung: 'hls' }, then: [], srt: null });
  call.onload();
  const tr = Q.transport();
  check('engine clock null: method unavailable and no latency figure', tr.rung === 'hls' && tr.latency_method === 'unavailable' && tr.latency_ms === null && tr.clock_error_ms === null, JSON.stringify(tr));
  // A usable clock, but the player gives no date-time mapping for any frame: still unavailable, no figure.
  const xhrs2 = xhrs.length;
  t = 2000;
  Q.start({ base: 'https://e.example/cast/TOKEN/', master, video: () => video, playerManager: { getShakaPlayer: () => ({ getPlayheadTimeAsDate: () => null }) }, storage: null, env: { navigator: { userAgent: 'x' } } });
  const c2 = xhrs[xhrs2];
  c2.status = 200;
  const t0 = c2.body.clock.t0;
  c2.responseText = JSON.stringify({ v: 1, clock: { source: 'epoch', epoch_generation: 1, media_us: 1, wall_unix_us: 1, t1_unix_us: t0 + 1000, t2_unix_us: t0 + 1100 }, attach: { rung: 'hls' }, then: [], srt: null });
  t = 2002; c2.onload();
  await new Promise((r) => setTimeout(r, 350));   // the playhead sampler runs (no rVFC): frames without a mapping
  video.currentTime = 1; await new Promise((r) => setTimeout(r, 250));
  const tr2 = Q.transport();
  check('a usable clock but no frame mapping: method unavailable, no figure', tr2.clock_error_ms !== null && tr2.latency_method === 'unavailable' && tr2.latency_ms === null, JSON.stringify(tr2));
  Q.stop();
  check('stop() ends the load', Q.transport() === null);
}

// ---- frame watchdog (Cast media status, rule 1)
{
  check('presented = total - dropped', P.presentedFrames({ totalVideoFrames: 100, droppedVideoFrames: 30 }) === 70 && P.presentedFrames(null) === null);
  let nowMs = 0; const ticks = [];
  const W = load({ performance: { timeOrigin: 0, now: () => nowMs } });
  const timers = { setInterval: (fn) => { ticks.push(fn); return 1; } };
  const video = { paused: false, readyState: 4, q: { totalVideoFrames: 0, droppedVideoFrames: 0 }, getVideoPlaybackQuality() { return this.q; } };
  const changes = [];
  const wd = new W.FrameWatchdog(() => video, (stalled) => changes.push(stalled), null, timers);
  const step = (ms, total, dropped) => { nowMs += ms; video.q = { totalVideoFrames: total, droppedVideoFrames: dropped }; ticks[0](); };
  check('a video without rVFC uses the quality counter', wd.counter === 'quality');
  step(250, 10, 0); step(250, 20, 0); step(1000, 50, 0);
  check('frames presented: not stalled', !wd.stalled() && changes.length === 0);
  // Every new frame is dropped: totalVideoFrames keeps rising, presented does not.
  for (let i = 0; i < 9; i++) step(250, 60 + i * 10, 10 + i * 10);
  check('a decoder that drops every frame fires the watchdog (total - dropped)', wd.stalled() && changes[0] === true, JSON.stringify(changes));
  step(250, 200, 100);
  check('frames presented again clear it', !wd.stalled() && changes[1] === false);
  // The element restarted its counts (a new load in CAF): a lower value is a new count, not progress.
  step(1000, 3, 0);
  for (let i = 0; i < 4; i++) step(250, 3, 0);
  check('a lower value starts a new count and is not progress', wd.stalled() && changes[2] === true, JSON.stringify(changes));
  step(250, 4, 0);
  check('progress on the new count clears it', !wd.stalled() && changes[3] === false);
  // Buffer starvation (readyState drops) while playing is a stall.
  video.readyState = 1;
  for (let i = 0; i < 9; i++) step(250, 4, 0);
  check('buffer starvation (readyState < 2, not paused) is a stall', wd.stalled());
  // Pausing clears it and never stalls.
  video.paused = true;
  step(250, 4, 0);
  check('pausing clears a stall', !wd.stalled() && changes[changes.length - 1] === false);
  for (let i = 0; i < 12; i++) step(250, 4, 0);
  check('a paused element is never stalled', !wd.stalled());
  wd.stop();
  // Before the first presented frame of a load the watchdog does not fire (the first-frame deadline applies).
  const v2 = { paused: false, readyState: 1, q: { totalVideoFrames: 0, droppedVideoFrames: 0 }, getVideoPlaybackQuality() { return this.q; } };
  const t2 = []; const c2 = [];
  const w2 = new W.FrameWatchdog(() => v2, (x) => c2.push(x), null, { setInterval: (fn) => { t2.push(fn); return 2; } });
  for (let i = 0; i < 20; i++) { nowMs += 250; t2[0](); }
  check('no stall before the first frame of the load', !w2.stalled() && c2.length === 0);
  w2.stop();
}

// ---- one receiver clock, also without performance.timeOrigin (old Chrome)
{
  const Old = load({ performance: { timing: { navigationStart: 1.7e12 }, now: () => 5000 } });
  check('without timeOrigin the clock uses navigationStart', Old.monoUs() === (1.7e12 + 5000) * 1000 && Old.originMs() === 1.7e12);
  const None = load({});
  check('without performance.now there is no receiver clock', None.monoUs() === null);
}

// ---- capabilities parsing and failure memory
{
  const c = P.codecsFromMaster('#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=3000000,CODECS="avc1.640028,mp4a.40.2",RESOLUTION=1920x1080,FRAME-RATE=29.970\nvideo.m3u8\n');
  check('codec strings and size from master.m3u8', c.video === 'avc1.640028' && c.audio === 'mp4a.40.2' && c.width === 1920 && c.height === 1080 && c.all.length === 2);
  const env = { RTCRtpReceiver: { getCapabilities: (k) => (k === 'video' ? { codecs: Array.from({ length: 12 }, (_, i) => ({ mimeType: 'video/H264', sdpFmtpLine: `level-asymmetry-allowed=1;profile-level-id=${(0x42e01f + i).toString(16)}` })).concat([{ mimeType: 'video/H264', sdpFmtpLine: 'profile-level-id=42E01F' }, { mimeType: 'video/VP8' }]) } : null) } };
  const profiles = P.h264Profiles(env);
  check('rtc_h264: at most 8, lower case, unique', profiles.length === 8 && profiles[0] === '42e01f' && new Set(profiles).size === 8, JSON.stringify(profiles));
  const store = { v: {}, getItem(k) { return this.v[k] || null; }, setItem(k, v) { this.v[k] = v; } };
  P.rememberFailure(store, 'moq', 600, 1_000_000);
  P.rememberFailure(store, 'hls', 600, 1_000_000);
  check('failure memory: seconds left, HLS never remembered', JSON.stringify(P.readSkip(store, 1_060_000)) === '{"moq":540}');
  check('failure memory expires', JSON.stringify(P.readSkip(store, 1_700_000)) === '{}');
  check('failure memory survives a broken store', JSON.stringify(P.readSkip({ getItem() { throw new Error('denied'); } }, 0)) === '{}');
}

// ---- decoder checks when the APIs exist
await new Promise((resolve) => {
  const env = {
    WebTransport: function () {}, RTCPeerConnection: function () {},
    MediaSource: { isTypeSupported: (t) => t === 'video/mp4; codecs="avc1.640028,mp4a.40.2"' },
    VideoDecoder: { isConfigSupported: (c) => Promise.resolve({ supported: c.hardwareAcceleration !== 'prefer-hardware' }) },
    AudioDecoder: { isConfigSupported: () => Promise.reject(new Error('no')) },
    RTCRtpReceiver: { getCapabilities: (k) => (k === 'audio' ? { codecs: [{ mimeType: 'audio/opus' }] } : { codecs: [] }) }
  };
  const video = { requestVideoFrameCallback() {} };
  P.detectCaps(env, P.codecsFromMaster('CODECS="avc1.640028,mp4a.40.2",RESOLUTION=1280x720'), video, (caps) => {
    check('caps with every API present', caps.wt === true && caps.mse === true && caps.vdec === 'supported' && caps.vdec_hw === false && caps.adec === 'unsupported' && caps.rtc === true && caps.rtc_opus === true && caps.rvfc === true && caps.frames === 'rvfc', JSON.stringify(caps));
    resolve();
  });
});

// ---- ES5 only, for every script a Cast device parses
{
  let esprima = null;
  try { esprima = require('esprima'); } catch { /* checked */ }
  check('esprima is available (npm ci --prefix tools/cast-proof)', !!esprima);
  const ES6 = new Set(['ArrowFunctionExpression', 'TemplateLiteral', 'TaggedTemplateExpression', 'ClassDeclaration', 'ClassExpression', 'SpreadElement', 'RestElement', 'ForOfStatement', 'ObjectPattern', 'ArrayPattern', 'AssignmentPattern', 'AwaitExpression', 'YieldExpression', 'MetaProperty', 'Super', 'ImportDeclaration', 'ExportNamedDeclaration']);
  for (const file of ['probe.js', 'receiver.js']) {
    if (!esprima) break;
    const found = [];
    const src = fs.readFileSync(path.join(dir, file), 'utf8');
    let ast = null;
    try { ast = esprima.parseScript(src, { range: true }); } catch (e) { found.push(`parse: ${e.message}`); }
    const walk = (n) => {
      if (!n || typeof n.type !== 'string') return;
      if (ES6.has(n.type)) found.push(n.type);
      if (n.type === 'VariableDeclaration' && n.kind !== 'var') found.push(n.kind);
      if ((n.type === 'FunctionDeclaration' || n.type === 'FunctionExpression') && (n.async || n.generator)) found.push('async/generator');
      if (n.type === 'Property' && (n.shorthand || n.method || n.computed)) found.push('ES6 property');
      if (n.type === 'Literal' && n.regex && /[uy]/.test(n.regex.flags)) found.push('regex flag');
      for (const k of Object.keys(n)) { const v = n[k]; if (Array.isArray(v)) v.forEach(walk); else if (v && typeof v === 'object' && k !== 'range') walk(v); }
    };
    if (ast) walk(ast);
    check(`${file} is ES5 syntax only`, found.length === 0, found.slice(0, 5).join(', '));
  }
}

console.log(`RESULT ${JSON.stringify({ test: 'probe-unit', pass: failed === 0, passed, failed: failedNames, parts: [] })}`);
process.exit(failed);
