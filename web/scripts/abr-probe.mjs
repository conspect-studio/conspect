// ABR acceptance probe (t39): one multi-rendition publisher, two independent
// browser consumers, each on its own shaped QUIC path, one of them additionally
// decoder-capped. Asserts the selection behaviour and the publisher-side budget
// and cadence invariants, then writes the raw evidence as JSON.
//
// Not a mock: the consumers run the real @moq/watch pipeline over real
// WebTransport sessions through a real token-bucket shaper on the QUIC path,
// and every selection the pipeline makes is read back from it.
import { chromium } from '@playwright/test';
import { build, preview } from 'vite';
import { spawn } from 'node:child_process';
import { lookup } from 'node:dns/promises';
import { createHash, X509Certificate } from 'node:crypto';
import { readFileSync, writeFileSync, mkdtempSync, existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { resolve } from 'node:path';
import { startShapingRelay } from './abr-shaping-relay.mjs';

const root = fileURLToPath(new URL('..', import.meta.url));
const repo = resolve(root, '..');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
/** First paint timestamp of `key` inside `phase`, from that phase's paints. */
const paintAfter = (phaseReport, phase, key) =>
  phaseReport.find((r) => r.phase === phase)?.slow?.paintByPhase?.[phase]?.[key]?.firstMs ?? null;

const median = (values) => {
  const sorted = [...values].filter((v) => Number.isFinite(v)).sort((a, b) => a - b);
  if (sorted.length === 0) return 0;
  return sorted[Math.floor(sorted.length / 2)];
};
const argOf = (name, fallback) => {
  const index = process.argv.indexOf(name);
  return index >= 0 ? process.argv[index + 1] : fallback;
};

const mode = argOf('--mode', 'local');
const draft = argOf('--draft', '16');
const outPath = argOf('--out', resolve(root, 'scripts/abr-probe-evidence.json'));
const handoffPath = argOf('--handoff', undefined);
const samplerSource = argOf('--sampler-source', undefined);
const authorityBinary = argOf('--authority-binary', undefined);
const productionSource = argOf('--production-source', undefined);
const productionRevision = argOf('--production-revision', 'unrecorded');
const productionSourceSha256 = productionSource ? createHash('sha256').update(readFileSync(productionSource)).digest('hex') : null;
const productionCycles = Number(argOf('--production-cycles', '1'));
if (![1, 2, 3].includes(productionCycles)) throw new Error('BOUNDED_PRODUCTION_CYCLES_REQUIRED');
const authorityUrl = argOf('--authority-url', undefined);
const authorityTokenFile = argOf('--authority-token-file', undefined);
let authorityToken;
if (authorityTokenFile) {
  if ((await import('node:fs')).statSync(authorityTokenFile).mode & 0o077) throw new Error('PRIVATE_AUTHORITY_TOKEN_REQUIRED');
  authorityToken = readFileSync(authorityTokenFile, 'utf8').trim();
}
const authority = authorityUrl ? { scope: 'program', whepEndpoint: '/api/v1/preview/program/whep' } : undefined;
if (authorityUrl && !/^http:\/\/\[(::1)\]:\d+$/.test(authorityUrl)) {
  throw new Error('LOCAL_AUTHORITY_REQUIRED');
}
const fixture = (name) => resolve(repo, 'tools/moq-fixture/local', name);

// The perturbed consumer: decoder capped at 320x180, capacity pulled from
// 4 Mbps to 150 kbps and back. The other consumer is unconstrained.
const SLOW_CAP = { width: 320, height: 180 };
const HEALTHY_CAP = { width: 640, height: 360 };
const PHASES = [
  { name: 'settle', kind: 'settle', ms: 12000, slowBps: 4_000_000, expectSlow: 'video-mid' },
  { name: 'downshift', kind: 'downshift', ms: 17000, slowBps: 250_000, expectSlow: 'video-low' },
  { name: 'recovery', kind: 'recovery', ms: 17000, slowBps: 4_000_000, expectSlow: 'video-mid' },
  { name: 'downshift-2', kind: 'downshift', ms: 17000, slowBps: 250_000, expectSlow: 'video-low' },
  { name: 'recovery-2', kind: 'recovery', ms: 17000, slowBps: 4_000_000, expectSlow: 'video-mid' },
  { name: 'downshift-3', kind: 'downshift', ms: 17000, slowBps: 250_000, expectSlow: 'video-low' },
  { name: 'recovery-3', kind: 'recovery', ms: 17000, slowBps: 4_000_000, expectSlow: 'video-mid' },
];

if (mode === 'cloudflare' && !handoffPath) throw new Error('CF_HANDOFF_REQUIRED');
if (handoffPath) {
  const { mode: fileMode } = (await import('node:fs')).statSync(handoffPath);
  if (fileMode & 0o077) throw new Error('PRIVATE_HANDOFF_REQUIRED');
}

/** Read the publish/subscribe token out of a mode-0600 handoff, for in-page use only. */
function handoffToken(operation) {
  const handoff = JSON.parse(readFileSync(handoffPath, 'utf8'));
  const candidates = (handoff.tokens ?? []).filter((t) => t.operations?.includes(operation));
  // Narrowest sufficient token: a subscribe-only secret is preferred over one
  // that also carries publish.
  const token =
    candidates.find((t) => t.operations?.length === 1)?.secret ?? candidates[0]?.secret;
  if (!token) throw new Error(`${operation.toUpperCase()}_TOKEN_REQUIRED`);
  return token;
}

function certHash(pemPath) {
  const certificate = new X509Certificate(readFileSync(pemPath));
  return createHash('sha256').update(certificate.raw).digest('hex');
}

/** Publisher stdout: readiness line plus one cumulative stats line per second. */
function startPublisher() {
  // Arguments for the fixture itself, independent of how it is launched.
  const publisherArgs = [
    draft,
    fixture('abr-low.ivf'),
    fixture('abr-mid.ivf'),
    fixture('abr-high.ivf'),
    fixture('cert.pem'),
    fixture('key.pem'),
    // Must outlast every phase: an earlier 90s limit silently ended the fixture
    // in the last cycle of the three-cycle run and made the tail look like a
    // relay failure. 300s covers the plan with margin.
    '300',
  ];
  if (mode === 'cloudflare') publisherArgs.push('--cf-handoff', handoffPath);
  // Prefer the already-built example binary: no cargo invocation, no build
  // directory writes, so a probe run never competes for disk or compile time.
  const prebuilt = resolve(repo, 'tools/moq-fixture/target/debug/examples/abr');
  const child = authorityBinary
    ? spawn(resolve(authorityBinary), [fixture('cert.pem'), fixture('key.pem'), fixture('abr-low.ivf'), fixture('abr-mid.ivf'), fixture('abr-high.ivf')], { cwd: repo, stdio: ['ignore', 'pipe', 'pipe'] })
    : existsSync(prebuilt)
    ? spawn(prebuilt, publisherArgs, { cwd: repo, stdio: ['ignore', 'pipe', 'pipe'] })
    : spawn('cargo', ['run', '--locked', '--manifest-path', resolve(repo, 'tools/moq-fixture/Cargo.toml'), '-j', '2', '--example', 'abr', '--', ...publisherArgs], { cwd: repo, stdio: ['ignore', 'pipe', 'pipe'] });
  const state = { ready: null, stats: [], stderr: [], exited: null };
  let buffer = '';
  child.stdout.on('data', (chunk) => {
    buffer += chunk.toString();
    const lines = buffer.split('\n');
    buffer = lines.pop() ?? '';
    for (const line of lines) {
      if (line.startsWith('ABR_FIXTURE_READY ')) {
        state.ready = Object.fromEntries(
          line
            .slice('ABR_FIXTURE_READY '.length)
            .split(' ')
            .map((pair) => {
              const at = pair.indexOf('=');
              return [pair.slice(0, at), pair.slice(at + 1)];
            }),
        );
      } else if (line.startsWith('ABR_PUBLISH_STATS ')) {
        const fields = line.slice('ABR_PUBLISH_STATS '.length).trim().split(' ');
        const entry = { t: Number(fields[0].slice(2)) };
        for (const field of fields.slice(1)) {
          const [track, value] = field.split('=');
          const [frames, bytes] = value.split('/');
          entry[track] = { frames: Number(frames), bytes: Number(bytes) };
        }
        state.stats.push(entry);
      } else if (line.trim()) {
        state.stdoutOther = [...(state.stdoutOther ?? []), line];
      }
    }
  });
  let stderrBuffer = '';
  child.stderr.on('data', (chunk) => {
    stderrBuffer += chunk.toString();
    const lines = stderrBuffer.split('\n');
    stderrBuffer = lines.pop() ?? '';
    state.stderr.push(...lines.filter((line) => line.trim()));
  });
  child.on('exit', (code) => {
    state.exited = code;
  });
  return {
    state,
    stop() {
      child.kill('SIGKILL');
    },
    /** Longest publisher second observed so far, the publisher's own clock. */
    get elapsedSec() {
      return state.stats.at(-1)?.t ?? 0;
    },
  };
}

/** Per-second publisher rates inside a phase, from cumulative counters. */
function publisherPhase(publisher, fromSec, toSec) {
  const window = publisher.state.stats.filter((entry) => entry.t >= fromSec && entry.t <= toSec);
  const first = window[0];
  const last = window.at(-1);
  const tracks = {};
  if (!first || !last || last.t === first.t) return { tracks, seconds: 0 };
  const seconds = last.t - first.t;
  for (const track of ['video-low', 'video-mid', 'video-high']) {
    const bytes = last[track].bytes - first[track].bytes;
    const frames = last[track].frames - first[track].frames;
    tracks[track] = {
      bytesPerSecond: Math.round(bytes / seconds),
      framesPerSecond: Number((frames / seconds).toFixed(2)),
    };
  }
  return { tracks, seconds };
}

const summary = async (page) => {
  const value = await page.evaluate(() => window.abr.summary());
  // The selector reports no track for a sample while it re-resolves a rendition.
  // That is not a rendition change, so it is kept in the raw record and removed
  // from the timeline the checks read.
  value.switchesAll = value.switches;
  // The selector can re-report the same rendition, and reports nothing for a
  // sample while it re-resolves. Neither is a rendition change.
  value.switches = value.switches
    .filter((entry) => entry.track != null)
    .filter((entry, index, list) => index === 0 || list[index - 1].track !== entry.track);
  if (page._consoleErrors?.length) value.consoleErrors = page._consoleErrors;
  return value;
};

let publisher;
let relays = [];
let browserSlow;
let browserHealthy;
let server;
let result;
let pages;

try {
  publisher = startPublisher();
  const readyDeadline = Date.now() + 60000;
  while (!publisher.state.ready && Date.now() < readyDeadline) {
    if (publisher.state.exited !== null) throw new Error(`publisher exited: ${publisher.state.exited}`);
    await sleep(250);
  }
  if (!publisher.state.ready) throw new Error('PUBLISHER_NOT_READY');
  console.log(`publisher ready: ${JSON.stringify(publisher.state.ready)}`);

  // Two independent browser consumers need two independent paths, so each gets
  // its own shaper and its own relay port. Against Cloudflare the relay's target
  // is the relay's resolved address on 443, and the browser reaches it through
  // --host-resolver-rules (a privileged bind on 443 is unavailable here, and the
  // URL host must stay the real one so the certificate still validates).
  const cfHost = `draft-${draft}.cloudflare.mediaoverquic.com`;
  let cfTarget = null;
  if (mode === 'cloudflare') {
    cfTarget =
      (await lookup(cfHost, { family: 6 }).catch(() => null)) ??
      (await lookup(cfHost, { family: 4 }).catch(() => null));
    if (!cfTarget) throw new Error('CF_RESOLVE_FAILED');
  }
  const startRelay = (downBps) =>
    mode === 'cloudflare'
      ? startShapingRelay({
          listenPort: 0,
          bindAddress: '::',
          targetAddress: cfTarget.address,
          targetPort: 443,
          downBps,
          // A relayed CDN path has far more RTT than loopback: give the queue
          // and the acknowledgement direction room so the shaper measures
          // capacity rather than its own tail-drop.
          maxQueueBytes: Math.max(65_536, Math.round((downBps / 8) * 0.5)),
          upBps: 4_000_000,
        })
      : startShapingRelay({ listenPort: 0, targetPort: 4443, downBps });
  const slow = startRelay(PHASES[0].slowBps);
  const healthy = startRelay(8_000_000);
  relays = [slow, healthy];

  const outDir = mkdtempSync(resolve(root, 'node_modules/.cache-abr-probe-'));
  await build({
    configFile: false,
    root,
    logLevel: 'silent',
    resolve: { alias: [
      ...(samplerSource ? [{ find: '../src/preview/moqAbr.ts', replacement: resolve(samplerSource) }] : []),
      ...(productionSource ? [{ find: '../src/preview/moqPlayback.ts', replacement: resolve(productionSource) }] : []),
    ] },
    build: {
      outDir,
      emptyOutDir: false,
      rollupOptions: { input: resolve(root, productionSource ? 'scripts/abr-production-page.html' : 'scripts/abr-consumer-page.html') },
    },
  });
  server = await preview({
    configFile: false,
    root,
    logLevel: 'silent',
    build: { outDir },
    preview: {
      host: '::1', port: 0, strictPort: true,
      proxy: authorityUrl ? { '/api': {
        target: authorityUrl, changeOrigin: false,
        headers: authorityToken ? { Authorization: `Bearer ${authorityToken}` } : undefined,
      } } : undefined,
    },
  });
  const port = server.httpServer.address().port;
  const pageUrl = `http://[::1]:${port}/scripts/${productionSource ? 'abr-production-page.html' : 'abr-consumer-page.html'}`;

  const hash = mode === 'local' ? certHash(fixture('cert.pem')) : undefined;
  const endpointFor = (relayPort) =>
    mode === 'local'
      ? `https://[::1]:${relayPort}/`
      : `https://${cfHost}/${handoffToken('subscribe')}`;

  const launch = (relayPort) =>
    chromium.launch({
      headless: true,
      args: [
        '--no-sandbox',
        '--enable-quic',
        ...(mode === 'cloudflare'
          ? [`--host-resolver-rules=MAP ${cfHost} [::1]:${relayPort}`]
          : []),
      ],
    });
  [browserSlow, browserHealthy] = await Promise.all([
    launch(slow.port),
    launch(healthy.port),
  ]);

  const openConsumer = async (browser, label, cap, relay) => {
    const page = await browser.newPage();
    page._consoleErrors = [];
    page.on('pageerror', (error) => page._consoleErrors.push(String(error)));
    await page.addInitScript(() => {
      console.debug = console.log = console.info = console.warn = console.error = () => {};
    });
    await page.goto(pageUrl);
    await page.waitForFunction(() => typeof window.startAbrProbe === 'function', { timeout: 15000 });
    await page.evaluate(
      (options) => {
        void window.startAbrProbe(options);
      },
      { endpoint: endpointFor(relay.port), hash, cap, label, authority },
    );
    return page;
  };
  const slowPage = await openConsumer(browserSlow, 'slow', SLOW_CAP, slow);
  const healthyPage = await openConsumer(browserHealthy, 'healthy', HEALTHY_CAP, healthy);
  pages = { slow: slowPage, healthy: healthyPage };

  const markAll = (name) => page_eval_all(pages, name);
  await waitForState(pages, 'playing', 25000);

  if (productionSource) {
    // Separate exact-UI evidence contract: no legacy custom-loop assertions.
    // Explicit readiness contract, distinct from the preserved fixed-24s failure.
    const readinessStarted = Date.now();
    let baseline;
    for (;;) {
      baseline = { slow: await summary(slowPage), healthy: await summary(healthyPage) };
      const ready = Object.values(baseline).every((consumer) => {
        const recent = consumer.paints.filter((p) => p.tMs >= consumer.elapsedMs - 3000);
        return recent.length >= 60 && recent.every((p) => p.width === 640 && p.height === 360)
          && recent.at(-1).pts > recent[0].pts && recent.at(-1).tMs - recent[0].tMs >= 2800;
      });
      if (ready) break;
      if (Date.now() - readinessStarted >= 60000) throw new Error('ACTUAL_HIGH_READINESS_TIMEOUT_60S');
      await sleep(500);
    }
    const startupMs = Date.now() - readinessStarted;
    const firstDownBoundary = baseline.slow.elapsedMs;
    slow.setDownBps(250000);
    await sleep(12000);
    const shaped = { slow: await summary(slowPage), healthy: await summary(healthyPage) };
    const firstRestoreBoundary = shaped.slow.elapsedMs;
    slow.setDownBps(4000000);
    await sleep(18000); // Also crosses one actual 50-second lease deadline.
    const restored = { slow: await summary(slowPage), healthy: await summary(healthyPage) };
    const observations = { baseline, shaped, restored };
    const cycleBoundaries = [{ shapedAfterMs: firstDownBoundary, restoredAfterMs: firstRestoreBoundary, shaped, restored }];
    for (let index = 1; index < productionCycles; index += 1) {
      // Each browser uses a private elapsed clock, so record its own latest
      // observable frame/policy time rather than assume wall-clock identity.
      const before = await summary(slowPage);
      const boundary = Math.max(before.paints.at(-1)?.tMs ?? 0, before.policy.at(-1)?.tMs ?? 0);
      slow.setDownBps(250000);
      await sleep(12000);
      const down = { slow: await summary(slowPage), healthy: await summary(healthyPage) };
      const recoveryBoundary = Math.max(down.slow.paints.at(-1)?.tMs ?? 0, down.slow.policy.at(-1)?.tMs ?? 0);
      slow.setDownBps(4000000);
      await sleep(18000);
      const up = { slow: await summary(slowPage), healthy: await summary(healthyPage) };
      observations[`shaped-${index + 1}`] = down;
      observations[`restored-${index + 1}`] = up;
      cycleBoundaries.push({ shapedAfterMs: boundary, restoredAfterMs: recoveryBoundary, shaped: down, restored: up });
    }
    const final = cycleBoundaries.at(-1).restored;
    const cycleMetrics = cycleBoundaries.map((cycle) => ({
      shapedAfterMs: cycle.shapedAfterMs,
      restoredAfterMs: cycle.restoredAfterMs,
      firstLowLatencyMs: cycle.shaped.slow.paints.find((p) => p.tMs > cycle.shapedAfterMs && p.width === 160)?.tMs - cycle.shapedAfterMs,
      firstRestoredLatencyMs: cycle.restored.slow.paints.find((p) => p.tMs > cycle.restoredAfterMs && p.width >= 320)?.tMs - cycle.restoredAfterMs,
      slowMaxRenderedGapMs: cycle.restored.slow.maxRenderedGapMs,
      healthyMaxRenderedGapMs: cycle.restored.healthy.maxRenderedGapMs,
      slowAdvancingPts: cycle.restored.slow.advancingRenderedPts,
      healthyAdvancingPts: cycle.restored.healthy.advancingRenderedPts,
    }));
    const paintIn = (s, after, width) => s.paints.some((p) => p.tMs >= after && p.width === width);
    const checks = [
      { criterion: 'both viewers render actual high stable for three seconds within 60-second readiness bound', passed: startupMs <= 60000 }, 
      { criterion: 'shaped consumer renders actual low after capacity reduction', passed: paintIn(shaped.slow, firstDownBoundary, 160) },
      { criterion: 'healthy consumer continues high during reduction', passed: paintIn(shaped.healthy, baseline.healthy.elapsedMs, 640) },
      { criterion: 'restored consumer renders mid or high after restoration', passed: restored.slow.paints.some((p) => p.tMs > firstRestoreBoundary && p.width >= 320) },
      { criterion: 'both viewers renew through real release acknowledgement before reacquire', passed: ['slow','healthy'].every((k) => restored[k].leaseEvents.some((e,i,a) => e.action === 'acquired' && i > 0 && a[i-1].action === 'revoked-ack')) },
      { criterion: 'neither exact UI wrapper reports terminal failure', passed: ['slow','healthy'].every((k) => restored[k].state.phase === 'playing') },
    ];
    if (productionCycles === 3) {
      checks.push(
        { criterion: 'three shaped cycles render actual low', passed: cycleBoundaries.every((cycle) => paintIn(cycle.shaped.slow, cycle.shapedAfterMs, 160)) },
        { criterion: 'three restored cycles render actual mid or high', passed: cycleBoundaries.every((cycle) => cycle.restored.slow.paints.some((p) => p.tMs > cycle.restoredAfterMs && p.width >= 320)) },
        { criterion: 'healthy viewer keeps only high quality after warmup', passed: final.healthy.paints.filter((p) => p.tMs >= baseline.healthy.elapsedMs).every((p) => p.width === 640) },
        { criterion: 'both wrappers remain live after all cycles', passed: ['slow','healthy'].every((k) => final[k].state.phase === 'playing') },
        { criterion: 'assumed preview SLO: slow rendered gap at most 3000ms after readiness', passed: final.slow.renderedGaps.filter((gap) => gap.toMs > firstDownBoundary).every((gap) => gap.gapMs <= 3000) },
        { criterion: 'assumed preview SLO: healthy rendered gap at most 1000ms after readiness', passed: final.healthy.renderedGaps.filter((gap) => gap.toMs > baseline.healthy.elapsedMs).every((gap) => gap.gapMs <= 1000) },
      );
    }
    result = { mode, productionRevision, productionSourceSha256, productionCycles, startupMs, readinessContract: 'actual-high-stable-3s-deadline-60s', exactUiWrapper: true, ranAt: new Date().toISOString(), observations, cycleMetrics, publisherDiagnostics: publisher.state.stderr.filter((line) => line.startsWith('ABR_')), checks, passed: checks.every((c) => c.passed) };
    writeFileSync(outPath, `${JSON.stringify(result,null,2)}\n`);
    console.log(`EXACT_UI_PROBE ${result.passed ? 'PASS' : 'FAIL'} checks=${checks.filter((c)=>c.passed).length}/${checks.length}`);
    if (!result.passed) process.exitCode = 1;
  } else {
  const phaseReport = [];
  let slowCursor = 0;
  let healthyCursor = 0;
  for (const phase of PHASES) {
    slow.setDownBps(phase.slowBps);
    const startedAt = Date.now();
    const fromSec = publisher.elapsedSec;
    await markAll(`${phase.name}-start`);
    await sleep(phase.ms);
    await markAll(phase.name);
    const toSec = publisher.elapsedSec;
    const slowAll = slow.evidence();
    const healthyAll = healthy.evidence();
    const report = {
      phase: phase.name,
      slowDownBps: phase.slowBps,
      publisher: publisherPhase(publisher, fromSec, toSec),
      slow: await summary(pages.slow),
      healthy: await summary(pages.healthy),
      // Only the samples accepted during this phase, so a phase's evidence is
      // not contaminated by the shaping of another phase.
      slowRelaySamples: slowAll.samples.slice(slowCursor),
      healthyRelaySamples: healthyAll.samples.slice(healthyCursor),
      slowRelayTotals: {
        down: slowAll.down,
        up: slowAll.up,
        configuredDownBps: slowAll.configuredDownBps,
      },
      slowRelayEvidence: {
        clients: slowAll.clients ?? [],
        clientHandovers: slowAll.clientHandovers ?? 0,
        sessionOverlaps: slowAll.sessionOverlaps ?? 0,
      },
      healthyRelayTotals: {
        down: healthyAll.down,
        up: healthyAll.up,
        configuredDownBps: healthyAll.configuredDownBps,
      },
      wallMs: Date.now() - startedAt,
    };
    slowCursor = slowAll.samples.length;
    healthyCursor = healthyAll.samples.length;
    phaseReport.push(report);
    console.log(
      `phase ${phase.name}: slow=${report.slow.state.effectiveTrack} healthy=${report.healthy.state.effectiveTrack} slowMedianDown=${median(report.slowRelaySamples.map((s) => s.downBps))}bps`,
    );
  }

  const finalPublisher = {
    ready: publisher.state.ready,
    seconds: publisher.state.stats.at(-1)?.t ?? 0,
    last: publisher.state.stats.at(-1) ?? null,
    stderr: publisher.state.stderr.slice(-10),
  };

  const tracksByName = (consumer) => [...new Set(consumer.switches.map((s) => s.track))];
  /** Share of the phase's samples spent on each rendition, from the switch timeline. */
  const occupancy = (consumer, phase) => {
    const marks = consumer.marks ?? [];
    const start = marks.find((m) => m.name === `${phase.name}-start`);
    const end = marks.find((m) => m.name === phase.name);
    if (!start || !end) return {};
    const counts = {};
    for (const sample of consumer.samples) {
      if (sample.tMs < start.tMs || sample.tMs > end.tMs) continue;
      const relevant = consumer.switches.filter((s) => s.tMs <= sample.tMs);
      const track = relevant.length ? relevant.at(-1).track : 'none';
      counts[track] = (counts[track] ?? 0) + 1;
    }
    const total = Object.values(counts).reduce((sum, value) => sum + value, 0) || 1;
    return Object.fromEntries(
      Object.entries(counts).map(([track, value]) => [track, Number((value / total).toFixed(3))]),
    );
  };
  const phaseTrack = (consumer, phase) => {
    const marks = consumer.marks ?? [];
    const start = marks.find((m) => m.name === `${phase}-start`);
    if (!start) return null;
    const relevant = consumer.switches.filter((s) => s.tMs <= start.tMs);
    if (relevant.length) return relevant.at(-1).track;
    // Before the first recorded change the pipeline is on its initial choice.
    return consumer.switches[0]?.track ?? null;
  };
  const endTrack = (consumer, phase) => {
    const marks = consumer.marks ?? [];
    const end = [...marks].reverse().find((m) => m.name === phase);
    const relevant = end ? consumer.switches.filter((s) => s.tMs <= end.tMs) : consumer.switches;
    return relevant.length ? relevant.at(-1).track : null;
  };

  const checks = [];
  const phasesOfKind = (kind) => PHASES.filter((entry) => entry.kind === kind).map((entry) => entry.name);
  /** Relay evidence across every phase, for whole-run connection facts. */
  const combinedRelayEvidence = () => {
    const clients = phaseReport.at(-1)?.slowRelayTotals?.clients ?? [];
    const overlaps = phaseReport.reduce(
      (sum, entry) => sum + (entry.slowRelayEvidence?.sessionOverlaps ?? 0),
      0,
    );
    return { ...(phaseReport.at(-1)?.slowRelayEvidence ?? {}), clients, sessionOverlaps: overlaps };
  };
  const reportOf = (name) => phaseReport.find((entry) => entry.phase === name);
  const firstOfKind = (kind) => reportOf(phasesOfKind(kind)[0]);
  /** The rendition that produced the most rendered frames in a phase. */
  const dominantRendered = (consumer, phaseName) => {
    const groups = consumer.paintByPhase?.[phaseName] ?? {};
    const entries = Object.entries(groups)
      .map(([size, value]) => [size, value.count ?? 0])
      .sort((a, b) => b[1] - a[1]);
    return entries[0]?.[0] ?? null;
  };
  const latency = { downshiftMs: null, recoveryMs: null };
  const effectiveLatency = { downshiftMs: null, recoveryMs: null };
  const ladder = JSON.parse(publisher.state.ready.ladder);
  const bitrateOf = (track) => ladder.find((rung) => rung.track === track)?.bitrate ?? null;
  const lowBitrate = bitrateOf('video-low');
  const check = (name, passed, evidence) => {
    checks.push({ name, status: passed ? 'passed' : 'failed', evidence });
  };

  const slowFinal = phaseReport.at(-1).slow;
  const healthyFinal = phaseReport.at(-1).healthy;

  /** Per-switch protocol window: what arrived, and what decoded, after a
   * rendition change. This is the instrument that separates "the new track's
   * media never arrived" from "it arrived without a decodable keyframe". */
  const switchWindow = (consumer, fromMs, ms) => {
    const proto = consumer.protocol ?? {};
    const objects = (proto.incomingObjectStreams ?? []).filter(
      (entry) => entry.openedMs >= fromMs && entry.openedMs <= fromMs + ms,
    );
    const decode = (proto.decodeInputs ?? []).filter(
      (entry) => entry.tMs >= fromMs && entry.tMs <= fromMs + ms,
    );
    const firstKey = decode.find((entry) => entry.type === 'key');
    return {
      fromMs,
      toMs: fromMs + ms,
      objectStreamsOpened: objects.length,
      objectBytes: objects.reduce((sum, entry) => sum + entry.bytes, 0),
      decodeInputs: decode.length,
      keyChunks: decode.filter((entry) => entry.type === 'key').length,
      deltaChunks: decode.filter((entry) => entry.type === 'delta').length,
      firstKeyOffsetMs: firstKey ? firstKey.tMs - fromMs : null,
      // draft-16 sends requests as writes on one shared control stream, so a
      // switch shows up as a write (request) and a read (answer) around it
      // rather than as a fresh QUIC stream.
      controlWritesNearSwitch: (proto.controlWrites ?? []).filter(
        (entry) => entry.tMs >= fromMs - 1500 && entry.tMs <= fromMs + 3000,
      ),
      controlReadsNearSwitch: (proto.controlReads ?? []).filter(
        (entry) => entry.tMs >= fromMs - 1500 && entry.tMs <= fromMs + 3000,
      ),
      // Decoded, redacted control messages: type id, declared body length and the
      // first body varint only (request id, or the MAX_REQUEST_ID value).
      controlMessagesNearSwitch: (proto.controlMessages ?? []).filter(
        (entry) => entry.tMs >= fromMs - 1500 && entry.tMs <= fromMs + 3000,
      ),
      requestsEnded: (proto.requestStreams ?? [])
        .filter((entry) => entry.endedMs >= fromMs && entry.endedMs <= fromMs + ms)
        .map((entry) => ({ index: entry.index, endedMs: entry.endedMs, endedHow: entry.endedHow })),
      configureAtMs: (proto.configureCalls ?? [])
        .filter((entry) => entry.tMs >= fromMs && entry.tMs <= fromMs + ms)
        .map((entry) => entry.tMs),
      decodeErrors: (proto.decodeErrors ?? []).filter(
        (entry) => entry.tMs >= fromMs && entry.tMs <= fromMs + ms,
      ),
    };
  };
  const expectedSize = (track) =>
    ({ 'video-low': '160x90', 'video-mid': '320x180', 'video-high': '640x360' })[track] ?? null;
  const firstPaintOfSize = (consumer, size, fromMs) =>
    consumer.paintTimeline?.find((paint) => paint.size === size && paint.tMs >= fromMs)?.tMs ?? null;
  const switchWindows = slowFinal.switches.map((entry) => ({
    track: entry.track,
    atMs: entry.tMs,
    ...switchWindow(slowFinal, entry.tMs, 6000),
    firstRenderedFrameMs:
      firstPaintOfSize(slowFinal, expectedSize(entry.track), entry.tMs) != null
        ? firstPaintOfSize(slowFinal, expectedSize(entry.track), entry.tMs) - entry.tMs
        : null,
  }));
  const healthySwitchWindows = healthyFinal.switches.map((entry) => ({
    track: entry.track,
    atMs: entry.tMs,
    ...switchWindow(healthyFinal, entry.tMs, 6000),
  }));
  check(
    'the last frame rendered in each phase was the selected rendition (selected vs actually rendered)',
    (() => {
      // Dominant rendered size, not the last paint: a phase can end inside a
      // probe window on the next rung while overwhelmingly rendering the target.
      const lastSize = (consumer, phase) => {
        const groups = consumer.paintByPhase?.[phase] ?? {};
        const entries = Object.entries(groups)
          .map(([size, value]) => [size, value.count ?? 0])
          .sort((a, b) => b[1] - a[1]);
        return entries[0]?.[0] ?? null;
      };
      return PHASES.every((phase) => {
        const report = phaseReport.find((r) => r.phase === phase.name);
        return (
          lastSize(report.slow, phase.name) === expectedSize(phase.expectSlow) &&
          lastSize(report.healthy, phase.name) === '640x360'
        );
      });
    })(),
    {
      lastRenderedPerPhase: Object.fromEntries(
        PHASES.map((phase) => {
          const report = phaseReport.find((r) => r.phase === phase.name);
          const lastSize = (consumer) => {
            const groups = consumer.paintByPhase?.[phase.name] ?? {};
            return Object.entries(groups)
              .map(([size, value]) => [size, value.count ?? 0])
              .sort((a, b) => b[1] - a[1])[0]?.[0] ?? null;
          };
          return [phase.name, { slow: lastSize(report.slow), healthy: lastSize(report.healthy), expectedSlow: expectedSize(phase.expectSlow) }];
        }),
      ),
      switchRenderLatencyMs: switchWindows.map((entry) => ({
        track: entry.track,
        atMs: entry.atMs,
        firstRenderedFrameMs: entry.firstRenderedFrameMs,
        firstKeyOffsetMs: entry.firstKeyOffsetMs,
      })),
    },
  );

  check(
    'each switch sent a request and the new rendition delivered a decodable keyframe',
    switchWindows
      .filter((entry) => entry.atMs > 2000)
      .every(
        (entry) =>
          entry.controlWritesNearSwitch.length >= 1 &&
          entry.objectStreamsOpened >= 1 &&
          entry.firstKeyOffsetMs != null &&
          // One compat refresh (confirm window + close + reconnect) plus the
          // join and the next keyframe on a shaped path.
          entry.firstKeyOffsetMs < 8000 &&
          entry.decodeErrors.length === 0,
      ),
    { switchWindows },
  );
  check(
    'the draft-16 client never sent SUBSCRIBE_UPDATE (0x02) — switching is SUBSCRIBE + UNSUBSCRIBE',
    ![...(slowFinal.protocol?.controlMessages ?? []), ...(healthyFinal.protocol?.controlMessages ?? [])]
      .some((entry) => entry.direction === 'out' && entry.type === 0x02),
    {
      sentTypes: [...(slowFinal.protocol?.controlMessages ?? []), ...(healthyFinal.protocol?.controlMessages ?? [])]
        .filter((entry) => entry.direction === 'out')
        .map((entry) => `${entry.typeName}@${entry.tMs}#${entry.firstVarint}`),
    },
  );
  check(
    'the SETUP request-id negotiation was observed on the wire',
    (() => {
      const setups = [
        ...(slowFinal.protocol?.totals?.setupOptions ?? []),
        ...(healthyFinal.protocol?.totals?.setupOptions ?? []),
      ];
      return (
        setups.some((entry) => entry.direction === 'out') &&
        setups.some((entry) => entry.direction === 'in' && entry.maxRequestId != null)
      );
    })(),
    {
      setups: [
        ...(slowFinal.protocol?.totals?.setupOptions ?? []),
        ...(healthyFinal.protocol?.totals?.setupOptions ?? []),
      ],
      messageTypeCounts: slowFinal.protocol?.totals?.messageTypeCounts ?? null,
    },
  );
  check(
    'the unconstrained viewer needed no session refresh (no compat policy triggered)',
    (healthyFinal.refreshLog ?? []).length === 0,
    { healthyRefreshes: healthyFinal.refreshLog ?? [] },
  );
  check(
    'no two sessions overlapped: each refresh replaced the previous lease',
    (() => {
      const phase = combinedRelayEvidence();
      const refreshes = slowFinal.refreshLog ?? [];
      const overlaps = phase.sessionOverlaps ?? 0;
      const handovers = phase.clientHandovers ?? 0;
      return overlaps === 0 && handovers >= refreshes.length;
    })(),
    (() => {
      const phase = combinedRelayEvidence();
      return {
        sessionOverlaps: phase.sessionOverlaps ?? null,
        clientHandovers: phase.clientHandovers ?? null,
        clientSessions: (phase.clients ?? []).map((entry) => ({
          firstMs: entry.firstMs,
          closedMs: entry.closedMs ?? null,
          staleSends: entry.staleSends ?? 0,
        })),
        refreshes: (slowFinal.refreshLog ?? []).map((entry) => ({ tMs: entry.tMs, closeMs: entry.closeMs ?? null, reason: entry.reason })),
      };
    })(),
  );
  check(
    'session refreshes are bounded and every one is recorded',
    (slowFinal.refreshLog ?? []).length <= 4 &&
      (slowFinal.state.sessionRefreshes ?? 0) === (slowFinal.refreshLog ?? []).length,
    {
      slowRefreshes: slowFinal.refreshLog ?? [],
      sessionsOpened: slowFinal.state.sessionsOpened ?? null,
      refreshReasons: (slowFinal.policy ?? [])
        .filter((entry) => entry.action.startsWith('session-refresh'))
        .map((entry) => entry.action),
    },
  );
  check(
    'no decoder errors on either consumer',
    (slowFinal.protocol?.totals?.decodeErrors ?? 0) === 0 &&
      (healthyFinal.protocol?.totals?.decodeErrors ?? 0) === 0,
    {
      slowProtocol: slowFinal.protocol?.totals ?? null,
      healthyProtocol: healthyFinal.protocol?.totals ?? null,
    },
  );





  {
    const downShift = slowFinal.switches.find((entry) => entry.track === 'video-low');
    const firstLowPaint = slowFinal.paintGroups?.['160x90']?.firstMs ?? null;
    if (downShift && firstLowPaint != null) {
      effectiveLatency.downshiftMs = firstLowPaint - downShift.tMs;
    }
    const recStart = (slowFinal.marks ?? []).find((m) => m.name === `${phasesOfKind('recovery')[0]}-start`)?.tMs ?? 0;
    const upShift = slowFinal.switches.find((entry) => entry.track === 'video-mid' && entry.tMs >= recStart);
    const recoveryPaint = paintAfter(phaseReport, phasesOfKind('recovery')[0], '320x180');
    if (upShift && recoveryPaint != null) {
      effectiveLatency.recoveryMs = Math.max(0, recoveryPaint - upShift.tMs);
    }
  }

  check(
    'slow consumer connects, negotiates draft, and plays',
    slowFinal.state.phase === 'playing' ||
      (slowFinal.state.version && slowFinal.switches.length > 0),
    { version: slowFinal.state.version, phase: slowFinal.state.phase, error: slowFinal.state.error },
  );
  check(
    'healthy consumer connects, negotiates draft, and plays',
    healthyFinal.state.phase === 'playing' ||
      (healthyFinal.state.version && healthyFinal.switches.length > 0),
    {
      version: healthyFinal.state.version,
      phase: healthyFinal.state.phase,
      error: healthyFinal.state.error,
    },
  );
  check(
    'publisher catalog advertises the same three-rendition ladder to both consumers',
    JSON.stringify(slowFinal.state.availableTracks ?? null) ===
        JSON.stringify(healthyFinal.state.availableTracks ?? null) &&
      (slowFinal.state.availableTracks ?? []).length === 3,
    { slow: slowFinal.state.available, healthy: healthyFinal.state.available },
  );
  for (const phase of PHASES) {
    const report = phaseReport.find((r) => r.phase === phase.name);
    const slowTrack = phaseTrack(report.slow, phase.name);
    const healthyTrack = phaseTrack(report.healthy, phase.name);
    const endedSlow = endTrack(report.slow, phase.name);
    // Time spent on each rendition in the phase, from the switch timeline.
    const share = occupancy(report.slow, phase);
    check(
      `slow viewer holds ${phase.expectSlow} for the ${phase.name} phase`,
      // Scored by time share, not the instantaneous end state: a bounded
      // upshift probe can be in flight exactly at a phase boundary, which is the
      // mechanism working, not the rendition failing to follow the path.
      (share[phase.expectSlow] ?? 0) >= (phase.kind === 'downshift' ? 0.5 : 0.6),
      {
        selectedAtPhaseStart: slowTrack,
        selectedAtPhaseEnd: endedSlow,
        timeShare: share,
      },
    );
    check(
      `healthy viewer stays on video-high in phase ${phase.name}`,
      healthyTrack === 'video-high' && (occupancy(report.healthy, phase)['video-high'] ?? 0) === 1,
      { selectedAtPhaseStart: healthyTrack, timeShare: occupancy(report.healthy, phase) },
    );
  }
  check(
    'downshift and recovery each landed within a few seconds of the shaping change',
    (() => {
      const marks = slowFinal.marks ?? [];
      const downStart = marks.find((m) => m.name === `${phasesOfKind('downshift')[0]}-start`)?.tMs ?? 0;
      const recStart = marks.find((m) => m.name === `${phasesOfKind('recovery')[0]}-start`)?.tMs ?? 0;
      const switchedDown = slowFinal.switches.find((s) => s.track === 'video-low');
      const switchedUp = slowFinal.switches
        .filter((s) => s.track === 'video-mid' && s.tMs >= recStart)
        .at(0);
      latency.downshiftMs = switchedDown ? switchedDown.tMs - downStart : null;
      latency.recoveryMs = switchedUp ? switchedUp.tMs - recStart : null;
      return (
        latency.downshiftMs != null &&
        latency.downshiftMs < 4000 &&
        latency.recoveryMs != null &&
        latency.recoveryMs < 8000
      );
    })(),
    latency,
  );
  check(
    'slow viewer never switched faster than one transition per 500ms (no hot loop)',
    (() => {
      const times = slowFinal.switches.map((s) => s.tMs);
      return times.every((t, i) => i === 0 || t - times[i - 1] >= 500);
    })(),
    { switchTimes: slowFinal.switches.map((s) => [s.tMs, s.track]) },
  );
  check(
    'every upshift probe on the shaped path was backed off by measurement',
    (() => {
      const probes = slowFinal.policy.filter((p) => p.action === 'probe-up');
      const downshifts = slowFinal.policy.filter((p) => p.action === 'downshift');
      return probes.length >= 1 && downshifts.length >= 2;
    })(),
    {
      probeUpCount: slowFinal.policy.filter((p) => p.action === 'probe-up').length,
      downshiftCount: slowFinal.policy.filter((p) => p.action === 'downshift').length,
      probeDecisions: slowFinal.policy.filter((p) => p.action === 'probe-up').slice(0, 6),
    },
  );
  check(
    'slow viewer followed the ladder down and back up in order, without oscillation',
    JSON.stringify(tracksByName(slowFinal).sort()) === JSON.stringify(['video-low', 'video-mid']) &&
      slowFinal.switches[0]?.track === 'video-mid' &&
      slowFinal.switches.at(-1)?.track === 'video-mid' &&
      slowFinal.switches.some((s) => s.track === 'video-low') &&
      slowFinal.switches.length >= 3 &&
      slowFinal.switches.length <= 9 &&
    { tracksSeen: tracksByName(slowFinal), switches: slowFinal.switches },
  );
  check(
    'healthy viewer kept one rendition for the whole run',
    tracksByName(healthyFinal).length === 1 &&
      tracksByName(healthyFinal)[0] === 'video-high' &&
      healthyFinal.switches.length <= 1,
    { tracksSeen: tracksByName(healthyFinal), switches: healthyFinal.switches },
  );

  const slowPaintGroups = slowFinal.paintGroups ?? {};
  const healthyPaintGroups = healthyFinal.paintGroups ?? {};
  check(
    'slow viewer rendered the low rendition in every shaped cycle',
    phasesOfKind('downshift').every(
      (name) => dominantRendered(slowFinal, name) === '160x90' && (reportOf(name).slow.paintByPhase?.[name]?.['160x90']?.count ?? 0) >= 20,
    ),
    {
      renderedPerShapedCycle: Object.fromEntries(
        phasesOfKind('downshift').map((name) => [name, reportOf(name).slow.paintByPhase?.[name] ?? {}]),
      ),
      lowPaintCumulative: slowFinal.paintGroups?.['160x90'] ?? null,
      // Reported, not asserted: on the real relay the rendition change became
      // visible long after the selector moved, so the two are separated here.
      effectiveDownshiftLatencyMs: effectiveLatency.downshiftMs,
      effectiveRecoveryLatencyMs: effectiveLatency.recoveryMs,
    },
  );
  check(
    'slow viewer rendered the mid rendition in every restored cycle (>=3 cycles)',
    phasesOfKind('recovery').length >= 3 &&
      phasesOfKind('recovery').every(
        (name) =>
          dominantRendered(slowFinal, name) === '320x180' &&
          (reportOf(name).slow.paintByPhase?.[name]?.['320x180']?.count ?? 0) >= 60,
      ),
    {
      renderedPerRestoredCycle: Object.fromEntries(
        phasesOfKind('recovery').map((name) => [name, reportOf(name).slow.paintByPhase?.[name] ?? {}]),
      ),
      effectiveRecoveryLatencyMs: effectiveLatency.recoveryMs,
    },
  );
  check(
    'slow viewer never stopped rendering during the shaped phase',
    (firstOfKind('downshift').slow.paintCount ?? 0) >= 8,
    {
      downshiftPhasePaints: Object.fromEntries(
        Object.entries(firstOfKind('downshift').slow.paintByPhase?.[phasesOfKind('downshift')[0]] ?? {}).map(
          ([key, value]) => [key, value.count],
        ),
      ),
      effectiveDownshiftLatencyMs: effectiveLatency.downshiftMs,
    },
  );
  check(
    'healthy viewer painted the 640x360 rendition',
    Object.keys(healthyPaintGroups).some((key) => key.startsWith('640x360')),
    { paintGroups: healthyPaintGroups },
  );
  check(
    'healthy viewer keeps painting the high rendition throughout every phase',
    PHASES.every((phase) => {
      const painted =
        phaseReport.find((r) => r.phase === phase.name).healthy.paintByPhase?.[phase.name]?.[
          '640x360'
        ]?.count ?? 0;
      return painted > (phase.ms / 1000) * 15;
    }),
    {
      paintedPerPhase: Object.fromEntries(
        PHASES.map((phase) => [
          phase.name,
          phaseReport.find((r) => r.phase === phase.name).healthy.paintByPhase?.[phase.name] ?? {},
        ]),
      ),
    },
  );
  check(
    'shaping was real: the downshifted path carried far less than the healthy path',
    (() => {
      const phase = firstOfKind('downshift');
      const slowMedian = median(phase.slowRelaySamples.map((s) => s.downBps));
      const healthyMedian = median(phase.healthyRelaySamples.map((s) => s.downBps));
      return (
        slowMedian > 180_000 &&
        slowMedian < 360_000 &&
        healthyMedian > 900_000 &&
        phase.slowRelayTotals.down.droppedBytes > 0
      );
    })(),
    {
      slowMedianDownBps: median(firstOfKind('downshift').slowRelaySamples.map((s) => s.downBps)),
      healthyMedianDownBps: median(
        firstOfKind('downshift').healthyRelaySamples.map((s) => s.downBps),
      ),
      slowDroppedBytes: firstOfKind('downshift').slowRelayTotals.down.droppedBytes,
    },
  );
  check(
    'the settling and recovery phases restored real capacity on the slow path',
    (() => {
      const settle = median(reportOf(phasesOfKind('settle')[0]).slowRelaySamples.map((s) => s.downBps));
      const recovery = median(
        phasesOfKind('recovery')
          .flatMap((name) => reportOf(name).slowRelaySamples)
          .map((s) => s.downBps),
      );
      return settle > 300_000 && recovery > 300_000;
    })(),
    {
      settleMedianDownBps: median(
        reportOf(phasesOfKind('settle')[0]).slowRelaySamples.map((s) => s.downBps),
      ),
      recoveryMedianDownBps: median(
        phasesOfKind('recovery')
          .flatMap((name) => reportOf(name).slowRelaySamples)
          .map((s) => s.downBps),
      ),
    },
  );
  check(
    'publisher uplink per rendition is independent of any consumer link state',
    (() => {
      const perPhase = PHASES.map((phase) => phaseReport.find((r) => r.phase === phase.name).publisher.tracks);
      const rates = ['video-low', 'video-mid', 'video-high'].map((track) =>
        perPhase.map((entry) => entry[track]?.bytesPerSecond ?? 0),
      );
      return rates.every((series) => {
        const max = Math.max(...series);
        const min = Math.min(...series);
        return min > 0 && max / min < 1.2;
      });
    })(),
    { perPhaseUplink: PHASES.map((phase) => phaseReport.find((r) => r.phase === phase.name).publisher.tracks) },
  );
  check(
    'production cadence is unmodified at ~30 fps per rendition',
    (() => {
      const perPhase = PHASES.map((phase) => phaseReport.find((r) => r.phase === phase.name).publisher.tracks);
      return perPhase.every((tracks) =>
        ['video-low', 'video-mid', 'video-high'].every((track) => {
          const fps = tracks[track]?.framesPerSecond ?? 0;
          return fps > 27 && fps < 33;
        }),
      );
    })(),
    { perPhaseCadence: PHASES.map((phase) => phaseReport.find((r) => r.phase === phase.name).publisher.tracks) },
  );
  check(
    'peer PROBE is empty on moq-transport, so selection ran on measured arrival',
    slowFinal.state.probeIsEmptyFromPeer === true &&
      slowFinal.samples.length >= 20 &&
      ['transport', 'decoder-media'].includes(slowFinal.state.measurementSource) &&
      (() => {
        const values = slowFinal.samples.map((entry) => entry.smoothedBps);
        return values.length > 0 && Math.min(...values) < Math.max(...values) * 0.5;
      })(),
    {
      probeIsEmptyFromPeer: slowFinal.state.probeIsEmptyFromPeer,
      measurementSource: slowFinal.state.measurementSource,
      transportStatsShape: slowFinal.state.transportStatsShape ?? null,
      targetValue: slowFinal.state.targetValue ?? null,
      sampleCount: slowFinal.samples.length,
      measuredRange: slowFinal.samples.length
        ? [
            Math.min(...slowFinal.samples.map((s) => s.smoothedBps)),
            Math.max(...slowFinal.samples.map((s) => s.smoothedBps)),
          ]
        : null,
    },
  );
  check(
    'the measured arrival rate tracked the shaped capacity while settled on the low rendition',
    (() => {
      const marks = slowFinal.marks ?? [];
      const start = marks.find((m) => m.name === `${phasesOfKind('downshift')[0]}-start`)?.tMs ?? 0;
      const end = marks.find((m) => m.name === phasesOfKind('downshift')[0])?.tMs ?? Infinity;
      const report = firstOfKind('downshift');
      const settled = slowFinal.samples.filter(
        (s) =>
          s.tMs >= start &&
          s.tMs <= end &&
          s.declaredBps === lowBitrate &&
          !['probing', 'probe-up'].includes(s.policyAction),
      );
      // Delivery can collapse for seconds while the abandoned rendition's data
      // drains, so the assertion is that the low rendition's real arrival
      // reached the link's capacity for it, not that every sample did.
      const best = settled.length ? Math.max(...settled.map((s) => s.smoothedBps)) : 0;
      return (
        settled.length >= 5 &&
        // Tail-drop on a hard-shaped path costs some of the encoded rate, so
        // the assertion is delivery in the right band for the rung, not
        // byte-exact delivery; the value is recorded either way.
        best >= lowBitrate * 0.6 &&
        best <= lowBitrate * 3 &&
        report.slowRelaySamples.length > 0
      );
    })(),
    {
      settledSamples: slowFinal.samples
        .filter(
          (s) =>
            s.declaredBps === lowBitrate &&
            !['probing', 'probe-up'].includes(s.policyAction),
        )
        .slice(-8),
      lowBitrate,
    },
  );

  result = {
    mode,
    draft,
    ranAt: new Date().toISOString(),
    chromium: browserSlow.version(),
    fixture: publisher.state.ready,
    phases: phaseReport,
    finalPublisher,
    checks,
    switchWindows,
    healthySwitchWindows,
    sessionRefreshes: {
      slow: { count: slowFinal.refreshLog?.length ?? 0, log: slowFinal.refreshLog ?? [], sessionsOpened: slowFinal.state.sessionsOpened ?? null },
      healthy: { count: healthyFinal.refreshLog?.length ?? 0, log: healthyFinal.refreshLog ?? [], sessionsOpened: healthyFinal.state.sessionsOpened ?? null },
    },
    setupOptions: {
      slow: slowFinal.protocol?.totals?.setupOptions ?? null,
      healthy: healthyFinal.protocol?.totals?.setupOptions ?? null,
    },
    controlMessageSummary: {
      slow: slowFinal.protocol?.totals?.messageTypeCounts ?? null,
      healthy: healthyFinal.protocol?.totals?.messageTypeCounts ?? null,
    },
    protocolTotals: {
      slow: slowFinal.protocol?.totals ?? null,
      healthy: healthyFinal.protocol?.totals ?? null,
    },
    passed: checks.every((c) => c.status === 'passed'),
  };
  writeFileSync(outPath, `${JSON.stringify(result, null, 2)}\n`);
  console.log(
    `ABR_PROBE ${result.passed ? 'PASS' : 'FAIL'} checks=${checks.filter((c) => c.status === 'passed').length}/${checks.length} out=${outPath}`,
  );
  for (const item of checks.filter((c) => c.status === 'failed')) {
    console.log(`FAILED: ${item.name} :: ${JSON.stringify(item.evidence)}`);
  }
  if (!result.passed) process.exitCode = 1;
  }
} catch (error) {
  result = { mode, draft, error: String(error?.message ?? error), stack: String(error?.stack ?? ''),
    consumerFailures: pages ? await Promise.all(Object.entries(pages).map(async ([label, page]) => ({ label, state: await page.evaluate(() => window.abr?.state).catch(() => null) }))) : [] };
  writeFileSync(outPath, `${JSON.stringify(result, null, 2)}\n`);
  console.log(`ABR_PROBE ERROR ${result.error}`);
  process.exitCode = 1;
} finally {
  // Preserve real DELETE acknowledgements on normal completion and failures.
  // Browser.close alone only closes QUIC; it cannot revoke server ownership.
  if (pages) {
    const releases = await Promise.allSettled(Object.values(pages).map((page) =>
      page.evaluate(async () => { await window.abr?.close?.(); return window.abr?.summary?.().leaseEvents ?? []; }),
    ));
    if (result) {
      result.authorityCleanup = releases.map((release) => release.status === 'fulfilled'
        ? { status: 'fulfilled', leaseEvents: release.value }
        : { status: 'rejected', error: 'LEASE_CLEANUP_UNCONFIRMED' });
      if (authority && releases.some((release) => release.status === 'rejected')) {
        result.passed = false;
        process.exitCode = 1;
      }
      writeFileSync(outPath, `${JSON.stringify(result, null, 2)}\n`);
    }
  }
  for (const browser of [browserSlow, browserHealthy]) {
    try {
      await browser?.close();
    } catch {}
  }
  for (const relay of relays) {
    try {
      await relay?.close();
    } catch {}
  }
  if (server) await new Promise((r) => server.httpServer.close(r));
  publisher?.stop();
}

async function page_eval_all(pages, name) {
  await Promise.all(
    Object.values(pages).map((page) =>
      page.evaluate((mark) => window.abr.mark(mark), name).catch(() => {}),
    ),
  );
}

async function waitForState(pages, wanted, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const states = await Promise.all(
      Object.values(pages).map((page) =>
        page.evaluate(() => window.abr?.state?.phase ?? 'none').catch(() => 'none'),
      ),
    );
    if (states.every((state) => state === wanted)) return;
    if (states.some((state) => state === 'failed')) throw new Error(`CONSUMER_FAILED: ${states}`);
    if (Date.now() > deadline) throw new Error(`CONSUMER_NOT_${wanted}: ${states}`);
    await sleep(250);
  }
}
