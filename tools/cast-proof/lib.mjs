// Shared harness helpers: Chrome launch, the in-page <video> sampler, the playback criteria, result printing.
import fs from 'node:fs';
import http from 'node:http';
import https from 'node:https';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawn } from 'node:child_process';
import puppeteer from 'puppeteer-core';

export const here = path.dirname(fileURLToPath(import.meta.url));
export const outDir = process.env.OUT_DIR ? path.resolve(process.env.OUT_DIR) : path.join(here, 'out');
// CHROME_PATH wins; else the newest Playwright Chromium in the cache (full Chromium, which decodes H.264 + AAC;
// chrome-headless-shell builds may not).
function findChrome() {
  if (process.env.CHROME_PATH) return process.env.CHROME_PATH;
  const cache = path.join(process.env.PLAYWRIGHT_BROWSERS_PATH || path.join(os.homedir(), '.cache', 'ms-playwright'));
  let dirs = [];
  try { dirs = fs.readdirSync(cache).filter((d) => /^chromium-\d+$/.test(d)).sort((a, b) => Number(b.split('-')[1]) - Number(a.split('-')[1])); } catch { /* no cache */ }
  for (const d of dirs) { const p = path.join(cache, d, 'chrome-linux64', 'chrome'); if (fs.existsSync(p)) return p; }
  return 'chrome';
}
export const CHROME = findChrome();
export const FPS = 30000 / 1001;
export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

export function parseArgs(argv, defaults = {}) {
  const out = { _: [], ...defaults };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a.startsWith('--')) { const k = a.slice(2); const v = argv[i + 1] !== undefined && !argv[i + 1].startsWith('--') ? argv[++i] : 'true'; out[k] = /^-?\d+(\.\d+)?$/.test(v) ? Number(v) : v; }
    else out._.push(a);
  }
  return out;
}

// Children this process started (Chrome, ffmpeg). They are killed when the process exits for any reason that
// node can see (normal exit, an exception, SIGINT/SIGTERM/SIGHUP, e.g. from `timeout` or run-all.sh's trap).
const children = new Set();
// Chrome profiles live under <out>/chrome-profiles (not in /tmp, where a killed run would leave them behind) and
// are removed when the browser closes or this process exits. Only directories created here are ever removed.
const profileRoot = path.join(outDir, 'chrome-profiles');
const profiles = new Set();
function dropProfile(dir) {
  if (!profiles.has(dir) || path.dirname(dir) !== profileRoot) return;
  profiles.delete(dir);
  try { fs.rmSync(dir, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 }); } catch { /* run-all.sh clears the root */ }
}
process.on('exit', () => {
  for (const c of children) { try { c.kill('SIGKILL'); } catch { /* gone */ } }
  for (const dir of [...profiles]) dropProfile(dir);
});
for (const sig of ['SIGINT', 'SIGTERM', 'SIGHUP']) process.on(sig, () => process.exit(128 + (sig === 'SIGINT' ? 2 : sig === 'SIGTERM' ? 15 : 1)));
function track(child) { if (child) { children.add(child); child.once('exit', () => children.delete(child)); } }

export async function launchBrowser() {
  fs.mkdirSync(profileRoot, { recursive: true });
  const userDataDir = fs.mkdtempSync(path.join(profileRoot, 'p-'));
  profiles.add(userDataDir);
  const browser = await puppeteer.launch({ handleSIGINT: false, handleSIGTERM: false, handleSIGHUP: false, userDataDir,
    executablePath: CHROME, headless: true, defaultViewport: { width: 1280, height: 720 },
    args: [`--conspect-cast-harness=${process.env.HARNESS_RUN_ID || 'manual'}`, '--no-sandbox', '--ignore-certificate-errors', '--autoplay-policy=no-user-gesture-required', '--disable-gpu', '--mute-audio', '--disable-background-timer-throttling', '--disable-renderer-backgrounding']
  });
  track(browser.process());
  browser.process().once('exit', () => setTimeout(() => dropProfile(userDataDir), 200));
  return browser;
}

