// Test 4: fault behaviour of the receiver app (REAL CAF SDK + fake platform). Self-contained: starts its own
// harness server (HTTPS, CSP applied) and its own reference stream generator.
//   node test-receiver-faults.mjs [--cases a,b,c,d,e,f,g,h,i,j,k,l,m] [--upstream <dir or base URL of a live cast stream>]
// With --upstream the stream is not generated here; case d (generator restart) is then skipped.
//  a  rejected LOAD (foreign origin, wrong path) -> LOAD_FAILED + error screen (out/receiver-error.png)
//  b  token answers 404 after 15 s of playing -> "ended or expired" error report within 20 s, no request storm,
//     then the app closes itself (what the SDK does without a platform is recorded)
//  c  segments held for 12 s, then released -> BUFFERING, back to PLAYING with no reload, latency under 20 s
//  d  stream generator restarted (media sequence from 0, new init) -> PLAYING again within 30 s
//  e  last sender disconnects (transport_closed, then closed_by_peer) -> still PLAYING 20 s later
//  f  customData.debug -> the CastDebugLogger overlay loads under the CSP (and is not requested otherwise)
//  g  a 12 s stall leaves the player about 8 s later -> one seek to the live edge after 2 min of clean playback
//  h  segments held for 36 s -> one reload after 20 s of BUFFERING, then PLAYING
//  i  segments are noise for 5 s -> recovery by the receiver's retry (fatal error) or its 20 s watchdog (silent stall)
//  j  segments stay noise -> exactly 3 retries, then the "cannot decode" error screen, no further loads
//  k  start on a young stream: LOAD at exactly 5 segments (k2, the engine's `ready` minimum) and at a full
//     10-segment window (k1, engine pre-roll); 3 segments (k0) is informational. [--k-segments 3,5,10]
//  l  a PAUSE sent although not advertised pauses, PLAY resumes; a stream left paused closes the app
//  m  the server becomes unreachable -> 3 retries, then "The TV cannot reach the Conspect server."
//  o  stream A's master probe is delayed 2 s then answered 404 while B is LOADed meanwhile -> B reaches PLAYING
//     and stays (MEDIA-2: A's late probe completion must not call endCast() / stop B)
// Cases run in the order given; all but k start on a full 10-segment window.
import fs from 'node:fs';
import path from 'node:path';
import { startServer, RECEIVER_DIR } from './server.mjs';
import { startFakePlatform, NS_CONSPECT, NS_MEDIA } from './fake-cast-platform.mjs';
import { parseArgs, openReceiver, startGenerator, waitForSegments, watchPlaylist, senderMedia, check, finish, save, sleep, waitUntil, outDir, here } from './lib.mjs';
import { execFileSync } from 'node:child_process';

const args = parseArgs(process.argv.slice(2), { cases: 'a,b,c,d,e,f,g,h,i,j,k,l,m,n,o', 'k-segments': '3,5,10' });
const wanted = String(args.cases).split(',');
const TOKEN = 'faultAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA';
const TOKEN_B = 'faultBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB';   // case o: a second stream, same directory, different token
const streamDir = path.join(outDir, 'streams', 'faults');
const hardStop = setTimeout(() => { console.error('hard timeout'); process.exit(3); }, 24 * 60 * 1000);

let server; let generator; let genCount = 0;
const newGenerator = (keep) => { const tag = `f${Date.now() % 100000}x${genCount++}`; const g = startGenerator('reference', streamDir, 900, { GEN: tag, KEEP: keep ? '1' : '0' }); g.tag = tag; return g; };
const sinceLoad = (ctx) => ctx.platform.mediaStatuses(ctx.loadAt);
const reportsSince = (ctx, t) => ctx.platform.custom(NS_CONSPECT, t).map((e) => ({ at: e.at, ...e.data }));
const receiverLines = (ctx, re) => ctx.log.console.filter((l) => l.includes('[conspect-cast]') && re.test(l));

/** What happened from t0 on: MEDIA_STATUS timeline, the receiver's own log lines, the 1 s video samples. */
async function trace(ctx, t0) {
  const p = await ctx.probe().catch(() => ({ samples: [], events: [] }));
  return {
    statuses: ctx.platform.mediaStatuses(t0).map((s) => [s.at - t0, s.playerState, s.idleReason || '']).filter((x, i, a) => i === 0 || x[1] !== a[i - 1][1]),
    log: ctx.log.console.filter((l) => l.includes('[conspect-cast]')).map((l) => l.replace(/^.*\[conspect-cast\] /, '').slice(0, 160)),
    samples: p.samples.filter((x) => x.at >= t0).map((x) => [x.at - t0, Math.round(x.ct * 10) / 10, x.extra && x.extra.state, Math.round(x.ahead * 10) / 10, x.extra && Math.round(x.extra.latency * 10) / 10, x.extra && x.extra.gaps]),
    video: p.events.filter((e) => e.at >= t0).map((e) => [e.at - t0, e.type, Math.round(e.ct * 10) / 10]).slice(0, 80)
  };
}

/** Open the receiver, send the sender's LOAD, wait for PLAYING and `playMs` of playback. */
async function playing(playMs) {
  const ctx = await openReceiver(server.secureOrigin + '/cast/receiver/', startFakePlatform);
  try {
    ctx.masterUrl = `${server.secureOrigin}/cast/${TOKEN}/master.m3u8`;
    ctx.loadAt = Date.now();
    ctx.platform.load(senderMedia(ctx.masterUrl));
    const first = await ctx.platform.waitForMediaStatus((s) => s.playerState === 'PLAYING', 15000, ctx.loadAt);
    ctx.toPlaying = first.at - ctx.loadAt;
    await sleep(playMs);
    return ctx;
  } catch (e) { await ctx.close(); throw e; }
}

