import { Connection, Origin, Path } from '@moq/net';
import { Signal } from '@moq/signals';
import type { Player, Video } from '@moq/watch';
import { connectMoq, ownMoqSession } from './moqConnection';
import { AbrSampler, abrSnapshot, authorizeRenditions, type AbrPersistedState, type AbrSource, type AbrState } from './moqAbr';

import type { components } from '../api/types';
export type MoqLease = components['schemas']['MoqLease'];
export interface MoqLeaseApi {
  acquire(signal: AbortSignal): Promise<MoqLease>;
  release(lease: MoqLease): Promise<void>;
}
export class MoqUnavailable extends Error {}
/** A local presentation watchdog expired; a fresh authorized session may recover it. */
export class MoqPresentationGap extends Error {}
/** The server's lease deadline elapsed; renewal still requires an acknowledged release. */
export class MoqLeaseExpired extends Error {}

/** Serialize fresh leases only after the previous playback's close and release ACK finish. */
export async function startMoqPlaybackWithRefresh(options: Parameters<typeof startMoqPlayback>[0]): Promise<void> {
  let resume = options.resume;
  let refreshes = 0;
  const presentation = { advancedAt: performance.now() };
  // Lease renewal is not a failed compatibility attempt. Abort or a failed ACK still stops it.
  for (;;) {
    try {
      await startMoqPlayback({ ...options, resume, presentation, onAbr: (state) => {
        resume = abrSnapshot(state);
        options.onAbr?.(state);
      } });
      return;
    } catch (error) {
      // Authorization, capacity, codec, arbitrary transport failure and release failure are
      // not retryable. The finally in startMoqPlayback must finish before this branch runs.
      if (options.signal.aborted) throw error;
      if (error instanceof MoqLeaseExpired) continue;
      if (!(error instanceof MoqPresentationGap) || refreshes >= 4) throw error;
      const delayMs = Math.min(1000 * 2 ** refreshes, 8000);
      refreshes += 1;
      await new Promise<void>((resolve, reject) => {
        const signal = options.signal;
        const abort = (): void => { clearTimeout(timer); reject(signal.reason instanceof Error ? signal.reason : new DOMException('MoQ playback aborted', 'AbortError')); };
        const timer = setTimeout(() => { signal.removeEventListener('abort', abort); resolve(); },
          delayMs);
        signal.addEventListener('abort', abort, { once: true });
        if (signal.aborted) abort();
      });
    }
  }
}

function description(value?: string | null): Uint8Array<ArrayBuffer> | undefined {
  return value ? Uint8Array.from(atob(value), (c) => c.charCodeAt(0)) : undefined;
}
export async function moqSupported(lease?: MoqLease, audio = false): Promise<boolean> {
  if (!Connection.isWebTransportSupported() || typeof VideoDecoder === 'undefined') return false;
  if (!lease) return true;
  const video = lease.video;
  const extra = description(video.description);
  if (!(await VideoDecoder.isConfigSupported({ codec: video.codec, codedWidth: video.coded_width,
    codedHeight: video.coded_height, ...(extra ? { description: extra } : {}) })).supported) return false;
  if (audio && lease.audio.length) {
    if (typeof AudioDecoder === 'undefined' || typeof AudioEncoder === 'undefined') return false;
    for (const track of lease.audio) {
      const extra = description(track.description);
      if (!(await AudioDecoder.isConfigSupported({ codec: track.codec, sampleRate: track.sample_rate,
        numberOfChannels: track.number_of_channels, ...(extra ? { description: extra } : {}) })).supported) return false;
    }
  }
  return true;
}

/**
 * Bind the ABR policy's inputs to the pinned APIs, nothing else.
 *
 * `stats()` is the browser's own `WebTransport.getStats()` snapshot (the measured arrival
 * counter); `videoStats()` is the video decoder's cumulative encoded bytes/decoded frames;
 * `selection()` is the pinned selector's verdict. The player is read lazily because the
 * source is constructed before the first sample and the player closes on teardown.
 */
