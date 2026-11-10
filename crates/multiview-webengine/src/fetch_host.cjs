#!/usr/bin/env node
'use strict';
// Internal URL-producer host: fetches ACTUAL video bytes with yt-dlp (not browser capture).
// Contract: docs/architecture/web-fetch-host-contract.md. Node builtins only.
//   stdin  : one JSON request (see validateRequest), then EOF.
//   stdout : media bytes ONLY (bounded stream, backpressured, never buffered whole).
//   stderr : NDJSON events {"event":"meta"|"progress"|"done"|"error",...}.
//   The "done" marker (ok:true) is emitted only after the downloader exited 0 AND
//   every media byte has been flushed to the parent's stdout. Exit 0 iff done emitted.
//   Mandatory media validation: the downloaded bytes are tee'd to a private (0600) spool file
//   inside the private (0700) per-run dir, bounded by the same max_bytes quota. After the
//   download closes OK, the configured ffmpeg DECODES the first video frame (protocol
//   whitelist file,pipe, no stdin). "done" is emitted only if that validator exits 0;
//   JSON/HTML/corrupt/audio-only payloads fail with code invalid_media. Spool is removed on
//   every path (it lives in the run dir, which cleanup() deletes).
//   SIGTERM/SIGINT or parent stdout closing = cancellation: owned process group is killed.
const { spawn } = require('node:child_process');
const { Transform, Writable } = require('node:stream');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const HARD_TIMEOUT_MS = 120000;
const MAX_REQUEST_BYTES = 64 * 1024;
const MAX_METADATA_BYTES = 4 * 1024 * 1024;
const DEFAULT_MAX_BYTES = 2 * 1024 * 1024 * 1024;
const DEFAULT_HOSTS = ['youtube.com', 'youtu.be', 'youtube-nocookie.com'];
const CODECS = { h264: 'vcodec^=avc1', vp9: 'vcodec^=vp09', av1: 'vcodec^=av01' };
const SECRETISH = /KEY|TOKEN|SECRET|PASS|COOKIE|AUTH|CRED|SESSION/i;
const PROBE_PREFIX = 'multiview-egress-boundary/1 ';

class FetchError extends Error {
  constructor(code, message) { super(message); this.code = code; }
}

function isAbs(p) { return typeof p === 'string' && path.isAbsolute(p) && !p.includes('\0') && path.normalize(p) === p; }

function validateUrl(raw, allow) {
  if (typeof raw !== 'string' || raw.length > 2048 || /[\s\0]/.test(raw)) throw new FetchError('bad_url', 'url invalid');
  let u;
  try { u = new URL(raw); } catch { throw new FetchError('bad_url', 'url unparsable'); }
  if (u.protocol !== 'https:') throw new FetchError('bad_url', 'https required');
  if (u.username || u.password) throw new FetchError('bad_url', 'userinfo forbidden');
  if (u.port && u.port !== '443') throw new FetchError('bad_url', 'port forbidden');
  const h = u.hostname.toLowerCase().replace(/\.$/, '');
  if (/^[\d.]+$/.test(h) || h.includes(':') || h.startsWith('[')) throw new FetchError('bad_url', 'ip literal forbidden');
  if (!allow.some((a) => h === a || h.endsWith('.' + a))) throw new FetchError('host_denied', 'host not allowlisted');
  return u.href;
}