async function caseA() {
  const ctx = await openReceiver(server.secureOrigin + '/cast/receiver/', startFakePlatform);
  try {
    const checks = []; const m = {};
    const bad = [
      ['http on a foreign origin', `http://example.com/cast/${TOKEN}/master.m3u8`],
      ['credentials in the URL', `https://u:p@example.com/cast/${TOKEN}/master.m3u8`],
      ['wrong path', `${server.secureOrigin}/hls/restream/master.m3u8`],
      ['short token', `${server.secureOrigin}/cast/short/master.m3u8`],
      ['not http', 'javascript:alert(1)']
    ];
    for (const [name, url] of bad) {
      const t0 = Date.now();
      const requestId = ctx.platform.load(senderMedia(url));
      let frame = null;
      try { frame = await ctx.platform.waitFor((e) => e.namespace === NS_MEDIA && e.data && e.data.requestId === requestId && e.data.type !== 'MEDIA_STATUS', 8000, 'a LOAD answer', t0); } catch { /* reported below */ }
      await sleep(600);
      const screen = await ctx.last();
      checks.push(check(`${name}: LOAD_FAILED to the sender`, frame && frame.data.type === 'LOAD_FAILED', frame ? `${frame.data.type} reason ${frame.data.reason || '-'} to ${frame.senderId}` : 'no answer', 'LOAD_FAILED'));
      const text = await ctx.page.evaluate(() => document.getElementById('overlay').innerText.replace(/\s+/g, ' ').trim());
      const state = await ctx.page.evaluate(() => document.getElementById('overlay').getAttribute('data-state'));
      checks.push(check(`${name}: error screen`, state === 'error' && /This receiver only plays Conspect cast streams/.test(text) && /905/.test(text), `${state}: "${text}"`, 'error + sentence + code'));
      if (name === 'http on a foreign origin') { m.load_failed_ms = frame ? frame.at - t0 : null; m.load_failed_reason = frame ? frame.data.reason : null; await ctx.page.screenshot({ path: path.join(outDir, 'receiver-error.png') }); }
      void screen;
    }
    const rejectReport = reportsSince(ctx, 0).find((r) => r.state === 'error');
    checks.push(check('error report broadcast to senders', rejectReport && rejectReport.error && rejectReport.error.code === 905 && rejectReport.error.reason === 'INVALID_REQUEST', rejectReport ? JSON.stringify(rejectReport.error).slice(0, 160) : 'none', 'code 905 INVALID_REQUEST'));
    const noFetch = server.requests.filter((r) => r.token === 'short').length + ctx.log.requestFailed.filter((l) => /example\.com/.test(l)).length;
    checks.push(check('rejected URLs are never fetched', noFetch === 0, noFetch, '0'));
    // A valid LOAD after a rejected one must play (the error screen is not a dead end).
    const t1 = Date.now();
    ctx.platform.load(senderMedia(`${server.secureOrigin}/cast/${TOKEN}/master.m3u8`));
    let ok = null; try { ok = await ctx.platform.waitForMediaStatus((s) => s.playerState === 'PLAYING', 15000, t1); } catch { /* reported */ }
    await sleep(1500);
    const after = await ctx.last();
    checks.push(check('a valid LOAD after a rejection plays', ok && after && after.extra.overlay === 'playing', ok ? `${ok.at - t1} ms, overlay ${after && after.extra.overlay}` : 'no PLAYING', 'PLAYING'));
    checks.push(check('zero CSP violations', (await ctx.log.csp()).length === 0, (await ctx.log.csp()).length, '0'));
    return { name: 'a-rejected-load', checks, metrics: m };
  } finally { await ctx.close(); }
}

async function caseB() {
  const ctx = await playing(15000);
  try {
    const checks = []; const m = { to_playing_ms: ctx.toPlaying };
    const faultAt = Date.now();
    server.setFault({ mode: '404', token: TOKEN });
    let rep = null;
    try { rep = await waitUntil(() => reportsSince(ctx, faultAt).find((r) => r.state === 'error' && r.error && /ended or its link expired/.test(r.error.message)), 20000, 'the ended-or-expired report'); } catch { /* reported */ }
    m.error_report_ms = rep ? rep.at - faultAt : null;
    checks.push(check('"ended or expired" error report within 20 s', !!rep, rep ? `${m.error_report_ms} ms: ${rep.error.message}` : 'none', '<= 20000 ms'));
    const reportAt = rep ? rep.at : Date.now();
    m.requests_before_report = server.requestsSince(TOKEN, faultAt).filter((r) => r.at < reportAt).length;
    const screen = await ctx.page.evaluate(() => ({ state: document.getElementById('overlay').getAttribute('data-state'), text: document.getElementById('overlay').innerText.replace(/\s+/g, ' ').trim() }));
    checks.push(check('error screen shows the sentence', screen.state === 'error' && /ended or its link expired/.test(screen.text), `${screen.state}: "${screen.text}"`, 'error screen'));
    await sleep(600);                                      // past the overlay's 0.25 s fade-in
    await ctx.page.screenshot({ path: path.join(outDir, 'receiver-ended.png') });
    // The app closes itself 20 s after the terminal error. Watch the 20 s for a request storm, then for the close.
    let closedAt = 0; ctx.platform.waitForClose(32000).then((t) => { closedAt = t; }).catch(() => {});
    await sleep(Math.max(0, reportAt + 20000 - Date.now()));
    m.requests_20s_after_report = server.requestsSince(TOKEN, reportAt).filter((r) => r.at < reportAt + 20000).length;
    checks.push(check('no retry storm: requests in the 20 s after the report', m.requests_20s_after_report < 12, `${m.requests_20s_after_report} (and ${m.requests_before_report} between the fault and the report)`, '< 12'));
    const retries = receiverLines(ctx, /retry \d of|reload:/).length;
    checks.push(check('no retry or reload of a revoked token', retries === 0, retries, '0'));
    try { await waitUntil(() => closedAt, 12000, 'the receiver to close'); } catch { /* reported */ }
    const stopLine = receiverLines(ctx, /closing the receiver app/).length;
    m.close_ms_after_report = closedAt ? closedAt - reportAt : null;
    m.ipc_close_code = ctx.platform.state.closeCode; m.page_closed = ctx.page.isClosed();
    const lastState = ctx.platform.mediaStatuses(faultAt).pop();
    m.final_media_status = lastState ? `${lastState.playerState}${lastState.idleReason ? '/' + lastState.idleReason : ''}` : null;
    m.system_frames_after_fault = ctx.platform.events.filter((e) => e.at >= faultAt && e.namespace.endsWith('.system')).map((e) => e.data && e.data.type);
    checks.push(check('context.stop(): the app closes (IPC closed) about 20 s after the error', stopLine === 1 && closedAt > 0 && m.close_ms_after_report >= 15000 && m.close_ms_after_report <= 30000, closedAt ? `${m.close_ms_after_report} ms, ws code ${m.ipc_close_code}, page closed ${m.page_closed}` : `no IPC close (stop logged ${stopLine}x)`, '15..30 s'));
    return { name: 'b-token-404', checks, metrics: m, detail: await trace(ctx, faultAt) };
  } finally { server.setFault({ mode: 'none' }); await ctx.close(); }
}

