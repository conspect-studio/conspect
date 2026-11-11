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
/** The server refused the MoQ lease; `reason` is its typed problem (e.g. `moq-unavailable`, `moq-capacity`). */
export class MoqLeaseRefused extends Error {
  constructor(readonly reason: string, readonly status: number) {
    super(`MoQ lease refused: ${reason}`);
  }
}
/** A local presentation watchdog expired; a fresh authorized session may recover it. */
export class MoqPresentationGap extends Error {}
/** The server's lease deadline elapsed; renewal still requires an acknowledged release. */
export class MoqLeaseExpired extends Error {}
/**
 * The session closed but the server did not acknowledge the lease release, so
 * the lease may still be held until its deadline. Every other playback error
 * means no lease is held.
 */
export class MoqReleaseUnacknowledged extends Error {
  constructor(cause: unknown) {
    super(`MoQ lease release not acknowledged: ${cause instanceof Error ? cause.message : 'unknown'}`, { cause });
  }
}
async function acknowledged(release: Promise<void>): Promise<void> {
  try {
    await release;
  } catch (error) {
    throw new MoqReleaseUnacknowledged(error);
  }
}

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
function abrSource(session: Connection.Established, player: () => Player | undefined, canvas: HTMLCanvasElement | undefined, frameBase = 0): { source: AbrSource; close: () => void; firstFrame: Promise<void>; publish: (to: HTMLCanvasElement | undefined, base: number) => void; total: () => number } {
  // Per-consumer render observation: this player's own decoded-output signal, not a global
  // instrument on the page's WebCodecs and not the received-payload counter in `out.stats`.
  const presented = { frames: 0, timestamp: -Infinity };
  // `publishTo` is the visible canvas once this session is the one on screen; a session still
  // connecting during a renewal observes frames but must not write the visible canvas's dataset.
  let publishTo = canvas;
  let base = frameBase;
  let last: { width: number; height: number } | undefined;
  let seen!: () => void;
  const firstFrame = new Promise<void>((resolve) => { seen = resolve; });
  const write = (): void => {
    if (!publishTo || !last) return;
    publishTo.dataset.moqFrames = String(base + presented.frames);
    publishTo.dataset.moqWidth = String(last.width);
    publishTo.dataset.moqHeight = String(last.height);
    publishTo.dataset.effectiveTransport = 'moq-webtransport';
  };
  const unsubscribe = player()?.video.out.frame.subscribe((frame) => {
    if (frame !== undefined && Number.isFinite(frame.timestamp) && frame.timestamp > presented.timestamp) {
      presented.timestamp = frame.timestamp;
      presented.frames += 1;
      last = { width: frame.displayWidth, height: frame.displayHeight };
      write();
      seen();
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
  return { source, firstFrame, total: () => base + presented.frames,
    close: () => { unsubscribe?.(); },
    publish: (to, offset) => { publishTo = to; base = offset; write(); } };
}

/**
 * Renewal lead: the next lease is acquired this long before the current one's deadline so
 * that acquire + connect + the first keyframe (GOP about 1 s on the Program output) finish
 * while the current session still plays. Capped to 40 % of a short TTL.
 */
export const MOQ_RENEWAL_LEAD_MS = 8000;
/** Pause between failed renewal attempts. */
const RENEWAL_RETRY_MS = 1000;
/** Do not start a renewal attempt with less than this left on the current lease. */
const RENEWAL_MIN_WINDOW_MS = 1500;

/** One lease + its WebTransport session + the player decoding it. */
interface Leg {
  lease: MoqLease;
  deadline: number;
  owned: ReturnType<typeof ownMoqSession>;
  session: Connection.Established;
  player: Player;
  origin: Origin.Producer;
  abr: AbrSampler;
  binding: ReturnType<typeof abrSource>;
  canvas: Signal<HTMLCanvasElement | undefined>;
  muted: Signal<boolean>;
  /** `live` is set once this leg is the one on screen; it gates its ABR reports. */
  flags: { live: boolean };
  expired: boolean;
  gap: boolean;
  timer?: ReturnType<typeof setTimeout> | undefined;
}

function sleep(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise<void>((resolve) => {
    const done = (): void => { clearTimeout(timer); signal.removeEventListener('abort', done); resolve(); };
    const timer = setTimeout(done, ms);
    signal.addEventListener('abort', done, { once: true });
    if (signal.aborted) done();
  });
}

/**
 * One playback = a chain of legs on one canvas.
 *
 * Make-before-break renewal: `MOQ_RENEWAL_LEAD_MS` before the current lease's deadline the
 * next lease is acquired and its session connected, decoding into a hidden canvas while the
 * current session keeps painting. At the new session's first decoded frame the pinned player's
 * `canvas` and `muted` inputs (signals) are swapped, so exactly one player paints and at most
 * one is audible; only then is the old session closed and its lease released.
 *
 * A renewal failure never ends a working session: the attempt's own lease is released and the
 * attempt is retried while time remains; the current session plays on to its deadline and
 * only then the existing `MoqLeaseExpired` path applies. At most two leases are held, only
 * during the overlap, and every lease acquired is released exactly once (abort included).
 *
 * An unacknowledged release of a REPLACED lease does not stop the new session. The old lease
 * is already at or near its server-side deadline and the server removes it itself then; it is
 * remembered, and if its deadline is still in the future when the whole playback ends, the end
 * reports `MoqReleaseUnacknowledged` so the caller keeps its hold. The last lease's release
 * behaves as before.
 *
 * One owner per session, no library reconnect or websocket fallback. Each player's `probe`
 * is the live session's PROBE estimate, its `target` is driven by the per-consumer ABR policy
 * (which always pins an explicit target, so probe-driven auto-selection can never choose a
 * rendition). ABR state and the presentation watchdog carry across the switch.
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
  // Aborted by the owner only to cancel a renewal; every leg also follows the caller's signal.
  const inner = new AbortController();
  const legSignal = AbortSignal.any([signal, inner.signal]);
  let resume = options.resume;
  const st: { active: Leg | undefined } = { active: undefined };
  let renewal: Promise<void> | undefined;
  let watchdog: ReturnType<typeof setInterval> | undefined;
  let renewalTimer: ReturnType<typeof setTimeout> | undefined;
  const cancelled = (): boolean => legSignal.aborted;
  const pendingCloses: Promise<void>[] = [];
  // Deadline (performance.now() clock) of the latest REPLACED lease whose release failed.
  let heldUntil = -Infinity;

  /** Acquire, connect, build the player. On failure everything acquired is released first. */
  const openLeg = async (target: HTMLCanvasElement | undefined, visible: boolean): Promise<Leg> => {
    // The first leg's failure is the caller's error as before; only a renewal attempt's
    // unacknowledged release is remembered as a possibly-held extra lease.
    const renewing = !visible;
    // Do not cancel an in-flight allocation: its response is needed to release it.
    const acquireStartedAt = performance.now();
    const lease = await api.acquire(AbortSignal.timeout(10000));
    // Conservatively debit the entire request time from the server-relative grant. Wall-clock
    // skew must never extend authorization. expires_at remains audit metadata, not a timer.
    const deadline = acquireStartedAt + lease.expires_in_ms;
    let owned: ReturnType<typeof ownMoqSession> | undefined;
    let player: Player | undefined;
    let abr: AbrSampler | undefined;
    let binding: ReturnType<typeof abrSource> | undefined;
    const origin = new Origin.Producer();
    try {
      legSignal.throwIfAborted();
      if (!await moqSupported(lease, audio)) throw new MoqUnavailable('MoQ codec unavailable');
      const remaining = deadline - performance.now();
      if (!Number.isFinite(remaining) || remaining <= 0 || remaining > 2147483647) throw new Error('Invalid MoQ lease expiry');
      const session = await connectMoq(new URL(lease.url), AbortSignal.any([legSignal, AbortSignal.timeout(10000)]), origin);
      owned = ownMoqSession(session, legSignal, () => api.release(lease));
      if (session.transport !== 'webtransport') throw new Error('Unexpected MoQ transport');
      legSignal.throwIfAborted();
      if (performance.now() >= deadline) {
        if (performance.now() - presentation.advancedAt >= 10000) throw new MoqPresentationGap('MoQ presentation watchdog expired');
        throw new MoqLeaseExpired('MoQ lease expired during connection');
      }
      const { Player } = await import('@moq/watch');
      legSignal.throwIfAborted();
      // `Player.in.target` is read reactively; the ABR policy writes this signal and the pinned
      // `Video.Source` resolves it against the real catalog, support probe and stalled flag.
      const abrTarget = new Signal<Video.Target | undefined>(undefined);
      const canvasInput = new Signal<HTMLCanvasElement | undefined>(target);
      // A session that is not on screen yet is silent.
      const muted = new Signal<boolean>(!audio || !visible);
      player = new Player({ origin, name: Path.from(lease.broadcast), canvas: canvasInput, enabled: true,
        announced: false, paused: false, muted, volume: 1, visible: 'always',
        probe: session.probe, target: abrTarget });
      const livePlayer = player;
      binding = abrSource(session, () => livePlayer, visible ? canvas : undefined);
      const flags = { live: visible };
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
        onState: (state) => {
          if (!flags.live) return;
          resume = abrSnapshot(state);
          options.onAbr?.(state);
        },
        ...(resume ? { resume } : {}),
        source: binding.source,
      });
      abr.start();
      return { lease, deadline, owned, session, player, origin, abr, binding, canvas: canvasInput, muted, flags,
        expired: false, gap: false };
    } catch (error) {
      abr?.close();
      binding?.close();
      player?.close();
      origin.close();
      // A failed close or release must prevent fallback and any replacement lease. Remember the
      // lease's true deadline so no further lease is opened while it may still be held.
      try {
        await acknowledged(owned ? owned.close() : api.release(lease));
      } catch (unacknowledged) {
        if (renewing) heldUntil = Math.max(heldUntil, deadline);
        throw unacknowledged;
      }
      throw error;
    }
  };

  /** Stop a leg's timers, player and session, then release its lease (resolves once ACKed). */
  const stopLeg = (leg: Leg): Promise<void> => {
    if (leg.timer !== undefined) clearTimeout(leg.timer);
    leg.abr.close();
    leg.binding.close();
    leg.player.close();
    leg.origin.close();
    return acknowledged(leg.owned.close());
  };
  /** Close a leg off the critical path; a failed release only marks the lease as maybe held. */
  const retireLeg = (leg: Leg): void => {
    pendingCloses.push(stopLeg(leg).catch(() => {
      // The lease expires on the server at its own deadline (already past or nearly past at a
      // renewal); it must not stop the session that replaced it, but it is not forgotten.
      heldUntil = Math.max(heldUntil, leg.deadline);
    }));
  };

  /** Arm the end-of-lease stop for the leg that is on screen. */
  const arm = (leg: Leg): void => {
    const margin = Math.min(5000, leg.lease.expires_in_ms * 0.1);
    leg.timer = setTimeout(() => {
      leg.gap = performance.now() - presentation.advancedAt >= 10000;
      leg.expired = !leg.gap;
      void leg.owned.close().catch(() => { /* release failure remains observable by owner */ });
    }, Math.max(0, leg.deadline - margin - performance.now()));
    const lead = Math.min(MOQ_RENEWAL_LEAD_MS, leg.lease.expires_in_ms * 0.4);
    renewalTimer = setTimeout(() => {
      renewal = renew(leg).catch(() => { /* a renewal never ends a working session */ });
    }, Math.max(0, leg.deadline - lead - performance.now()));
  };

  const switchTo = (next: Leg): void => {
    const old = st.active;
    if (!old) { retireLeg(next); return; }
    // Silence and detach the old player first, then bring the new one up: one painter, one voice.
    old.flags.live = false;
    old.muted.set(true);
    old.canvas.set(undefined);
    next.flags.live = true;
    next.binding.publish(canvas, old.binding.total());
    next.canvas.set(canvas);
    next.muted.set(!audio);
    st.active = next;
    arm(next);
    retireLeg(old);
  };

  /** Acquire and connect the next leg until one decodes a frame or the current lease runs out. */
  const renew = async (cur: Leg): Promise<void> => {
    const hidden = document.createElement('canvas');
    // The old session is stopped (with a small margin) before its deadline; stay inside that.
    const latest = cur.deadline - Math.min(5000, cur.lease.expires_in_ms * 0.1);
    while (!cancelled() && st.active === cur && heldUntil <= performance.now() && latest - performance.now() > RENEWAL_MIN_WINDOW_MS) {
      const attempt: { leg: Leg | undefined } = { leg: undefined };
      try {
        const next = await openLeg(hidden, false);
        attempt.leg = next;
        const closedFlag = { value: false };
        const ready = new Promise<boolean>((resolve) => {
          void next.binding.firstFrame.then(() => { resolve(true); });
          void next.session.closed.then(() => { closedFlag.value = true; resolve(false); }, () => { closedFlag.value = true; resolve(false); });
          void sleep(Math.max(0, latest - performance.now()), legSignal).then(() => { resolve(false); });
        });
        if (await ready && !closedFlag.value && !cancelled() && st.active === cur && !cur.expired && !cur.gap) {
          switchTo(next);
          return;
        }
        throw new Error('MoQ renewal session did not present');
      } catch {
        if (attempt.leg) {
          // A connected attempt that did not present: release it; remember it if not ACKed.
          const failed = attempt.leg;
          await stopLeg(failed).catch(() => { heldUntil = Math.max(heldUntil, failed.deadline); });
        }
        if (cancelled()) return;
        await sleep(RENEWAL_RETRY_MS, legSignal);
      }
    }
  };

  let failure: Error | undefined;
  try {
    st.active = await openLeg(canvas, true);
    // Everything above this point releases its own lease on failure; from here the finally owns it.
    arm(st.active);
    let lastPresented = 0;
    let watched: Leg | undefined;
    watchdog = setInterval(() => {
      const current = st.active;
      if (!current) return;
      if (watched !== current) { watched = current; lastPresented = 0; }
      const count = current.binding.source.presentedFrames() ?? 0;
      if (count > lastPresented) {
        lastPresented = count;
        presentation.advancedAt = performance.now();
      } else if (performance.now() - presentation.advancedAt >= 10000) {
        current.gap = true;
        void current.owned.close().catch(() => { /* owner observes release failure */ });
      }
    }, 1000);
    onActive();
    for (;;) {
      const current: Leg = st.active;
      await current.session.closed.catch(() => undefined);
      if (st.active !== current) continue; // switched: wait on the new session
      if (!signal.aborted) {
        if (current.expired) throw new MoqLeaseExpired('MoQ lease expired');
        if (current.gap) throw new MoqPresentationGap('MoQ presentation watchdog expired');
        throw new Error('MoQ session ended');
      }
      break;
    }
  } catch (error) {
    if (!signal.aborted) failure = error instanceof Error ? error : new Error('MoQ playback failed');
  }
  {
    if (watchdog !== undefined) clearInterval(watchdog);
    if (renewalTimer !== undefined) clearTimeout(renewalTimer);
    inner.abort();
    // Let an in-flight renewal release its own lease before the last lease is released.
    await renewal;
    const last = st.active;
    st.active = undefined;
    let lastRelease: Error | undefined;
    if (last) await stopLeg(last).catch((error: unknown) => { lastRelease = error instanceof Error ? error : new Error('MoQ release failed'); });
    await Promise.all(pendingCloses);
    // A failed close or release must prevent fallback and any replacement lease.
    if (lastRelease !== undefined) throw lastRelease;
    if (heldUntil > performance.now()) throw new MoqReleaseUnacknowledged(new Error('replaced lease may still be held'));
  }
  if (failure !== undefined) throw failure;
}
