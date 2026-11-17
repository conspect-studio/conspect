// Test 5 (#418): audio and subtitle tracks through the REAL CAF receiver SDK + our receiver app + the fake platform.
// Self-contained: its own harness server (HTTPS, CSP) and its own stream (gen-tracks-stream.mjs: video, AAC
// "Program"/en 440 Hz + "Commentary"/de 880 Hz, WebVTT "Captions"/en with one known cue per 2 s segment).
//   node test-receiver-tracks.mjs [--cases a,b,c,d,e,f] [--offset 100000] [--size 1280x720]
//  a  MEDIA_STATUS lists 2 AUDIO + 1 TEXT track with the manifest's names/languages; Program active, text off
//  b  EDIT_TRACKS_INFO [Commentary, Captions]: the active Shaka variant's audio becomes de, no BUFFERING > 1 s and
//     the picture keeps moving (no stall > 1 s); the text track is shown and the expected cue is active at the
//     right media time (+-0.5 s); the report carries tracks with the new active flags
//  c  LOAD with customData.audioLanguage 'de' starts on Commentary (and text stays off)
//  d  after an EDIT_TRACKS_INFO choice a forced self-reload (the receiver's own retry path) keeps the choice
//  e  LOAD with activeTrackIds (ids from a previous MEDIA_STATUS, with media.tracks as a sender sends them) is
//     honoured; a sender text track URL is never fetched; unknown ids in EDIT_TRACKS_INFO are ignored
//  f  customData.textLanguage 'en' starts with subtitles on; textLanguage null keeps them off
// --offset: the media timeline (tfdt) starts there; the default 100000 s is past the 33-bit MPEG-TS wrap at
// 95443.7 s, as an engine that has run for a day (cue timing then proves the X-TIMESTAMP-MAP mapping).
import path from 'node:path';
import { spawn, execFileSync } from 'node:child_process';
import { startServer } from './server.mjs';
import { startFakePlatform, NS_CONSPECT, NS_MEDIA } from './fake-cast-platform.mjs';
import { parseArgs, openReceiver, waitForSegments, senderMedia, check, finish, save, sleep, waitUntil, outDir, here } from './lib.mjs';

const args = parseArgs(process.argv.slice(2), { cases: 'a,b,c,d,e,f', offset: 100000, size: '1280x720' });
const wanted = String(args.cases).split(',');
const TOKEN = 'tracksAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA';
const streamDir = path.join(outDir, 'streams', 'tracks');
const hardStop = setTimeout(() => { console.error('hard timeout'); process.exit(3); }, 12 * 60 * 1000);
let server; let generator;

/** In-page: CAF's track managers, the Shaka variants and text cues. */
function inPage() {
  const pm = cast.framework.CastReceiverContext.getInstance().getPlayerManager();
  const r = { audio: [], text: [], audioActive: null, textActive: [] };
  try { r.audio = pm.getAudioTracksManager().getTracks(); r.audioActive = pm.getAudioTracksManager().getActiveId(); r.text = pm.getTextTracksManager().getTracks(); r.textActive = pm.getTextTracksManager().getActiveIds(); } catch (e) { r.err = String(e); }
  const sp = pm.getShakaPlayer && pm.getShakaPlayer();
  const v = window.__probe && window.__probe.video;
  r.ct = v ? v.currentTime : null;
  if (sp) {
    const act = sp.getVariantTracks().find((t) => t.active);
    r.variantLanguage = act ? act.language : null; r.variantLabel = act ? act.label : null;
    r.textVisible = sp.isTextTrackVisible();
    try { const d = sp.getPlayheadTimeAsDate(); r.date = d ? d.getTime() : null; } catch (e) { r.date = null; }
  }
  r.cues = []; r.activeCues = [];
  if (v) for (const t of v.textTracks) {
    if (t.mode !== 'showing') continue;
    if (t.cues) for (const c of t.cues) r.cues.push([c.startTime, c.endTime, c.text]);
    if (t.activeCues) for (const c of t.activeCues) r.activeCues.push(c.text);
  }
  return r;
}
const state = (ctx) => ctx.page.evaluate(inPage);
const lastStatusMedia = (ctx, since) => { const s = ctx.platform.mediaStatuses(since).filter((x) => x.media && x.media.tracks); return s.length ? s[s.length - 1].media : null; };
const lastActive = (ctx, since) => { const s = ctx.platform.mediaStatuses(since).filter((x) => Array.isArray(x.activeTrackIds)); return s.length ? s[s.length - 1].activeTrackIds : null; };
const byName = (tracks, name) => (tracks || []).find((t) => t.name === name);
const reportsSince = (ctx, t) => ctx.platform.custom(NS_CONSPECT, t).map((e) => ({ at: e.at, ...e.data }));
const masterUrl = () => `${server.secureOrigin}/cast/${TOKEN}/master.m3u8`;