async function caseC() {
  const ctx = await playing(12000);
  try {
    const checks = []; const m = { to_playing_ms: ctx.toPlaying };
    const before = await ctx.last();
    m.latency_before_s = before && before.extra ? before.extra.latency : null;
    const faultAt = Date.now();
    server.setFault({ mode: 'stall', seconds: 12, token: TOKEN });
    let buffering = null;
    try { buffering = await ctx.platform.waitForMediaStatus((s) => s.playerState === 'BUFFERING', 14000, faultAt); } catch { /* reported */ }
    m.buffering_after_ms = buffering ? buffering.at - faultAt : null;
    checks.push(check('BUFFERING while segments are held', !!buffering, m.buffering_after_ms, 'within the 12 s stall'));
    const scrim = buffering ? await waitUntil(async () => { const s = await ctx.last(); return s && s.extra.overlay === 'buffering' ? s : null; }, 5000, 'the buffering screen').catch(() => null) : null;
    checks.push(check('buffering screen with the feed title', scrim && /Program/.test(scrim.extra.overlayText) && /Buffering/.test(scrim.extra.overlayText), scrim ? scrim.extra.overlayText : 'not shown', 'Program + Buffering'));
    let back = null;
    try { back = await ctx.platform.waitForMediaStatus((s) => s.playerState === 'PLAYING', 30000, faultAt + 11000); } catch { /* reported */ }
    m.playing_again_ms_after_release = back ? back.at - (faultAt + 12000) : null;
    checks.push(check('back to PLAYING after the release', !!back, back ? `${m.playing_again_ms_after_release} ms after release` : 'never', '<= 18 s'));
    await sleep(22000);                                    // long enough for the receiver's latency check (5 s tick)
    const after = await ctx.last();
    const statuses = ctx.platform.mediaStatuses(faultAt);
    m.idle_statuses = statuses.filter((s) => s.playerState === 'IDLE').length;
    m.reloads = receiverLines(ctx, /reload:|retry \d of/).length;
    m.live_seeks = receiverLines(ctx, /seeking to the live edge/).length;
    m.latency_after_s = after && after.extra ? after.extra.latency : null;
    m.shaka_stalls = after && after.extra ? after.extra.stalls : null;
    const rep = reportsSince(ctx, faultAt).pop();
    m.reported_stalls = rep && rep.stats ? rep.stats.stalls : null;
    checks.push(check('no reload loop (no reload, no retry, no IDLE)', m.reloads === 0 && m.idle_statuses === 0, `${m.reloads} reload(s), ${m.idle_statuses} IDLE`, '0'));
    checks.push(check('final state PLAYING, overlay hidden', after && after.extra.state === 'PLAYING' && after.extra.overlay === 'playing' && !after.extra.overlayVisible, after ? `${after.extra.state} / ${after.extra.overlay}` : 'none', 'PLAYING'));
    checks.push(check('final latency under 20 s', typeof m.latency_after_s === 'number' && m.latency_after_s < 20, `${m.latency_after_s} s (before ${m.latency_before_s} s, live seeks ${m.live_seeks})`, '< 20 s'));
    checks.push(check('the stall is counted in the report', m.reported_stalls >= 1, m.reported_stalls, '>= 1'));
    return { name: 'c-segments-stalled-12s', checks, metrics: m, detail: await trace(ctx, faultAt) };
  } finally { server.setFault({ mode: 'none' }); await ctx.close(); }
}

async function caseD() {
  const ctx = await playing(12000);
  try {
    const checks = []; const m = { to_playing_ms: ctx.toPlaying };
    const before = await ctx.last();
    // Restart: kill ffmpeg, start a new one in the same directory (new init, new names, media sequence 0).
    const killAt = Date.now();
    await generator.stop();
    generator = newGenerator(true);
    const tag = generator.tag;
    try { await waitUntil(() => { const t = fs.readFileSync(path.join(streamDir, 'video.m3u8'), 'utf8'); return t.includes(`seg-video-${tag}-`) && /MEDIA-SEQUENCE:0\b/.test(t); }, 20000, 'the restarted playlist'); }
    catch (e) { checks.push(check('generator restarted', false, e.message, 'new playlist')); }
    const restartedAt = Date.now();
    m.playlist_restarted_ms_after_kill = restartedAt - killAt;
    // Recovered = the picture moves again on the new stream: clock advancing for 3 s in a row with PLAYING status.
    let recovered = null;
    try {
      recovered = await waitUntil(async () => {
        const p = await ctx.probe(); const s = p.samples.filter((x) => x.at > restartedAt + 1000).slice(-4);
        return s.length === 4 && s.every((x, i) => x.extra.state === 'PLAYING' && (i === 0 || (x.ct - s[i - 1].ct > 0.7 && x.vb > s[i - 1].vb))) ? s[0] : null;
      }, 34000, 'playback of the restarted stream');
    } catch { /* reported */ }
    m.recovered_ms_after_restart = recovered ? recovered.at - restartedAt : null;
    m.new_stream_playing = recovered ? recovered.at > restartedAt : false;
    checks.push(check('PLAYING again within 30 s of the restart', recovered && m.recovered_ms_after_restart <= 30000, recovered ? m.recovered_ms_after_restart : 'not recovered', '<= 30000 ms'));
    await sleep(8000);
    const after = await ctx.last();
    m.reloads = receiverLines(ctx, /reload:/).length; m.retries = receiverLines(ctx, /retry \d of/).length;
    m.how = receiverLines(ctx, /reload:|retry \d of|error /).map((l) => l.replace(/^.*\[conspect-cast\] /, '').slice(0, 90)).slice(0, 6);
    m.idle_statuses = ctx.platform.mediaStatuses(killAt).filter((s) => s.playerState === 'IDLE').length;
    m.latency_after_s = after && after.extra ? after.extra.latency : null;
    checks.push(check('still PLAYING 8 s later, overlay hidden', after && after.extra.state === 'PLAYING' && after.extra.overlay === 'playing' && after.ct !== before.ct, after ? `${after.extra.state} / ${after.extra.overlay}` : 'none', 'PLAYING'));
    checks.push(check('bounded recovery (at most 3 reloads)', m.reloads + m.retries <= 3, `${m.reloads} reload(s), ${m.retries} retry(ies)`, '<= 3'));
    checks.push(check('latency after recovery under 20 s', typeof m.latency_after_s === 'number' && m.latency_after_s < 20, m.latency_after_s, '< 20 s'));
    return { name: 'd-generator-restart', checks, metrics: m, detail: await trace(ctx, killAt) };
  } finally { await ctx.close(); }
}

async function caseE() {
  const ctx = await playing(8000);
  try {
    const checks = []; const m = { to_playing_ms: ctx.toPlaying };
    const step = async (label, reason) => {
      const t0 = Date.now(); const a = await ctx.last();
      ctx.platform.disconnectSender('sender-0', reason);
      await sleep(20000);
      const closed = ctx.platform.state.closedAt > 0;
      const b = closed ? null : await ctx.last();
      const ok = !closed && b && b.extra.state === 'PLAYING' && b.ct - a.ct > 17 && b.vb > a.vb && b.ab > a.ab;
      checks.push(check(`${label}: still PLAYING 20 s later`, ok, closed ? 'the app closed' : `${b.extra.state}, advanced ${Math.round((b.ct - a.ct) * 10) / 10} s`, 'PLAYING, >= 17 s'));
      const posted = server.reports.filter((r) => r.token === TOKEN && r.at >= t0 && r.report.state === 'playing').length;
      checks.push(check(`${label}: reports still POSTed with no sender`, posted >= 2, posted, '>= 2'));
      return ok;
    };
    // 1. the tab was closed / the network dropped: not an explicit stop
    await step('transport_closed', 'transport_closed');
    // 2. a sender reconnects, then disconnects explicitly ("closed_by_peer" = requested_by_sender): CAF's default
    //    would stop the app here; the receiver's last-sender handler keeps a live cast running.
    ctx.platform.connectSender('sender-0');
    await sleep(500);
    await step('closed_by_peer (explicit disconnect)', 'closed_by_peer');
    m.kept_playing_log = receiverLines(ctx, /last sender left: the stream keeps playing/).length;
    // 3. the cast still ends on a real stop: a sender reconnects and sends STOP -> idle screen
    if (!ctx.platform.state.closedAt) {
      ctx.platform.connectSender('sender-1'); await sleep(300);
      const t2 = Date.now(); ctx.platform.stop('sender-1');
      let idle = null; try { idle = await ctx.platform.waitForMediaStatus((s) => s.playerState === 'IDLE', 8000, t2); } catch { /* reported */ }
      await sleep(800);
      const o = await ctx.page.evaluate(() => document.getElementById('overlay').getAttribute('data-state'));
      checks.push(check('STOP from a sender ends the stream (idle screen)', idle && o === 'idle', idle ? `IDLE/${idle.idleReason}, overlay ${o}` : 'no IDLE', 'IDLE + idle screen'));
    }
    return { name: 'e-sender-disconnect', checks, metrics: m };
  } finally { await ctx.close(); }
}