/** Collect console errors, page errors, failed requests and CSP violations of a page (call before goto). */
export async function watchPage(page) {
  const log = { console: [], errors: [], pageErrors: [], requestFailed: [], httpErrors: [] };
  page.on('response', (r) => { if (r.status() >= 400) log.httpErrors.push(`${r.status()} ${r.url().slice(0, 200)}`); });
  page.on('console', (m) => { const line = `${m.type()}: ${m.text().slice(0, 500)}`; log.console.push(line); if (m.type() === 'error') log.errors.push(line); });
  page.on('pageerror', (e) => log.pageErrors.push(String(e && e.stack || e).slice(0, 600)));
  page.on('requestfailed', (r) => log.requestFailed.push(`${r.url().slice(0, 200)} ${(r.failure() || {}).errorText}`));
  await page.evaluateOnNewDocument(() => {
    window.__csp = [];
    document.addEventListener('securitypolicyviolation', (e) => window.__csp.push({ directive: e.violatedDirective, blocked: e.blockedURI, source: e.sourceFile, line: e.lineNumber, sample: e.sample }));
  });
  log.csp = () => page.evaluate(() => window.__csp || []).catch(() => []);
  return log;
}

/**
 * Runs in the page. Samples the <video> (also inside <cast-media-player>'s shadow root) once per second and
 * records media events. window.__probeExtra() may add player figures to each sample.
 */
export function installSampler() {
  const P = window.__probe = { installedAt: Date.now(), events: [], samples: [], video: null, startedAt: 0 };
  const find = () => { const c = document.querySelector('cast-media-player'); return (c && c.shadowRoot && c.shadowRoot.querySelector('video')) || document.querySelector('video'); };
  const ahead = (v) => { const b = v.buffered; for (let i = 0; i < b.length; i++) if (v.currentTime >= b.start(i) - 0.25 && v.currentTime <= b.end(i)) return b.end(i) - v.currentTime; return 0; };
  let lastCt = -1; let advancing = 0; let n = 0; let runStart = null;
  setInterval(() => {
    if (!P.video) {
      const v = find(); if (!v) return;
      P.video = v;
      for (const type of ['loadeddata', 'canplay', 'playing', 'waiting', 'stalled', 'pause', 'seeking', 'seeked', 'ended', 'error', 'emptied', 'ratechange'])
        v.addEventListener(type, () => P.events.push({ at: Date.now(), type, ct: v.currentTime, rate: v.playbackRate }));
    }
    const v = P.video;
    // "started": the picture really moves (three 100 ms polls in a row with the clock advancing).
    const beforeCt = lastCt;
    // Moving = the clock went forward by a plausible amount for one 100 ms poll. A jump (the initial seek to the
    // live position, a gap jump, a live-edge seek) is not playback: run 3 of the second series took the clock
    // from before that seek as the start and reported 71.9 s of advance for 60 s of pictures.
    const step = v.currentTime - lastCt;
    if (step > 0.02 && step < 0.5 && !v.paused && lastCt >= 0) advancing++; else advancing = 0;
    lastCt = v.currentTime;
    // The clock began to move between the previous poll and this one: remember that moment.
    if (advancing === 1) { const q0 = v.getVideoPlaybackQuality ? v.getVideoPlaybackQuality() : null; runStart = { at: Date.now() - 50, ct: beforeCt, frames: q0 ? q0.totalVideoFrames : 0, dropped: q0 ? q0.droppedVideoFrames : 0 }; }
    if (advancing === 3 && !P.startedAt) { P.startedAt = runStart.at; P.startCt = runStart.ct; P.start = runStart; P.events.push({ at: runStart.at, type: 'advancing', ct: runStart.ct }); }
    if (++n % 10 !== 0) return;
    const q = v.getVideoPlaybackQuality ? v.getVideoPlaybackQuality() : { totalVideoFrames: 0, droppedVideoFrames: 0 };
    let extra = null; try { extra = window.__probeExtra ? window.__probeExtra() : null; } catch (e) { extra = { extraError: String(e) }; }
    P.samples.push({ at: Date.now(), ct: v.currentTime, rs: v.readyState, paused: v.paused, rate: v.playbackRate, ahead: ahead(v),
      vb: v.webkitVideoDecodedByteCount, ab: v.webkitAudioDecodedByteCount, frames: q.totalVideoFrames, dropped: q.droppedVideoFrames,
      w: v.videoWidth, h: v.videoHeight, extra });
  }, 100);
}

const r1 = (x) => (typeof x === 'number' && isFinite(x) ? Math.round(x * 10) / 10 : null);
export const check = (name, pass, value, limit) => ({ name, pass: !!pass, value, limit });

