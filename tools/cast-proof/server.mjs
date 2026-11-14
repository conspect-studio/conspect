// Harness HTTP server: the receiver app with the engine's CSP, token-shaped media routes, receiver reports,
// fault injection. One listener on [::] (dual-stack), reachable as http://localhost:<port>.
//
//   node server.mjs --port 8080 --map <token>=<dir | http(s) base URL> [--map ...] [--receiver <dir>]  (default: crates/multiview-control/src/cast_receiver)
//                   [--no-csp] [--static /dmr/=<dir>]
//   GET  /cast/receiver/...            receiver app (CSP + Cache-Control: no-cache)
//   GET  /cast/<token>/<file>          media from the mapped directory or proxied from the mapped base URL
//   POST /cast/<token>/report          receiver report (<= 2 KiB JSON, 1/s per token); GET /_reports lists them
//   POST /_fault {mode:'none'|'404'|'stall'|'slow'|'corrupt'|'delay404', seconds?, bps?, token?}   (corrupt: .m4s
//                 bodies are replaced by noise for `seconds`: a decode failure, fatal in the player; delay404:
//                 master.m3u8 alone is held for `seconds` then answered 404, everything else serves normally)
//   POST /_map {token, target}         add or replace a token mapping;  GET /_requests  request log per token
//   --tls-port N --tls-key F --tls-cert F : the same routes over HTTPS as well (self-signed, see make-cert.sh).
//   The engine serves the receiver over HTTPS; CAF loads Shaka from a protocol-relative URL
//   (//ajax.googleapis.com/...), so only an https page satisfies the production CSP unchanged.
import http from 'node:http';
import https from 'node:https';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

export const CSP = "default-src 'none'; script-src 'self' https://www.gstatic.com https://ajax.googleapis.com; " +
  "style-src 'self' 'unsafe-inline'; img-src 'self' data:; media-src 'self' https: blob:; " +
  "connect-src 'self' https: ws://localhost:8008; font-src 'self' https://fonts.gstatic.com; worker-src blob:";

const TYPES = {
  '.m3u8': 'application/vnd.apple.mpegurl', '.m4s': 'video/iso.segment', '.mp4': 'video/mp4', '.ts': 'video/mp2t',
  '.html': 'text/html; charset=utf-8', '.js': 'text/javascript; charset=utf-8', '.css': 'text/css; charset=utf-8'
};
const TOKEN_ROUTE = /^\/cast\/([A-Za-z0-9_-]{20,128})\/([A-Za-z0-9._-]+)$/;
const here = path.dirname(fileURLToPath(import.meta.url));
// The receiver files under test are the ones the engine embeds (include_str!): never a second copy.
export const RECEIVER_DIR = path.resolve(here, '..', '..', 'crates', 'multiview-control', 'src', 'cast_receiver');

