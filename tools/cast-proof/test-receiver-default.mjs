// Test 3: the Default-Media-Receiver equivalent. A bare page on a DIFFERENT origin than the stream (its own
// port) with only <cast-media-player> + context.start() (default options, no interceptor, no CSP), driven by the
// fake platform with the LOAD our sender sends. Decides whether the sender may fall back to Google's Default
// Media Receiver before the Conspect app id is registered.
//   node test-receiver-default.mjs <masterUrl> [--seconds 30] [--variants fmp4,none,ts] [--label name]
// Variant fmp4 (the sender's LOAD) must reach PLAYING and hold; `none` (no hls*SegmentFormat fields) and `ts`
// (hlsSegmentFormat 'ts', hlsVideoSegmentFormat 'mpeg2_ts') are informational unless listed in --require.
import http from 'node:http';
import { startFakePlatform } from './fake-cast-platform.mjs';
import { parseArgs, launchBrowser, watchPage, installSampler, evaluatePlayback, evaluateLive, statusChecks, senderMedia, redactUrl, check, finish, save, sleep } from './lib.mjs';

const args = parseArgs(process.argv.slice(2), { seconds: 30, variants: 'fmp4,none,ts', require: 'fmp4', label: 'default-receiver' });
const masterUrl = args._[0];
if (!masterUrl) { console.error('usage: test-receiver-default.mjs <masterUrl> [--seconds 30]'); process.exit(2); }
const seconds = Number(args.seconds);
const variants = String(args.variants).split(',');
const required = String(args.require).split(',');
const hardStop = setTimeout(() => { console.error('hard timeout'); process.exit(3); }, (variants.length * (seconds + 60) + 30) * 1000);

const MEDIA = {
  fmp4: (u) => senderMedia(u),
  none: (u) => { const m = senderMedia(u); delete m.hlsSegmentFormat; delete m.hlsVideoSegmentFormat; return m; },
  ts: (u) => senderMedia(u, { hlsSegmentFormat: 'ts', hlsVideoSegmentFormat: 'mpeg2_ts' })
};
const PAGE = `<!doctype html><html><head><meta charset="utf-8">
<script src="https://www.gstatic.com/cast/sdk/libs/caf_receiver/v3/cast_receiver_framework.js"></script></head>
<body><cast-media-player></cast-media-player><script src="/start.js"></script></body></html>`;
const START = `window.__err = [];
var ctx = cast.framework.CastReceiverContext.getInstance();
ctx.getPlayerManager().addEventListener(cast.framework.events.EventType.ERROR, function (e) {
  window.__err.push({ at: Date.now(), detailed: e.detailedErrorCode, severity: e.severity, reason: e.reason, shaka: e.error && e.error.shakaErrorCode, data: String(e.error && e.error.shakaErrorData).slice(0, 160) }); });
ctx.start();`;

