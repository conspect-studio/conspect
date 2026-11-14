// Test 1: Shaka Player 4.15.56 (node_modules; the version and build CAF loads) in headless Chrome, playing a live
// HLS URL with (a) the Shaka config CAF 3.0.0157 applies by default and (b) CAF's config with our receiver's
// playbackConfig (read from the crate's cast_receiver/receiver.js, so the two cannot drift apart).
//   node test-shaka.mjs <masterUrl> [--seconds 60] [--configs caf-default,receiver] [--label name] [--custom <json>]
import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import { RECEIVER_DIR } from './server.mjs';
import { parseArgs, launchBrowser, watchPage, installSampler, evaluatePlayback, evaluateLive, check, finish, save, sleep, waitUntil, getJson, runFor, here } from './lib.mjs';

const args = parseArgs(process.argv.slice(2), { seconds: 60, configs: 'caf-default,receiver', label: 'shaka' });
const masterUrl = args._[0];
if (!masterUrl) { console.error('usage: test-shaka.mjs <masterUrl> [--seconds 60]'); process.exit(2); }
const seconds = Number(args.seconds);
const hardStop = setTimeout(() => { console.error('hard timeout'); process.exit(3); }, (seconds + 90) * 1000);

// What CAF 3.0.0157 passes to shaka.Player.configure for Shaka >= 4.15 and < 5 (read from the SDK source):
// retry 4 x 400 ms x 2, 30 s timeout; rebufferingGoal = autoResumeDuration || 10; stallThreshold 5;
// defaultBandwidthEstimate = initialBandwidth || 2e6. `jumpLargeGaps` is only set for Shaka < 4.0.0 (the option
// no longer exists in Shaka 4: gaps are always jumped), `availabilityWindowOverride` only for Shaka < 3.1.
function cafConfig(playbackConfig) {
  const retry = { maxAttempts: 4, baseDelay: 400, backoffFactor: 2, timeout: 30000 };
  const base = {
    abr: { defaultBandwidthEstimate: playbackConfig.initialBandwidth || 2e6 },
    manifest: { retryParameters: { ...retry } },
    streaming: { rebufferingGoal: playbackConfig.autoResumeDuration || 10, retryParameters: { ...retry }, stallThreshold: 5, dispatchAllEmsgBoxes: true },
    mediaSource: { codecSwitchingStrategy: 'reload' }
  };
  const merge = (into, from) => { for (const k of Object.keys(from)) { if (into[k] && typeof into[k] === 'object' && from[k] && typeof from[k] === 'object') merge(into[k], from[k]); else into[k] = from[k]; } return into; };
  return playbackConfig.shakaConfig ? merge(base, JSON.parse(JSON.stringify(playbackConfig.shakaConfig))) : base;
}

function receiverPlaybackConfig() {
  const src = fs.readFileSync(path.join(RECEIVER_DIR, 'receiver.js'), 'utf8');
  const start = src.indexOf('var PRESENTATION_DELAY_S = '); const end = src.indexOf('};', src.indexOf('var SHAKA_CONFIG = ')) + 2;
  if (start < 0 || end < 2) throw new Error('cannot find SHAKA_CONFIG in receiver.js');
  const shakaConfig = new Function(src.slice(start, end) + '\nreturn SHAKA_CONFIG;')();
  const numberOf = (name) => { const m = new RegExp('playbackConfig\\.' + name + ' = ([0-9.e]+);').exec(src); if (!m) throw new Error(`cannot find playbackConfig.${name}`); return Number(m[1]); };
  return { autoResumeDuration: numberOf('autoResumeDuration'), initialBandwidth: numberOf('initialBandwidth'), shakaConfig };
}

const CONFIGS = { 'caf-default': () => cafConfig({}), receiver: () => cafConfig(receiverPlaybackConfig()) };
// Experiments: --custom '<JSON playbackConfig: {autoResumeDuration, initialBandwidth, shakaConfig}>' adds a config
// named "custom" (CAF's config with that playbackConfig), selected with --configs custom.
// --fault '{"at":12,"mode":"stall","seconds":4}' posts that fault to the harness server `at` seconds after the load.
const fault = args.fault && args.fault !== 'true' ? JSON.parse(String(args.fault)) : null;
if (args.custom && args.custom !== 'true') { const pc = JSON.parse(String(args.custom)); CONFIGS.custom = () => cafConfig(pc); }

