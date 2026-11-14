// Checks (e)-(g) of run.sh against the REAL engine: takes during playback, revoke, authz.
//   node test-engine.mjs --api <engine http base> --origin <https proxy origin> --stream <id> --path <master path>
//        --cast-dir <engine cast dir> --validator <cast_validate binary> [--width 1920] [--proxy-log <file>]
// Bearer: CAST_PROOF_BEARER. Every route used here is the engine's own (through the TLS proxy for the browser).
//  e  while the real receiver plays: take the cast output to source b, then back to Program -> playback continues
//     (no IDLE, BUFFERING under 3 s in total, same URL), the validator still passes, at most 2 discontinuities
//  f  DELETE the stream -> the master answers 404, the receiver shows the "ended" screen and stops requesting, the
//     cast directory holds no stream file after the linger; a second stream then works (new token, new files)
//  g  the media URL with one token character changed answers 404 with the body of an unknown file; a wrong bearer
//     on GET /api/v1/cast/streams answers 401
import fs from 'node:fs';
import http from 'node:http';
import https from 'node:https';
import { spawnSync } from 'node:child_process';
import { startFakePlatform, NS_CONSPECT } from './fake-cast-platform.mjs';
import { parseArgs, openReceiver, senderMedia, statusChecks, check, finish, save, sleep, waitUntil } from './lib.mjs';

const args = parseArgs(process.argv.slice(2), { width: 1920 });
for (const k of ['api', 'origin', 'stream', 'path', 'cast-dir', 'validator']) if (!args[k]) { console.error(`missing --${k}`); process.exit(2); }
const W = Number(args.width);
const BEARER = process.env.CAST_PROOF_BEARER;
const hardStop = setTimeout(() => { console.error('hard timeout'); process.exit(3); }, 300000);

/** One request; resolves {status, text, json}. Never rejects on HTTP status. */
function req(url, { method = 'GET', body = null, headers = {} } = {}) {
  return new Promise((resolve, reject) => {
    const u = new URL(url); const mod = u.protocol === 'https:' ? https : http;
    const r = mod.request(u, { method, rejectUnauthorized: false, timeout: 10000, headers: { ...(body ? { 'content-type': 'application/json' } : {}), ...headers } }, (res) => {
      const c = []; res.on('data', (x) => c.push(x));
      res.on('end', () => { const text = Buffer.concat(c).toString('utf8'); let json = null; try { json = JSON.parse(text); } catch { /* not JSON */ } resolve({ status: res.statusCode, text, json }); });
    });
    r.on('timeout', () => r.destroy(new Error(`${url}: timeout`))); r.on('error', reject);
    r.end(body ? JSON.stringify(body) : undefined);
  });
}
const auth = (extra = {}) => ({ authorization: `Bearer ${BEARER}`, ...extra });
const api = (p, o = {}) => req(args.api + p, { ...o, headers: auth(o.headers) });
let takeN = 0;
const take = (feed) => api('/api/v1/routing/output/take', { method: 'POST', headers: { 'idempotency-key': `cast-proof-${process.pid}-${++takeN}` }, body: { target: { kind: 'output', output: OUTPUT, stream: 'video' }, feed } });
const validate = () => { const r = spawnSync(args.validator, [args['cast-dir']], { encoding: 'utf8', timeout: 30000 }); const out = (r.stdout || '') + (r.stderr || ''); const d = /discontinuities\s+(\d+) listed/.exec(out); return { exit: r.status, discontinuities: d ? Number(d[1]) : null, tail: out.trim().split('\n').slice(-3).join(' | ') }; };
const streamFiles = () => { try { return fs.readdirSync(args['cast-dir']).filter((f) => /\.(m3u8|m4s|mp4)$/.test(f)); } catch { return []; } };
const tokenOf = (p) => p.split('/')[2];