function validateRequest(r) {
  if (!r || typeof r !== 'object' || r.version !== 1) throw new FetchError('bad_request', 'version must be 1');
  for (const k of ['ytdlp_path', 'ffmpeg_path', 'egress_wrapper']) {
    if (!isAbs(r[k])) throw new FetchError('bad_request', `${k} must be a normalized absolute path`);
  }
  if (typeof r.egress_token !== 'string' || r.egress_token.length < 16 || !/^[\w.-]+$/.test(r.egress_token)) {
    throw new FetchError('egress_required', 'egress_token (>=16 chars [\\w.-]) required');
  }
  const allow = (r.host_allowlist === undefined ? DEFAULT_HOSTS : r.host_allowlist);
  if (!Array.isArray(allow) || !allow.length || !allow.every((a) => typeof a === 'string' && /^[a-z0-9.-]+$/.test(a))) {
    throw new FetchError('bad_request', 'host_allowlist invalid');
  }
  const url = validateUrl(r.url, allow);
  const codec = r.codec === undefined ? 'h264' : r.codec;
  if (!Object.hasOwn(CODECS, codec)) throw new FetchError('bad_request', 'codec invalid');
  const int = (v, d, lo, hi, n) => {
    if (v === undefined) return d;
    if (!Number.isInteger(v) || v < lo || v > hi) throw new FetchError('bad_request', `${n} out of range`);
    return v;
  };
  const env = {};
  for (const [k, v] of Object.entries(r.child_env || {})) {
    if (!/^[A-Z][A-Z0-9_]*$/.test(k) || SECRETISH.test(k) || typeof v !== 'string' || v.includes('\0')) {
      throw new FetchError('bad_request', `child_env ${k} rejected`);
    }
    env[k] = v;
  }
  return {
    url, codec, env,
    ytdlp: r.ytdlp_path, ffmpeg: r.ffmpeg_path, wrapper: r.egress_wrapper, token: r.egress_token,
    maxHeight: int(r.max_height, 1080, 144, 4320, 'max_height'),
    maxBytes: int(r.max_bytes, DEFAULT_MAX_BYTES, 1, DEFAULT_MAX_BYTES, 'max_bytes'),
    maxDuration: int(r.max_duration_secs, 4 * 3600, 1, 24 * 3600, 'max_duration_secs'),
    timeoutMs: int(r.timeout_ms, HARD_TIMEOUT_MS, 1, HARD_TIMEOUT_MS, 'timeout_ms'),
    metadata: r.extract_metadata !== false,
  };
}

function emit(obj) { try { process.stderr.write(JSON.stringify(obj) + '\n'); } catch { /* ignore */ } }

function baseArgs(q) {
  return ['--ignore-config', '--no-playlist', '--no-exec', '--no-cache-dir', '--no-mtime',
    '--no-write-info-json', '--no-cookies', '--socket-timeout', '20', '--retries', '3',
    '--ffmpeg-location', q.ffmpeg, '--max-filesize', String(q.maxBytes),
    '--match-filter', `duration<=${q.maxDuration}`];
}
function format(q) {
  const f = `[ext=mp4][${CODECS[q.codec]}][height<=${q.maxHeight}]`;
  return `b${f}`; // single progressive stream: streamable to stdout without merge
}
// Exported for tests: argv for the downloader (after wrapper `--`).
function downloadArgv(q) {
  return [q.ytdlp, ...baseArgs(q), '--format', format(q), '--quiet', '--progress', '--newline',
    '--progress-template', 'download:MVPROG %(progress.downloaded_bytes)s %(progress.total_bytes)s',
    '--output', '-', '--', q.url];
}
function metadataArgv(q) {
  return [q.ytdlp, ...baseArgs(q), '--format', format(q), '-J', '--no-warnings', '--', q.url];
}

// Exported for tests: argv for the local decode validator (run directly, never via network).
function validatorArgv(q, file) {
  return [q.ffmpeg, '-nostdin', '-v', 'error', '-f', 'mov', '-protocol_whitelist', 'file', '-enable_drefs', '0',
    '-i', 'file:' + file,
    '-map', '0:v:0', '-frames:v', '1', '-f', 'null', '-'];
}

function childEnv(q, home) {
  return { PATH: '/usr/bin:/bin', HOME: home, LANG: 'C', LC_ALL: 'C', ...q.env };
}

function run() {
  let chunks = []; let size = 0;
  process.stdin.on('data', (c) => {
    size += c.length;
    if (size > MAX_REQUEST_BYTES) { fail(new FetchError('bad_request', 'request too large')); return; }
    chunks.push(c);
  });
  process.stdin.on('end', () => {
    let q;
    try { q = validateRequest(JSON.parse(Buffer.concat(chunks).toString('utf8'))); } catch (e) {
      return fail(e instanceof FetchError ? e : new FetchError('bad_request', 'request not JSON'));
    }
    main(q).catch((e) => fail(e instanceof FetchError ? e : new FetchError('internal', String(e && e.message))));
  });
}

