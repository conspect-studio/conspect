'use strict';
const movColor = (() => { const module = {exports:{}}; /*MOV_COLOR*/; return module.exports; })();
const windyTemplate = (() => { const module = {exports:{}}; /*WINDY_TEMPLATE*/; return module.exports; })();
// Internal producer host. stdout contains only encoded media; never protocol or
// credentials. The Rust owner supplies trusted job configuration over stdin.
const {spawn} = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');
const {once} = require('node:events');
const {finished} = require('node:stream/promises');
const children = new Set();
let profile;
function launch(bin, args, options = {}) {
  const p = spawn(bin, args, options);
  children.add(p);
  p.on('error', () => {});
  if (p.stdin) p.stdin.on('error', () => {});
  return p;
}
async function cleanup() {
  for (const p of children) {
    if (p.exitCode === null && p.signalCode === null) { try { p.kill('SIGKILL'); } catch {} }
  }
  await Promise.all([...children].map(p => p.exitCode !== null || p.signalCode !== null
    ? Promise.resolve() : Promise.race([once(p, 'exit').catch(() => {}), new Promise(r => setTimeout(r, 1000))])));
  // The Rust owner removes the private directory only after process cleanup.
}
process.on('SIGTERM', () => { cleanup().finally(() => process.exit(143)); });
process.on('SIGINT', () => { cleanup().finally(() => process.exit(130)); });
function sleep(ms) { return new Promise(r => setTimeout(r, ms)); }
class Cdp {
  constructor(child) {
    this.child = child;
    this.pending = new Map();
    this.id = 0;
    this.listeners = new Set();
    this.parts = [];
    this.byteCount = 0;
    this.failed = false;
    child.stdio[4].on('data', chunk => {
      if (this.failed) return;
      let start = 0;
      while (start < chunk.length) {
        const end = chunk.indexOf(0, start);
        const piece = chunk.subarray(start, end === -1 ? chunk.length : end);
        this.byteCount += piece.length;
        if (this.byteCount > 16 * 1024 * 1024) return this.fail();
        if (piece.length) this.parts.push(piece);
        if (end === -1) break;
        let msg;
        try { msg = JSON.parse(Buffer.concat(this.parts, this.byteCount).toString('utf8')); }
        catch { return this.fail(); }
        this.parts = [];
        this.byteCount = 0;
        if (!msg || typeof msg !== 'object' || Array.isArray(msg)) return this.fail();
        if (msg.method) for (const listener of this.listeners) listener(msg);
        if (msg.id) {
          const waiter = this.pending.get(msg.id);
          if (waiter) {
            this.pending.delete(msg.id);
            clearTimeout(waiter.timer);
            msg.error ? waiter.reject(new Error('browser command rejected')) : waiter.resolve(msg.result);
          }
        }
        start = end + 1;
      }
    });
    child.on('exit', () => this.fail());
    child.on('error', () => this.fail());
  }
  fail() {
    this.failed = true;
    this.parts = [];
    this.byteCount = 0;
    for (const w of this.pending.values()) { clearTimeout(w.timer); w.reject(new Error('browser pipe failed')); }
    this.pending.clear();
  }
  send(method, params = {}, sessionId) {
    if (this.failed) return Promise.reject(new Error('browser session closed'));
    if (this.pending.size >= 8) return Promise.reject(new Error('browser pending command limit'));
    const id = ++this.id;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => { this.pending.delete(id); reject(new Error('browser command deadline')); }, 15000);
      this.pending.set(id, {resolve, reject, timer});
      this.child.stdio[3].write(JSON.stringify({id, method, params, ...(sessionId ? {sessionId} : {})}) + '\0', err => {
        if (err) { clearTimeout(timer); this.pending.delete(id); reject(new Error('browser pipe closed')); }
      });
    });
  }
}
async function readRequest() {
  let input = '';
  for await (const b of process.stdin) {
    input += b.toString('utf8');
    if (Buffer.byteLength(input) > 1024 * 1024) throw new Error('request limit');
  }
  return JSON.parse(input);
}
async function main() {
  const req = await readRequest();
  if (!req || !Number.isSafeInteger(req.width) || !Number.isSafeInteger(req.height)
      || req.width < 64 || req.width > 3840 || req.height < 64 || req.height > 2160
      || req.width % 2 || req.height % 2 || !Number.isSafeInteger(req.frames)
      || req.frames < 2 || req.frames > 600 || !Number.isSafeInteger(req.fps)
      || req.fps < 1 || req.fps > 30) throw new Error('invalid capture limits');
  if (!req.browser || !req.ffmpeg) throw new Error('tool missing');
  if (req.background !== 'transparent' && req.background !== 'opaque') throw new Error('invalid capture limits');
  const alpha = req.background === 'transparent';
  const isWindy = req.source.kind === 'windy';
  if (!isWindy && req.source.kind !== 'html') throw new Error('template not implemented');
  if (isWindy && alpha) throw new Error('invalid capture limits');
  if (isWindy) {
    windyTemplate.validate(req.source.view);
    if (!req.source.view.terms_accepted || req.width < 1024) throw new Error('Windy acceptance or geometry missing');
  } else if (typeof req.source.document !== 'string' || Buffer.byteLength(req.source.document) > 128 * 1024)
    throw new Error('document limit');
  if (typeof req.profile !== 'string' || !path.isAbsolute(req.profile)) throw new Error('private profile required');
  profile = req.profile;
  fs.chmodSync(profile, 0o700);
  const args = ['--remote-debugging-pipe', '--user-data-dir=' + profile,
    '--disable-dev-shm-usage', '--no-first-run', '--no-default-browser-check',
    '--disable-background-networking', '--disable-component-update',
    ...(!isWindy ? ['--host-resolver-rules=MAP * ~NOTFOUND'] : []),
    '--password-store=basic', '--disable-save-password-bubble',
    '--window-size=' + req.width + ',' + req.height, 'about:blank'];
  // Sandbox stays ON. GPU selection belongs to the parent admission policy.
  const browser = launch(req.browser, args, {stdio: ['ignore', 'ignore', 'ignore', 'pipe', 'pipe']});
  const cdp = new Cdp(browser);
  await cdp.send('Browser.getVersion');
  const {targetId} = await cdp.send('Target.createTarget', {url: 'about:blank'});
  const {sessionId} = await cdp.send('Target.attachToTarget', {targetId, flatten: true});
  const send = (m, p = {}) => cdp.send(m, p, sessionId);
  await send('Page.enable');
  await send('Network.enable');
  const inflight = new Set();
  let networkChanged = Date.now();
  cdp.listeners.add(message => {
    if (message.sessionId !== sessionId) return;
    const id = message.params?.requestId;
    if (message.method === 'Network.requestWillBeSent'
        && /(?:tiles[^/]*\.windy\.com|rdr\.windy\.com)/.test(message.params?.request?.url || '')
        && !['WebSocket', 'EventSource'].includes(message.params?.type)) {
      inflight.add(id); networkChanged = Date.now();
    } else if (['Network.loadingFinished', 'Network.loadingFailed'].includes(message.method) && inflight.delete(id)) {
      networkChanged = Date.now();
    }
  });
  if (!isWindy) await send('Network.setBlockedURLs', {urls: ['http://*', 'https://*', 'file://*', 'ftp://*', 'ws://*', 'wss://*']});
  await send('Emulation.setDeviceMetricsOverride', {width: req.width, height: req.height, deviceScaleFactor: 1, mobile: false});
  if (isWindy) {
    await send('Page.navigate', {url: windyTemplate.url(req.source.view)});
    let ready = false;
    let loginAttempted = false;
    for (let attempt = 0; attempt < 60; attempt++) {
      const result = await send('Runtime.evaluate', {
        expression: windyTemplate.statusExpression(req.source.view.overlay), returnByValue:true});
      let state;
      try { state = JSON.parse(result.result?.value); } catch {}
      if (attempt >= 20 && state && !state.noData && state.canvases > 0 && state.tilesLoaded > 0 && state.tilesPending === 0 && state.overlay === req.source.view.overlay) {
        if (req.source.view.require_premium && (state.loginVisible || !/premium/.test(state.subscription || ''))) {
          const credentials = req.source.view.credentials;
          if (!credentials) throw new Error('authenticated Windy session required');
          if (loginAttempted) { await sleep(500); continue; }
          loginAttempted = true;
          await send('Runtime.evaluate', {expression:"document.querySelector('.rhpane__top-icons__login')?.click()"});
          let formReady = false;
          for (let j = 0; j < 20; j++) {
            const form = await send('Runtime.evaluate', {expression:"!!document.querySelector('#email') && !!document.querySelector('#password')",returnByValue:true});
            if (form.result?.value) { formReady = true; break; }
            await sleep(250);
          }
          if (!formReady) throw new Error('Windy login form unavailable');
          // Native input events, private CDP pipe only; never stringify this in logs.
          const expression = `(()=>{const setter=Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set;
            for(const [id,value] of [['email',${JSON.stringify(credentials.username)}],['password',${JSON.stringify(credentials.password)}]]){
              const input=document.getElementById(id);setter.call(input,value);input.dispatchEvent(new Event('input',{bubbles:true}));input.dispatchEvent(new Event('change',{bubbles:true}));}
            document.querySelector('#submitLogin')?.click();})()`;
          await send('Runtime.evaluate', {expression});
          await sleep(2000);
          continue;
        }
        ready = true; break;
      }
      await sleep(500);
    }
    if (!ready) throw new Error('Windy requested layer unavailable');
    for (let i = 0; inflight.size || Date.now() - networkChanged < 1500; i++) {
      if (i > 60) throw new Error('Windy map requests did not settle');
      await sleep(250);
    }
    await send('Runtime.evaluate', {expression:`(()=>{const s=document.createElement('style');s.textContent=${JSON.stringify(windyTemplate.CLEAN)};document.head.appendChild(s)})()`});
    await send('Runtime.evaluate', {expression:"document.querySelector('.play-pause')?.click()"});
    await sleep(1500);
  } else {
    if (alpha) await send('Emulation.setDefaultBackgroundColorOverride', {color: {r: 0, g: 0, b: 0, a: 0}});
    const {frameTree} = await send('Page.getFrameTree');
    await send('Page.setDocumentContent', {frameId: frameTree.frame.id, html: req.source.document});
    await send('Runtime.evaluate', {expression: 'document.fonts.ready', awaitPromise: true});
  }
  // MOV is spooled to a private, bounded file so it can be validated before any
  // byte reaches stdout; nothing is published for a stripped/short encode.
  const pixFmt = alpha ? 'yuva420p' : 'yuv420p';
  const spool = path.join(profile, 'mv-spool.mov');
  const SPOOL_MAX = 512 * 1024 * 1024;
  const encoder = launch(req.ffmpeg, ['-nostdin', '-hide_banner', '-loglevel', 'error',
    '-f', 'image2pipe', '-framerate', String(req.fps), '-vcodec', 'png', '-i', 'pipe:0',
    '-an', '-vf', 'scale=in_range=full:out_color_matrix=bt709:out_range=tv,format=' + pixFmt + ',setparams=range=limited:color_primaries=bt709:color_trc=iec61966-2-1:colorspace=bt709',
    '-c:v', 'ffv1', '-threads', '2', '-pix_fmt', pixFmt,
    '-color_primaries', 'bt709', '-color_trc', 'iec61966-2-1', '-colorspace', 'bt709', '-color_range', 'tv',
    '-fs', String(SPOOL_MAX), '-y', '-f', 'mov', spool],
    {stdio: ['pipe', 'ignore', 'ignore']});
  const encoderExit = new Promise((resolve, reject) => {
    encoder.on('error', () => reject(new Error('encoder launch failed')));
    encoder.on('exit', (code, signal) => code === 0 && !signal ? resolve() : reject(new Error('encoder failed')));
  });
  encoderExit.catch(() => {});
  const inputComplete = finished(encoder.stdin, {readable: false});
  inputComplete.catch(() => {});
  let last;
  for (let i = 0; i < req.frames; i++) {
    // Authored templates can expose this hook for deterministic capture.
    if (isWindy) {
      await sleep(Math.round(1000 / req.fps));
      const check = await send('Runtime.evaluate', {expression:windyTemplate.statusExpression(req.source.view.overlay),returnByValue:true});
      const state = JSON.parse(check.result?.value || '{}');
      if (state.noData || state.overlay !== req.source.view.overlay || state.tilesPending !== 0)
        throw new Error('Windy changed or incomplete during capture');
    }
    const tick = await send('Runtime.evaluate', {
      expression: `typeof window.mvCaptureFrame === 'function' ? window.mvCaptureFrame(${i},${req.fps}) : null`,
      awaitPromise: true, returnByValue: true});
    if (tick.exceptionDetails) throw new Error('template frame hook failed');
    const result = await send('Page.captureScreenshot', {format: 'png', captureBeyondViewport: false});
    if (!result.data || result.data.length > 15 * 1024 * 1024) throw new Error('screenshot limit');
    last = Buffer.from(result.data, 'base64');
    if (!encoder.stdin.write(last)) await once(encoder.stdin, 'drain');
  }
  encoder.stdin.end();
  await inputComplete;
  await encoderExit;
  movColor.tagLimitedSrgb(spool);
  // Planned (req.frames) must equal actual encoded frames, with real alpha.
  if (fs.statSync(spool).size >= SPOOL_MAX) throw new Error('encoded size limit');
  const planeBytes = req.width * req.height;
  const probe = await runFfmpeg(req.ffmpeg, ['-hide_banner', '-f', 'mov', '-protocol_whitelist', 'file', '-enable_drefs', '0', '-i', spool], 0);
  if (!/Video: ffv1/.test(probe.stderr) || !new RegExp('\\b' + pixFmt + '\\b').test(probe.stderr)
      || (!alpha && /yuva/.test(probe.stderr))) throw new Error('encoded format mismatch');
  // Opaque output decodes plane 0 as gray; transparent output extracts the alpha plane,
  // which fails outright when the encoder stripped it.
  const vf = alpha ? 'alphaextract,format=gray' : 'format=gray';
  const decoded = await runFfmpeg(req.ffmpeg, ['-nostdin', '-hide_banner', '-loglevel', 'error', '-f', 'mov', '-protocol_whitelist', 'file', '-enable_drefs', '0', '-i', spool,
    '-map', '0:v:0', '-vf', vf, '-f', 'rawvideo', 'pipe:1'], planeBytes * req.frames + 1);
  if (decoded.code !== 0 || decoded.bytes !== planeBytes * req.frames) throw new Error('encoded frame count mismatch');
  await new Promise((resolve, reject) => {
    const rs = fs.createReadStream(spool);
    process.stdout.once('error', reject);
    rs.once('error', reject);
    rs.once('end', () => process.stdout.end(resolve));
    rs.pipe(process.stdout, {end: false});
  });
  try { fs.unlinkSync(spool); } catch {}
  process.stderr.write(JSON.stringify({event: 'capture_complete', frames: req.frames,
    fps: req.fps, width: req.width, height: req.height, codec: 'ffv1', alpha,
    pix_fmt: pixFmt, color: 'bt709/srgb/limited'}) + '\n');
}
// Run ffmpeg to completion, counting stdout bytes (capped) and keeping bounded stderr.
function runFfmpeg(bin, args, cap) {
  return new Promise((resolve, reject) => {
    const p = launch(bin, args, {stdio: ['ignore', 'pipe', 'pipe']});
    let bytes = 0; let stderr = '';
    p.stdout.on('data', c => { bytes += c.length; if (cap && bytes > cap) p.kill('SIGKILL'); });
    p.stderr.on('data', c => { if (stderr.length < 65536) stderr += c.toString('latin1'); });
    p.on('error', () => reject(new Error('encoder launch failed')));
    p.on('close', code => resolve({code, bytes, stderr}));
  });
}
const timeout = setTimeout(() => { cleanup().finally(() => process.exit(124)); }, 120000);
main().then(async () => { clearTimeout(timeout); await cleanup(); process.exitCode = 0; })
  .catch(async error => { clearTimeout(timeout);
    const safe = new Set(['authenticated Windy session required','Windy login form unavailable','Windy requested layer unavailable','Windy map requests did not settle','Windy changed or incomplete during capture']);
    process.stderr.write((safe.has(error.message) ? error.message : 'capture failed') + '\n');
    await cleanup(); process.exitCode = 1;
  });