async function caseF() {
  // customData.debug === true loads Google's CastDebugLogger overlay lazily; without it the script is never requested.
  const ctx = await openReceiver(server.secureOrigin + '/cast/receiver/', startFakePlatform);
  try {
    const checks = []; const m = {};
    const debugScripts = () => ctx.page.evaluate(() => Array.from(document.scripts).filter((x) => /debug_layer/.test(x.src)).length);
    const t0 = Date.now();
    ctx.platform.load(senderMedia(`${server.secureOrigin}/cast/${TOKEN}/master.m3u8`));
    await ctx.platform.waitForMediaStatus((x) => x.playerState === 'PLAYING', 15000, t0);
    await sleep(2000);
    checks.push(check('no debug script without customData.debug', await debugScripts() === 0, await debugScripts(), '0'));
    const t1 = Date.now();
    ctx.platform.load(senderMedia(`${server.secureOrigin}/cast/${TOKEN}/master.m3u8`, { customData: { title: 'Program', debug: true } }));
    let ok = null; try { ok = await ctx.platform.waitForMediaStatus((x) => x.playerState === 'PLAYING', 15000, t1 + 50); } catch { /* reported */ }
    await sleep(4000);
    const page = await ctx.page.evaluate(() => ({ logger: !!(window.cast && cast.debug && cast.debug.CastDebugLogger), overlay: !!document.querySelector('cast-debug-overlay'), scripts: Array.from(document.scripts).filter((x) => /debug_layer/.test(x.src)).length }));
    const csp = await ctx.log.csp();
    m.csp_violations = csp.map((v) => `${v.directive} ${v.blocked}`.slice(0, 120)); m.debug = page;
    await ctx.page.screenshot({ path: path.join(outDir, 'receiver-debug.png') });
    checks.push(check('debug overlay script loads under the CSP', page.scripts === 1 && page.logger, JSON.stringify(page), 'CastDebugLogger defined'));
    checks.push(check('debug overlay element present', page.overlay, page.overlay, 'true'));
    checks.push(check('zero CSP violations with the debug overlay', csp.length === 0, csp.length ? m.csp_violations.join(' | ') : 0, '0'));
    checks.push(check('still PLAYING with the debug overlay', ok && ctx.platform.lastPlayerState() === 'PLAYING', ctx.platform.lastPlayerState(), 'PLAYING'));
    return { name: 'f-debug-overlay', checks, metrics: m, detail: await trace(ctx, t0) };
  } finally { await ctx.close(); }
}

async function caseG() {
  // One 12 s stall leaves the player about 8 s later than before. After 2 minutes of clean playback the receiver
  // seeks to the live edge once (the 25 s rule cannot be reached with a 20 s playlist window: Shaka moves first).
  const ctx = await playing(10000);
  try {
    const checks = []; const m = {};
    const first = await ctx.last(); m.latency_start_s = Math.round(first.extra.latency * 10) / 10;
    const faultAt = Date.now();
    server.setFault({ mode: 'stall', seconds: 12, token: TOKEN });
    await ctx.platform.waitForMediaStatus((x) => x.playerState === 'BUFFERING', 14000, faultAt).catch(() => {});
    await ctx.platform.waitForMediaStatus((x) => x.playerState === 'PLAYING', 25000, faultAt + 11000).catch(() => {});
    await sleep(8000);
    const before = await ctx.last(); m.latency_after_stall_s = before && before.extra ? Math.round(before.extra.latency * 10) / 10 : null;
    // Two outcomes, depending on where the stall hits the segment cycle (both seen):
    //  late   : the player resumes where it froze and stays that much later -> the receiver must seek once, 2 min later
    //  jumped : Shaka itself moved the playhead forward at the end of the stall -> the receiver must not seek
    m.outcome = m.latency_after_stall_s > m.latency_start_s + 4 ? 'late' : 'jumped';
    let seekLine = null;
    if (m.outcome === 'late') {
      try { seekLine = await waitUntil(() => receiverLines(ctx, /seeking to the live edge/)[0], 135000, 'the live-edge seek'); } catch { /* reported */ }
      m.seek_ms_after_release = seekLine ? Date.now() - (faultAt + 12000) : null;
      await sleep(12000);
    } else await sleep(20000);
    const after = await ctx.last(); m.latency_after_s = after && after.extra ? Math.round(after.extra.latency * 10) / 10 : null;
    m.seeks = receiverLines(ctx, /seeking to the live edge/).length; m.reloads = receiverLines(ctx, /reload:|retry \d of/).length;
    if (m.outcome === 'late') checks.push(check('late after the stall: one seek to the live edge after 2 min of clean playback', m.seeks === 1 && m.seek_ms_after_release > 100000, seekLine ? `${m.latency_start_s} -> ${m.latency_after_stall_s} s; seek ${m.seek_ms_after_release} ms after release` : `${m.seeks} seek(s)`, '1, after >= 2 min'));
    else checks.push(check('Shaka re-placed the playhead itself: no receiver seek', m.seeks === 0, `${m.latency_start_s} -> ${m.latency_after_stall_s} s; ${m.seeks} seek(s)`, '0'));
    checks.push(check('latency back between 4 and 11 s, PLAYING', after && after.extra.state === 'PLAYING' && after.extra.overlay === 'playing' && m.latency_after_s >= 4 && m.latency_after_s <= 11, `${m.latency_after_s} s, ${after && after.extra.state}`, '4..11 s'));
    checks.push(check('no reload', m.reloads === 0, m.reloads, '0'));
    return { name: 'g-live-edge-catch-up', checks, metrics: m, detail: await trace(ctx, faultAt) };
  } finally { server.setFault({ mode: 'none' }); await ctx.close(); }
}

