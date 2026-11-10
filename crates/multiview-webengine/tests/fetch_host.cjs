'use strict';
// Run: node --test crates/multiview-webengine/tests/fetch_host.cjs  (no network; fake tools)
const test = require('node:test');
const assert = require('node:assert');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawn } = require('node:child_process');

const HOST = path.join(__dirname, '..', 'src', 'fetch_host.cjs');
const TOKEN = 'tok-0123456789abcdef';
const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'fh-test-'));
test.after(() => fs.rmSync(dir, { recursive: true, force: true }));

function script(name, body) {
  const p = path.join(dir, name); fs.writeFileSync(p, `#!/bin/sh\n${body}\n`, { mode: 0o755 }); return p;
}
const wrapper = (token = TOKEN) => script('wrap-' + token, `if [ "$1" = "--probe" ]; then echo "multiview-egress-boundary/1 ${token}"; exit 0; fi
[ "$1" = "--" ] || exit 97; shift; exec "$@"`);
const BYTES = 'REALBYTES-0123456789';
function ytdlp(name, dl, extra = '') {
  return script(name, `echo "$@" | grep -q -- ' -J ' && { echo '{"title":"T","uploader":"U","duration":5}'; exit 0; }
printf '%s\\n' "$0" "$@" > ${dir}/${name}.argv; env > ${dir}/${name}.env
${extra}
${dl}`);
}
// EXPLICIT FAKE validator (always accepts): only for tests of the download plumbing.
// Real decode validation is proven by the 'real ffmpeg' tests below.
const ffmpeg = script('ffmpeg-fake-ok', 'exit 0');
const REAL_FFMPEG = ['/usr/bin/ffmpeg', '/usr/local/bin/ffmpeg'].find((p) => fs.existsSync(p));
function needReal(t) {
  if (REAL_FFMPEG) return false;
  assert.notStrictEqual(process.env.CONSPECT_REQUIRE_REAL_BROWSER, '1', 'ffmpeg required');
  t.skip('real ffmpeg not found'); return true;
}
// Generates a tiny real MP4 with the real ffmpeg (local lavfi source, no network).
function genMedia(name, args) {
  const f = path.join(dir, name);
  const r = require('node:child_process').spawnSync(REAL_FFMPEG, ['-nostdin', '-v', 'error', '-y', ...args, f], { encoding: 'utf8' });
  assert.strictEqual(r.status, 0, r.stderr);
  return f;
}

function req(over = {}) {
  return { version: 1, url: 'https://www.youtube.com/watch?v=abc', ytdlp_path: ytdlp('y-ok', `printf '${BYTES}'`),
    ffmpeg_path: ffmpeg, egress_wrapper: wrapper(), egress_token: TOKEN, ...over };
}
function host(r, { env = {}, kill } = {}) {
  return new Promise((resolve) => {
    const c = spawn(process.execPath, [HOST], { env: { PATH: process.env.PATH, ...env }, stdio: ['pipe', 'pipe', 'pipe'] });
    const out = []; let err = '';
    c.stdout.on('data', (d) => out.push(d)); c.stderr.on('data', (d) => (err += d));
    c.on('close', (code) => resolve({ code, out: Buffer.concat(out), events: err.trim().split('\n').filter(Boolean).map((l) => JSON.parse(l)) }));
    c.stdin.end(JSON.stringify(r));
    if (kill) setTimeout(() => c.kill('SIGTERM'), kill);
  });
}
const last = (r) => r.events[r.events.length - 1];

test('success: real bytes, argv safe, done marker last', async () => {
  const r = await host(req());
  assert.strictEqual(r.code, 0);
  assert.strictEqual(r.out.toString(), BYTES);
  assert.deepStrictEqual(last(r), { event: 'done', ok: true, bytes: BYTES.length });
  assert.ok(r.events.some((e) => e.event === 'meta' && e.title === 'T' && e.uploader === 'U'));
  const argv = fs.readFileSync(path.join(dir, 'y-ok.argv'), 'utf8').split('\n');
  for (const f of ['--ignore-config', '--no-playlist', '--no-exec']) assert.ok(argv.includes(f), f);
  assert.strictEqual(argv[argv.indexOf('--output') + 1], '-');
  assert.strictEqual(argv[argv.length - 2], 'https://www.youtube.com/watch?v=abc');
  assert.strictEqual(argv[argv.length - 3], '--');
});

test('env is cleared: no inherited credentials', async () => {
  await host(req(), { env: { AWS_SECRET_ACCESS_KEY: 'leak', HTTPS_PROXY: 'x', YT_TOKEN: 'leak' } });
  const env = fs.readFileSync(path.join(dir, 'y-ok.env'), 'utf8');
  assert.ok(!/leak|HTTPS_PROXY/.test(env));
});