/** Open the receiver and LOAD (media/extra overrides), wait for PLAYING. */
async function loaded(media = {}, extra = {}) {
  const ctx = await openReceiver(server.secureOrigin + '/cast/receiver/', startFakePlatform);
  try {
    ctx.loadAt = Date.now();
    ctx.platform.load(senderMedia(masterUrl(), media), extra);
    await ctx.platform.waitForMediaStatus((s) => s.playerState === 'PLAYING', 15000, ctx.loadAt);
    await waitUntil(async () => (await state(ctx)).audio.length > 0, 10000, 'the track managers');
    return ctx;
  } catch (e) { await ctx.close(); throw e; }
}

async function caseA() {
  const ctx = await loaded();
  try {
    await sleep(2000);
    const checks = []; const m = {};
    const media = lastStatusMedia(ctx, ctx.loadAt); const tracks = (media && media.tracks) || [];
    const audio = tracks.filter((t) => t.type === 'AUDIO'); const text = tracks.filter((t) => t.type === 'TEXT');
    m.tracks = tracks.map((t) => [t.trackId, t.type, t.name, t.language, t.trackContentType]);
    m.activeTrackIds = lastActive(ctx, ctx.loadAt);
    const p = byName(audio, 'Program'); const c = byName(audio, 'Commentary'); const cap = byName(text, 'Captions');
    checks.push(check('MEDIA_STATUS: 2 AUDIO + 1 TEXT track', audio.length === 2 && text.length === 1, `${audio.length} audio, ${text.length} text`, '2 + 1'));
    checks.push(check('names and languages from the manifest', p && p.language === 'en' && c && c.language === 'de' && cap && cap.language === 'en', JSON.stringify(m.tracks), 'Program/en, Commentary/de, Captions/en'));
    checks.push(check('Program active, subtitles off', p && JSON.stringify(m.activeTrackIds) === JSON.stringify([p.trackId]), JSON.stringify(m.activeTrackIds), `[${p && p.trackId}]`));
    const s = await state(ctx);
    checks.push(check('Shaka plays the en audio variant', s.variantLanguage === 'en', s.variantLanguage, 'en'));
    return { name: 'a-tracks-published', checks, metrics: m };
  } finally { await ctx.close(); }
}

