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
  feed(lat, 3_000_000, 4_000_000, 33_000, 500);
  lat.reset(4_000_000);   // the epoch stepped: samples discarded, the warm-up restarts
  check('an epoch step discards the samples and restarts the warm-up', lat.values.length === 0 && lat.startUs === 7_000_000);
}
check('rVFC present: method frame', P.methodFor(true, true) === 'frame');
check('no requestVideoFrameCallback: method playhead', P.methodFor(false, true) === 'playhead');
check('no absolute-time mapping: method unavailable', P.methodFor(true, false) === 'unavailable');
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
  const pm = { getAbsoluteTimeForMediaTime: (m) => 1.8e9 + m };
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
  Q.stop();
  check('stop() ends the load', Q.transport() === null);
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