/**
 * The playback criteria shared by the Shaka and the receiver tests.
 * probe = window.__probe; loadAt = wall-clock ms of the load; seconds = planned run length after the start.
 */
export function evaluatePlayback(probe, loadAt, seconds, o = {}) {
  const checks = []; const m = {};
  const ev = probe.events; const startedAt = probe.startedAt || 0;
  const firstFrame = ev.find((e) => e.type === 'loadeddata' && e.at >= loadAt - 50);
  m.first_frame_ms = firstFrame ? firstFrame.at - loadAt : null;
  m.start_ms = startedAt ? startedAt - loadAt : null;
  checks.push(check('first frame within 10 s of load', m.first_frame_ms !== null && m.first_frame_ms <= 10000, m.first_frame_ms, '<= 10000 ms'));
  const startLimit = o.startLimitMs || 10000;
  checks.push(check(`picture moving within ${startLimit / 1000} s of load`, m.start_ms !== null && m.start_ms <= startLimit, m.start_ms, `<= ${startLimit} ms`));
  const s = probe.samples.filter((x) => startedAt && x.at >= startedAt);
  const first = s[0]; const last = s[s.length - 1];
  const wall = first ? (last.at - first.at) / 1000 : 0;
  m.run_s = r1(wall);
  // Exact figures when the probe carries them (clock at the moment the picture started moving and at the end of
  // the run); the run lasts `seconds` from the picture start (see runFor), so the 6 s allowance is stalls only.
  const startCt = typeof probe.startCt === 'number' ? probe.startCt : (first ? first.ct : 0);
  const endCt = probe.end ? probe.end.ct : (last ? last.ct : 0);
  m.advance_s = first ? r1(endCt - startCt) : 0;
  checks.push(check('currentTime advances >= seconds - 6', m.advance_s >= seconds - 6, m.advance_s, `>= ${seconds - 6} s`));
  // waiting events after the start, each measured until the next playing/canplay
  let maxWait = 0; let waits = 0;
  for (let i = 0; i < ev.length; i++) {
    if (ev[i].type !== 'waiting' || !startedAt || ev[i].at < startedAt) continue;
    waits++;
    const end = ev.slice(i + 1).find((e) => e.type === 'playing' || e.type === 'canplay');
    maxWait = Math.max(maxWait, (end ? end.at : (last ? last.at : ev[i].at)) - ev[i].at);
  }
  m.waiting_events = waits; m.max_waiting_ms = maxWait;
  checks.push(check('no waiting longer than 500 ms after start', maxWait <= 500, maxWait, '<= 500 ms'));
  let frozen = 0; let vbStuck = 0; let abStuck = 0;
  for (let i = 1; i < s.length; i++) {
    if (s[i].ct - s[i - 1].ct < 0.5) frozen++;
    if (!(s[i].vb > s[i - 1].vb)) vbStuck++;
    if (!(s[i].ab > s[i - 1].ab)) abStuck++;
  }
  m.frozen_samples = frozen;
  checks.push(check('no 1 s sample with a frozen clock', frozen === 0, frozen, '0'));
  checks.push(check('video decoded bytes strictly increase', s.length > 5 && vbStuck === 0, `${vbStuck} stuck of ${s.length}`, '0 stuck'));
  if (o.audio !== false) checks.push(check('audio decoded bytes strictly increase', s.length > 5 && abStuck === 0, `${abStuck} stuck of ${s.length}`, '0 stuck'));
  const exact = probe.start && probe.end && typeof probe.end.frames === 'number';
  const frames = exact ? probe.end.frames - probe.start.frames : (first ? last.frames - first.frames : 0);
  const expected = (exact ? endCt - startCt : (first ? last.ct - first.ct : 0)) * (o.fps || FPS);
  m.frames = frames; m.frames_ratio = expected > 0 ? Math.round(frames / expected * 1000) / 1000 : 0;
  checks.push(check('decoded frames >= 0.95 x expected', expected > 0 && frames >= 0.95 * expected, `${frames} of ${Math.round(expected)}`, '>= 0.95'));
  const dropped = exact ? probe.end.dropped - probe.start.dropped : (first ? last.dropped - first.dropped : 0);
  m.dropped_pct = frames > 0 ? Math.round(dropped / frames * 10000) / 100 : null;
  checks.push(check('dropped frames under 2 %', frames > 0 && dropped / frames < 0.02, `${dropped} (${m.dropped_pct} %)`, '< 2 %'));
  const aheads = s.map((x) => x.ahead);
  m.buffer_min_s = aheads.length ? r1(Math.min(...aheads)) : null;
  m.buffer_avg_s = aheads.length ? r1(aheads.reduce((a, b) => a + b, 0) / aheads.length) : null;
  m.width = last ? last.w : null; m.height = last ? last.h : null;
  return { checks, metrics: m, samples: s };
}

