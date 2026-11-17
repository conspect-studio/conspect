// Check (h) of run.sh (#418): the REAL engine's cast output carries 2 audio + 1 subtitle rendition.
//   node test-engine-tracks.mjs --api <engine http base> --origin <https proxy origin> --stream <id> --path <master path>
//        --cast-dir <engine cast dir> --validator <cast_validate binary>
// Bearer: CAST_PROOF_BEARER. run.sh configures the cast output with audio slot 0 = the program (language en, NAME
// "Program"), slot 1 = track "commentary" (route title "Commentary", language de, from the caption file's 880 Hz
// tone) and one subtitle slot "English"/en patched to the caption file source, whose subrip cues read
// "CAP <n> T=<file seconds>", one per 2 s (see run.sh).
//  h1  the validator's JSON report: ok, 2 audio renditions, 1 subtitle rendition, subtitle cues > 0
//  h2  Shaka 4.15.56 with the SDK's default config (as test-shaka.mjs): 2 audio languages en/de with the configured
//      names, 1 text track en "English"; selectVariantTrack(de, clearBuffer false) switches with no stall > 1 s, no
//      skipped media, audio decoding and the de rendition fetched; the text track shown carries cues matching the
//      pattern, every cue on one timeline (+-0.5 s). Informational (h2i): a CAF-style switch back (clearBuffer,
//      safeMargin 0) under CAF's default rebufferingGoal of 10 s holds the picture while 10 s are refetched (measured
//      2 to 6 s); our receiver sets rebufferingGoal 4 and h3 measures that switch with the real SDK.
//  h3  the real CAF SDK + the engine's receiver (fake platform): MEDIA_STATUS lists 2 AUDIO + 1 TEXT tracks with
//      names/languages; EDIT_TRACKS_INFO [Commentary, English] switches to de and shows the text; the receiver's
//      report reaches the ENGINE (GET /api/v1/cast/streams/{id} last_report carries tracks with the new choice)
import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { startFakePlatform } from './fake-cast-platform.mjs';
import { parseArgs, launchBrowser, watchPage, installSampler, openReceiver, senderMedia, check, finish, save, sleep, waitUntil, getJson, here } from './lib.mjs';

const args = parseArgs(process.argv.slice(2), {});
for (const k of ['api', 'origin', 'stream', 'path', 'cast-dir', 'validator']) if (!args[k]) { console.error(`missing --${k}`); process.exit(2); }
const masterUrl = args.origin + args.path;
const hardStop = setTimeout(() => { console.error('hard timeout'); process.exit(3); }, 270000);
const CUE = /^CAP (\d+) T=(\d+)$/;
const NAMES = { en: 'Program', de: 'Commentary', text: 'English' };
const r2 = (x) => Math.round(x * 100) / 100;

/** Cue timing: each cue's media start minus its file time is the file's offset on the stream timeline; one
 * timeline means every cue gives the same offset (+-0.5 s). Returns {cues, spread, offset}. */
function cueTimeline(raw) {
  const cues = raw.map(([start, end, text]) => { const m = CUE.exec(String(text).trim()); return m ? { start, end, n: Number(m[1]), t: Number(m[2]) } : null; }).filter(Boolean);
  const offsets = cues.map((c) => c.start - c.t);
  return { cues, spread: offsets.length ? r2(Math.max(...offsets) - Math.min(...offsets)) : null, offset: offsets.length ? r2(offsets[offsets.length - 1]) : null };
}

/** Stall figures over the 1 s samples since t0: worst (wall - clock advance) and the total lag. */
function stallFigures(samples, t0) {
  const s = samples.filter((x) => x.at >= t0); let worst = 0;
  for (let i = 1; i < s.length; i++) worst = Math.max(worst, (s[i].at - s[i - 1].at) / 1000 - (s[i].ct - s[i - 1].ct));
  const adv = s.length > 1 ? s[s.length - 1].ct - s[0].ct : 0; const wall = s.length > 1 ? (s[s.length - 1].at - s[0].at) / 1000 : 0;
  return { n: s.length, worst: r2(worst), advance: r2(adv), wall: r2(wall), lag: r2(wall - adv) };
}

