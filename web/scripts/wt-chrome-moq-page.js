// The app's own MoQ playback (startMoqPlaybackWithRefresh: connectMoq -> @moq/net,
// @moq/watch, ABR, lease renewal) against an h3 front door, for
// `wt-chrome-probe.mjs` with `moq`. Only two things are test instrumentation:
// the lease API hands out server-admitted leases passed in by the test, and
// WebTransport gets the local certificate pin.
import { startMoqPlaybackWithRefresh } from '../src/preview/moqPlayback.ts';

window.probe = { phase: 'loaded', paints: [], error: null, version: null, protocols: [], leases: 0, transports: [] };
window.startMoq = async ({ leases, hash, playMs }) => {
  const state = window.probe;
  const pin = Uint8Array.from(hash.match(/../g), (v) => parseInt(v, 16));
  const Native = globalThis.WebTransport;
  globalThis.WebTransport = class extends Native {
    constructor(url, options) {
      super(url, { ...options, serverCertificateHashes: [{ algorithm: 'sha-256', value: pin }] });
      const entry = { offered: options?.protocols ?? null, atMs: Math.round(performance.now()) };
      this.closed.then((i) => { entry.closedAtMs = Math.round(performance.now()); entry.close = i; }, (e) => { entry.closedAtMs = Math.round(performance.now()); entry.closeError = String(e?.message ?? e); });
      state.transports.push(entry);
      this.ready.then(() => { entry.ready = true; entry.protocol = this.protocol; },
        (e) => { entry.ready = false; entry.error = String(e?.message ?? e); });
    }
  };
  const draw = CanvasRenderingContext2D.prototype.drawImage;
  CanvasRenderingContext2D.prototype.drawImage = function (image, ...args) {
    if (image instanceof VideoFrame) state.paints.push(image.timestamp);
    return draw.call(this, image, ...args);
  };
  const api = {
    async acquire() {
      const lease = leases[state.leases];
      if (!lease) throw new Error('test lease pool exhausted');
      state.leases += 1;
      return { ...lease, expires_at: new Date(Date.now() + lease.expires_in_ms).toISOString() };
    },
    async release() {},
  };
  const t0 = performance.now();
  const controller = new AbortController();
  state.phase = 'playing';
  const task = startMoqPlaybackWithRefresh({ api, canvas: document.querySelector('canvas'), signal: controller.signal,
    audio: false, onActive: () => { state.active = true; } })
    .catch((error) => {
      const text = `${error?.constructor?.name}: ${error?.message ?? error}`;
      // Only a failure before the probe's own abort counts.
      if (!controller.signal.aborted) { state.error = text; state.errorAtMs = Math.round(performance.now() - t0); } else state.afterAbort = text;
    });
  const startWait = performance.now();
  while (state.paints.length === 0 && !state.error && performance.now() - startWait < 15000) await new Promise((r) => setTimeout(r, 50));
  state.firstFrameMs = Math.round(performance.now() - startWait);
  const counted = state.paints.length;
  await Promise.race([new Promise((r) => setTimeout(r, playMs)), task]);
  state.framesInWindow = state.paints.length - counted;
  controller.abort();
  await task;
  state.phase = state.error ? 'failed' : 'played';
};