test('secret-looking child_env rejected', async () => {
  const r = await host(req({ child_env: { API_TOKEN: 'x' } }));
  assert.strictEqual(r.code, 1); assert.strictEqual(last(r).code, 'bad_request');
});

test('fails closed: missing / wrong egress attestation', async () => {
  let r = await host(req({ egress_token: undefined }));
  assert.strictEqual(last(r).code, 'egress_required');
  r = await host(req({ egress_wrapper: wrapper('other-0123456789abcdef') }));
  assert.strictEqual(r.code, 1); assert.strictEqual(last(r).code, 'egress_required'); assert.strictEqual(r.out.length, 0);
  r = await host(req({ egress_wrapper: 'wrapper' }));
  assert.strictEqual(last(r).code, 'bad_request');
});

test('url validation', async () => {
  for (const url of ['http://www.youtube.com/x', 'https://evil.com/x', 'https://youtube.com.evil.com/x',
    'https://u:p@www.youtube.com/x', 'https://127.0.0.1/x', 'https://www.youtube.com:8443/x', 'file:///etc/passwd']) {
    const r = await host(req({ url })); assert.strictEqual(r.code, 1, url); assert.ok(['bad_url', 'host_denied'].includes(last(r).code), url);
  }
});

test('nonzero exit after partial bytes: no success marker', async () => {
  const r = await host(req({ ytdlp_path: ytdlp('y-fail', `printf 'PART'; echo 'ERROR: boom' >&2; exit 1`) }));
  assert.strictEqual(r.code, 1);
  assert.ok(!r.events.some((e) => e.event === 'done'));
  assert.strictEqual(last(r).code, 'downloader_failed'); assert.match(last(r).message, /boom/);
});

test('empty output is not success; max_bytes enforced', async () => {
  let r = await host(req({ ytdlp_path: ytdlp('y-empty', 'exit 0') }));
  assert.strictEqual(last(r).code, 'empty');
  r = await host(req({ max_bytes: 5 }));
  assert.strictEqual(r.code, 1); assert.ok(!r.events.some((e) => e.event === 'done'));
});

test('stderr noise bounded, progress parsed, never in stdout', async () => {
  const noise = `i=0; while [ $i -lt 3000 ]; do echo "NOISE line $i xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx" >&2; i=$((i+1)); done
echo "MVPROG 10 20" >&2; sleep 1; echo "MVPROG 20 20" >&2`;
  const r = await host(req({ ytdlp_path: ytdlp('y-noise', `printf '${BYTES}'`, noise) }));
  assert.strictEqual(r.code, 0); assert.strictEqual(r.out.toString(), BYTES);
  assert.ok(r.events.some((e) => e.event === 'progress' && e.downloaded === 10 && e.total === 20));
  assert.ok(r.events.length < 10);
});

test('deadline kills owned children', async () => {
  const pidf = path.join(dir, 'pid-dl');
  const r = await host(req({ timeout_ms: 600, extract_metadata: false, ytdlp_path: ytdlp('y-hang', 'sleep 30', `echo $$ > ${pidf}`) }));
  assert.strictEqual(last(r).code, 'timeout'); assert.ok(!r.events.some((e) => e.event === 'done'));
  await new Promise((s) => setTimeout(s, 300));
  assert.throws(() => process.kill(Number(fs.readFileSync(pidf, 'utf8')), 0));
});

test('SIGTERM cancellation kills children, no success', async () => {
  const pidf = path.join(dir, 'pid-cancel');
  const r = await host(req({ extract_metadata: false, ytdlp_path: ytdlp('y-hang2', 'sleep 30', `echo $$ > ${pidf}`) }), { kill: 700 });
  assert.notStrictEqual(r.code, 0); assert.ok(!r.events.some((e) => e.event === 'done'));
  await new Promise((s) => setTimeout(s, 300));
  assert.throws(() => process.kill(Number(fs.readFileSync(pidf, 'utf8')), 0));
});

test('large stream flows with bounded memory path (64 MiB)', async () => {
  const r = await host(req({ ytdlp_path: ytdlp('y-big', 'head -c 67108864 /dev/zero') }));
  assert.strictEqual(r.code, 0); assert.strictEqual(r.out.length, 67108864); assert.strictEqual(last(r).bytes, 67108864);
});

// ---- mandatory media validation ----
const catFile = (f) => `cat ${f}`;
function realReq(src, over = {}) {
  return req({ ffmpeg_path: REAL_FFMPEG, ytdlp_path: ytdlp('y-' + path.basename(src), catFile(src)), ...over });
}
function tmpHost(r) {
  const t = fs.mkdtempSync(path.join(dir, 'tmp-'));
  return host(r, { env: { TMPDIR: t } }).then((x) => ({ ...x, tmp: t }));
}