async function caseB() {
  const ctx = await loaded();
  try {
    await sleep(3000);
    const checks = []; const m = {};
    const media = lastStatusMedia(ctx, ctx.loadAt);
    const c = byName(media.tracks, 'Commentary'); const cap = byName(media.tracks, 'Captions');
    const t0 = Date.now();
    ctx.platform.media({ type: 'EDIT_TRACKS_INFO', mediaSessionId: ctx.platform.mediaSessionId(), activeTrackIds: [c.trackId, cap.trackId] });
    let switched = null;
    try { switched = await waitUntil(async () => { const s = await state(ctx); return s.variantLanguage === 'de' ? { at: Date.now(), s } : null; }, 8000, 'the de variant'); } catch { /* reported */ }
    m.audio_switch_ms = switched ? switched.at - t0 : null;
    checks.push(check('EDIT_TRACKS_INFO: the active Shaka variant becomes de', !!switched, switched ? `${m.audio_switch_ms} ms` : 'still ' + (await state(ctx)).variantLanguage, 'de within 8 s'));
    await sleep(8000);
    const end = Date.now();
    // No stall: no BUFFERING spell > 1 s, and the video clock advanced at least wall - 1 s.
    const st = ctx.platform.mediaStatuses(t0); let maxBuf = 0;
    for (let i = 0; i < st.length; i++) if (st[i].playerState === 'BUFFERING' && (i === 0 || st[i - 1].playerState !== 'BUFFERING')) { const next = st.slice(i + 1).find((x) => x.playerState !== 'BUFFERING'); maxBuf = Math.max(maxBuf, (next ? next.at : end) - st[i].at); }
    const probe = await ctx.probe(); const samples = probe.samples.filter((x) => x.at >= t0);
    let worstStall = 0; for (let i = 1; i < samples.length; i++) { const wall = (samples[i].at - samples[i - 1].at) / 1000; const adv = samples[i].ct - samples[i - 1].ct; worstStall = Math.max(worstStall, wall - adv); }
    const totalAdv = samples.length > 1 ? samples[samples.length - 1].ct - samples[0].ct : 0; const totalWall = samples.length > 1 ? (samples[samples.length - 1].at - samples[0].at) / 1000 : 0;
    m.max_buffering_ms = maxBuf; m.worst_sample_stall_s = Math.round(worstStall * 100) / 100; m.advance_s = Math.round(totalAdv * 10) / 10; m.wall_s = Math.round(totalWall * 10) / 10;
    checks.push(check('no BUFFERING > 1 s across the switch', maxBuf <= 1000, `${maxBuf} ms`, '<= 1000 ms'));
    checks.push(check('picture keeps moving (no stall > 1 s)', samples.length > 5 && totalWall - totalAdv <= 1, `advanced ${m.advance_s} s in ${m.wall_s} s, worst 1 s sample ${m.worst_sample_stall_s} s`, 'lag <= 1 s'));
    const active = lastActive(ctx, t0);
    checks.push(check('MEDIA_STATUS activeTrackIds = [Commentary, Captions]', active && active.length === 2 && active.includes(c.trackId) && active.includes(cap.trackId), JSON.stringify(active), JSON.stringify([c.trackId, cap.trackId])));
    // Cue timing, independent of X-TIMESTAMP-MAP: each cue "CUE <seq> ... PDT=<ms>" belongs at the media time
    // where the playhead's program date (Shaka, from EXT-X-PROGRAM-DATE-TIME) equals that PDT. With the playhead at
    // (ct, date): expected start = ct - (date - PDT) / 1000. Every cue must start there (+-0.5 s).
    const parse = (q) => q.cues.map(([a, b, text]) => { const mm = /^CUE (\d+) T=([0-9.]+) PDT=(\d+)$/.exec(text); return mm ? { start: a, end: b, seq: Number(mm[1]), tfdt: Number(mm[2]), pdt: Number(mm[3]) } : null; }).filter(Boolean);
    const s = await state(ctx); const cues = parse(s);
    let worst = null;
    if (typeof s.date === 'number') for (const x of cues) { const err = Math.abs(x.start - (s.ct - (s.date - x.pdt) / 1000)); worst = worst === null ? err : Math.max(worst, err); }
    m.cues = cues.slice(-3).map((x) => [Math.round(x.start * 1000) / 1000, x.seq, x.tfdt]); m.ct = s.ct; m.cue_timing_worst_s = worst === null ? null : Math.round(worst * 1000) / 1000;
    checks.push(check('text track shown with cues', s.textVisible && cues.length >= 2, `visible ${s.textVisible}, ${cues.length} cue(s)`, 'visible, >= 2 cues'));
    checks.push(check(`cue start = its segment's media time (+-0.5 s; tfdt from ${args.offset} s)`, worst !== null && worst <= 0.5, worst === null ? 'no playhead date' : `worst ${m.cue_timing_worst_s} s over ${cues.length} cues, e.g. ${JSON.stringify(m.cues[m.cues.length - 1])}`, '<= 0.5 s'));
    // A known cue becomes active at its time: the active cue's PDT window holds the playhead date (+-0.5 s).
    let hit = null;
    try {
      hit = await waitUntil(async () => {
        const q = await state(ctx); const a = q.activeCues.map((x) => /^CUE (\d+)/.exec(x)).filter(Boolean);
        if (!a.length || typeof q.date !== 'number') return null;
        const cue = parse(q).find((x) => x.seq === Number(a[0][1]));
        return cue ? { seq: cue.seq, ct: q.ct, start: cue.start, end: cue.end, late_s: (q.date - cue.pdt) / 1000 } : null;
      }, 6000, 'an active cue');
    } catch { /* reported */ }
    m.active_cue = hit;
    checks.push(check('a known cue is active at its time (+-0.5 s)', hit && hit.late_s >= -0.5 && hit.late_s <= 1.5 + 0.5, hit ? `CUE ${hit.seq} active ${hit.late_s.toFixed(2)} s after its PDT (window 0-1.5 s), media [${hit.start.toFixed(2)}, ${hit.end.toFixed(2)}] at ct ${hit.ct.toFixed(2)}` : 'none', '-0.5 .. 2.0 s'))
    // Reports carry the tracks.
    await sleep(5500);
    const reps = reportsSince(ctx, t0).filter((r) => r.tracks);
    const r = reps[reps.length - 1];
    const posted = server.reports.filter((x) => x.token === TOKEN && x.at >= t0 && x.report.tracks);
    const pr = posted.length ? posted[posted.length - 1].report.tracks : null;
    const okShape = (tr) => tr && tr.audio.length === 2 && tr.text.length === 1 && tr.audio.some((x) => x.name === 'Commentary' && x.language === 'de' && x.active) && tr.audio.some((x) => x.name === 'Program' && !x.active) && tr.text[0].name === 'Captions' && tr.text[0].active;
    m.report_tracks = r ? r.tracks : null;
    checks.push(check('custom-namespace report: tracks with active flags', okShape(r && r.tracks), JSON.stringify(r && r.tracks).slice(0, 300), 'Commentary active, Captions active'));
    checks.push(check('POSTed report: tracks, body < 2 KiB', okShape(pr) && posted.every((x) => x.bytes < 2048), pr ? `${posted.length} report(s), max ${Math.max(...posted.map((x) => x.bytes))} bytes` : 'none', 'tracks, < 2048'));
    checks.push(check('zero console errors', ctx.log.errors.length === 0, ctx.log.errors.slice(0, 2).join(' | ').slice(0, 300) || 0, '0'));
    checks.push(check('zero CSP violations', (await ctx.log.csp()).length === 0, (await ctx.log.csp()).length, '0'));
    return { name: 'b-edit-tracks', checks, metrics: m };
  } finally { await ctx.close(); }
}