/** Latency criteria from per-sample figures {latency, seekLen}. */
export function evaluateLive(samples, pick, o = {}) {
  const checks = []; const m = {};
  const lat = samples.map((x) => pick(x).latency).filter((x) => typeof x === 'number' && isFinite(x) && x > 0);
  const seek = samples.map((x) => pick(x).seekLen).filter((x) => typeof x === 'number' && isFinite(x));
  m.latency_min_s = lat.length ? r1(Math.min(...lat)) : null; m.latency_max_s = lat.length ? r1(Math.max(...lat)) : null;
  m.latency_drift_s = lat.length ? r1(Math.max(...lat) - Math.min(...lat)) : null;
  m.seek_range_min_s = seek.length ? r1(Math.min(...seek)) : null;
  checks.push(check('live latency between 4 and 20 s', lat.length > 5 && m.latency_min_s >= 4 && m.latency_max_s <= 20, `${m.latency_min_s}..${m.latency_max_s}`, '4..20 s'));
  // The design constraint for our own receiver config: 6 to 10 s behind live.
  if (o.designWindow) checks.push(check('latency within the design window, 6 to 10 s', lat.length > 5 && m.latency_min_s >= 6 && m.latency_max_s <= 10, `${m.latency_min_s}..${m.latency_max_s}`, '6..10 s'));
  checks.push(check('latency drift under 1.5 s', lat.length > 5 && m.latency_drift_s < 1.5, m.latency_drift_s, '< 1.5 s'));
  checks.push(check('seek range length >= 12 s', seek.length > 5 && m.seek_range_min_s >= 12, m.seek_range_min_s, '>= 12 s'));
  return { checks, metrics: m };
}

export function printTable(title, checks) {
  const w = Math.max(...checks.map((c) => c.name.length), 10);
  console.log(`\n== ${title}`);
  for (const c of checks) console.log(`  ${c.pass ? 'PASS' : 'FAIL'}  ${c.name.padEnd(w)}  ${String(c.value)}  (${c.limit})`);
}

/** Print the RESULT line, save it under out/, and return the number of failed checks. */
export function finish(test, parts, extra = {}) {
  // A part marked `informational` is printed and recorded but does not decide the exit code.
  const failed = parts.filter((p) => !p.informational).flatMap((p) => p.checks.filter((c) => !c.pass).map((c) => `${p.name}: ${c.name} = ${c.value}`));
  const summary = { test, pass: failed.length === 0, failed, at: new Date().toISOString(), ...extra,
    parts: parts.map((p) => ({ name: p.name, pass: p.checks.every((c) => c.pass), informational: !!p.informational,
      failed: p.checks.filter((c) => !c.pass).map((c) => `${c.name} = ${c.value}`), metrics: p.metrics })) };
  for (const p of parts) printTable(`${test} / ${p.name}${p.informational ? ' (informational)' : ''}`, p.checks);
  console.log('RESULT ' + JSON.stringify(summary));
  return failed.length;
}

export function save(name, data) {
  fs.mkdirSync(outDir, { recursive: true });
  const file = path.join(outDir, name);
  fs.writeFileSync(file, typeof data === 'string' ? data : JSON.stringify(data, null, 1));
  return file;
}

/**
 * Let the page play for `seconds` counted from the moment the picture moves (bounded: 10 s after the load to get
 * there, else counted from the load), then return the probe:
 * {events, samples, startedAt, startCt, start, end: {at, ct, frames, dropped}}.
 * Why from the picture start and not from the first frame: with CAF's default rebufferingGoal (10 s) Shaka shows
 * the first frame at once and then holds playback for 4 to 6.6 s. That hold is the start time (reported and
 * gated as `start_ms`); the run after it measures continuity.
 */