function abrSource(session: Connection.Established, player: () => Player | undefined, canvas: HTMLCanvasElement): { source: AbrSource; close: () => void } {
  // Per-consumer render observation: this player's own decoded-output signal, not a global
  // instrument on the page's WebCodecs and not the received-payload counter in `out.stats`.
  const presented = { frames: 0, timestamp: -Infinity };
  const unsubscribe = player()?.video.out.frame.subscribe((frame) => {
    if (frame !== undefined && Number.isFinite(frame.timestamp) && frame.timestamp > presented.timestamp) {
      presented.timestamp = frame.timestamp;
      presented.frames += 1;
      canvas.dataset.moqFrames = String(presented.frames);
      canvas.dataset.moqWidth = String(frame.displayWidth);
      canvas.dataset.moqHeight = String(frame.displayHeight);
      canvas.dataset.effectiveTransport = 'moq-webtransport';
    }
  });
  const source: AbrSource = {
    stats: () => session.stats(),
    probe: () => session.probe.peek(),
    catalog: () => player()?.broadcast.out.catalog.peek()?.video,
    available: () => player()?.video.source.out.available.peek(),
    unsupported: () => player()?.video.source.out.error.peek() === 'unsupported',
    videoBytes: () => player()?.video.out.stats.peek()?.bytesReceived,
    presentedFrames: () => presented.frames,
    presentedFrame: () => {
      const frame = player()?.video.out.frame.peek();
      return frame === undefined ? undefined : { width: frame.displayWidth, height: frame.displayHeight };
    },
    stalled: () => player()?.video.out.stalled.peek() ?? false,
    bufferedMs: () => {
      const ranges = player()?.video.out.buffered.peek();
      const first = ranges?.[0];
      const last = ranges?.[ranges.length - 1];
      return first !== undefined && last !== undefined ? Number(last.end) - Number(first.start) : undefined;
    },
    selection: () => ({
      track: player()?.video.source.out.track.peek(),
      config: player()?.video.source.out.config.peek(),
    }),
  };
  return { source, close: () => { unsubscribe?.(); } };
}

/**
 * One owner, one QUIC session, no library reconnect or websocket fallback.
 *
 * The player's `probe` input is wired to the live session's PROBE estimates (the pinned
 * `Established.probe` getter) so `Sync` can size its jitter buffer from the peer-measured
 * RTT, and its `target` input is driven by the per-consumer ABR policy. The ABR always pins
 * an explicit target, so `Video.Source`'s probe-driven auto-selection — which would read the
 * peer's estimate of our own uplink — can never choose a rendition.
 */