async function caseH() {
  // No player error, but BUFFERING for more than 20 s: the receiver reloads once. The player has up to 6.5 s
  // buffered when the fault starts, so segments are held for 36 s to get more than 20 s of BUFFERING
  // (run 1 of the first series held them for 26 s: BUFFERING lasted 18.5 s and, correctly, nothing was reloaded).
  const HOLD_S = 36;
  const ctx = await playing(10000);
  try {
    const checks = []; const m = {};
    const faultAt = Date.now();
    server.setFault({ mode: 'stall', seconds: HOLD_S, token: TOKEN });
    let reloadLine = null;
    try { reloadLine = await waitUntil(() => receiverLines(ctx, /reload: buffering for 20 s/)[0], 34000, 'the buffering reload'); } catch { /* reported */ }
    m.reload_ms_after_fault = reloadLine ? Date.now() - faultAt : null;
    const firstBuffering = ctx.platform.mediaStatuses(faultAt).find((x) => x.playerState === 'BUFFERING');
    m.buffering_ms_before_reload = reloadLine && firstBuffering ? faultAt + m.reload_ms_after_fault - firstBuffering.at : null;
    let ok = null;
    try { ok = await waitUntil(async () => { const x = await ctx.last(); return x && x.at > faultAt + HOLD_S * 1000 && x.extra.state === 'PLAYING' && x.extra.overlay === 'playing' ? x : null; }, 55000, 'PLAYING after the stall'); } catch { /* reported */ }
    await sleep(6000);
    const after = await ctx.last();
    m.playing_ms_after_release = ok ? ok.at - (faultAt + HOLD_S * 1000) : null;
    m.reloads = receiverLines(ctx, /reload:/).length; m.retries = receiverLines(ctx, /retry \d of/).length;
    m.latency_after_s = after && after.extra ? Math.round(after.extra.latency * 10) / 10 : null;
    checks.push(check('one reload after 20 s of BUFFERING', !!reloadLine && m.reloads === 1 && m.retries === 0 && m.buffering_ms_before_reload >= 19500 && m.buffering_ms_before_reload <= 23000, `${m.reloads} reload(s), ${m.retries} retry(ies), after ${m.buffering_ms_before_reload} ms of BUFFERING`, '1 reload at 20 s'));
    checks.push(check('PLAYING within 10 s of the release', !!ok && m.playing_ms_after_release <= 10000 && after && after.extra.state === 'PLAYING', ok ? `${m.playing_ms_after_release} ms after release` : 'never', '<= 10000 ms'));
    // The reload happens while segments are still held, so the new playhead (placed at the reload) waits for the
    // release: latency = normal + the rest of the outage (measured 18.5 s for 10.6 s). It must stay under 20 s; the
    // receiver's catch-up seek (case g) brings it back after 2 minutes of clean playback.
    m.outage_after_reload_s = m.reload_ms_after_fault ? Math.round((HOLD_S * 1000 - m.reload_ms_after_fault) / 100) / 10 : null;
    checks.push(check('latency under 20 s after the reload', typeof m.latency_after_s === 'number' && m.latency_after_s < 20, `${m.latency_after_s} s (outage went on ${m.outage_after_reload_s} s after the reload)`, '< 20 s'));
    return { name: 'h-buffering-20s-reload', checks, metrics: m, detail: await trace(ctx, faultAt) };
  } finally { server.setFault({ mode: 'none' }); await ctx.close(); }
}

/** Segments are noise for `seconds`: a fatal decode error. Returns what the receiver did. */
async function corrupt(seconds, waitMs, untilErrorScreen = false) {
  const ctx = await playing(8000);
  try {
    const faultAt = Date.now();
    server.setFault({ mode: 'corrupt', seconds, token: TOKEN });
    if (untilErrorScreen) await waitUntil(() => ctx.page.evaluate(() => document.getElementById('overlay').getAttribute('data-state') === 'error'), waitMs, 'the error screen').catch(() => {});
    else await sleep(waitMs);
    const after = await ctx.last();
    const lines = ctx.log.console.filter((l) => l.includes('[conspect-cast]')).map((l) => l.replace(/^.*\[conspect-cast\] /, ''));
    const retryDelays = lines.filter((l) => /^retry \d of 3 in/.test(l)).map((l) => Number(/in (\d+) ms/.exec(l)[1]));
    const errors = lines.filter((l) => /^error /.test(l));
    const screen = await ctx.page.evaluate(() => ({ state: document.getElementById('overlay').getAttribute('data-state'), text: document.getElementById('overlay').innerText.replace(/\s+/g, ' ').trim() }));
    const reports = reportsSince(ctx, faultAt);
    const fatal = ctx.platform.mediaStatuses(faultAt).filter((x) => x.playerState === 'IDLE' && x.idleReason === 'ERROR').length;
    const loads = server.requestsSince(TOKEN, faultAt).filter((r) => r.file === 'master.m3u8');
    return { ctx, faultAt, after, retryDelays, errors, screen, reports, fatal, loads, detail: await trace(ctx, faultAt) };
  } catch (e) { await ctx.close(); throw e; }
}

async function caseI() {
  // Segments are noise for 5 s. Three outcomes were seen in Chrome, depending on which segments get hit: a fatal
  // decode error (IDLE/ERROR, then the receiver's retry after 2 s); a silent stall with data buffered and no error
  // (then the receiver's 20 s BUFFERING watchdog reloads once); or Shaka skips the unparsable segments itself.
  // Whatever the path, the stream must come back by itself, without a loop. Case j pins the retry path.
  const r = await corrupt(5, 45000);
  try {
    const m = { retry_delays_ms: r.retryDelays, fatal_idle_statuses: r.fatal, first_error: (r.errors[0] || '').slice(0, 80), latency_after_s: r.after && r.after.extra ? Math.round(r.after.extra.latency * 10) / 10 : null };
    // recovered = the first `playing` report after the last report that was not `playing`
    let lastBad = -1; r.reports.forEach((x, i) => { if (x.state !== 'playing') lastBad = i; });
    const back = r.reports.slice(lastBad + 1).find((x) => x.state === 'playing' && x.at > r.faultAt + 5000) || null;
    m.recovered_ms = back ? back.at - r.faultAt : null;
    m.reloads = r.ctx.log.console.filter((l) => /\[conspect-cast\] reload: buffering/.test(l)).length;
    m.gaps_jumped = r.after && r.after.extra ? r.after.extra.gaps : null;
    m.path = r.fatal >= 1 ? 'fatal-error-retry' : (m.reloads === 1 ? 'silent-stall-watchdog-reload' : (m.reloads === 0 ? 'player-skipped-the-bad-segments' : 'reload-loop'));
    const pathOk = m.path === 'fatal-error-retry' ? (r.retryDelays.length >= 1 && r.retryDelays.every((d, i) => d === [2000, 4000, 8000][i])) : m.path !== 'reload-loop';
    const checks = [
      check('bounded recovery: retry 2/4/8 s, one watchdog reload, or the player skips the bad segments', pathOk, `${m.path}, retries ${JSON.stringify(r.retryDelays)}, reloads ${m.reloads}`, 'no loop'),
      check('PLAYING again within 45 s of the fault', !!back && m.recovered_ms <= 45000 && r.after.extra.state === 'PLAYING' && r.after.extra.overlay === 'playing', back ? `${m.recovered_ms} ms, ${r.after.extra.state}` : `not recovered (${r.screen.state})`, '<= 45000 ms'),
      check('error cleared from the report after recovery', r.reports.length && r.reports[r.reports.length - 1].error === null, JSON.stringify(r.reports.length ? r.reports[r.reports.length - 1].error : 'no report').slice(0, 100), 'null')
    ];
    return { name: 'i-corrupt-segments-5s', checks, metrics: m, detail: r.detail };
  } finally { server.setFault({ mode: 'none' }); await r.ctx.close(); }
}

