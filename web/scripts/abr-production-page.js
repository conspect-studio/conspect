// Exact UI playback wrapper. Test instrumentation adds only the local certificate
// pin and routes the server-issued lease through this consumer's real UDP shaper.
import { startMoqPlaybackWithRefresh } from '../src/preview/moqPlayback.ts';
import { createMoqLeaseApi } from '../src/preview/moqLeaseApi.ts';
import { installProtocolInstrumentation } from './abr-protocol-instrument.js';

window.startAbrProbe = async ({ endpoint, hash, label }) => {
  const started = performance.now();
  const t = () => Math.round(performance.now() - started);
  const state = { phase: 'connecting', label, exactUiWrapper: true };
  const policy = [], paints = [], leaseEvents = [], transportEvents = [];
  const protocol = installProtocolInstrumentation(started);
  const controller = new AbortController();
  const nativeTransport = globalThis.WebTransport;
  if (hash) {
    globalThis.WebTransport = class extends nativeTransport {
      constructor(url, options) {
        super(url, { ...options, serverCertificateHashes: [{ algorithm: 'sha-256', value: Uint8Array.from(hash.match(/../g), (v) => parseInt(v, 16)) }] });
        transportEvents.push({ action: 'constructed', tMs: t() });
        this.closed.then(
          (info) => transportEvents.push({ action: 'closed', tMs: t(), closeCode: info?.closeCode }),
          (error) => transportEvents.push({ action: 'closed-error', tMs: t(), errorClass: error?.constructor?.name, source: error?.source, streamErrorCode: error?.streamErrorCode }),
        );
      }
      close(info) {
        transportEvents.push({ action: 'close-requested', tMs: t(), closeCode: info?.closeCode });
        return super.close(info);
      }
    };
  }
  const context = CanvasRenderingContext2D.prototype;
  const draw = context.drawImage;
  context.drawImage = function (image, ...args) {
    if (image instanceof VideoFrame) paints.push({ tMs: t(), width: image.displayWidth, height: image.displayHeight, pts: image.timestamp });
    return draw.call(this, image, ...args);
  };
  const real = createMoqLeaseApi('program', '/api/v1/preview/program/whep');
  const api = {
    async acquire(signal) {
      const acquireStartedMs = t();
      const lease = await real.acquire(signal);
      const acquireFinishedMs = t();
      const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(lease.lease_id));
      const leaseIdHash = Array.from(new Uint8Array(digest)).map((v) => v.toString(16).padStart(2, '0')).join('');
      leaseEvents.push({ action: 'acquired', tMs: t(), acquireStartedMs, acquireFinishedMs, expiresInMs: lease.expires_in_ms, generation: lease.generation, leaseIdHash });
      const route = new URL(endpoint), url = new URL(lease.url);
      url.hostname = route.hostname; url.port = route.port;
      return { ...lease, url: url.href };
    },
    async release(lease) {
      const releaseStartedMs = t();
      await real.release(lease);
      const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(lease.lease_id));
      const leaseIdHash = Array.from(new Uint8Array(digest)).map((v) => v.toString(16).padStart(2, '0')).join('');
      leaseEvents.push({ action: 'revoked-ack', tMs: t(), releaseStartedMs, generation: lease.generation, leaseIdHash });
    },
  };
  const task = startMoqPlaybackWithRefresh({ api, canvas: document.querySelector('canvas'), signal: controller.signal, audio: false,
    onActive: () => { state.phase = 'playing'; },
    onAbr: (abr) => { policy.push({ tMs: t(), ...abr }); },
  }).catch((error) => { state.phase = 'failed'; state.error = error.constructor.name; state.message = error.message; });
  window.abr = { state, policy, paints, leaseEvents, mark: () => {},
    summary: () => ({ elapsedMs: t(), state, policy, leaseEvents, transportEvents, paints, protocol: protocol.snapshot(), switches: [], refreshLog: [],
      maxRenderedGapMs: paints.slice(1).reduce((max, p, i) => Math.max(max, p.tMs - paints[i].tMs), 0),
      advancingRenderedPts: paints.slice(1).filter((p, i) => p.pts > paints[i].pts).length,
      renderedGaps: paints.slice(1).filter((p, i) => p.tMs - paints[i].tMs > 250).map((p) => {
        const index = paints.indexOf(p);
        return { fromMs: paints[index - 1].tMs, toMs: p.tMs, gapMs: p.tMs - paints[index - 1].tMs, ptsDeltaUs: p.pts - paints[index - 1].pts };
      }),
    }),
    close: async () => { controller.abort(); await task; context.drawImage = draw; globalThis.WebTransport = nativeTransport; },
  };
};
