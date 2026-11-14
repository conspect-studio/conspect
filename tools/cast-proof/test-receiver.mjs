// Test 2: the REAL CAF receiver SDK + our receiver app + the fake platform IPC, playing a live cast stream.
//   node test-receiver.mjs <receiverUrl> <masterUrl> [--seconds 60] [--label name] [--engine <api base> --stream <id>]
// --engine: the stream is the real engine's; reports are read where the engine keeps them (GET
// /api/v1/cast/streams/<id>, polled every 1 s: last_report + fetches; bearer from CAST_PROOF_BEARER) instead of the
// harness server's /_reports.
// receiverUrl e.g. http://localhost:8080/cast/receiver/ ; masterUrl e.g. http://localhost:8080/cast/<token>/master.m3u8
import path from 'node:path';
import { startFakePlatform, NS_CONSPECT } from './fake-cast-platform.mjs';
import { parseArgs, launchBrowser, watchPage, installSampler, evaluatePlayback, evaluateLive, statusChecks, senderMedia, redactUrl, check, finish, save, sleep, getJson, runFor, outDir } from './lib.mjs';

const args = parseArgs(process.argv.slice(2), { seconds: 60, label: 'receiver', width: 1920 });
const [receiverUrl, masterUrl] = args._;
if (!receiverUrl || !masterUrl) { console.error('usage: test-receiver.mjs <receiverUrl> <masterUrl> [--seconds 60]'); process.exit(2); }
const seconds = Number(args.seconds); const W = Number(args.width);
const origin = new URL(receiverUrl).origin;
const hardStop = setTimeout(() => { console.error('hard timeout'); process.exit(3); }, (seconds + 90) * 1000);