export async function startServer(opts = {}) {
  const receiverDir = path.resolve(opts.receiverDir || RECEIVER_DIR);
  const csp = opts.csp === undefined ? CSP : opts.csp;
  const tokens = new Map(Object.entries(opts.tokens || {}));
  const statics = Object.entries(opts.statics || {});       // [urlPrefix, dir]
  const reports = [];
  const requests = [];                                       // {at, token, file, status}
  const lastReportAt = new Map();
  let fault = { mode: 'none', until: 0, bps: 0, token: null };
  const held = new Set();                                    // release callbacks of stalled responses

  const note = (token, file, status) => { requests.push({ at: Date.now(), token, file, status }); if (requests.length > 20000) requests.splice(0, 5000); };
  const faults = new Map();                                  // per token ('*' = every token), so runs do not interfere
  const faultFor = (token) => { const f = faults.get(token) || faults.get('*'); return f && f.mode !== 'none' ? f : null; };
  const release = () => { for (const fn of [...held]) fn(); held.clear(); };

  function sendStatic(res, dir, rel, headers) {
    const file = path.join(dir, rel === '' ? 'index.html' : rel);
    if (!file.startsWith(dir) || !fs.existsSync(file) || !fs.statSync(file).isFile()) { res.writeHead(404); return res.end(); }
    res.writeHead(200, { 'content-type': TYPES[path.extname(file)] || 'application/octet-stream', ...headers });
    res.end(fs.readFileSync(file));
  }

  function body(req, limit) {
    return new Promise((resolve, reject) => {
      const chunks = []; let size = 0;
      req.on('data', (c) => { size += c.length; if (size > limit) { reject(new Error('too large')); req.destroy(); } else chunks.push(c); });
      req.on('end', () => resolve(Buffer.concat(chunks).toString('utf8')));
      req.on('error', reject);
    });
  }

  function deliver(res, status, headers, buf, f) {
    const write = () => {
      if (res.destroyed) return;
      res.writeHead(status, headers);
      if (!f || f.mode !== 'slow' || !f.bps) return res.end(buf);
      const step = Math.max(1, Math.floor(f.bps / 8 / 10)); let off = 0;      // 10 writes per second
      const timer = setInterval(() => {
        if (res.destroyed || off >= buf.length) { clearInterval(timer); return res.end(); }
        res.write(buf.subarray(off, off + step)); off += step;
      }, 100);
    };
    if (f && f.mode === 'stall' && Date.now() < f.until) {
      const timer = setTimeout(() => { held.delete(go); write(); }, f.until - Date.now());
      const go = () => { clearTimeout(timer); write(); };
      held.add(go);
    } else write();
  }

  function media(req, res, token, file, cors) {
    // A mapping may carry a format experiment: "<dir or base>#holdback=<seconds>" inserts
    // #EXT-X-SERVER-CONTROL:HOLD-BACK=<seconds> into every media playlist served under the token.
    const [target, option] = String(tokens.get(token) || '').split('#');
    const holdback = /^holdback=([0-9.]+)$/.exec(option || '');
    const patch = (buf) => (holdback && file.endsWith('.m3u8') && buf.includes('#EXT-X-TARGETDURATION'))
      ? Buffer.from(buf.toString('utf8').replace(/(#EXT-X-TARGETDURATION:[^\n]*\n)/, `$1#EXT-X-SERVER-CONTROL:HOLD-BACK=${Number(holdback[1]).toFixed(1)}\n`)) : buf;
    const ext = path.extname(file);
    const f = faultFor(token);
    if (!target || !tokens.has(token) || !TYPES[ext] || (f && f.mode === '404')) { note(token, file, 404); res.writeHead(404, cors); return res.end(); }
    // delay404: only master.m3u8 is held, then answered 404 (a stuck/slow master probe for one stream while
    // another stream's LOAD and playback must be unaffected). Everything else (segments, other files) is normal.
    if (f && f.mode === 'delay404' && file === 'master.m3u8') {
      const delayMs = Math.max(0, f.until - Date.now());
      setTimeout(() => { note(token, file, 404); if (!res.headersSent) { res.writeHead(404, cors); res.end(); } }, delayMs);
      return;
    }
    const isPlaylist = ext === '.m3u8';
    const headers = { ...cors, 'content-type': TYPES[ext], 'cache-control': isPlaylist ? 'max-age=1' : 'public, max-age=3600, immutable' };
    const hold = f && !isPlaylist ? f : null;                // stall and slow act on segments and init files
    if (/^https?:\/\//.test(target)) {
      const u = new URL(file, target.endsWith('/') ? target : target + '/');
      const mod = u.protocol === 'https:' ? https : http;
      const up = mod.get(u, { timeout: 15000 }, (r) => {
        const chunks = []; r.on('data', (c) => chunks.push(c));
        r.on('end', () => { note(token, file, r.statusCode); deliver(res, r.statusCode, headers, patch(Buffer.concat(chunks)), hold); });
      });
      up.on('timeout', () => up.destroy(new Error('upstream timeout')));
      up.on('error', () => { note(token, file, 502); if (!res.headersSent) { res.writeHead(502, cors); res.end(); } });
      return;
    }
    fs.readFile(path.join(target, file), (err, raw) => {
      if (err) { note(token, file, 404); res.writeHead(404, cors); return res.end(); }
      let buf = patch(raw);
      if (f && f.mode === 'corrupt' && ext === '.m4s' && Date.now() < f.until) { buf = Buffer.from(buf); for (let i = 0; i < buf.length; i++) buf[i] = (i * 2654435761) >>> 24; }
      note(token, file, 200);
      if (req.method === 'HEAD') { res.writeHead(200, { ...headers, 'content-length': buf.length }); return res.end(); }
      deliver(res, 200, { ...headers, 'content-length': buf.length }, buf, hold);
    });
  }

  const handler = async (req, res) => {
    const url = new URL(req.url, 'http://x');
    const p = url.pathname;
    const origin = req.headers.origin;
    const cors = origin ? { 'access-control-allow-origin': origin, vary: 'Origin' } : {};
    try {
      if (p === '/cast/receiver' ) { res.writeHead(308, { location: '/cast/receiver/' }); return res.end(); }
      if (p.startsWith('/cast/receiver/')) {
        const headers = { 'cache-control': 'no-cache', 'x-content-type-options': 'nosniff' };
        if (csp) headers['content-security-policy'] = csp;
        return sendStatic(res, receiverDir, p.slice('/cast/receiver/'.length), headers);
      }
      for (const [prefix, dir] of statics) {
        if (p.startsWith(prefix)) return sendStatic(res, path.resolve(dir), p.slice(prefix.length), { 'cache-control': 'no-cache' });
      }
      const m = TOKEN_ROUTE.exec(p);
      if (m) {
        const [, token, file] = m;
        if (req.method === 'OPTIONS') {
          res.writeHead(204, { ...cors, 'access-control-allow-methods': 'GET, HEAD, POST, OPTIONS', 'access-control-allow-headers': 'content-type', 'access-control-max-age': '600' });
          return res.end();
        }
        if (file === 'report') {
          const f = faultFor(token);
          if (req.method !== 'POST') { res.writeHead(405, cors); return res.end(); }
          if (!tokens.has(token) || (f && f.mode === '404')) { note(token, file, 404); res.writeHead(404, cors); return res.end(); }
          const last = lastReportAt.get(token) || 0;
          if (Date.now() - last < 900) { note(token, file, 429); res.writeHead(429, cors); return res.end(); }
          let text; try { text = await body(req, 2048); } catch { note(token, file, 413); res.writeHead(413, cors); return res.end(); }
          let json; try { json = JSON.parse(text); } catch { note(token, file, 400); res.writeHead(400, cors); return res.end(); }
          lastReportAt.set(token, Date.now());
          reports.push({ at: Date.now(), token, bytes: text.length, contentType: req.headers['content-type'] || '', report: json });
          if (reports.length > 5000) reports.splice(0, 1000);
          note(token, file, 204); res.writeHead(204, cors); return res.end();
        }
        if (req.method !== 'GET' && req.method !== 'HEAD') { res.writeHead(405, cors); return res.end(); }
        return media(req, res, token, file, cors);
      }
      const json = (obj) => { res.writeHead(200, { 'content-type': 'application/json' }); res.end(JSON.stringify(obj)); };
      if (p === '/_reports') return json(reports);
      if (p === '/_requests') return json(requests);
      if (p === '/_fault' && req.method === 'POST') { api.setFault(JSON.parse(await body(req, 4096))); return json(fault); }
      if (p === '/_map' && req.method === 'POST') { const b = JSON.parse(await body(req, 4096)); tokens.set(b.token, b.target); return json({ ok: true }); }
      res.writeHead(404); res.end();
    } catch (e) {
      if (!res.headersSent) res.writeHead(500); res.end(String(e && e.message));
    }
  };
  const server = http.createServer(handler);
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(opts.port || 0, '::', resolve); });
  const port = server.address().port;
  let secure = null;
  if (opts.tls) {
    secure = https.createServer({ key: fs.readFileSync(opts.tls.key), cert: fs.readFileSync(opts.tls.cert) }, handler);
    await new Promise((resolve, reject) => { secure.once('error', reject); secure.listen(opts.tls.port || 0, '::', resolve); });
  }
  const api = {
    port, origin: `http://localhost:${port}`, secureOrigin: secure ? `https://localhost:${secure.address().port}` : null, csp, reports, requests, tokens,
    mapToken: (token, target) => tokens.set(token, target),
    setFault: (f) => {
      fault = { mode: f.mode || 'none', until: Date.now() + (Number(f.seconds) || 0) * 1000, bps: Number(f.bps) || 0, token: f.token || null };
      if (fault.mode === 'none' && !fault.token) faults.clear(); else faults.set(fault.token || '*', fault);
      if (fault.mode !== 'stall') release();
      return fault;
    },
    requestsSince: (token, sinceMs) => requests.filter((r) => r.token === token && r.at >= sinceMs),
    close: async () => {
      release();
      for (const srv of [server, secure]) if (srv) { srv.closeAllConnections(); await new Promise((resolve) => srv.close(() => resolve())); }
    }
  };
  return api;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const args = process.argv.slice(2); const o = { tokens: {}, statics: {} };
  for (let i = 0; i < args.length; i++) {
    if (args[i] === '--port') o.port = Number(args[++i]);
    else if (args[i] === '--receiver') o.receiverDir = args[++i];
    else if (args[i] === '--no-csp') o.csp = null;
    else if (args[i] === '--csp') o.csp = args[++i];
    else if (args[i] === '--tls-port') { o.tls = o.tls || {}; o.tls.port = Number(args[++i]); }
    else if (args[i] === '--tls-key') { o.tls = o.tls || {}; o.tls.key = args[++i]; }
    else if (args[i] === '--tls-cert') { o.tls = o.tls || {}; o.tls.cert = args[++i]; }
    else if (args[i] === '--map') { const [t, ...rest] = args[++i].split('='); o.tokens[t] = rest.join('='); }
    else if (args[i] === '--static') { const [t, ...rest] = args[++i].split('='); o.statics[t] = rest.join('='); }
  }
  const s = await startServer(o);
  console.log(`harness server on ${s.origin} (receiver: ${s.origin}/cast/receiver/)` + (s.secureOrigin ? ` and ${s.secureOrigin}` : ''));
  console.log('LISTENING ' + JSON.stringify({ http: s.origin, https: s.secureOrigin }));
  for (const [t, target] of s.tokens) console.log(`  ${s.origin}/cast/${t}/master.m3u8 -> ${target}`);
  const bye = () => s.close().then(() => process.exit(0));
  process.on('SIGINT', bye); process.on('SIGTERM', bye);
}