export async function runFor(page, loadAt, seconds) {
  let startedAt = 0;
  for (const limit = loadAt + 10000; Date.now() < limit && !startedAt; await sleep(100)) startedAt = await page.evaluate(() => window.__probe.startedAt);
  await sleep(Math.max(0, (startedAt || loadAt) + seconds * 1000 - Date.now()));
  return page.evaluate(() => {
    const p = window.__probe; const q = p.video && p.video.getVideoPlaybackQuality ? p.video.getVideoPlaybackQuality() : null;
    return { events: p.events, samples: p.samples, startedAt: p.startedAt, startCt: p.startCt, start: p.start || null,
      end: { at: Date.now(), ct: p.video ? p.video.currentTime : 0, frames: q ? q.totalVideoFrames : null, dropped: q ? q.droppedVideoFrames : null } };
  });
}

/** GET JSON from the harness server (http, or https with its self-signed certificate). */
export function getJson(url, method = 'GET', payload = null, extraHeaders = {}) {
  return new Promise((resolve, reject) => {
    const u = new URL(url); const mod = u.protocol === 'https:' ? https : http;
    const req = mod.request(u, { method, rejectUnauthorized: false, timeout: 10000, headers: { ...(payload ? { 'content-type': 'application/json' } : {}), ...extraHeaders } }, (res) => {
      const chunks = []; res.on('data', (c) => chunks.push(c));
      res.on('end', () => { try { resolve(JSON.parse(Buffer.concat(chunks).toString('utf8'))); } catch (e) { reject(new Error(`${url}: ${res.statusCode} not JSON`)); } });
    });
    req.on('timeout', () => req.destroy(new Error(`${url}: timeout`))); req.on('error', reject);
    req.end(payload ? JSON.stringify(payload) : undefined);
  });
}

/** Bounded wait for a condition polled every 100 ms. */
export async function waitUntil(fn, timeoutMs, what) {
  const end = Date.now() + timeoutMs;
  for (;;) {
    const v = await fn(); if (v) return v;
    if (Date.now() > end) throw new Error(`timeout after ${timeoutMs} ms waiting for ${what}`);
    await sleep(100);
  }
}

/** MEDIA_STATUS criteria after the first PLAYING: BUFFERING spells and IDLE count. */
export function statusChecks(statuses, playing, end, from = 0, label = 'first PLAYING') {
  let maxBuffering = 0; let idleCount = 0; let bufferingEvents = 0;
  if (playing) {
    const after = statuses.filter((s) => s.at >= Math.max(playing.at, from));
    for (let i = 0; i < after.length; i++) {
      if (after[i].playerState === 'IDLE') idleCount++;
      if (after[i].playerState !== 'BUFFERING' || (i > 0 && after[i - 1].playerState === 'BUFFERING')) continue;
      bufferingEvents++;
      const next = after.slice(i + 1).find((s) => s.playerState !== 'BUFFERING');
      maxBuffering = Math.max(maxBuffering, (next ? next.at : end) - after[i].at);
    }
  }
  const last = statuses.length ? statuses[statuses.length - 1].playerState : null;
  return {
    metrics: { max_buffering_ms: maxBuffering, buffering_events: bufferingEvents, idle_after_playing: idleCount, final_state: last },
    checks: [
      check(`no BUFFERING longer than 1 s after ${label}`, playing && maxBuffering <= 1000, `${maxBuffering} ms in ${bufferingEvents} event(s)`, '<= 1000 ms'),
      check(`no IDLE after ${label}`, playing && idleCount === 0, idleCount, '0'),
      check('final player state PLAYING', last === 'PLAYING', last, 'PLAYING')
    ]
  };
}

/** The LOAD media object the Conspect web sender sends. */
export function senderMedia(masterUrl, extra = {}) {
  return { contentId: masterUrl, contentUrl: masterUrl, contentType: 'application/vnd.apple.mpegurl', streamType: 'LIVE',
    hlsSegmentFormat: 'fmp4', hlsVideoSegmentFormat: 'fmp4', customData: { title: 'Program' }, ...extra };
}
export const redactUrl = (u) => String(u).replace(/\/cast\/([^/]{6})[^/]*/, '/cast/$1...');

