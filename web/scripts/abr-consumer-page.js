// Real two-consumer acceptance harness. Selection uses production AbrSampler.
// Canvas observations prove actual rendered dimensions, independently of selection.
import { Origin, Path, Signals } from '@moq/net';
import { Player } from '@moq/watch';
import { AbrSampler } from '../src/preview/moqAbr.ts';
import { connectMoq } from '../src/preview/moqConnection.ts';
import { createMoqLeaseApi } from '../src/preview/moqLeaseApi.ts';
import { installProtocolInstrumentation } from './abr-protocol-instrument.js';

const now = () => (typeof performance === 'undefined' ? Date.now() : performance.now());

export const PROBE_CONSTANTS = {
  SWITCH_CONFIRM_MS: 4000,
  REFRESH_MAX: 4,
  REFRESH_COOLDOWN_MS: 1500,
  PROBE_INTERVAL_MS: 8000,
  PROBE_WINDOW_MS: 2500,
  PROBE_BACKOFF_MS: 3000,
  SAMPLE_MS: 500,
};

window.startAbrProbe = async ({ endpoint, hash, cap, label, authority }) => {
  // Same-origin /api must proxy the real control server. No callback can assert
  // authority: every refresh consumes its typed, generation-bound DELETE ACK.
  const leaseApi = authority
    ? createMoqLeaseApi(authority.scope, authority.whepEndpoint)
    : null;
  let lease = null;
  const leaseEvents = [];
  const {
    SWITCH_CONFIRM_MS,
    REFRESH_MAX,
    REFRESH_COOLDOWN_MS,
    PROBE_INTERVAL_MS,
    PROBE_WINDOW_MS,
    PROBE_BACKOFF_MS,
    SAMPLE_MS,
  } = PROBE_CONSTANTS;

  // The endpoint path carries the relay token, so only the origin and the path
  // length are recorded. No token reaches the evidence.
  const parsed = new URL(endpoint);
  const state = {
    phase: 'module-loaded',
    label: label ?? null,
    endpointOrigin: parsed.origin,
    endpointPathLength: parsed.pathname.length,
    endpoint: `${parsed.origin}/<redacted>`,
    cap: cap ?? null,
    sessionsOpened: 0,
  };
  const samples = [];
  const paints = [];
  const decoder = [];
  const switches = [];
  const marks = [];
  const policy = [];
  const refreshLog = [];
  let instrument = null;

  window.abr = { state, samples, paints, decoder, switches, marks, policy, refreshLog };
  const started = now();
  const since = () => Math.round(now() - started);
  window.abr.mark = (name) => marks.push({ name, tMs: since() });
  window.abr.summary = () => {
    const group = (list) => {
      const groups = {};
      for (const paint of list) {
        const key = `${paint.width}x${paint.height}`;
        groups[key] = groups[key] ?? { count: 0, firstMs: paint.tMs, lastMs: paint.tMs };
        groups[key].count += 1;
        groups[key].lastMs = paint.tMs;
      }
      return groups;
    };
    const paintByPhase = {};
    for (const end of marks) {
      if (end.name.endsWith('-start')) continue;
      const start = marks.find((m) => m.name === `${end.name}-start`);
      if (!start) continue;
      paintByPhase[end.name] = group(
        paints.filter((paint) => paint.tMs >= start.tMs && paint.tMs <= end.tMs),
      );
    }
    return {
      state,
      marks,
      switches,
      protocol: instrument ? instrument.snapshot() : null,
      samples: samples.slice(-400),
      policy,
      refreshLog,
      leaseEvents,
      decoder: decoder.slice(-40),
      paintCount: paints.length,
      // Canvas draws are observable output, not encoded-receive notifications.
      // Report freezes across sessions without claiming capture-to-display latency.
      renderedGapsMs: paints.slice(1).map((paint, index) => paint.tMs - paints[index].tMs),
      maxRenderedGapMs: paints.slice(1).reduce((gap, paint, index) => Math.max(gap, paint.tMs - paints[index].tMs), 0),
      advancingRenderedPts: paints.slice(1).filter((paint, index) => paint.timestampUs > paints[index].timestampUs).length,
      paintGroups: group(paints),
      paintByPhase,
      // What actually reached the canvas: the rendered size of every frame.
      paintTimeline: paints.map((paint) => ({ tMs: paint.tMs, size: `${paint.width}x${paint.height}` })),
    };
  };

  // Ground truth for "which rendition is really on screen": the decoded
  // VideoFrame that reaches the canvas, with its own dimensions. This is a
  // decoder output count, not an encoded-receive counter.
  const context = CanvasRenderingContext2D.prototype;
  const originalDrawImage = context.drawImage;
  context.drawImage = function (image, ...args) {
    if (image instanceof VideoFrame) {
      paints.push({
        tMs: since(),
        width: image.displayWidth,
        height: image.displayHeight,
        timestampUs: Number(image.timestamp),
      });
    }
    return originalDrawImage.call(this, image, ...args);
  };

  // Inputs the pipeline observes live. Kept across session refreshes so the
  // target and the hysteresis survive: no identity reset.
  const probe = new Signals.Signal({});
  const target = new Signals.Signal(cap ? { width: cap.width, height: cap.height } : undefined);

  let sampler = null;
  let resume;
  let lastEstimate = null;
  let refreshing = false;
  let pending = null;
  let refreshes = 0;
  let live = { origin: null, session: null, player: null, sourceOut: null };

  const rungs = () => {
    const available = state.available ?? {};
    const out = [];
    for (const [name, config] of Object.entries(available)) {
      if (!config?.bitrate) continue;
      if (cap?.width != null && (config.codedWidth ?? 0) > cap.width) continue;
      if (cap?.height != null && (config.codedHeight ?? 0) > cap.height) continue;
      out.push({
        name,
        bitrate: config.bitrate,
        size: `${config.codedWidth}x${config.codedHeight}`,
        width: config.codedWidth,
        height: config.codedHeight,
      });
    }
    return out.sort((a, b) => a.bitrate - b.bitrate);
  };
  const sizeOf = (track) => rungs().find((rung) => rung.name === track)?.size ?? null;

  const establish = async (reason) => {
    if (leaseApi) {
      if (lease) throw new Error('PREVIOUS_LEASE_NOT_RELEASED');
      lease = await leaseApi.acquire(AbortSignal.timeout(10000));
      leaseEvents.push({ action: 'acquired', tMs: since(), generation: lease.generation });
    }
    const origin = new Origin.Producer();
    const transportUrl = new URL(lease?.url ?? endpoint);
    // The lease path remains unchanged. Only loopback UDP routing goes through
    // this consumer's independent shaper; the authority still owns the token.
    if (lease && ['localhost', '[::1]', '::1'].includes(transportUrl.hostname)) {
      transportUrl.hostname = new URL(endpoint).hostname;
      transportUrl.port = new URL(endpoint).port;
    }
    const session = await connectMoq(
      transportUrl,
      AbortSignal.timeout(20000),
      origin,
      hash ? { serverCertificateHashes: [{ value: hash }] } : {},
    );
    state.version = session.version;
    const player = new Player({
      origin,
      name: Path.from(lease?.broadcast ?? 'fixture'),
      canvas: document.querySelector('canvas'),
      enabled: true,
      announced: false,
      muted: true,
      paused: false,
      visible: 'always',
      probe,
      target,
    });
    const sourceOut = player?.video?.source?.out;
    if (sourceOut?.track?.subscribe) {
      sourceOut.track.subscribe((value) => {
        switches.push({
          tMs: since(),
          track: value ?? null,
          measuredBpsAtSwitch: lastEstimate,
          session: refreshes,
        });
        if (value && sizeOf(value)) {
          pending = { track: value, size: sizeOf(value), atMs: since(), tried: 0 };
        }
      });
    }
    if (sourceOut?.available?.subscribe) {
      sourceOut.available.subscribe((value) => {
        state.available = value ?? null;
        state.availableTracks = value ? Object.keys(value) : null;

      });
    }
    if (sourceOut?.error?.subscribe) {
      sourceOut.error.subscribe((value) => {
        state.sourceError = value ?? null;
      });
    }
    live = { origin, session, player, sourceOut };
    let presentedFrames = 0;
    let presentedFrame;
    let lastPresentedTimestamp;
    player.video.out.frame.subscribe((frame) => {
      if (!frame || frame.timestamp === lastPresentedTimestamp) return;
      if (lastPresentedTimestamp !== undefined && frame.timestamp < lastPresentedTimestamp) return;
      lastPresentedTimestamp = frame.timestamp;
      presentedFrames += 1;
      presentedFrame = { width: frame.displayWidth, height: frame.displayHeight };
    });
    sampler = new AbrSampler({
      source: {
        stats: () => session.stats(),
        probe: () => session.probe.peek(),
        catalog: () => player.broadcast.out.catalog.peek()?.video,
        available: () => Object.fromEntries(Object.entries(sourceOut.available.peek() ?? {})
          .filter(([, c]) => (!cap || (c.codedWidth <= cap.width && c.codedHeight <= cap.height)))),
        unsupported: () => sourceOut.error.peek() === 'unsupported',
        videoBytes: () => player.video.out.stats.peek()?.bytesReceived,
        presentedFrames: () => presentedFrames,
        presentedFrame: () => presentedFrame,
        stalled: () => player.video.out.stalled.peek(),
        bufferedMs: () => player.video.out.buffered.peek(),
        selection: () => ({ track: sourceOut.track.peek(), config: sourceOut.config.peek() }),
      },
      target,
      resume,
      onState: (abr) => {
        state.productionAbr = abr;
        policy.push({ tMs: since(), action: abr.reason, ...abr });
      },
    });
    sampler.start();

    state.sessionsOpened = refreshes + 1;
    state.sessionReason = reason;
    return live;
  };

  const closeSession = async () => {
    resume = sampler?.snapshot();
    sampler?.close();
    sampler = null;
    const { player, session, origin } = live;
    const closeStartedMs = now();
    try {
      player?.close();
    } catch {}
    try {
      session?.close();
    } catch {}
    // Wait for the peer-visible close before opening a fresh session: two
    // overlapping sessions would be two leases on the same relay.
    if (session?.closed && typeof session.closed.then === 'function') {
      try {
        await Promise.race([session.closed.then(() => {}), new Promise((_, reject) => setTimeout(() => reject(new Error('SESSION_CLOSE_TIMEOUT')), 3000))]);
      } catch { throw new Error('SESSION_CLOSE_UNCONFIRMED'); }
    }
    try {
      origin?.close();
    } catch {}
    const closedMs = Math.round(now() - closeStartedMs);
    refreshLog.at(-1) && (refreshLog.at(-1).closeMs = closedMs);
    live = { origin: null, session: null, player: null, sourceOut: null };
  };

  const refreshSession = async (reason) => {
    if (refreshing) return false;
    refreshing = true;
    refreshes += 1;
    refreshLog.push({ tMs: since(), reason, sessionIndex: refreshes });
    policy.push({ tMs: since(), action: 'session-refresh', reason, sessionsOpened: refreshes + 1 });
    try {
      await closeSession();
      if (!leaseApi || !lease) throw new Error('PRODUCTION_LEASE_ACK_UNAVAILABLE');
      await leaseApi.release(lease);
      leaseEvents.push({ action: 'revoked-ack', tMs: since(), generation: lease.generation });
      lease = null;
      await establish(reason);
    } catch (error) {
      policy.push({ tMs: since(), action: 'session-refresh-failed', reason, error: String(error?.message ?? error) });
      state.phase = 'refresh-failed';
      refreshing = false;
      return false;
    }
    refreshing = false;
    return true;
  };


  try {
    state.phase = 'connecting';
    instrument = installProtocolInstrumentation(started);
    const { session } = await establish('initial');

    let lastBytes = null;
    let lastAt = null;
    let smoothed = null;
    const loop = setInterval(async () => {
      try {
        if (refreshing) return;
        if (lease && Date.parse(lease.expires_at) - Date.now() < 10000) {
          if (refreshes >= REFRESH_MAX) throw new Error('LEASE_RENEWAL_BUDGET_EXHAUSTED');
          await refreshSession('lease-expiry');
          return;
        }
        const at = now();
        let bytes = null;
        let source = null;
        try {
          const stats = await live.session?.stats?.();
          if (stats && typeof stats.bytesReceived === 'number') {
            bytes = stats.bytesReceived;
            source = 'transport';
          } else {
            state.transportStatsShape = stats ? Object.keys(stats) : null;
          }
        } catch (error) {
          state.transportStatsError = String(error?.message ?? error);
        }
        if (bytes === null) {
          const decoded = live.player?.video?.out?.stats?.peek?.();
          if (decoded && typeof decoded.bytesReceived === 'number') {
            bytes = decoded.bytesReceived;
            source = 'decoder-media';
          }
        }
        state.measurementSource = source ?? null;
        if (bytes !== null && lastBytes !== null && at > lastAt && bytes >= lastBytes) {
          const seconds = (at - lastAt) / 1000;
          const instant = ((bytes - lastBytes) * 8) / seconds;
          smoothed = smoothed === null ? instant : smoothed * 0.4 + instant * 0.6;
          const observed = Math.round(smoothed);
          samples.push({
            tMs: since(),
            source,
            bytes,
            measuredBps: Math.round(instant),
            smoothedBps: observed,
            declaredBps: state.effectiveConfig?.bitrate ?? null,
            currentTrack: state.effectiveTrack ?? null,
            sessionsOpened: refreshes + 1,
          });
          lastEstimate = observed;

        }
        if (bytes !== null) {
          lastBytes = bytes;
          lastAt = at;
        }
        const decoded = live.player?.video?.out?.stats?.peek?.();
        if (decoded) {
          decoder.push({
            tMs: since(),
            frameCount: decoded.frameCount,
            bytesReceived: decoded.bytesReceived,
          });
        }
        const display = live.player?.video?.out?.display?.peek?.();
        if (display) state.display = display;
        state.effectiveTrack = live.sourceOut?.track?.peek?.() ?? null;
        state.effectiveConfig = live.sourceOut?.config?.peek?.() ?? null;
        state.injectedProbe = probe.peek();
        state.targetValue = target.peek() ?? null;
        state.sessionRefreshes = refreshes;
        state.probeIsEmptyFromPeer =
          Object.keys(live.session?.probe?.peek?.() ?? {}).length === 0;

        // Compatibility watchdog: a rendition change that renders nothing is not
        // a switch. Bounded refresh, cooldown, and every decision recorded.
        if (pending) {
          const honoured = paints.some(
            (paint) => paint.tMs >= pending.atMs && `${paint.width}x${paint.height}` === pending.size,
          );
          if (honoured) {
            policy.push({
              tMs: since(),
              action: 'switch-honoured',
              track: pending.track,
              waitedMs: since() - pending.atMs,
            });
            pending = null;
          } else if (since() - pending.atMs > SWITCH_CONFIRM_MS) {
            const stalled = pending;
            if (refreshes < REFRESH_MAX) {
              pending = null;
              const refreshed = await refreshSession(`unhonoured-switch:${stalled.track}`);
              if (refreshed) {
                await new Promise((r) => setTimeout(r, REFRESH_COOLDOWN_MS));
                pending = { track: stalled.track, size: stalled.size, atMs: since(), tried: stalled.tried + 1 };
              }
            } else {
              policy.push({
                tMs: since(),
                action: 'switch-unhonoured-final',
                track: stalled.track,
                waitedMs: since() - stalled.atMs,
                refreshes,
              });
              pending = null;
            }
          }
        }
      } catch (error) {
        state.loopError = String(error?.message ?? error);
      }
    }, SAMPLE_MS);

    state.phase = 'playing';
    window.abr.close = async () => {
      clearInterval(loop);
      context.drawImage = originalDrawImage;
      await closeSession();
      if (leaseApi && lease) {
        await leaseApi.release(lease);
        leaseEvents.push({ action: 'revoked-ack', tMs: since(), generation: lease.generation });
        lease = null;
      }
      state.phase = 'closed';
    };
  } catch (error) {
    if (leaseApi && lease) {
      try {
        await closeSession();
        await leaseApi.release(lease);
        leaseEvents.push({ action: 'revoked-ack', tMs: since(), generation: lease.generation });
        lease = null;
      } catch { state.cleanupError = 'LEASE_CLEANUP_UNCONFIRMED'; }
    }
    state.phase = 'failed';
    state.error = 'CONNECT_FAILED';
    state.errorType = error?.constructor?.name;
    state.message = String(error?.message ?? error);
  }
};