let platform; let browser; let failures = 1;
try {
  platform = await startFakePlatform();
  browser = await launchBrowser();
  const page = await browser.newPage();
  const log = await watchPage(page);
  await page.evaluateOnNewDocument(installSampler);
  await page.evaluateOnNewDocument(() => {
    window.__probeExtra = () => {
      const pm = cast.framework.CastReceiverContext.getInstance().getPlayerManager();
      const st = pm.getStats() || {}; const r = pm.getLiveSeekableRange();
      const o = document.getElementById('overlay'); const cs = o ? getComputedStyle(o) : null;
      return { state: pm.getPlayerState(), latency: st.liveLatency, seekLen: r ? r.end - r.start : null, stalls: st.stallsDetected, gaps: st.gapsJumped,
        overlay: o ? o.getAttribute('data-state') : null, overlayVisible: cs ? (cs.visibility !== 'hidden' && Number(cs.opacity) > 0.01) : null };
    };
  });
  const response = await page.goto(receiverUrl, { waitUntil: 'load', timeout: 30000 });
  const cspHeader = response.headers()['content-security-policy'] || '';
  await platform.waitForReceiver(20000);
  await sleep(1000);
  const idle = await page.evaluate(() => ({ state: document.getElementById('overlay').getAttribute('data-state'), text: document.getElementById('overlay').innerText.replace(/\s+/g, ' ').trim() }));
  await page.screenshot({ path: path.join(outDir, 'receiver-idle.png') });
  await page.setViewport({ width: 1920, height: 1080 });
  await page.screenshot({ path: path.join(outDir, 'receiver-idle-1080.png') });
  await page.setViewport({ width: 1280, height: 720 });

  const loadAt = Date.now();
  const engineReports = []; let engineDoc = null; let enginePoll = null;
  if (args.engine) {
    const auth = { authorization: `Bearer ${process.env.CAST_PROOF_BEARER}` };
    enginePoll = setInterval(async () => {
      try {
        const d = await getJson(`${args.engine}/api/v1/cast/streams/${args.stream}`, 'GET', null, auth); engineDoc = d;
        const r = d.last_report; if (r && !engineReports.some((x) => x.received_unix_ms === r.received_unix_ms)) engineReports.push(r);
      } catch { /* counted by the checks */ }
    }, 1000);
  }
  platform.load(senderMedia(masterUrl));
  let playing = null; let playingError = '';
  try { playing = await platform.waitForMediaStatus((s) => s.playerState === 'PLAYING', 15000, loadAt); } catch (e) { playingError = e.message; }
  const toPlaying = playing ? playing.at - loadAt : null;
  if (playing) {
    await sleep(Math.max(0, loadAt + 10000 - Date.now()));
    await page.screenshot({ path: path.join(outDir, 'receiver-playing.png') });
    platform.sendCustom(NS_CONSPECT, { type: 'ping' });
    platform.setVolume({ level: 0.5 });
  }
  const probe = playing ? await runFor(page, loadAt, seconds) : await runFor(page, loadAt, 0);
  const end = probe.end.at;
  const page_ = await page.evaluate(() => ({ overlay: document.getElementById('overlay').getAttribute('data-state'), shaka: window.shaka && shaka.Player.version, caf: cast.framework.VERSION,
    commands: cast.framework.CastReceiverContext.getInstance().getPlayerManager().getSupportedMediaCommands(),
    elementVolume: window.__probe.video ? window.__probe.video.volume : null }));
  const csp = await log.csp();
  if (enginePoll) { clearInterval(enginePoll); await sleep(1100); }
  // Engine mode: the engine keeps the last report only, so the polled reports stand in for the list (one POST per
  // 5 s is caught by a 1 s poll); size and rate of the POSTs are checked by the engine itself (413 / 429).
  const reports = args.engine
    ? engineReports.map((r) => ({ token: new URL(masterUrl).pathname.split('/')[2], at: r.received_unix_ms, report: r.report, bytes: JSON.stringify(r.report).length, contentType: 'application/json' }))
    : await getJson(origin + '/_reports');

  // --- criteria
  const statuses = platform.mediaStatuses(loadAt);
  const play = evaluatePlayback(probe, loadAt, seconds, { startLimitMs: 3000 });
  const live = evaluateLive(play.samples, (x) => x.extra || {}, { designWindow: true });
  const checks = [check('MEDIA_STATUS PLAYING within 15 s', toPlaying !== null && toPlaying <= 15000, toPlaying === null ? playingError : toPlaying, '<= 15000 ms')];
  const st = statusChecks(statuses, playing, end); const maxBuffering = st.metrics.max_buffering_ms;
  checks.push(...st.checks);
  checks.push(...play.checks, ...live.checks);
  const sdkStalls = Math.max(0, ...play.samples.map((x) => (x.extra && x.extra.stalls) || 0)); const gaps = Math.max(0, ...play.samples.map((x) => (x.extra && x.extra.gaps) || 0));
  checks.push(check('zero Shaka stalls detected', sdkStalls === 0, sdkStalls, '0'));
  checks.push(check('gaps jumped <= 1', gaps <= 1, gaps, '<= 1'));
  checks.push(check('idle screen before LOAD', idle.state === 'idle' && /Conspect/.test(idle.text) && /Ready to cast/.test(idle.text), `${idle.state}: "${idle.text}"`, 'idle + wordmark'));
  const visible = play.samples.filter((x) => x.extra && x.extra.state === 'PLAYING' && x.extra.overlayVisible).length;
  checks.push(check('overlay hidden while playing', play.samples.length > 5 && visible <= 1 && page_.overlay === 'playing', `${visible} visible sample(s), final ${page_.overlay}`, 'hidden'));
  const custom = platform.custom(NS_CONSPECT, loadAt).map((e) => e.data);
  const good = custom.filter((r) => r && r.v === 1 && r.state === 'playing' && r.stats && r.stats.width === W);
  checks.push(check(`custom-namespace reports: playing, width ${W}`, good.length >= Math.floor((seconds - 15) / 5), `${good.length} of ${custom.length}`, `>= ${Math.floor((seconds - 15) / 5)}`));
  const token = new URL(masterUrl).pathname.split('/')[2];
  const posted = reports.filter((r) => r.token === token && r.at >= loadAt);
  const postedGood = posted.filter((r) => r.report.state === 'playing' && r.report.stats && r.report.stats.width === W);
  if (args.engine) {
    const f = (engineDoc && engineDoc.fetches) || {};
    checks.push(check('engine counted the fetches (not a Cast device)', f.playlists > seconds / 4 && f.segments > seconds / 2 && f.cast_device === false, `playlists ${f.playlists}, segments ${f.segments}, cast_device ${f.cast_device}`, `> ${seconds / 4} / > ${seconds / 2} / false`));
    const lr = engineDoc && engineDoc.last_report && engineDoc.last_report.report;
    checks.push(check(`engine last_report: playing, width ${W}`, !!lr && lr.state === 'playing' && lr.stats && lr.stats.width === W, lr ? `${lr.state} ${lr.stats && lr.stats.width}` : 'none', `playing ${W}`));
  }
  checks.push(check(args.engine ? 'engine received POSTed reports' : '/_reports holds POSTed reports', postedGood.length >= Math.floor((seconds - 15) / 5), `${postedGood.length} of ${posted.length}`, `>= ${Math.floor((seconds - 15) / 5)}`));
  const maxBytes = Math.max(0, ...posted.map((r) => r.bytes)); const jsonType = posted.every((r) => /^application\/json/.test(r.contentType));
  checks.push(check('report body under 2 KiB, application/json', posted.length > 0 && maxBytes < 2048 && jsonType, `${maxBytes} bytes`, '< 2048'));
  let minGap = Infinity; for (let i = 1; i < posted.length; i++) minGap = Math.min(minGap, posted[i].at - posted[i - 1].at);
  checks.push(check('at most one report POST per second', posted.length > 1 && minGap >= 900, minGap, '>= 900 ms'));
  const pong = custom.filter((r) => r && r.v === 1).length;
  // Without a platform the SDK applies SET_VOLUME to the media element and reports it in MEDIA_STATUS
  // (on a device it forwards a system `setvolume` to the platform instead; recorded, not required).
  const sysVol = platform.events.find((e) => e.data && e.data.type === 'setvolume');
  const volStatus = statuses.find((s) => s.volume && Math.abs(s.volume.level - 0.5) < 0.01);
  checks.push(check('SET_VOLUME honoured (MEDIA_STATUS volume 0.5)', !!volStatus, `status ${volStatus ? volStatus.volume.level : 'none'}, element ${page_.elementVolume}, system setvolume ${sysVol ? 'sent' : 'not sent'}`, 'level 0.5'));
  const cmds = statuses.length ? statuses[statuses.length - 1].supportedMediaCommands : null;
  checks.push(check('supported commands: volume + mute only (no PAUSE, no SEEK)', cmds === 12 && page_.commands === 12, `${cmds} / ${page_.commands}`, '12'));
  checks.push(check('CSP header applied', /default-src 'none'/.test(cspHeader), cspHeader ? 'present' : 'absent', 'present'));
  checks.push(check('zero CSP violations', csp.length === 0, csp.length ? JSON.stringify(csp).slice(0, 400) : 0, '0'));
  checks.push(check('zero console errors', log.errors.length === 0, log.errors.length ? log.errors.slice(0, 3).join(' | ').slice(0, 400) : 0, '0'));
  checks.push(check('zero page errors', log.pageErrors.length === 0, log.pageErrors.length ? log.pageErrors[0].slice(0, 300) : 0, '0'));
  checks.push(check('zero HTTP error responses', log.httpErrors.length === 0, log.httpErrors.length ? log.httpErrors.slice(0, 3).join(' | ') : 0, '0'));
  checks.push(check('zero failed requests', log.requestFailed.length === 0, log.requestFailed.length ? log.requestFailed.slice(0, 2).join(' | ') : 0, '0'));
  const tokenLeak = log.console.filter((l) => l.includes('[conspect-cast]') && l.includes(token)).length;
  checks.push(check('token never logged by the receiver', tokenLeak === 0, tokenLeak, '0'));

  const metrics = { to_playing_ms: toPlaying, ...play.metrics, ...live.metrics, max_buffering_ms: maxBuffering, stalls: sdkStalls, gaps, reports_custom: custom.length, reports_posted: posted.length, shaka: page_.shaka, caf: page_.caf, csp: cspHeader };
  save(`${args.label}.detail.json`, { metrics, statuses: statuses.map((s) => [s.at - loadAt, s.playerState, s.idleReason || '']), events: probe.events.map((e) => [e.at - loadAt, e.type, Math.round(e.ct * 100) / 100]), samples: play.samples, lastReport: custom[custom.length - 1], console: log.console, csp, pong });
  failures = finish('test-receiver', [{ name: args.label, checks, metrics }], { receiverUrl, masterUrl: redactUrl(masterUrl), seconds, label: args.label });
} catch (e) {
  console.error('test-receiver failed:', e && e.stack || e);
  console.log('RESULT ' + JSON.stringify({ test: 'test-receiver', pass: false, failed: [String(e && e.message || e)] }));
  failures = 1;
} finally {
  clearTimeout(hardStop);
  if (browser) await browser.close().catch(() => {});
  if (platform) await platform.close().catch(() => {});
}
process.exit(failures ? 1 : 0);