// ---- h1: the validator
function h1() {
  const r = spawnSync(args.validator, [args['cast-dir'], '--json'], { encoding: 'utf8', timeout: 30000 });
  let j = null; try { j = JSON.parse((r.stdout || '').trim().split('\n').pop()); } catch { /* checked */ }
  const m = j ? { audio_renditions: j.audio_renditions, subtitle_renditions: j.subtitle_renditions, subtitle_segments: j.subtitle_segments, subtitle_cues: j.subtitle_cues, video_segments: j.video_segments, errors: j.errors } : { stderr: (r.stderr || '').slice(0, 300) };
  const checks = [
    check('validator passes (JSON report ok, exit 0)', j && j.ok === true && r.status === 0, j ? `ok ${j.ok}, exit ${r.status}${j.errors.length ? ': ' + j.errors.slice(0, 2).join(' | ') : ''}` : `exit ${r.status}, no JSON`, 'ok'),
    check('2 audio renditions + 1 subtitle rendition', j && j.audio_renditions === 2 && j.subtitle_renditions === 1, j ? `${j.audio_renditions} audio, ${j.subtitle_renditions} subtitle` : 'none', '2 + 1'),
    check('subtitle segments for the whole window, cues > 0', j && j.subtitle_segments === j.video_segments && j.subtitle_cues > 0, j ? `${j.subtitle_segments} of ${j.video_segments} segments, ${j.subtitle_cues} cues` : 'none', 'all, > 0')
  ];
  return { name: 'h1-validator-tracks', checks, metrics: m };
}

// ---- h2: Shaka 4.15.56, the SDK's default config
// What CAF 3.0.0157 passes to shaka.Player.configure by default (copied from test-shaka.mjs cafConfig({}), which is
// a script, not a module).
const RETRY = { maxAttempts: 4, baseDelay: 400, backoffFactor: 2, timeout: 30000 };
const CAF_DEFAULT = { abr: { defaultBandwidthEstimate: 2e6 }, manifest: { retryParameters: { ...RETRY } },
  streaming: { rebufferingGoal: 10, retryParameters: { ...RETRY }, stallThreshold: 5, dispatchAllEmsgBoxes: true }, mediaSource: { codecSwitchingStrategy: 'reload' } };
const shakaJs = fs.readFileSync(path.join(here, 'node_modules', 'shaka-player', 'dist', 'shaka-player.compiled.js'));
const html = '<!doctype html><meta charset="utf-8"><body style="margin:0;background:#000"><video id="v" autoplay muted style="width:960px;height:540px"></video><script src="/shaka.js"></script><script src="/run.js"></script>';
const runJs = `(${async function run() {
  const cfg = await (await fetch('/config')).json();
  const R = window.__run = { loadAt: Date.now(), loaded: false, errors: [], version: shaka.Player.version };
  shaka.polyfill.installAll();
  const v = document.getElementById('v'); const p = window.__player = new shaka.Player();
  await p.attach(v); p.configure(cfg.config);
  p.addEventListener('error', (e) => R.errors.push({ code: e.detail && e.detail.code, data: String(e.detail && e.detail.data).slice(0, 200) }));
  try { await p.load(cfg.url, null, 'application/vnd.apple.mpegurl'); R.loaded = true; v.play().catch(() => {}); }
  catch (e) { R.errors.push({ code: e && e.code, data: String(e && e.data || e).slice(0, 200), load: true }); }
}.toString()})();`;
function shakaState() {
  const p = window.__player; const v = document.getElementById('v');
  const act = p.getVariantTracks().find((t) => t.active);
  const cues = []; const active = [];
  for (const t of v.textTracks) { if (t.mode !== 'showing') continue; if (t.cues) for (const c of t.cues) cues.push([c.startTime, c.endTime, c.text]); if (t.activeCues) for (const c of t.activeCues) active.push(c.text); }
  return { ct: v.currentTime, language: act ? act.language : null, label: act ? act.audioLabel || act.label : null,
    audio: p.getAudioTracks ? p.getAudioTracks().map((t) => [t.language, t.label]) : null,
    variants: p.getVariantTracks().map((t) => [t.language, t.audioLabel || t.label]),
    text: p.getTextTracks().map((t) => [t.language, t.label, t.kind]), textVisible: p.isTextTrackVisible(), cues, active };
}