async function caseJ() {
  // The decode error does not go away: exactly 3 retries (2, 4, 8 s), then the error screen; no further loads.
  const r = await corrupt(300, 120000, true);
  try {
    const m = { retry_delays_ms: r.retryDelays, screen: r.screen.text, master_requests: r.loads.length, error_screen_ms: Date.now() - r.faultAt };
    const last = r.reports[r.reports.length - 1] || {};
    m.error = last.error || null;
    const t1 = Date.now(); await sleep(12000);
    m.master_requests_12s_after = server.requestsSince(TOKEN, t1).filter((x) => x.file === 'master.m3u8').length;
    const checks = [
      check('exactly three retries: 2 s, 4 s, 8 s', JSON.stringify(r.retryDelays) === '[2000,4000,8000]', JSON.stringify(r.retryDelays), '[2000,4000,8000]'),
      check('error screen: cannot decode + H.264 profile/level + codes', r.screen.state === 'error' && /This TV cannot decode the stream \(H\.264 High level 4\.1\)\./.test(r.screen.text) && /shaka 3\d{3}/.test(r.screen.text), `${r.screen.state}: "${r.screen.text}"`, 'sentence + code line'),
      check('error report: state error, shaka category 3, severity 2', last.state === 'error' && last.error && last.error.shaka_category === 3 && last.error.severity === 2, JSON.stringify(last.error || null).slice(0, 220), 'error report'),
      check('no load after the third retry', m.master_requests_12s_after === 0, m.master_requests_12s_after, '0'),
      check('the app stays open on the error screen (idle timer, not a crash)', !r.ctx.platform.state.closedAt, r.ctx.platform.state.closedAt ? 'closed' : 'open', 'open')
    ];
    await r.ctx.page.screenshot({ path: path.join(outDir, 'receiver-decode-error.png') });
    return { name: 'j-fatal-error-persists', checks, metrics: m, detail: r.detail };
  } finally { server.setFault({ mode: 'none' }); await r.ctx.close(); }
}

/**
 * A start on a young stream: a brand-new generator, LOAD at exactly `n` listed segments, 30 s of playback.
 * The engine hands a receiver either a full 10-segment window (pre-roll) or, without pre-roll, a stream with at
 * least 5 segments (its `ready` rule). 3 segments is kept for the record only.
 */