let finished = false;
const owned = new Set();
let home = null;
function killAll(sig) {
  for (const c of owned) { try { process.kill(-c.pid, sig); } catch { try { c.kill(sig); } catch { /* gone */ } } }
}
function cleanup() { if (home) { try { fs.rmSync(home, { recursive: true, force: true }); } catch { /* ignore */ } home = null; } }
function fail(err) {
  if (finished) return;
  finished = true;
  killAll('SIGTERM');
  setTimeout(() => killAll('SIGKILL'), 1500).unref();
  cleanup();
  emit({ event: 'error', ok: false, code: err.code || 'internal', message: err.message });
  process.exitCode = 1;
  setTimeout(() => process.exit(1), 50).unref();
}

function spawnOwned(q, argv, stdio) {
  const c = spawn(q.wrapper, ['--', ...argv], { env: childEnv(q, home), stdio, detached: true, shell: false, cwd: home });
  owned.add(c);
  c.on('close', () => owned.delete(c));
  return c;
}

function writeAll(fd, buf) {
  return new Promise((resolve, reject) => {
    let off = 0;
    const step = () => fs.write(fd, buf, off, buf.length - off, null, (err, n) => {
      if (err) return reject(err);
      off += n;
      return off >= buf.length ? resolve() : step();
    });
    step();
  });
}

// Tee sink: spool file first, then parent stdout; the callback fires only once stdout has
// accepted the chunk (or drained), so backpressure propagates to the downloader pipe.
function teeSink(fd) {
  return new Writable({
    highWaterMark: 64 * 1024,
    write(chunk, _e, cb) {
      writeAll(fd, chunk).then(() => {
        if (process.stdout.write(chunk)) cb(); else process.stdout.once('drain', () => cb());
      }, () => cb(new FetchError('spool_io', 'spool write failed')));
    },
  });
}

// Decode-validate the spooled file with the trusted ffmpeg (own process group, cleared env).
function runValidator(q, file) {
  return new Promise((resolve) => {
    const c = spawn(q.ffmpeg, validatorArgv(q, file).slice(1), {
      env: childEnv(q, home), stdio: ['ignore', 'ignore', 'pipe'], detached: true, shell: false, cwd: home,
    });
    owned.add(c);
    const tail = tailCollector(2048);
    c.stderr.on('data', (d) => tail.add(d.toString('utf8')));
    c.on('error', () => { owned.delete(c); resolve({ code: -1, tail: 'validator spawn failed' }); });
    c.on('close', (code, sig) => { owned.delete(c); resolve({ code: code === null ? sig : code, tail: tail.get() }); });
  });
}

function tailCollector(max) {
  let buf = '';
  return { add(s) { buf = (buf + s).slice(-max); }, get() { return buf; } };
}

function lines(onLine) {
  let acc = '';
  return (d) => {
    acc += d.toString('utf8');
    let i;
    while ((i = acc.indexOf('\n')) >= 0) { onLine(acc.slice(0, i).replace(/\r$/, '')); acc = acc.slice(i + 1); }
    if (acc.length > 8192) acc = acc.slice(-8192); // bound noise without newlines
  };
}

async function probeWrapper(q) {
  // Fail closed: wrapper must prove it is the controller-provisioned egress boundary.
  const c = spawn(q.wrapper, ['--probe'], { env: { PATH: '/usr/bin:/bin' }, stdio: ['ignore', 'pipe', 'ignore'], shell: false, detached: true });
  owned.add(c);
  let out = '';
  c.stdout.on('data', (d) => { if (out.length < 512) out += d.toString('utf8'); });
  const code = await new Promise((res) => { c.on('error', () => res(-1)); c.on('close', res); });
  owned.delete(c);
  if (code !== 0 || out.trim() !== PROBE_PREFIX + q.token) throw new FetchError('egress_required', 'egress boundary attestation failed');
}

function runMetadata(q) {
  return new Promise((resolve) => {
    const c = spawnOwned(q, metadataArgv(q), ['ignore', 'pipe', 'ignore']);
    const bufs = []; let n = 0; let over = false;
    c.stdout.on('data', (d) => { n += d.length; if (n > MAX_METADATA_BYTES) { over = true; try { process.kill(-c.pid, 'SIGKILL'); } catch { /* */ } } else bufs.push(d); });
    c.on('error', () => resolve(null));
    c.on('close', (code) => {
      if (code !== 0 || over) return resolve(null);
      try {
        const j = JSON.parse(Buffer.concat(bufs).toString('utf8'));
        const s = (v) => (typeof v === 'string' ? v.slice(0, 300) : null);
        resolve({ title: s(j.title), uploader: s(j.uploader), duration: Number.isFinite(j.duration) ? j.duration : null });
      } catch { resolve(null); }
    });
  });
}