async function h2() {
  const checks = []; const m = {};
  const server = http.createServer((req, res) => {
    const u = new URL(req.url, 'http://x');
    if (u.pathname === '/') { res.writeHead(200, { 'content-type': 'text/html' }); return res.end(html); }
    if (u.pathname === '/shaka.js') { res.writeHead(200, { 'content-type': 'text/javascript' }); return res.end(shakaJs); }
    if (u.pathname === '/run.js') { res.writeHead(200, { 'content-type': 'text/javascript' }); return res.end(runJs); }
    if (u.pathname === '/config') { res.writeHead(200, { 'content-type': 'application/json' }); return res.end(JSON.stringify({ url: masterUrl, config: CAF_DEFAULT })); }
    res.writeHead(404); res.end();
  });
  await new Promise((resolve) => server.listen(0, '::', resolve));
  const browser = await launchBrowser();
  try {
    const page = await browser.newPage(); const log = await watchPage(page);
    await page.evaluateOnNewDocument(installSampler);
    await page.goto(`http://localhost:${server.address().port}/`, { waitUntil: 'load', timeout: 30000 });
    const run = await waitUntil(() => page.evaluate(() => window.__run && (window.__run.loaded || window.__run.errors.length) ? window.__run : null), 30000, 'the Shaka load');
    checks.push(check('Shaka 4.15.56 loads the engine master', /^v?4\.15\.56/.test(run.version) && run.loaded, `${run.version}, loaded ${run.loaded}${run.errors.length ? ' ' + JSON.stringify(run.errors[0]) : ''}`, 'loaded'));
    await waitUntil(() => page.evaluate(() => window.__probe.startedAt), 15000, 'the picture moving').catch(() => null);
    await sleep(3000);
    const s0 = await page.evaluate(shakaState);
    const audio = s0.audio || [...new Map(s0.variants.map((x) => [x.join('|'), x])).values()];
    m.audio = audio; m.text = s0.text;
    const has = (lang, name) => audio.some(([l, n]) => l === lang && n === name);
    checks.push(check('2 audio languages, en/de with the configured names', audio.length === 2 && has('en', NAMES.en) && has('de', NAMES.de), JSON.stringify(audio), `en/${NAMES.en}, de/${NAMES.de}`));
    checks.push(check('1 text track, en', s0.text.length === 1 && s0.text[0][0] === 'en' && s0.text[0][1] === NAMES.text, JSON.stringify(s0.text), `en/${NAMES.text}`));
    checks.push(check('starts on the default audio (en)', s0.language === 'en', s0.language, 'en'));
    // Switch the audio language; then enable the text track.
    // Seamless switch: selectVariantTrack(de, clearBuffer = false) keeps what is buffered and appends de from the
    // buffer's end. (Measured: clearBuffer with safeMargin 0, the call CAF makes, holds the picture while CAF's
    // default rebufferingGoal of 10 s refetches (h2i, informational); safeMargin 5 left a hole Shaka jumped 7 s over.)
    const t0 = Date.now();
    await page.evaluate(() => { const p = window.__player; const act = p.getVariantTracks().find((t) => t.active); const t = p.getVariantTracks().find((x) => x.language === 'de' && x.videoId === act.videoId); if (t) p.selectVariantTrack(t, false); });
    let switched = null;
    try { switched = await waitUntil(async () => ((await page.evaluate(shakaState)).language === 'de' ? Date.now() : null), 8000, 'the de variant'); } catch { /* checked */ }
    m.audio_switch_ms = switched ? switched - t0 : null;
    checks.push(check('selectVariantTrack(de): the active variant becomes de', !!switched, switched ? `${m.audio_switch_ms} ms` : 'still en', 'within 8 s'));
    await page.evaluate(() => { const p = window.__player; const t = p.getTextTracks()[0]; if (t) p.selectTextTrack(t); p.setTextTrackVisibility(true); });
    await sleep(16000);   // the buffered en (about 10 s under CAF's defaults) plays out, then de
    const probe = await page.evaluate(() => ({ events: window.__probe.events, samples: window.__probe.samples }));
    const st = stallFigures(probe.samples, t0);
    let maxWait = 0;
    for (let i = 0; i < probe.events.length; i++) { const e = probe.events[i]; if (e.type !== 'waiting' || e.at < t0) continue; const end = probe.events.slice(i + 1).find((x) => x.type === 'playing' || x.type === 'canplay'); maxWait = Math.max(maxWait, (end ? end.at : Date.now()) - e.at); }
    m.stall = st; m.max_waiting_ms = maxWait;
    checks.push(check('no stall > 1 s across the switch, picture moving', st.n > 5 && st.lag <= 1 && st.worst <= 1 && maxWait <= 1000, `advanced ${st.advance} s in ${st.wall} s (lag ${st.lag} s, worst 1 s sample ${st.worst} s), longest waiting ${maxWait} ms`, 'lag <= 1 s, waiting <= 1000 ms'));
    const after = probe.samples.filter((x) => x.at >= t0); let abStuck = 0; let jump = 0;
    for (let i = 1; i < after.length; i++) { if (!(after[i].ab > after[i - 1].ab)) abStuck++; jump = Math.max(jump, (after[i].ct - after[i - 1].ct) - (after[i].at - after[i - 1].at) / 1000); }
    m.max_forward_jump_s = r2(jump);
    checks.push(check('no forward jump > 0.5 s (no media skipped)', after.length > 5 && jump <= 0.5, `${m.max_forward_jump_s} s`, '<= 0.5 s'));
    checks.push(check('audio keeps decoding across the switch', after.length > 5 && abStuck === 0, `${abStuck} stuck of ${after.length}`, '0 stuck'));
    const fetched = await page.evaluate((since) => performance.getEntriesByType('resource').filter((e) => performance.timeOrigin + e.startTime >= since).map((e) => new URL(e.name).pathname.split('/').pop()), t0);
    const deSegs = fetched.filter((f) => /^seg-a1-/.test(f)).length; const enSegs = fetched.filter((f) => /^seg-a-/.test(f)).length;
    m.segments_after_switch = { de: deSegs, en: enSegs };
    checks.push(check('after the switch Shaka fetches the de rendition (audio1), not en', deSegs >= 3 && deSegs > enSegs, `de ${deSegs}, en ${enSegs}`, 'de >= 3, > en'));
    let hit = null;
    try { hit = await waitUntil(async () => { const q = await page.evaluate(shakaState); const a = q.active.find((x) => CUE.test(String(x).trim())); return a && q.textVisible ? { text: a, q } : null; }, 8000, 'an active cue'); } catch { /* checked */ }
    const q = hit ? hit.q : await page.evaluate(shakaState);
    const tl = cueTimeline(q.cues);
    m.active_cue = hit ? hit.text : null; m.cues = tl.cues.length; m.cue_offset_s = tl.offset; m.cue_timeline_spread_s = tl.spread; m.ct = r2(q.ct);
    m.cue_sample = tl.cues.slice(-2).map((c) => [r2(c.start), r2(c.end), c.n, c.t]);
    checks.push(check('text track shown: a cue matching "CAP <n> T=<s>" is active', !!hit, hit ? `"${hit.text}" at ct ${m.ct}` : `visible ${q.textVisible}, ${q.cues.length} cue(s), active ${JSON.stringify(q.active)}`, 'active cue'));
    const activeOk = hit ? tl.cues.some((c) => `CAP ${c.n} T=${c.t}` === String(hit.text).trim() && c.start <= q.ct + 0.5 && q.ct <= c.end + 0.5) : false;
    checks.push(check('cues on one timeline (+-0.5 s), the active cue spans the playhead', tl.cues.length >= 2 && tl.spread <= 0.5 && activeOk, `${tl.cues.length} cues, spread ${tl.spread} s, offset ${tl.offset} s, last ${JSON.stringify(m.cue_sample)}`, '<= 0.5 s'));
    checks.push(check('zero Shaka errors, zero page errors', run.errors.length === 0 && (await page.evaluate(() => window.__run.errors.length)) === 0 && log.pageErrors.length === 0, `${(await page.evaluate(() => window.__run.errors)).length} / ${log.pageErrors.length}`, '0 / 0'));
    // Informational: back to en the way CAF switches (selectVariantTrack(track, clearBuffer = true), safeMargin 0)
    // under CAF's default config: how long the picture holds while rebufferingGoal (10 s) refetches.
    const t1 = Date.now();
    await page.evaluate(() => { const p = window.__player; const act = p.getVariantTracks().find((t) => t.active); const t = p.getVariantTracks().find((x) => x.language === 'en' && x.videoId === act.videoId); if (t) p.selectVariantTrack(t, true); });
    await sleep(12000);
    const cafSt = stallFigures((await page.evaluate(() => window.__probe.samples)), t1);
    const back = (await page.evaluate(shakaState)).language;
    const info = { name: 'h2i-caf-style-clear-buffer-switch', informational: true, metrics: { stall: cafSt, language: back },
      checks: [check('CAF-style switch (clearBuffer, safeMargin 0) under CAF defaults: no stall > 1 s', back === 'en' && cafSt.n > 5 && cafSt.lag <= 1, `${back}, advanced ${cafSt.advance} s in ${cafSt.wall} s (lag ${cafSt.lag} s)`, 'lag <= 1 s')] };
    save('engine-tracks-shaka.detail.json', { metrics: m, caf_style: info.metrics, state: q, events: probe.events, samples: await page.evaluate(() => window.__probe.samples) });
    return [{ name: 'h2-shaka-sdk-defaults-tracks', checks, metrics: m }, info];
  } finally { await browser.close().catch(() => {}); server.closeAllConnections(); server.close(); }
}

