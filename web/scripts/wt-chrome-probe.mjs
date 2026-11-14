// Headless-Chrome WebTransport probe (#382): opens `new WebTransport(url, opts)` from a
// secure-context page and prints one JSON line per attempt. Used by the multiview-h3
// `browser-integration` test; also runnable by hand:
//   node web/scripts/wt-chrome-probe.mjs '<json config>'
// config: { url, hash | certDer, attempts: [{ protocols?: string[], expect?: string|null }], holdMs? }
import { chromium } from '@playwright/test';
import { createServer } from 'node:http';
import { createHash } from 'node:crypto';
import { mkdtempSync, rmSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { resolve } from 'node:path';

const config = JSON.parse(process.argv[2]);
// The certificate pin: either `hash` (hex SHA-256 of the DER) or `certDer` (hex DER).
const hash = config.hash ?? createHash('sha256').update(Buffer.from(config.certDer, 'hex')).digest('hex');
const root = fileURLToPath(new URL('..', import.meta.url));
const executablePath = process.env.CONSPECT_WEB_BROWSER || undefined;
let failed = false;
let serve;
let cleanup = () => {};
if (config.moq) {
  // The production client path, built by Vite exactly as the app bundles it.
  const { build, preview } = await import('vite');
  const outDir = mkdtempSync(resolve(root, 'node_modules/.cache-wt-probe-'));
  cleanup = () => rmSync(outDir, { recursive: true, force: true });
  await build({ configFile: false, root, logLevel: 'silent', build: { outDir, emptyOutDir: false,
    rollupOptions: { input: resolve(root, 'scripts/wt-chrome-moq-page.html') } } });
  const server = await preview({ configFile: false, root, logLevel: 'silent', build: { outDir },
    preview: { host: '::1', port: 0, strictPort: true } });
  serve = { url: `http://[::1]:${server.httpServer.address().port}/scripts/wt-chrome-moq-page.html`,
    close: () => new Promise((r) => server.httpServer.close(r)) };
} else {
  const http = createServer((_, res) => { res.writeHead(200, { 'content-type': 'text/html' }); res.end('<!doctype html><title>wt probe</title>'); });
  await new Promise((r) => http.listen(0, '::1', r));
  serve = { url: `http://[::1]:${http.address().port}/`, close: () => new Promise((r) => http.close(r)) };
}
const browser = await chromium.launch({ headless: true, executablePath, args: ['--no-sandbox', '--enable-quic'] });
try {
  const page = await browser.newPage();
  await page.goto(serve.url);
  if (config.moq) {
    await page.waitForFunction(() => typeof window.startMoq === 'function', null, { timeout: 15000 });
    await page.evaluate((args) => { void window.startMoq(args); }, { leases: config.moq.leases, hash, playMs: config.moq.playMs });
    await page.waitForFunction(() => ['played', 'failed'].includes(window.probe.phase), null, { timeout: config.moq.playMs + 40000 }).catch(() => {});
    const result = await page.evaluate(() => {
      const p = window.probe;
      return { phase: p.phase, error: p.error, afterAbort: p.afterAbort ?? null, errorAtMs: p.errorAtMs ?? null, active: p.active ?? false, leasesUsed: p.leases, transports: p.transports,
        frames: p.paints.length, framesInWindow: p.framesInWindow ?? 0, firstFrameMs: p.firstFrameMs ?? null,
        uniqueTimestamps: new Set(p.paints).size };
    });
    console.log(JSON.stringify({ browser: browser.version(), moq: true, ...result }));
    if (result.phase !== 'played') failed = true;
  }
  for (const attempt of config.attempts ?? []) {
    const result = await page.evaluate(async ({ url, hash, attempt, holdMs }) => {
      const bytes = new Uint8Array(hash.match(/../g).map((h) => parseInt(h, 16)));
      const opts = { serverCertificateHashes: [{ algorithm: 'sha-256', value: bytes }] };
      if (attempt.protocols) opts.protocols = attempt.protocols;
      const started = performance.now();
      let wt;
      try {
        wt = new WebTransport(url, opts);
        await wt.ready;
        const out = { ok: true, protocol: wt.protocol, readyMs: Math.round(performance.now() - started) };
        if (holdMs) await new Promise((r) => setTimeout(r, holdMs));
        wt.close();
        return out;
      } catch (e) {
        return { ok: false, error: String(e?.message ?? e), source: e?.source ?? null };
      }
    }, { url: config.url, hash, attempt, holdMs: config.holdMs ?? 0 });
    const line = { browser: browser.version(), offered: attempt.protocols ?? null, ...result };
    console.log(JSON.stringify(line));
    if (attempt.expect !== undefined && (result.ok ? result.protocol : null) !== attempt.expect) failed = true;
  }
} finally {
  await browser.close();
  await serve.close();
  cleanup();
}
process.exitCode = failed ? 1 : 0;