test('real ffmpeg: valid generated MP4 passes decode validation, bytes identical', async (t) => {
  if (needReal(t)) return;
  const f = genMedia('ok.mp4', ['-f', 'lavfi', '-i', 'testsrc=size=64x64:rate=5:duration=1', '-pix_fmt', 'yuv420p', '-c:v', 'mpeg4', '-f', 'mp4']);
  const r = await tmpHost(realReq(f));
  assert.strictEqual(r.code, 0, JSON.stringify(r.events));
  assert.ok(r.out.equals(fs.readFileSync(f)));
  assert.strictEqual(last(r).event, 'done');
  assert.deepStrictEqual(fs.readdirSync(r.tmp), [], 'spool/run dir cleaned');
});

test('real ffmpeg: valid MOV passes', async (t) => {
  if (needReal(t)) return;
  const f = genMedia('ok.mov', ['-f', 'lavfi', '-i', 'testsrc=size=64x64:rate=5:duration=1', '-pix_fmt', 'yuv420p', '-c:v', 'mpeg4', '-f', 'mov']);
  const r = await host(realReq(f));
  assert.strictEqual(r.code, 0, JSON.stringify(r.events)); assert.strictEqual(last(r).event, 'done');
});

test('real ffmpeg: arbitrary bytes, JSON, HTML all rejected (invalid_media, no done, spool cleaned)', async (t) => {
  if (needReal(t)) return;
  for (const [n, body] of [['fake.bin', BYTES], ['e.json', '{"error":"nope"}'], ['e.html', '<!doctype html><html><body>blocked</body></html>']]) {
    const f = path.join(dir, n); fs.writeFileSync(f, body);
    const r = await tmpHost(realReq(f));
    assert.strictEqual(r.code, 1, n); assert.strictEqual(last(r).code, 'invalid_media', n);
    assert.ok(!r.events.some((e) => e.event === 'done'), n);
    assert.deepStrictEqual(fs.readdirSync(r.tmp), [], n);
  }
});

test('real ffmpeg: corrupt (truncated header) and audio-only media rejected', async (t) => {
  if (needReal(t)) return;
  const good = genMedia('c.mp4', ['-f', 'lavfi', '-i', 'testsrc=size=64x64:rate=5:duration=1', '-pix_fmt', 'yuv420p', '-c:v', 'mpeg4', '-f', 'mp4']);
  const bad = path.join(dir, 'corrupt.mp4'); const g = fs.readFileSync(good);
  fs.writeFileSync(bad, Buffer.concat([g.subarray(0, 64), Buffer.alloc(512, 0xaa)]));
  let r = await host(realReq(bad));
  assert.strictEqual(last(r).code, 'invalid_media'); assert.ok(!r.events.some((e) => e.event === 'done'));
  const audio = genMedia('audio.mp4', ['-f', 'lavfi', '-i', 'sine=d=1', '-c:a', 'aac', '-f', 'mp4']);
  r = await host(realReq(audio));
  assert.strictEqual(last(r).code, 'invalid_media'); assert.ok(!r.events.some((e) => e.event === 'done'));
});

test('validator argv is the hardened decode contract', () => {
  const { validatorArgv } = require(HOST);
  assert.deepStrictEqual(validatorArgv({ ffmpeg: '/f' }, '/s/spool'), ['/f', '-nostdin', '-v', 'error', '-f', 'mov', '-protocol_whitelist', 'file', '-enable_drefs', '0',
    '-i', 'file:/s/spool', '-map', '0:v:0', '-frames:v', '1', '-f', 'null', '-']);
});

test('fake validator: spool is private 0600 in 0700 dir, env cleared, run dir cleaned, done after validator', async () => {
  const rec = path.join(dir, 'val.rec');
  const v = script('ffmpeg-fake-rec', `printf '%s\\n' "$@" > ${rec}.argv; env > ${rec}.env
for a in "$@"; do f="$a"; done; i=""; for a in "$@"; do [ "$p" = -i ] && i="$a"; p="$a"; done
f="\${i#file:}"; stat -c '%a' "$f" > ${rec}.fmode; stat -c '%a' "$(dirname "$f")" > ${rec}.dmode; cat "$f" > ${rec}.copy
sleep 0.3; exit 0`);
  const r = await tmpHost(req({ ffmpeg_path: v }));
  assert.strictEqual(r.code, 0, JSON.stringify(r.events));
  assert.strictEqual(fs.readFileSync(rec + '.fmode', 'utf8').trim(), '600');
  assert.strictEqual(fs.readFileSync(rec + '.dmode', 'utf8').trim(), '700');
  assert.strictEqual(fs.readFileSync(rec + '.copy', 'utf8'), BYTES);
  assert.ok(!/leak|HTTPS_PROXY/.test(fs.readFileSync(rec + '.env', 'utf8')));
  assert.deepStrictEqual(fs.readdirSync(r.tmp), []);
  const argv = fs.readFileSync(rec + '.argv', 'utf8').split('\n');
  assert.ok(argv.includes('-nostdin') && argv.includes('file') && argv.includes('mov') && argv.includes('-enable_drefs') && argv.includes('0:v:0'));
});