// ---- h3: CAF SDK + the engine's receiver (helpers as test-receiver-tracks.mjs)
function inPage() {
  const pm = cast.framework.CastReceiverContext.getInstance().getPlayerManager();
  const r = { audio: [] };
  try { r.audio = pm.getAudioTracksManager().getTracks(); } catch (e) { r.err = String(e); }
  const sp = pm.getShakaPlayer && pm.getShakaPlayer(); const v = window.__probe && window.__probe.video;
  r.ct = v ? v.currentTime : null;
  if (sp) { const act = sp.getVariantTracks().find((t) => t.active); r.variantLanguage = act ? act.language : null; r.textVisible = sp.isTextTrackVisible(); }
  r.cues = []; r.activeCues = [];
  if (v) for (const t of v.textTracks) { if (t.mode !== 'showing') continue; if (t.cues) for (const c of t.cues) r.cues.push([c.startTime, c.endTime, c.text]); if (t.activeCues) for (const c of t.activeCues) r.activeCues.push(c.text); }
  return r;
}

async function h3() {
  const checks = []; const m = {};
  const ctx = await openReceiver(args.origin + '/cast/receiver/', startFakePlatform);
  try {
    const state = () => ctx.page.evaluate(inPage);
    const loadAt = Date.now(); ctx.platform.load(senderMedia(masterUrl));
    let playing = null; try { playing = await ctx.platform.waitForMediaStatus((s) => s.playerState === 'PLAYING', 15000, loadAt); } catch { /* checked */ }
    checks.push(check('MEDIA_STATUS PLAYING within 15 s', !!playing, playing ? `${playing.at - loadAt} ms` : 'none', '<= 15000 ms'));
    await waitUntil(async () => (await state()).audio.length > 0, 10000, 'the track managers').catch(() => null);
    await sleep(3000);
    const withTracks = ctx.platform.mediaStatuses(loadAt).filter((x) => x.media && x.media.tracks);
    const tracks = withTracks.length ? withTracks[withTracks.length - 1].media.tracks : [];
    const audio = tracks.filter((t) => t.type === 'AUDIO'); const text = tracks.filter((t) => t.type === 'TEXT');
    const by = (list, name) => list.find((t) => t.name === name);
    const p = by(audio, NAMES.en); const c = by(audio, NAMES.de); const cap = by(text, NAMES.text);
    m.tracks = tracks.map((t) => [t.trackId, t.type, t.name, t.language]);
    checks.push(check('MEDIA_STATUS: 2 AUDIO + 1 TEXT track', audio.length === 2 && text.length === 1, `${audio.length} audio, ${text.length} text`, '2 + 1'));
    checks.push(check('names and languages from the engine config', p && p.language === 'en' && c && c.language === 'de' && cap && cap.language === 'en', JSON.stringify(m.tracks), `${NAMES.en}/en, ${NAMES.de}/de, ${NAMES.text}/en`));
    if (!(c && cap)) return { name: 'h3-receiver-tracks-engine', checks, metrics: m };
    const t0 = Date.now();
    ctx.platform.media({ type: 'EDIT_TRACKS_INFO', mediaSessionId: ctx.platform.mediaSessionId(), activeTrackIds: [c.trackId, cap.trackId] });
    let switched = null;
    try { switched = await waitUntil(async () => ((await state()).variantLanguage === 'de' ? Date.now() : null), 8000, 'the de variant'); } catch { /* checked */ }
    m.audio_switch_ms = switched ? switched - t0 : null;
    checks.push(check('EDIT_TRACKS_INFO [Commentary, English]: the active variant becomes de', !!switched, switched ? `${m.audio_switch_ms} ms` : 'still ' + (await state()).variantLanguage, 'de within 8 s'));
    await sleep(8000);
    const end = Date.now(); const st = ctx.platform.mediaStatuses(t0); let maxBuf = 0;
    for (let i = 0; i < st.length; i++) if (st[i].playerState === 'BUFFERING' && (i === 0 || st[i - 1].playerState !== 'BUFFERING')) { const next = st.slice(i + 1).find((x) => x.playerState !== 'BUFFERING'); maxBuf = Math.max(maxBuf, (next ? next.at : end) - st[i].at); }
    const stall = stallFigures((await ctx.probe()).samples, t0);
    m.max_buffering_ms = maxBuf; m.stall = stall;
    checks.push(check('no BUFFERING > 1 s and no stall > 1 s across the switch', maxBuf <= 1000 && stall.n > 5 && stall.lag <= 1, `BUFFERING ${maxBuf} ms, advanced ${stall.advance} s in ${stall.wall} s (worst sample ${stall.worst} s)`, '<= 1000 ms, lag <= 1 s'));
    const actives = ctx.platform.mediaStatuses(t0).filter((x) => Array.isArray(x.activeTrackIds)); const active = actives.length ? actives[actives.length - 1].activeTrackIds : null;
    checks.push(check('MEDIA_STATUS activeTrackIds = [Commentary, English]', active && active.length === 2 && active.includes(c.trackId) && active.includes(cap.trackId), JSON.stringify(active), JSON.stringify([c.trackId, cap.trackId])));
    let hit = null;
    try { hit = await waitUntil(async () => { const q = await state(); const a = q.activeCues.find((x) => CUE.test(String(x).trim())); return a && q.textVisible ? { text: a, q } : null; }, 8000, 'an active cue'); } catch { /* checked */ }
    const tl = cueTimeline(hit ? hit.q.cues : (await state()).cues);
    m.active_cue = hit ? hit.text : null; m.cues = tl.cues.length; m.cue_timeline_spread_s = tl.spread;
    checks.push(check('text shown: a "CAP <n> T=<s>" cue is active, cues on one timeline (+-0.5 s)', !!hit && tl.cues.length >= 2 && tl.spread <= 0.5, hit ? `"${hit.text}", ${tl.cues.length} cues, spread ${tl.spread} s` : 'no active cue', 'active, <= 0.5 s'));
    // The receiver reports every 5 s; the engine keeps the last one.
    const auth = { authorization: `Bearer ${process.env.CAST_PROOF_BEARER}` };
    let doc = null;
    try {
      doc = await waitUntil(async () => {
        const d = await getJson(`${args.api}/api/v1/cast/streams/${args.stream}`, 'GET', null, auth).catch(() => null);
        const lr = d && d.last_report; const tr = lr && lr.received_unix_ms > t0 + 500 && lr.report && lr.report.tracks;
        return tr && tr.audio && tr.audio.some((x) => x.name === NAMES.de && x.active) ? lr : null;
      }, 15000, 'a report with tracks at the engine');
    } catch { /* checked */ }
    const tr = doc && doc.report.tracks; m.engine_report_tracks = tr || null;
    const okShape = tr && tr.audio.length === 2 && tr.text.length === 1 && tr.audio.some((x) => x.name === NAMES.de && x.language === 'de' && x.active) && tr.audio.some((x) => x.name === NAMES.en && !x.active) && tr.text[0].name === NAMES.text && tr.text[0].active;
    checks.push(check('engine last_report carries tracks (Commentary + English active)', !!okShape, JSON.stringify(tr || null).slice(0, 300), 'tracks with the new choice'));
    checks.push(check('zero console errors, zero CSP violations', ctx.log.errors.length === 0 && (await ctx.log.csp()).length === 0, `${ctx.log.errors.length} / ${(await ctx.log.csp()).length}${ctx.log.errors.length ? ': ' + ctx.log.errors[0].slice(0, 200) : ''}`, '0 / 0'));
    return { name: 'h3-receiver-tracks-engine', checks, metrics: m };
  } finally { await ctx.close(); }
}

const parts = [];
for (const [name, fn] of [['h1', h1], ['h2', h2], ['h3', h3]]) {
  try { const r = await fn(); parts.push(...(Array.isArray(r) ? r : [r])); }
  catch (e) { parts.push({ name: `${name}-crashed`, checks: [check('case runs to completion', false, String(e && e.message || e).slice(0, 300), 'no exception')], metrics: {} }); }
}
clearTimeout(hardStop);
save('engine-tracks.detail.json', parts);
const failures = finish('test-engine-tracks', parts, { stream: args.stream });
process.exit(failures ? 1 : 0);
