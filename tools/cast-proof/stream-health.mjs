// Is the live stream itself on time? For 6 s, every 250 ms: lag = now - (PROGRAM-DATE-TIME of the newest segment
// + its EXTINF). A healthy 2 s-segment stream shows 0.1 to about 2.6 s (a segment is listed when it is complete,
// then ages until the next one). More means the encoder/packager cannot keep up with real time (a starved CI
// host): every latency figure then includes that lag, and the latency criteria say nothing about the player.
//   node stream-health.mjs <media playlist URL>      prints one JSON line: {lag_min_s, lag_max_s, segments, ok}
import http from 'node:http'; import https from 'node:https';
const url = process.argv[2]; const limit = Number(process.argv[3] || 3.5);
const get = (u) => new Promise((resolve, reject) => { const m = u.startsWith('https') ? https : http;
  m.get(u, { rejectUnauthorized: false, timeout: 4000 }, (r) => { let t = ''; r.on('data', (c) => { t += c; }); r.on('end', () => resolve(t)); }).on('error', reject); });
const lags = []; let segments = 0; let noPdt = false;
for (const end = Date.now() + 6000; Date.now() < end; await new Promise((r) => setTimeout(r, 250))) {
  let text; try { text = await get(url); } catch { continue; }
  const lines = text.split('\n'); let pdt = null; let dur = null; segments = (text.match(/#EXTINF/g) || []).length;
  for (const l of lines) { if (l.startsWith('#EXT-X-PROGRAM-DATE-TIME:')) pdt = Date.parse(l.slice(25).replace(/([+-]\d{2})(\d{2})$/, '$1:$2')); if (l.startsWith('#EXTINF:')) dur = parseFloat(l.slice(8)); }
  if (pdt === null || dur === null || isNaN(pdt)) { noPdt = true; continue; }
  lags.push((Date.now() - (pdt + dur * 1000)) / 1000);
}
const r1 = (x) => Math.round(x * 10) / 10;
const out = lags.length ? { lag_min_s: r1(Math.min(...lags)), lag_max_s: r1(Math.max(...lags)), segments, limit_s: limit, ok: Math.max(...lags) <= limit }
  : { lag_min_s: null, lag_max_s: null, segments, limit_s: limit, ok: null, note: noPdt ? 'no PROGRAM-DATE-TIME in the playlist' : 'playlist not reachable' };
console.log(JSON.stringify(out));