/** Start a stream generator (gen-stream.sh execs ffmpeg, so the child IS ffmpeg). Returns {proc, stop()}. */
export function startGenerator(variant, dir, seconds, env = {}) {
  const proc = spawn(path.join(here, 'gen-stream.sh'), [variant, dir, String(seconds)], { stdio: ['ignore', 'ignore', 'pipe'], env: { ...process.env, ...env } });
  track(proc);
  let stderr = ''; proc.stderr.on('data', (c) => { stderr = (stderr + c).slice(-2000); });
  const exited = new Promise((resolve) => proc.once('exit', resolve));
  return { proc, stderr: () => stderr, stop: async () => { if (proc.exitCode === null && proc.signalCode === null) { proc.kill('SIGKILL'); } await exited; } };
}

/**
 * Wait until the media playlist in `dir` lists at least `n` segments. `tag` = the generator's GEN: a playlist left
 * by an earlier generator (the directory is cleared asynchronously) does not count.
 */
export function waitForSegments(dir, n, timeoutMs, tag = '', playlist = 'video.m3u8') {
  return waitUntil(() => {
    try {
      const text = fs.readFileSync(path.join(dir, playlist), 'utf8');
      return (!tag || text.includes(`-${tag}-`)) && (text.match(/#EXTINF/g) || []).length >= n && fs.existsSync(path.join(dir, 'master.m3u8'));
    } catch { return false; }
  }, timeoutMs, `${n} segments in ${dir}`);
}

/** In-page figures of the CAF receiver, sampled once per second by installSampler. */
export function installReceiverExtra() {
  window.__probeExtra = () => {
    const pm = cast.framework.CastReceiverContext.getInstance().getPlayerManager();
    const st = pm.getStats() || {}; const r = pm.getLiveSeekableRange();
    const o = document.getElementById('overlay'); const cs = o ? getComputedStyle(o) : null;
    return { state: pm.getPlayerState(), latency: st.liveLatency, seekLen: r ? r.end - r.start : null, stalls: st.stallsDetected, gaps: st.gapsJumped,
      overlay: o ? o.getAttribute('data-state') : null, overlayVisible: cs ? (cs.visibility !== 'hidden' && Number(cs.opacity) > 0.01) : null,
      overlayText: o ? o.innerText.replace(/\s+/g, ' ').trim().slice(0, 200) : null };
  };
}

/** Open a receiver page against a fresh fake platform. Returns {platform, browser, page, log, close()}. */
export async function openReceiver(receiverUrl, startFakePlatform) {
  const platform = await startFakePlatform();
  let browser;
  try {
    browser = await launchBrowser();
    const page = await browser.newPage();
    const log = await watchPage(page);
    await page.evaluateOnNewDocument(installSampler);
    await page.evaluateOnNewDocument(installReceiverExtra);
    const response = await page.goto(receiverUrl, { waitUntil: 'load', timeout: 30000 });
    await platform.waitForReceiver(20000);
    await sleep(1000);
    const probe = () => page.evaluate(() => ({ events: window.__probe.events, samples: window.__probe.samples, startedAt: window.__probe.startedAt }));
    const last = async () => { const p = await probe(); return p.samples[p.samples.length - 1] || null; };
    return { platform, browser, page, log, probe, last, cspHeader: response.headers()['content-security-policy'] || '',
      close: async () => { await browser.close().catch(() => {}); await platform.close().catch(() => {}); } };
  } catch (e) {
    if (browser) await browser.close().catch(() => {});
    await platform.close().catch(() => {});
    throw e;
  }
}

/**
 * Watch a generator directory: when was each new segment published? Tells a late STREAM (encoder starved on a
 * loaded host) from a player fault. Returns {stop(), maxIntervalMs(sinceMs), count(sinceMs)}.
 */
export function watchPlaylist(dir, playlist = 'video.m3u8') {
  const seen = []; let last = '';
  const timer = setInterval(() => {
    let text; try { text = fs.readFileSync(path.join(dir, playlist), 'utf8'); } catch { return; }
    const lines = text.trim().split('\n'); const name = lines[lines.length - 1];
    if (name && name !== last && !name.startsWith('#')) { last = name; seen.push({ at: Date.now(), name }); }
  }, 100);
  return {
    stop: () => clearInterval(timer),
    count: (since = 0) => seen.filter((x) => x.at >= since).length,
    published: (since = 0) => seen.filter((x) => x.at >= since),
    maxIntervalMs: (since = 0) => { const a = seen.filter((x) => x.at >= since); let m = 0; for (let i = 1; i < a.length; i++) m = Math.max(m, a[i].at - a[i - 1].at); return a.length > 1 ? m : null; }
  };
}