async function startAt(n, name, informational) {
  // The receiver page is opened first (a real sender launches the app, waits for `ready`, then LOADs).
  const ctx = await openReceiver(server.secureOrigin + '/cast/receiver/', startFakePlatform);
  let watch = null;
  try {
    await generator.stop();
    generator = newGenerator(false);
    watch = watchPlaylist(streamDir);
    await waitForSegments(streamDir, n, 60000, generator.tag);
    const segmentsAtLoad = (fs.readFileSync(path.join(streamDir, 'video.m3u8'), 'utf8').match(/#EXTINF/g) || []).length;
    const loadAt = Date.now();
    ctx.platform.load(senderMedia(`${server.secureOrigin}/cast/${TOKEN}/master.m3u8`));
    let first = null; try { first = await ctx.platform.waitForMediaStatus((x) => x.playerState === 'PLAYING', 15000, loadAt); } catch { /* reported */ }
    await sleep(30000);
    const probe = await ctx.probe(); const end = Date.now();
    const s2 = probe.samples.filter((x) => probe.startedAt && x.at >= probe.startedAt);
    const lat = s2.map((x) => x.extra && x.extra.latency).filter((x) => typeof x === 'number' && x > 0);
    const frozen = s2.filter((x, i) => i > 0 && x.ct - s2[i - 1].ct < 0.5).length;
    const st = ctx.platform.mediaStatuses(loadAt); let maxBuf = 0;
    for (let i = 0; i < st.length; i++) if (probe.startedAt && st[i].at >= probe.startedAt && st[i].playerState === 'BUFFERING') { const nx = st.slice(i + 1).find((x) => x.playerState !== 'BUFFERING'); maxBuf = Math.max(maxBuf, (nx ? nx.at : end) - st[i].at); }
    const m = { segments_at_load: segmentsAtLoad, to_playing_ms: first ? first.at - loadAt : null, start_ms: probe.startedAt ? probe.startedAt - loadAt : null,
      latency_min_s: lat.length ? Math.round(Math.min(...lat) * 10) / 10 : null, latency_max_s: lat.length ? Math.round(Math.max(...lat) * 10) / 10 : null,
      buffer_min_s: s2.length > 3 ? Math.round(Math.min(...s2.slice(3).map((x) => x.ahead)) * 10) / 10 : null, frozen_samples: frozen, max_buffering_ms: maxBuf,
      // the stream itself: longest time between two published segments while playing (nominal 2002 ms)
      stream_max_segment_interval_ms: watch.maxIntervalMs(loadAt) };
    // the player: how long after its publication was each new video segment fetched (playlist poll + request)?
    const fetched = server.requestsSince(TOKEN, loadAt).filter((r) => r.status === 200 && /^seg-video-/.test(r.file));
    const lags = watch.published(loadAt).map((pub) => { const r = fetched.find((x) => x.file === pub.name); return r ? r.at - pub.at : null; }).filter((x) => x !== null);
    m.fetch_lag_max_ms = lags.length ? Math.max(...lags) : null;
    m.fetch_lag_avg_ms = lags.length ? Math.round(lags.reduce((a, b) => a + b, 0) / lags.length) : null;
    const checks = [
      check(`LOAD sent at ${n} segments`, segmentsAtLoad === n, segmentsAtLoad, String(n)),
      check('MEDIA_STATUS PLAYING within 15 s', !!first, m.to_playing_ms, '<= 15000 ms'),
      check('picture moving under 3 s after LOAD', m.start_ms !== null && m.start_ms < 3000, m.start_ms, '< 3000 ms'),
      check('no freeze and no BUFFERING over 1 s in the first 30 s', frozen === 0 && maxBuf <= 1000, `${frozen} frozen sample(s), ${maxBuf} ms buffering`, '0'),
      check('latency between 6 and 10 s', lat.length > 5 && m.latency_min_s >= 6 && m.latency_max_s <= 10, `${m.latency_min_s}..${m.latency_max_s}`, '6..10 s'),
      check('buffer of at least 3 s held', m.buffer_min_s >= 3, `${m.buffer_min_s} s (segments published at most ${m.stream_max_segment_interval_ms} ms apart, nominal 2002; fetched at most ${m.fetch_lag_max_ms} ms after publication)`, '>= 3 s')
    ];
    return { name, informational, checks, metrics: m, detail: await trace(ctx, loadAt) };
  } finally { if (watch) watch.stop(); await ctx.close(); }
}

async function caseK() {
  // --k-segments 3,5,10 (default). 5 (the engine's `ready` minimum) and 10 (pre-roll: a full window) are required;
  // any other count is informational: recorded in the summary, never counted as pass or fail.
  const names = { 3: 'k0-cold-start-3-segments', 5: 'k2-start-5-segments', 10: 'k1-start-full-window' };
  const parts = [];
  try {
    for (const n of String(args['k-segments']).split(',').map(Number)) parts.push(await startAt(n, names[n] || `kx-start-${n}-segments`, n !== 5 && n !== 10));
  } finally { await waitForSegments(streamDir, 10, 40000, generator.tag).catch(() => {}); }
  return parts;
}

async function caseL() {
  // supportedMediaCommands is advisory: a PAUSE sent anyway (voice) pauses, PLAY resumes. A stream left paused is
  // closed by the receiver (20 min in the shipped file; this case serves a copy with that constant set to 6 s).
  const alt = path.join(outDir, 'receiver-short-pause'); fs.mkdirSync(alt, { recursive: true });
  for (const f of ['index.html', 'receiver.css']) fs.copyFileSync(path.join(RECEIVER_DIR, f), path.join(alt, f));
  const js = fs.readFileSync(path.join(RECEIVER_DIR, 'receiver.js'), 'utf8');
  const patched = js.replace('var PAUSED_STOP_MS = 20 * 60 * 1000;', 'var PAUSED_STOP_MS = 6000;');
  fs.writeFileSync(path.join(alt, 'receiver.js'), patched);
  const server2 = await startServer({ tokens: { [TOKEN]: streamDir }, receiverDir: alt, tls: { key: path.join(outDir, 'tls', 'key.pem'), cert: path.join(outDir, 'tls', 'cert.pem') } });
  const ctx = await openReceiver(server2.secureOrigin + '/cast/receiver/', startFakePlatform);
  try {
    const checks = [check('test copy of receiver.js patched', patched !== js, patched !== js, 'true')]; const m = {};
    const t0 = Date.now(); ctx.platform.load(senderMedia(`${server2.secureOrigin}/cast/${TOKEN}/master.m3u8`));
    await ctx.platform.waitForMediaStatus((x) => x.playerState === 'PLAYING', 15000, t0); await sleep(5000);
    let t = Date.now(); ctx.platform.media({ type: 'PAUSE', mediaSessionId: ctx.platform.mediaSessionId() });
    const paused = await ctx.platform.waitForMediaStatus((x) => x.playerState === 'PAUSED', 5000, t).catch(() => null); await sleep(1500);
    const screen = await ctx.last();
    checks.push(check('PAUSE from a sender pauses; screen says Paused', !!paused && /Paused/.test(screen.extra.overlayText), paused ? screen.extra.overlayText : 'not paused', 'PAUSED + Paused'));
    t = Date.now(); ctx.platform.media({ type: 'PLAY', mediaSessionId: ctx.platform.mediaSessionId() });
    const resumed = await ctx.platform.waitForMediaStatus((x) => x.playerState === 'PLAYING', 8000, t).catch(() => null); await sleep(3000);
    const after = await ctx.last();
    checks.push(check('PLAY resumes, overlay hidden', !!resumed && after.extra.state === 'PLAYING' && after.extra.overlay === 'playing', resumed ? `${after.extra.state} / ${after.extra.overlay}` : 'not resumed', 'PLAYING'));
    checks.push(check('a short pause does not close the app', !ctx.platform.state.closedAt, ctx.platform.state.closedAt ? 'closed' : 'open', 'open'));
    t = Date.now(); ctx.platform.media({ type: 'PAUSE', mediaSessionId: ctx.platform.mediaSessionId() });
    const closedAt = await ctx.platform.waitForClose(12000).catch(() => 0);
    m.closed_ms_after_pause = closedAt ? closedAt - t : null;
    checks.push(check('left paused: the app closes after the limit (6 s here, 20 min shipped)', closedAt > 0 && m.closed_ms_after_pause >= 5500 && m.closed_ms_after_pause <= 9000, m.closed_ms_after_pause, '6000..9000 ms'));
    return { name: 'l-pause-and-paused-stop', checks, metrics: m };
  } finally { await ctx.close(); await server2.close(); }
}

async function caseM() {
  // The server becomes unreachable mid-play (connection refused): fatal network error, three retries, then the
  // "cannot reach the Conspect server" screen. Uses its own server, which it closes.
  const server2 = await startServer({ tokens: { [TOKEN]: streamDir }, tls: { key: path.join(outDir, 'tls', 'key.pem'), cert: path.join(outDir, 'tls', 'cert.pem') } });
  const ctx = await openReceiver(server2.secureOrigin + '/cast/receiver/', startFakePlatform);
  try {
    const t0 = Date.now(); ctx.platform.load(senderMedia(`${server2.secureOrigin}/cast/${TOKEN}/master.m3u8`));
    await ctx.platform.waitForMediaStatus((x) => x.playerState === 'PLAYING', 15000, t0); await sleep(8000);
    const lostAt = Date.now(); await server2.close();
    const read = () => ctx.page.evaluate(() => ({ state: document.getElementById('overlay').getAttribute('data-state'), text: document.getElementById('overlay').innerText.replace(/\s+/g, ' ').trim() }));
    let screen = await read();
    try { await waitUntil(async () => { screen = await read(); return screen.state === 'error'; }, 90000, 'the error screen'); } catch { /* reported */ }
    const m = { error_screen_ms: Date.now() - lostAt };
    await sleep(1500); screen = await read();                    // CAF's trailing 905 must not replace the sentence
    const lines = ctx.log.console.filter((l) => l.includes('[conspect-cast]')).map((l) => l.replace(/^.*\[conspect-cast\] /, ''));
    m.retry_delays_ms = lines.filter((l) => /^retry \d of 3 in/.test(l)).map((l) => Number(/in (\d+) ms/.exec(l)[1]));
    const rep = reportsSince(ctx, lostAt).pop() || {};
    m.error = rep.error || null;
    const checks = [
      check('three retries: 2 s, 4 s, 8 s', JSON.stringify(m.retry_delays_ms) === '[2000,4000,8000]', JSON.stringify(m.retry_delays_ms), '[2000,4000,8000]'),
      check('error screen: the TV cannot reach the Conspect server', screen.state === 'error' && /The TV cannot reach the Conspect server\./.test(screen.text) && /shaka 1002/.test(screen.text), `${screen.state}: "${screen.text}"`, 'network sentence + code'),
      check('error report to senders carries the network cause', rep.state === 'error' && rep.error && rep.error.shaka_code === 1002 && /cannot reach/.test(rep.error.message), JSON.stringify(rep.error || null).slice(0, 200), 'shaka 1002'),
      check('error screen within 90 s of the loss', m.error_screen_ms <= 90000, m.error_screen_ms, '<= 90000 ms')
    ];
    return { name: 'm-server-unreachable', checks, metrics: m };
  } finally { await ctx.close(); await server2.close().catch(() => {}); }
}

async function caseN() {
  // The hosted receiver (cast.conspect.studio) plays an engine on ANOTHER https origin: the page comes from
  // `server`, the stream and the report POST go to `server3` (another port = another origin, CORS on every
  // request). Then the engine revokes the token (404): the receiver ends the cast from the cross-origin answers.
  const server3 = await startServer({ tokens: { [TOKEN]: streamDir }, tls: { key: path.join(outDir, 'tls', 'key.pem'), cert: path.join(outDir, 'tls', 'cert.pem') } });
  const ctx = await openReceiver(server.secureOrigin + '/cast/receiver/', startFakePlatform);
  try {
    const checks = []; const m = { page_origin: server.secureOrigin, stream_origin: server3.secureOrigin };
    const t0 = Date.now(); ctx.platform.load(senderMedia(`${server3.secureOrigin}/cast/${TOKEN}/master.m3u8`));
    let first = null; try { first = await ctx.platform.waitForMediaStatus((x) => x.playerState === 'PLAYING', 15000, t0); } catch { /* reported */ }
    m.to_playing_ms = first ? first.at - t0 : null;
    checks.push(check('cross-origin stream: PLAYING within 15 s', !!first, first ? `${m.to_playing_ms} ms` : 'no PLAYING', '<= 15000 ms'));
    const a = await ctx.last(); await sleep(20000); const b = await ctx.last();
    checks.push(check('cross-origin stream: plays 20 s', b && b.extra.state === 'PLAYING' && b.ct - a.ct > 17 && b.vb > a.vb && b.ab > a.ab, b ? `${b.extra.state}, advanced ${Math.round((b.ct - a.ct) * 10) / 10} s` : 'none', 'PLAYING, >= 17 s'));
    const posted = server3.reports.filter((r) => r.token === TOKEN && r.at >= t0).length;
    const wrong = server.reports.filter((r) => r.token === TOKEN && r.at >= t0).length;
    m.reports_stream_origin = posted; m.reports_page_origin = wrong;
    checks.push(check('reports go to the stream origin, never the page origin', posted >= 3 && wrong === 0, `${posted} to the stream origin, ${wrong} to the page origin`, '>= 3 and 0'));
    m.page_origin_media_requests = server.requestsSince(TOKEN, t0).length;
    checks.push(check('no media request to the page origin', m.page_origin_media_requests === 0, m.page_origin_media_requests, '0'));
    checks.push(check('zero CSP violations', (await ctx.log.csp()).length === 0, (await ctx.log.csp()).length, '0'));
    const faultAt = Date.now(); server3.setFault({ mode: '404', token: TOKEN });
    let rep = null;
    try { rep = await waitUntil(() => reportsSince(ctx, faultAt).find((r) => r.state === 'error' && r.error && /ended or its link expired/.test(r.error.message)), 20000, 'the ended-or-expired report'); } catch { /* reported */ }
    m.revoked_report_ms = rep ? rep.at - faultAt : null;
    checks.push(check('cross-origin revoke: "ended or expired" within 20 s', !!rep, rep ? `${m.revoked_report_ms} ms` : 'none', '<= 20000 ms'));
    return { name: 'n-cross-origin-engine', checks, metrics: m };
  } finally { await ctx.close(); await server3.close().catch(() => {}); }
}

async function caseO() {
  // MEDIA-2: stream A is LOADed first, which fires its own-LOAD master probe (onLoad's `probe(...)` call). The
  // server holds that GET for 2 s then answers 404. While it is in flight, the sender LOADs stream B (a fresh
  // `current`, a bumped loadGeneration). A's probe completion must be dropped: it must not call endCast() and
  // must not stop B. B must reach PLAYING and keep playing with no endCast/app-stop.
  const ctx = await openReceiver(server.secureOrigin + '/cast/receiver/', startFakePlatform);
  try {
    const checks = []; const m = {};
    server.setFault({ mode: 'delay404', seconds: 2, token: TOKEN });
    const t0 = Date.now();
    ctx.platform.load(senderMedia(`${server.secureOrigin}/cast/${TOKEN}/master.m3u8`));
    await sleep(300);                                        // A's own-load probe is now in flight, held by the server
    const t1 = Date.now();
    ctx.platform.load(senderMedia(`${server.secureOrigin}/cast/${TOKEN_B}/master.m3u8`));
    let first = null; try { first = await ctx.platform.waitForMediaStatus((x) => x.playerState === 'PLAYING', 15000, t1); } catch { /* reported */ }
    m.to_playing_ms = first ? first.at - t1 : null;
    checks.push(check('B reaches PLAYING within 15 s of its LOAD', !!first, first ? `${m.to_playing_ms} ms` : 'no PLAYING', '<= 15000 ms'));
    // Wait past A's delayed 404 (at t0 + 2000) plus the receiver's 3 s probe-completion handling margin.
    await sleep(Math.max(0, t0 + 2000 - Date.now()) + 3000);
    const after = await ctx.last();
    const a1 = await ctx.last();
    await sleep(8000);
    const a2 = await ctx.last();
    checks.push(check('B still PLAYING after A\'s delayed 404 resolves, no endCast', after && after.extra.state === 'PLAYING' && after.extra.overlay === 'playing', after ? `${after.extra.state} / ${after.extra.overlay}` : 'none', 'PLAYING'));
    checks.push(check('B keeps advancing 8 s later (not stopped)', a2 && a1 && a2.ct > a1.ct && a2.extra.state === 'PLAYING', a2 ? `${a2.extra.state}, advanced ${Math.round((a2.ct - a1.ct) * 10) / 10} s` : 'none', 'PLAYING, advancing'));
    m.end_cast_lines = receiverLines(ctx, /token gone/).length;
    m.closed = ctx.platform.state.closedAt > 0;
    checks.push(check('no endCast (token gone) fired for the stale A probe', m.end_cast_lines === 0 && !m.closed, `${m.end_cast_lines} endCast line(s), closed=${m.closed}`, '0, not closed'));
    // IDLE at/just after the LOAD request itself (before the first PLAYING) is CAF's normal transient status, not
    // a stop; only an IDLE after B is already PLAYING would mean the stale A probe tore B down.
    const idleAfterB = first ? ctx.platform.mediaStatuses(first.at).filter((s) => s.playerState === 'IDLE').length : -1;
    checks.push(check('no IDLE / app stop after B reaches PLAYING', idleAfterB === 0, idleAfterB, '0'));
    return { name: 'o-stale-probe-during-another-load', checks, metrics: m, detail: await trace(ctx, t0) };
  } finally { server.setFault({ mode: 'none' }); await ctx.close(); }
}

const CASES = { a: caseA, b: caseB, c: caseC, d: caseD, e: caseE, f: caseF, g: caseG, h: caseH, i: caseI, j: caseJ, k: caseK, l: caseL, m: caseM, n: caseN, o: caseO };
let failures = 1;
try {
  execFileSync(path.join(here, 'make-cert.sh'), [path.join(outDir, 'tls')]);
  const upstream = args.upstream && args.upstream !== 'true' ? String(args.upstream) : null;
  if (!upstream) { generator = newGenerator(false); await waitForSegments(streamDir, 10, 60000, generator.tag); }
  server = await startServer({ tokens: { [TOKEN]: upstream || streamDir, [TOKEN_B]: upstream || streamDir }, tls: { key: path.join(outDir, 'tls', 'key.pem'), cert: path.join(outDir, 'tls', 'cert.pem') } });
  const parts = [];
  for (const id of wanted) {
    if (!CASES[id]) continue;
    if ((id === 'd' || id === 'k') && upstream) { console.log('case d skipped: the stream is not generated by this script'); continue; }
    try { const r = await CASES[id](); parts.push(...(Array.isArray(r) ? r : [r])); }
    catch (e) { parts.push({ name: `${id}-crashed`, checks: [check('case runs to completion', false, String(e && e.message || e).slice(0, 300), 'no exception')], metrics: {} }); }
  }
  save('faults.detail.json', { parts });
  failures = finish('test-receiver-faults', parts, { cases: wanted });
} catch (e) {
  console.error('test-receiver-faults failed:', e && e.stack || e);
  console.log('RESULT ' + JSON.stringify({ test: 'test-receiver-faults', pass: false, failed: [String(e && e.message || e)] }));
} finally {
  clearTimeout(hardStop);
  if (generator) await generator.stop().catch(() => {});
  if (server) await server.close().catch(() => {});
}
process.exit(failures ? 1 : 0);