test('fake validator exit nonzero -> invalid_media, no done (explicit fake vs real distinguished)', async () => {
  const r = await host(req({ ffmpeg_path: script('ffmpeg-fake-bad', 'echo "Invalid data" >&2; exit 1') }));
  assert.strictEqual(r.code, 1); assert.strictEqual(last(r).code, 'invalid_media'); assert.match(last(r).message, /Invalid data/);
  assert.ok(!r.events.some((e) => e.event === 'done'));
});

test('deadline during validation kills validator group and cleans spool', async () => {
  const pidf = path.join(dir, 'pid-val');
  const r = await tmpHost(req({ timeout_ms: 800, extract_metadata: false, ffmpeg_path: script('ffmpeg-fake-hang', `echo $$ > ${pidf}; sleep 30`) }));
  assert.strictEqual(last(r).code, 'timeout'); assert.ok(!r.events.some((e) => e.event === 'done'));
  await new Promise((s) => setTimeout(s, 300));
  assert.throws(() => process.kill(Number(fs.readFileSync(pidf, 'utf8')), 0));
  assert.deepStrictEqual(fs.readdirSync(r.tmp), []);
});

test('SIGTERM during validation cancels, no done, spool cleaned', async () => {
  const r = await (async () => {
    const t = fs.mkdtempSync(path.join(dir, 'tmp-'));
    const x = await host(req({ extract_metadata: false, ffmpeg_path: script('ffmpeg-fake-hang2', 'sleep 30') }), { env: { TMPDIR: t }, kill: 900 });
    return { ...x, tmp: t };
  })();
  assert.notStrictEqual(r.code, 0); assert.ok(!r.events.some((e) => e.event === 'done'));
  assert.deepStrictEqual(fs.readdirSync(r.tmp), []);
});

test('quota unaffected: over-quota fails too_large before validation, spool cleaned', async () => {
  const marker = path.join(dir, 'val-ran');
  const r = await tmpHost(req({ max_bytes: 5, ffmpeg_path: script('ffmpeg-fake-mark', `touch ${marker}; exit 0`) }));
  assert.strictEqual(r.code, 1); assert.ok(!r.events.some((e) => e.event === 'done'));
  assert.ok(!fs.existsSync(marker));
  assert.deepStrictEqual(fs.readdirSync(r.tmp), []);
});

test('real ffmpeg: planted valid sibling + ffconcat payload is rejected, never done', async (t) => {
  if (needReal(t)) return;
  const sib = genMedia('sibling.mp4', ['-f', 'lavfi', '-i', 'testsrc=size=64x64:rate=5:duration=1', '-pix_fmt', 'yuv420p', '-c:v', 'mpeg4', '-f', 'mp4']);
  const payload = path.join(dir, 'concat.mp4');
  fs.writeFileSync(payload, "ffconcat version 1.0\nfile 'planted.mp4'\n");
  // The downloader executes in the spool directory: plant a valid relative
  // sibling there. The old autodetect validator accepts this exact payload.
  const downloader = ytdlp('y-relative-playlist', `cp '${sib}' planted.mp4; cat '${payload}'`);
  const r = await tmpHost(realReq(payload, {ytdlp_path: downloader}));
  assert.strictEqual(r.code, 1, JSON.stringify(r.events));
  assert.strictEqual(last(r).code, 'invalid_media');
  assert.ok(!r.events.some((e) => e.event === 'done'));
  assert.deepStrictEqual(fs.readdirSync(r.tmp), []);
});

test('slow reader: exact stdout byte count, marker matches, natural exit', async () => {
  for (const n of [4194304, 67108864]) {
    const r = await new Promise((resolve) => {
      const c = spawn(process.execPath, [HOST], { env: { PATH: process.env.PATH }, stdio: ['pipe', 'pipe', 'pipe'] });
      let total = 0; let err = '';
      c.stderr.on('data', (d) => (err += d));
      c.stdout.on('data', (d) => { total += d.length; c.stdout.pause(); setTimeout(() => c.stdout.resume(), 2); });
      c.on('close', (code) => resolve({ code, total, events: err.trim().split('\n').filter(Boolean).map((l) => JSON.parse(l)) }));
      c.stdin.end(JSON.stringify(req({ ytdlp_path: ytdlp('y-slow' + n, `head -c ${n} /dev/zero`) })));
    });
    assert.strictEqual(r.code, 0, JSON.stringify(r.events));
    assert.strictEqual(r.total, n);
    assert.strictEqual(last(r).bytes, n);
  }
});