async function caseC() {
  const ctx = await loaded({ customData: { title: 'Program', audioLanguage: 'de' } });
  try {
    await sleep(2000);
    const checks = []; const m = {};
    const media = lastStatusMedia(ctx, ctx.loadAt); const c = byName(media.tracks, 'Commentary');
    const s = await state(ctx); const active = lastActive(ctx, ctx.loadAt);
    // Which audio did Shaka fetch first? (A start on de, not a switch after an en start.)
    const firstAudio = server.requestsSince(TOKEN, ctx.loadAt).find((r) => /^seg-audio1?-/.test(r.file));
    m.first_audio_segment = firstAudio ? firstAudio.file.replace(/-\d+-/, '-<gen>-') : null; m.active = active; m.variant = s.variantLanguage;
    checks.push(check('LOAD customData.audioLanguage de: Commentary active', active && active.length === 1 && active[0] === c.trackId && s.variantLanguage === 'de', `${JSON.stringify(active)}, variant ${s.variantLanguage}`, `[${c.trackId}], de`));
    checks.push(check('starts on de (first audio segment fetched is Commentary)', firstAudio && /^seg-audio1-/.test(firstAudio.file), m.first_audio_segment, 'seg-audio1-*'));
    return { name: 'c-load-audio-language', checks, metrics: m };
  } finally { await ctx.close(); }
}