async function runVariant(name, pageOrigin) {
  const platform = await startFakePlatform();
  const browser = await launchBrowser();
  try {
    const page = await browser.newPage();
    const log = await watchPage(page);
    await page.evaluateOnNewDocument(installSampler);
    await page.evaluateOnNewDocument(() => {
      window.__probeExtra = () => {
        const pm = cast.framework.CastReceiverContext.getInstance().getPlayerManager();
        const st = pm.getStats() || {}; const r = pm.getLiveSeekableRange();
        return { state: pm.getPlayerState(), latency: st.liveLatency, seekLen: r ? r.end - r.start : null, stalls: st.stallsDetected, gaps: st.gapsJumped };
      };
    });
    await page.goto(pageOrigin + '/', { waitUntil: 'load', timeout: 30000 });
    await platform.waitForReceiver(20000);
    await sleep(1500);                                   // let the SDK finish loading its player libraries
    const loadAt = Date.now();
    platform.load(MEDIA[name](masterUrl));
    let playing = null; let why = '';
    try { playing = await platform.waitForMediaStatus((s) => s.playerState === 'PLAYING', 15000, loadAt); } catch (e) { why = e.message; }
    // "Hold" is measured from the moment the picture moves: with CAF's default rebufferingGoal (10 s) the SDK
    // reports PLAYING, then BUFFERING while Shaka fills 10 s, then PLAYING. That start-up spell is reported as
    // startup_buffering_ms, and the run lasts `seconds` from the picture start (start bounded at 15 s).
    let startedAt = 0;
    for (const limit = loadAt + 15000; Date.now() < limit && !startedAt; await sleep(200)) startedAt = await page.evaluate(() => window.__probe.startedAt);
    await sleep(Math.max(0, (startedAt || loadAt) + seconds * 1000 - Date.now()));
    const end = Date.now();
    const got = await page.evaluate(() => ({ probe: { events: window.__probe.events, samples: window.__probe.samples, startedAt: window.__probe.startedAt }, errors: window.__err, shaka: window.shaka && shaka.Player.version }));
    const statuses = platform.mediaStatuses(loadAt);
    const st = statusChecks(statuses, playing, end, startedAt, 'the picture moves');
    const startup = statuses.filter((x) => startedAt && x.at <= startedAt);
    let startupBuffering = 0;
    for (let i = 0; i < startup.length; i++) if (playing && startup[i].at >= playing.at && startup[i].playerState === 'BUFFERING' && (i === 0 || startup[i - 1].playerState !== 'BUFFERING')) {
      const next = statuses.find((x) => x.at > startup[i].at && x.playerState !== 'BUFFERING'); startupBuffering += (next ? next.at : end) - startup[i].at;
    }
    const play = evaluatePlayback(got.probe, loadAt, seconds);
    const live = evaluateLive(play.samples, (x) => x.extra || {});
    const fatal = got.errors.filter((e) => e.severity !== 1);
    const loadFailed = platform.events.find((e) => e.at >= loadAt && e.data && (e.data.type === 'LOAD_FAILED' || e.data.type === 'LOAD_CANCELLED' || e.data.type === 'INVALID_REQUEST'));
    const toPlaying = playing ? playing.at - loadAt : null;
    const checks = [
      check('page origin differs from stream origin', new URL(masterUrl).origin !== pageOrigin, `${pageOrigin} vs ${new URL(masterUrl).origin}`, 'different'),
      check('MEDIA_STATUS PLAYING within 15 s', toPlaying !== null && toPlaying <= 15000, toPlaying === null ? why : toPlaying, '<= 15000 ms'),
      ...st.checks, ...play.checks, ...live.checks,
      check('no fatal player error', fatal.length === 0, fatal.length ? JSON.stringify(fatal[0]) : 0, '0'),
      check('no LOAD_FAILED', !loadFailed, loadFailed ? loadFailed.data.type : 0, '0')
    ];
    const metrics = { to_playing_ms: toPlaying, startup_buffering_ms: startupBuffering, ...st.metrics, ...play.metrics, ...live.metrics, errors: got.errors.length, fatal_errors: fatal.length, shaka: got.shaka,
      stalls: Math.max(0, ...play.samples.map((x) => (x.extra && x.extra.stalls) || 0)), commands: statuses.length ? statuses[statuses.length - 1].supportedMediaCommands : null };
    save(`${args.label}-${name}.detail.json`, { metrics, load: MEDIA[name](redactUrl(masterUrl)), statuses: statuses.map((s) => [s.at - loadAt, s.playerState, s.idleReason || '']), errors: got.errors,
      events: got.probe.events.map((e) => [e.at - loadAt, e.type, Math.round(e.ct * 100) / 100]), samples: play.samples, console: log.console.slice(0, 300), requestFailed: log.requestFailed });
    return { name, checks, metrics };
  } finally {
    await browser.close().catch(() => {});
    await platform.close().catch(() => {});
  }
}

let failures = 1; let server;
try {
  server = http.createServer((req, res) => {
    if (req.url === '/') { res.writeHead(200, { 'content-type': 'text/html' }); return res.end(PAGE); }
    if (req.url === '/start.js') { res.writeHead(200, { 'content-type': 'text/javascript' }); return res.end(START); }
    res.writeHead(404); res.end();
  });
  await new Promise((resolve) => server.listen(0, '::', resolve));
  const pageOrigin = `http://localhost:${server.address().port}`;
  const parts = [];
  for (const name of variants) parts.push(await runVariant(name, pageOrigin));
  for (const p of parts) p.informational = !required.includes(p.name);
  failures = finish('test-receiver-default', parts, { masterUrl: redactUrl(masterUrl), seconds, label: args.label });
} catch (e) {
  console.error('test-receiver-default failed:', e && e.stack || e);
  console.log('RESULT ' + JSON.stringify({ test: 'test-receiver-default', pass: false, failed: [String(e && e.message || e)] }));
} finally {
  clearTimeout(hardStop);
  if (server) { server.closeAllConnections(); server.close(); }
}
process.exit(failures ? 1 : 0);