let OUTPUT = null; const parts = []; let ctx = null;
try {
  const doc = await api(`/api/v1/cast/streams/${args.stream}`);
  OUTPUT = doc.json && doc.json.output;
  const masterUrl = args.origin + args.path; const token = tokenOf(args.path);

  // ---- (e) takes during playback
  {
    const checks = []; const m = {};
    ctx = await openReceiver(args.origin + '/cast/receiver/', startFakePlatform);
    const loadAt = Date.now(); ctx.platform.load(senderMedia(masterUrl));
    let playing = null; try { playing = await ctx.platform.waitForMediaStatus((s) => s.playerState === 'PLAYING', 15000, loadAt); } catch { /* checked */ }
    checks.push(check('PLAYING before the takes', !!playing, playing ? playing.at - loadAt : 'none', '<= 15000 ms'));
    await sleep(10000);
    const t1 = await take({ source: 'b' }); m.take_b = t1.status;
    await sleep(15000);
    const t2 = await take({ bus: 'program' }); m.take_program = t2.status;
    checks.push(check('both takes accepted (202)', t1.status === 202 && t2.status === 202, `${t1.status} / ${t2.status}`, '202 / 202'));
    await sleep(15000);
    const end = Date.now();
    const statuses = ctx.platform.mediaStatuses(loadAt);
    const st = statusChecks(statuses, playing, end, 0, 'first PLAYING');
    let buffering = 0;
    for (let i = 0; i < statuses.length; i++) if (playing && statuses[i].at >= playing.at && statuses[i].playerState === 'BUFFERING') { const next = statuses.slice(i + 1).find((s) => s.playerState !== 'BUFFERING'); buffering += (next ? next.at : end) - statuses[i].at; if (next) i = statuses.indexOf(next) - 1; }
    m.buffering_total_ms = buffering; m.idle_after_playing = st.metrics.idle_after_playing;
    checks.push(check('no IDLE across the takes', playing && st.metrics.idle_after_playing === 0, st.metrics.idle_after_playing, '0'));
    checks.push(check('BUFFERING under 3 s in total across the takes', playing && buffering < 3000, `${buffering} ms`, '< 3000 ms'));
    checks.push(check('final player state PLAYING', st.metrics.final_state === 'PLAYING', st.metrics.final_state, 'PLAYING'));
    const loads = ctx.log.console.filter((l) => l.includes('[conspect-cast]') && /reload:/.test(l)).length;
    checks.push(check('same URL: no receiver reload', loads === 0, loads, '0'));
    const last = await ctx.last(); m.overlay = last && last.extra && last.extra.overlay;
    checks.push(check('picture still moving after the takes', last && last.extra && last.extra.state === 'PLAYING', last ? `${last.extra.state}, ct ${Math.round(last.ct)}` : 'none', 'PLAYING'));
    const v = validate(); m.validator = v;
    checks.push(check('validator passes after the takes', v.exit === 0, v.tail, 'exit 0'));
    checks.push(check('discontinuities explained by the takes (<= 2)', v.discontinuities !== null && v.discontinuities <= 2, v.discontinuities, '<= 2'));
    parts.push({ name: 'e-takes-during-playback', checks, metrics: m });
  }

  // ---- (f) revoke, cleanup, second stream
  {
    const checks = []; const m = {};
    const delAt = Date.now();
    const del = await api(`/api/v1/cast/streams/${args.stream}`, { method: 'DELETE' });
    const master = await req(masterUrl);
    checks.push(check('DELETE answers 204, then the master answers 404', del.status === 204 && master.status === 404, `${del.status} / ${master.status}`, '204 / 404'));
    let rep = null;
    try { rep = await waitUntil(() => ctx.platform.custom(NS_CONSPECT, delAt).map((e) => ({ at: e.at, ...e.data })).find((r) => r.state === 'error' && r.error && /ended or its link expired/.test(r.error.message)), 20000, 'the ended report'); } catch { /* checked */ }
    m.ended_report_ms = rep ? rep.at - delAt : null;
    const screen = await ctx.page.evaluate(() => ({ state: document.getElementById('overlay').getAttribute('data-state'), text: document.getElementById('overlay').innerText.replace(/\s+/g, ' ').trim() }));
    checks.push(check('receiver shows the "ended" screen within 20 s', !!rep && screen.state === 'error' && /ended or its link expired/.test(screen.text), rep ? `${m.ended_report_ms} ms: "${screen.text.slice(0, 80)}"` : 'no report', 'ended screen'));
    // Stops requesting: count proxy requests for this token in the 15 s after the report (paths cut to 6 characters).
    if (args['proxy-log'] && rep) {
      await sleep(15000);
      const pre = `/cast/${token.slice(0, 6)}...`;
      const lines = fs.readFileSync(args['proxy-log'], 'utf8').trim().split('\n').map((l) => { try { return JSON.parse(l); } catch { return null; } }).filter(Boolean);
      m.requests_15s_after_report = lines.filter((l) => l.at > rep.at + 1000 && l.path.startsWith(pre) && !l.path.endsWith('/report')).length;
      checks.push(check('receiver stops requesting (15 s after the report)', m.requests_15s_after_report < 6, m.requests_15s_after_report, '< 6'));
    }
    await ctx.close(); ctx = null;
    let emptyAt = null;
    try { emptyAt = await waitUntil(() => (streamFiles().length === 0 ? Date.now() : 0), 45000, 'an empty cast directory'); } catch { /* checked */ }
    m.empty_after_ms = emptyAt ? emptyAt - delAt : null;
    checks.push(check('cast directory empty after the linger', emptyAt !== null, emptyAt ? `${m.empty_after_ms} ms` : `${streamFiles().length} file(s) left`, '<= 45000 ms'));
    const before = new Set(streamFiles());
    const s2 = await api('/api/v1/cast/streams', { method: 'POST', body: { feed: { bus: 'program' } } });
    let ready = null;
    if (s2.status === 201) { try { ready = await waitUntil(async () => { const d = await api(`/api/v1/cast/streams/${s2.json.id}`); return d.json && d.json.ready ? d.json : null; }, 40000, 'the second stream ready'); } catch { /* checked */ } }
    const token2 = s2.json && s2.json.path ? tokenOf(s2.json.path) : null;
    const master2 = token2 ? await req(args.origin + s2.json.path) : { status: null, text: '' };
    const inits = streamFiles().filter((f) => f.startsWith('init-'));
    checks.push(check('a second stream works (201, ready, master 200, new token, new init files)', s2.status === 201 && !!ready && master2.status === 200 && /#EXT-X-STREAM-INF/.test(master2.text) && token2 !== token && inits.length > 0 && inits.every((f) => !before.has(f)), `${s2.status}, ready ${!!ready}, master ${master2.status}, new token ${token2 !== token}, init ${inits.join(',')}`, 'all'));
    parts.push({ name: 'f-revoke-and-restart', checks, metrics: m });
    globalThis.second = { token2, id: s2.json && s2.json.id };
  }

  // ---- (g) authz smoke
  {
    const checks = []; const m = {};
    const t2 = globalThis.second.token2 || token;
    const flip = t2.slice(0, 10) + (t2[10] === 'A' ? 'B' : 'A') + t2.slice(11);
    const bad = await req(`${args.origin}/cast/${flip}/master.m3u8`);
    const unknown = await req(`${args.origin}/cast/${t2}/nope-0.m4s`);
    m.bad = bad.status; m.unknown = unknown.status;
    checks.push(check('one changed token character: 404 with the unknown-file body', bad.status === 404 && unknown.status === 404 && bad.text === unknown.text, `${bad.status} / ${unknown.status}, same body ${bad.text === unknown.text}`, '404, same body'));
    const wrong = await req(`${args.api}/api/v1/cast/streams`, { headers: { authorization: 'Bearer admin.wrong-token' } });
    checks.push(check('wrong bearer on GET /api/v1/cast/streams: 401', wrong.status === 401, wrong.status, '401'));
    parts.push({ name: 'g-authz', checks, metrics: m });
  }
} catch (e) {
  console.error('test-engine failed:', e && e.stack || e);
  parts.push({ name: 'error', checks: [check('test ran to the end', false, String(e && e.message || e), 'no exception')], metrics: {} });
} finally {
  clearTimeout(hardStop);
  if (ctx) await ctx.close();
}
save('engine.detail.json', parts);
const failures = finish('test-engine', parts, { second_stream: globalThis.second ? globalThis.second.id : null });
process.exit(failures ? 1 : 0);