async function caseD() {
  // The receiver's own self-reload (reload(), shared by the retry and the BUFFERING watchdog): segments held for
  // 36 s -> one watchdog reload after 20 s of BUFFERING (case h of the fault test), a new LOAD with new track ids.
  const ctx = await loaded();
  try {
    await sleep(3000);
    const checks = []; const m = {};
    const media = lastStatusMedia(ctx, ctx.loadAt); const c = byName(media.tracks, 'Commentary'); const cap = byName(media.tracks, 'Captions');
    ctx.platform.media({ type: 'EDIT_TRACKS_INFO', mediaSessionId: ctx.platform.mediaSessionId(), activeTrackIds: [c.trackId, cap.trackId] });
    await waitUntil(async () => (await state(ctx)).variantLanguage === 'de', 8000, 'the de variant');
    await sleep(3000);
    const sessionBefore = ctx.platform.mediaSessionId();
    const faultAt = Date.now();
    server.setFault({ mode: 'stall', seconds: 36, token: TOKEN });
    let reload = null;
    try { reload = await waitUntil(() => ctx.log.console.find((l) => /\[conspect-cast\] reload: /.test(l)), 40000, 'the self-reload'); } catch { /* reported */ }
    m.reload_ms = reload ? Date.now() - faultAt : null;
    checks.push(check('the receiver reloaded itself', !!reload, reload ? `${m.reload_ms} ms after the fault: ${reload.replace(/^.*reload: /, '').slice(0, 60)}` : 'no reload', 'reload'));
    let back = null;
    try { back = await waitUntil(async () => { const x = await state(ctx); return Date.now() > faultAt + 36000 && x.variantLanguage && x.audio.length ? x : null; }, 40000, 'the reloaded stream'); } catch { /* reported */ }
    try { await ctx.platform.waitForMediaStatus((s) => s.playerState === 'PLAYING', 20000, faultAt + 36000); } catch { /* reported below */ }
    await sleep(4000);
    server.setFault({ mode: 'none' });
    const s = await state(ctx);
    const media2 = lastStatusMedia(ctx, faultAt); const active = lastActive(ctx, faultAt + 36000);
    const c2 = media2 && byName(media2.tracks, 'Commentary'); const cap2 = media2 && byName(media2.tracks, 'Captions');
    m.session_before = sessionBefore; m.session_after = ctx.platform.mediaSessionId(); m.active_after = active; m.variant_after = s.variantLanguage; m.text_after = s.textActive;
    checks.push(check('a new media session (a real reload)', m.session_after !== sessionBefore, `${sessionBefore} -> ${m.session_after}`, 'changed'));
    checks.push(check('after the reload: Commentary (de) still plays', !!back && s.variantLanguage === 'de' && c2 && active && active.includes(c2.trackId), `variant ${s.variantLanguage}, active ${JSON.stringify(active)}`, `de, includes ${c2 && c2.trackId}`));
    checks.push(check('after the reload: Captions still on', cap2 && active && active.includes(cap2.trackId) && s.textVisible, `active ${JSON.stringify(active)}, visible ${s.textVisible}`, `includes ${cap2 && cap2.trackId}`));
    return { name: 'd-self-reload-keeps-choice', checks, metrics: m };
  } finally { server.setFault({ mode: 'none' }); await ctx.close(); }
}

async function caseE() {
  // A sender LOADs with media.tracks (its own list, including a side-loaded text URL) and activeTrackIds.
  const evil = `${server.secureOrigin}/cast/${TOKEN}/evil.vtt`;
  const tracks = [
    { trackId: 1, type: 'AUDIO', name: 'Program', language: 'en' },
    { trackId: 2, type: 'AUDIO', name: 'Commentary', language: 'de' },
    { trackId: 3, type: 'TEXT', subtype: 'SUBTITLES', name: 'Captions', language: 'en', trackContentType: 'text/vtt' },
    { trackId: 9, type: 'TEXT', subtype: 'SUBTITLES', name: 'Injected', language: 'xx', trackContentId: evil, trackContentType: 'text/vtt' }
  ];
  const ctx = await loaded({ tracks }, { activeTrackIds: [2, 9] });
  try {
    await sleep(3000);
    const checks = []; const m = {};
    const s = await state(ctx); const media = lastStatusMedia(ctx, ctx.loadAt); const active = lastActive(ctx, ctx.loadAt);
    const c = byName(media.tracks, 'Commentary');
    m.tracks = media.tracks.map((t) => [t.trackId, t.type, t.name, t.trackContentId ? 'URL' : '']); m.active = active;
    checks.push(check('LOAD activeTrackIds: the manifest track it names plays (Commentary)', s.variantLanguage === 'de' && active && active.includes(c.trackId), `variant ${s.variantLanguage}, active ${JSON.stringify(active)}`, 'de'));
    const fetched = server.requestsSince(TOKEN, ctx.loadAt).filter((r) => r.file === 'evil.vtt').length + ctx.log.httpErrors.filter((l) => /evil\.vtt/.test(l)).length;
    checks.push(check('a sender text track URL is never fetched', fetched === 0, fetched, '0'));
    checks.push(check('MEDIA_STATUS lists only the manifest tracks', media.tracks.length === 3 && !media.tracks.some((t) => t.trackContentId || t.name === 'Injected'), JSON.stringify(m.tracks), '3, no URL'));
    // Unknown ids in EDIT_TRACKS_INFO change nothing: CAF refuses the request (INVALID_REQUEST, its own check
    // against media.tracks) and playback and the choice stay; a following valid request applies.
    const p = byName(media.tracks, 'Program');
    const t0 = Date.now();
    ctx.platform.media({ type: 'EDIT_TRACKS_INFO', mediaSessionId: ctx.platform.mediaSessionId(), activeTrackIds: [p.trackId, 999] });
    await sleep(2500);
    const after = await state(ctx);
    const answers = ctx.platform.events.filter((e) => e.at >= t0 && e.namespace === NS_MEDIA && e.data && e.data.type && e.data.type !== 'MEDIA_STATUS').map((e) => e.data.type + (e.data.reason ? ':' + e.data.reason : ''));
    checks.push(check('EDIT_TRACKS_INFO with an unknown id: ignored (refused), audio unchanged, still PLAYING', after.variantLanguage === 'de' && ctx.platform.lastPlayerState() === 'PLAYING', `answers ${JSON.stringify(answers)}, variant ${after.variantLanguage}, ${ctx.platform.lastPlayerState()}`, 'de, PLAYING'));
    ctx.platform.media({ type: 'EDIT_TRACKS_INFO', mediaSessionId: ctx.platform.mediaSessionId(), activeTrackIds: [p.trackId] });
    let en = null; try { en = await waitUntil(async () => ((await state(ctx)).variantLanguage === 'en' ? true : null), 8000, 'en'); } catch { /* reported */ }
    checks.push(check('then a valid EDIT_TRACKS_INFO [Program] applies', !!en, en ? 'en' : 'not en', 'en'));
    checks.push(check('zero CSP violations / sender URL requests in the page', (await ctx.log.csp()).length === 0 && !ctx.log.httpErrors.some((l) => /evil/.test(l)), (await ctx.log.csp()).length, '0'));
    m.edit_unknown_answers = answers;
    return { name: 'e-load-track-ids-no-sender-urls', checks, metrics: m };
  } finally { await ctx.close(); }
}