const shakaJs = fs.readFileSync(path.join(here, 'node_modules', 'shaka-player', 'dist', 'shaka-player.compiled.js'));
const html = '<!doctype html><meta charset="utf-8"><body style="margin:0;background:#000"><video id="v" autoplay style="width:1280px;height:720px"></video><script src="/shaka.js"></script><script src="/run.js"></script>';
const runJs = `(${async function run() {
  const cfg = await (await fetch('/config' + location.search)).json();
  const R = window.__run = { loadAt: 0, loaded: false, loadMs: null, live: null, errors: [], buffering: [], stalls: 0, gapEvents: 0, version: shaka.Player.version, configureOk: null };
  shaka.polyfill.installAll();
  const v = document.getElementById('v');
  const p = window.__player = new shaka.Player();
  await p.attach(v);
  R.configureOk = p.configure(cfg.config);
  p.addEventListener('error', (e) => R.errors.push({ at: Date.now(), code: e.detail && e.detail.code, category: e.detail && e.detail.category, severity: e.detail && e.detail.severity, data: String(e.detail && e.detail.data).slice(0, 200) }));
  p.addEventListener('buffering', (e) => R.buffering.push({ at: Date.now(), on: e.buffering }));
  p.addEventListener('stalldetected', () => { R.stalls++; });
  p.addEventListener('gapjumped', () => { R.gapEvents++; });
  window.__probeExtra = () => { const s = p.getStats(); const r = p.seekRange(); return { latency: s.liveLatency, seekLen: r.end - r.start, stalls: s.stallsDetected, gaps: s.gapsJumped, buffering: p.isBuffering(), estimate: s.estimatedBandwidth }; };
  R.loadAt = Date.now();
  try {
    await p.load(cfg.url, null, 'application/vnd.apple.mpegurl');
    R.loaded = true; R.loadMs = Date.now() - R.loadAt; R.live = p.isLive();
    v.play().catch(() => {});
  } catch (e) { R.errors.push({ at: Date.now(), code: e && e.code, category: e && e.category, severity: e && e.severity, data: String(e && e.data || e).slice(0, 200), load: true }); }
}.toString()})();`;