export async function startMoqPlayback(options: {
  api: MoqLeaseApi; canvas: HTMLCanvasElement; signal: AbortSignal; audio: boolean;
  onActive: () => void; onAbr?: ((state: AbrState) => void) | undefined;
  /** Adaptation carried from the session this one replaces (see {@link abrSnapshot}). */
  resume?: AbrPersistedState | undefined;
  /** One consumer's progress deadline, retained across lease renewal. */
  presentation?: { advancedAt: number } | undefined;
}): Promise<void> {
  const { api, canvas, signal, audio, onActive } = options;
  const presentation = options.presentation ?? { advancedAt: performance.now() };
  if (!await moqSupported()) throw new MoqUnavailable('MoQ transport unavailable');
  signal.throwIfAborted();
  // Do not cancel an in-flight allocation: its response is needed to release it.
  const acquireStartedAt = performance.now();
  const lease = await api.acquire(AbortSignal.timeout(10000));
  // Conservatively debit the entire request time from the server-relative grant. Wall-clock
  // skew must never extend authorization. expires_at remains audit metadata, not a timer.
  const leaseDeadline = acquireStartedAt + lease.expires_in_ms;
  let owned: ReturnType<typeof ownMoqSession> | undefined;
  let player: Player | undefined;
  let expiry: ReturnType<typeof setTimeout> | undefined;
  let watchdog: ReturnType<typeof setInterval> | undefined;
  let presentationGap = false;
  let leaseExpired = false;
  let abr: AbrSampler | undefined;
  let abrBinding: ReturnType<typeof abrSource> | undefined;
  // `Player.in.target` is read reactively; the ABR policy writes this signal and the pinned
  // `Video.Source` resolves it against the real catalog, support probe and stalled flag.
  const abrTarget = new Signal<Video.Target | undefined>(undefined);
  const origin = new Origin.Producer();
  try {
    signal.throwIfAborted();
    if (!await moqSupported(lease, audio)) throw new MoqUnavailable('MoQ codec unavailable');
    const remaining = leaseDeadline - performance.now();
    if (!Number.isFinite(remaining) || remaining <= 0 || remaining > 2147483647) throw new Error('Invalid MoQ lease expiry');
    const session = await connectMoq(new URL(lease.url), AbortSignal.any([signal, AbortSignal.timeout(10000)]), origin);
    owned = ownMoqSession(session, signal, () => api.release(lease));
    if (session.transport !== 'webtransport') throw new Error('Unexpected MoQ transport');
    signal.throwIfAborted();
    if (performance.now() >= leaseDeadline) {
      if (performance.now() - presentation.advancedAt >= 10000) throw new MoqPresentationGap('MoQ presentation watchdog expired');
      throw new MoqLeaseExpired('MoQ lease expired during connection');
    }
    const renewalMarginMs = Math.min(5000, lease.expires_in_ms * 0.1);
    expiry = setTimeout(() => {
      presentationGap = performance.now() - presentation.advancedAt >= 10000;
      leaseExpired = !presentationGap;
      void owned?.close().catch(() => { /* release failure remains observable by owner */ });
    }, Math.max(0, leaseDeadline - renewalMarginMs - performance.now()));
    const { Player } = await import('@moq/watch');
    signal.throwIfAborted();
    player = new Player({ origin, name: Path.from(lease.broadcast), canvas, enabled: true,
      announced: false, paused: false, muted: !audio, volume: 1, visible: 'always',
      probe: session.probe, target: abrTarget });
    abrBinding = abrSource(session, () => player, canvas);
    abr = new AbrSampler({
      target: abrTarget,
      authorized: lease.video_alternatives.length > 0
        ? authorizeRenditions(lease.video_alternatives.map((grant) => ({
          track: grant.track, codec: grant.codec, codedWidth: grant.coded_width,
          codedHeight: grant.coded_height, description: grant.description,
          bitrateBps: grant.bitrate_bps, cadenceNumerator: grant.cadence_numerator,
          cadenceDenominator: grant.cadence_denominator,
        })))
        : { codec: lease.video.codec, codedWidth: lease.video.coded_width, codedHeight: lease.video.coded_height },
      ...(options.onAbr ? { onState: options.onAbr } : {}),
      ...(options.resume ? { resume: options.resume } : {}),
      source: abrBinding.source,
    });
    abr.start();
    let lastPresented = 0;
    watchdog = setInterval(() => {
      const count = abrBinding?.source.presentedFrames() ?? 0;
      if (count > lastPresented) {
        lastPresented = count;
        presentation.advancedAt = performance.now();
      } else if (performance.now() - presentation.advancedAt >= 10000) {
        presentationGap = true;
        void owned?.close().catch(() => { /* owner observes release failure */ });
      }
    }, 1000);
    onActive();
    // Read through functions so callback mutations remain visible to flow analysis.
    const expired = (): boolean => leaseExpired;
    const gap = (): boolean => presentationGap;
    await session.closed;
    if (!signal.aborted) {
      if (expired()) throw new MoqLeaseExpired('MoQ lease expired');
      if (gap()) throw new MoqPresentationGap('MoQ presentation watchdog expired');
      throw new Error('MoQ session ended');
    }
  } catch (error) {
    if (!signal.aborted) throw error;
  } finally {
    if (expiry !== undefined) clearTimeout(expiry);
    if (watchdog !== undefined) clearInterval(watchdog);
    abr?.close();
    abrBinding?.close();
    player?.close();
    origin.close();
    // A failed close or release must prevent fallback and any replacement lease.
    if (owned) await owned.close();
    else await api.release(lease);
  }
}