async function caseF() {
  const parts = [];
  for (const [label, custom, want] of [['textLanguage en', { textLanguage: 'en' }, true], ['textLanguage null', { textLanguage: null, textName: 'Captions' }, false]]) {
    const ctx = await loaded({ customData: { title: 'Program', ...custom } });
    try {
      await sleep(3000);
      const s = await state(ctx); const media = lastStatusMedia(ctx, ctx.loadAt); const active = lastActive(ctx, ctx.loadAt);
      const cap = byName(media.tracks, 'Captions'); const on = !!(active && active.includes(cap.trackId)) && s.textVisible;
      parts.push(check(`LOAD customData.${label}: subtitles ${want ? 'on' : 'off'}`, on === want && s.variantLanguage === 'en', `active ${JSON.stringify(active)}, visible ${s.textVisible}, variant ${s.variantLanguage}`, want ? `includes ${cap.trackId}` : 'no text'));
    } finally { await ctx.close(); }
  }
  return { name: 'f-load-text-language', checks: parts, metrics: {} };
}

const CASES = { a: caseA, b: caseB, c: caseC, d: caseD, e: caseE, f: caseF };
let failures = 1;
try {
  execFileSync(path.join(here, 'make-cert.sh'), [path.join(outDir, 'tls')]);
  generator = spawn(process.execPath, [path.join(here, 'gen-tracks-stream.mjs'), streamDir, '1800', '--offset', String(args.offset), '--size', String(args.size)], { stdio: ['ignore', 'ignore', 'inherit'] });
  process.on('exit', () => { try { generator.kill('SIGTERM'); } catch { /* gone */ } });   // it kills its ffmpeg
  await waitForSegments(streamDir, 10, 60000, '', 'subs.m3u8');
  server = await startServer({ tokens: { [TOKEN]: streamDir }, tls: { key: path.join(outDir, 'tls', 'key.pem'), cert: path.join(outDir, 'tls', 'cert.pem') } });
  const parts = [];
  for (const id of wanted) {
    if (!CASES[id]) continue;
    try { const r = await CASES[id](); parts.push(...(Array.isArray(r) ? r : [r])); }
    catch (e) { parts.push({ name: `${id}-crashed`, checks: [check('case runs to completion', false, String(e && e.message || e).slice(0, 300), 'no exception')], metrics: {} }); }
  }
  save('tracks.detail.json', { parts });
  failures = finish('test-receiver-tracks', parts, { cases: wanted, offset: args.offset });
} catch (e) {
  console.error('test-receiver-tracks failed:', e && e.stack || e);
  console.log('RESULT ' + JSON.stringify({ test: 'test-receiver-tracks', pass: false, failed: [String(e && e.message || e)] }));
} finally {
  clearTimeout(hardStop);
  if (generator) { try { generator.kill('SIGTERM'); } catch { /* gone */ } }
  if (server) await server.close().catch(() => {});
}
process.exit(failures ? 1 : 0);