async function runOne(name, origin) {
  const browser = await launchBrowser();
  try {
    const page = await browser.newPage();
    const log = await watchPage(page);
    await page.evaluateOnNewDocument(installSampler);
    await page.goto(`${origin}/?c=${name}`, { waitUntil: 'load', timeout: 30000 });
    const loadAt = await waitUntil(() => page.evaluate(() => window.__run && window.__run.loadAt), 15000, 'the Shaka load to begin');
    if (fault) {                                           // experiment: one network hiccup during the run
      await sleep(Math.max(0, loadAt + fault.at * 1000 - Date.now()));
      const u = new URL(masterUrl);
      await getJson(`${u.origin}/_fault`, 'POST', { ...fault, token: u.pathname.split('/')[2] });
    }
    const probe = await runFor(page, loadAt, seconds);
    const run = await page.evaluate(() => window.__run);
    const end = probe.end.at;
    // LAN constraint for our own config: picture under 3 s. CAF's defaults are only held to the 10 s criterion.
    const play = evaluatePlayback(probe, loadAt, seconds, { startLimitMs: name === 'caf-default' ? 10000 : 3000 });
    const live = evaluateLive(play.samples, (x) => x.extra || {}, { designWindow: name === 'receiver' });
    const started = probe.startedAt || 0;
    // buffering=true time: before the picture first moves it is the start delay; after it, a rebuffer.
    let initial = 0; let rebuffer = 0;
    const spells = run.buffering.filter((b, i, a) => i === 0 || b.on !== a[i - 1].on);     // drop repeated states
    for (let i = 0; i < spells.length; i++) {
      if (!spells[i].on) continue;
      const from = spells[i].at; const to = spells[i + 1] ? spells[i + 1].at : end;
      if (!started) initial += to - from;
      else if (to <= started + 250) initial += to - from;
      else if (from >= started) rebuffer += to - from;
      else { initial += started - from; rebuffer += Math.max(0, to - started - 250); }
    }
    const firstPlaying = probe.events.find((e) => e.type === 'playing');
    const shakaErrors = run.errors;
    const gaps = Math.max(run.gapEvents, ...play.samples.map((x) => (x.extra && x.extra.gaps) || 0));
    const checks = [
      check('Shaka version is 4.15.56', /^v?4\.15\.56/.test(run.version), run.version, '4.15.56'),
      check('load succeeds', run.loaded, run.loaded ? `${run.loadMs} ms` : JSON.stringify(shakaErrors[0] || null), 'loaded'),
      check('isLive()', run.live === true, run.live, 'true'),
      ...play.checks,
      check('zero stalldetected', run.stalls === 0, run.stalls, '0'),
      check('zero Shaka errors', shakaErrors.length === 0, shakaErrors.length ? JSON.stringify(shakaErrors.slice(0, 2)) : 0, '0'),
      check('buffering=true under 1 s after playback start', started && rebuffer < 1000, rebuffer, '< 1000 ms'),
      check('gapsJumped <= 1 (the initial one)', gaps <= 1, gaps, '<= 1'),
      ...live.checks,
      check('zero page errors', log.pageErrors.length === 0, log.pageErrors.length ? log.pageErrors[0].slice(0, 200) : 0, '0')
    ];
    const metrics = { load_ms: run.loadMs, ...play.metrics, ...live.metrics, initial_buffering_ms: initial, rebuffering_ms: rebuffer,
      playing_event_ms: firstPlaying ? firstPlaying.at - loadAt : null, stalls: run.stalls, gaps, errors: shakaErrors.length, configure_ok: run.configureOk };
    save(`${args.label}-${name}.detail.json`, { metrics, config: CONFIGS[name](), run, events: probe.events.map((e) => [e.at - loadAt, e.type, Math.round(e.ct * 100) / 100]), samples: play.samples, console: log.console.slice(0, 200) });
    return { name, checks, metrics };
  } finally { await browser.close().catch(() => {}); }
}

let failures = 1; let server;
try {
  server = http.createServer((req, res) => {
    const u = new URL(req.url, 'http://x');
    if (u.pathname === '/') { res.writeHead(200, { 'content-type': 'text/html' }); return res.end(html); }
    if (u.pathname === '/shaka.js') { res.writeHead(200, { 'content-type': 'text/javascript' }); return res.end(shakaJs); }
    if (u.pathname === '/run.js') { res.writeHead(200, { 'content-type': 'text/javascript' }); return res.end(runJs); }
    if (u.pathname === '/config') { res.writeHead(200, { 'content-type': 'application/json' }); return res.end(JSON.stringify({ url: masterUrl, config: CONFIGS[u.searchParams.get('c')]() })); }
    res.writeHead(404); res.end();
  });
  await new Promise((resolve) => server.listen(0, '::', resolve));
  const origin = `http://localhost:${server.address().port}`;
  const names = String(args.configs).split(',').filter((n) => CONFIGS[n]);
  // One browser per config, side by side (same stream moment, so the two columns compare like with like).
  // --sequential runs them one after the other.
  const parts = [];
  if (args.sequential) for (const name of names) parts.push(await runOne(name, origin));
  else parts.push(...await Promise.all(names.map((name) => runOne(name, origin))));
  failures = finish('test-shaka', parts, { masterUrl: masterUrl.replace(/\/cast\/([^/]{6})[^/]*/, '/cast/$1...'), seconds, label: args.label });
} catch (e) {
  console.error('test-shaka failed:', e && e.stack || e);
  console.log('RESULT ' + JSON.stringify({ test: 'test-shaka', pass: false, failed: [String(e && e.message || e)] }));
} finally {
  clearTimeout(hardStop);
  if (server) { server.closeAllConnections(); server.close(); }
}
process.exit(failures ? 1 : 0);