async function main(q) {
  home = fs.mkdtempSync(path.join(os.tmpdir(), 'mv-fetch-'));
  fs.chmodSync(home, 0o700);
  const spool = path.join(home, 'spool.media');
  const spoolFd = fs.openSync(spool, 'wx', 0o600);
  let spoolOpen = true;
  const closeSpool = () => { if (spoolOpen) { spoolOpen = false; try { fs.closeSync(spoolFd); } catch { /* ignore */ } } };
  process.once('exit', closeSpool);
  const deadline = setTimeout(() => fail(new FetchError('timeout', `deadline ${q.timeoutMs}ms exceeded`)), q.timeoutMs);
  for (const s of ['SIGTERM', 'SIGINT']) process.on(s, () => fail(new FetchError('cancelled', `cancelled by ${s}`)));
  process.stdout.on('error', () => fail(new FetchError('cancelled', 'parent stdout closed')));

  await probeWrapper(q);
  if (finished) return;
  if (q.metadata) {
    const m = await runMetadata(q);
    if (finished) return;
    if (m) emit({ event: 'meta', ...m });
  }

  const dl = spawnOwned(q, downloadArgv(q), ['ignore', 'pipe', 'pipe']);
  const tail = tailCollector(2048);
  let lastProg = 0;
  dl.stderr.on('data', lines((l) => {
    if (l.startsWith('MVPROG ')) {
      const now = Date.now();
      if (now - lastProg < 500) return;
      lastProg = now;
      const [, a, b] = l.split(' ');
      const done = Number(a); const total = Number(b);
      emit({ event: 'progress', downloaded: Number.isFinite(done) ? done : null, total: Number.isFinite(total) ? total : null });
    } else tail.add(l.slice(0, 300) + '\n');
  }));

  let bytes = 0;
  const counter = new Transform({
    transform(chunk, _e, cb) {
      bytes += chunk.length;
      if (bytes > q.maxBytes) return cb(new FetchError('too_large', 'max_bytes exceeded'));
      cb(null, chunk);
    },
  });
  counter.on('error', (e) => fail(e instanceof FetchError ? e : new FetchError('internal', 'stream error')));
  const sink = teeSink(spoolFd);
  sink.on('error', (e) => fail(e instanceof FetchError ? e : new FetchError('internal', 'sink error')));
  dl.stdout.pipe(counter).pipe(sink); // pipe() honours backpressure end to end
  const drained = new Promise((res) => sink.on('finish', res));
  const exit = new Promise((res) => { dl.on('error', () => res({ code: -1 })); dl.on('close', (code, sig) => res({ code, sig })); });

  const [ex] = await Promise.all([exit, drained]);
  if (finished) return;
  if (ex.code !== 0) return fail(new FetchError('downloader_failed', `downloader exit ${ex.code ?? ex.sig}: ${tail.get().trim().slice(-300)}`));
  if (bytes === 0) return fail(new FetchError('empty', 'downloader produced no bytes'));
  closeSpool();
  const v = await runValidator(q, spool);
  if (finished) return;
  if (v.code !== 0) return fail(new FetchError('invalid_media', `ffmpeg decode validation failed (${v.code}): ${v.tail.trim().slice(-300)}`));
  await new Promise((res, reject) => {
    process.stdout.once('error', reject);
    process.stdout.once('finish', res);
    process.stdout.end();
  }); // all media bytes flushed before the completion marker
  if (finished) return;
  finished = true;
  clearTimeout(deadline);
  cleanup();
  emit({ event: 'done', ok: true, bytes });
  process.exitCode = 0; // natural exit: no forced process.exit, stdout already flushed
}

if (require.main === module) run();
module.exports = { validateRequest, downloadArgv, validatorArgv, metadataArgv, PROBE_PREFIX };
