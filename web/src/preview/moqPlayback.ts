import { Connection, Origin, Path } from '@moq/net';
import type { Player } from '@moq/watch';
import { connectMoq, ownMoqSession } from './moqConnection';

import type { components } from '../api/schema';
export type MoqLease = components['schemas']['MoqLease'];
export interface MoqLeaseApi {
  acquire(signal: AbortSignal): Promise<MoqLease>;
  release(lease: MoqLease): Promise<void>;
}
export class MoqUnavailable extends Error {}

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

/** One owner, one QUIC session, no library reconnect or websocket fallback. */
export async function startMoqPlayback(options: {
  api: MoqLeaseApi; canvas: HTMLCanvasElement; signal: AbortSignal; audio: boolean;
  onActive: () => void;
}): Promise<void> {
  const { api, canvas, signal, audio, onActive } = options;
  if (!await moqSupported()) throw new MoqUnavailable('MoQ transport unavailable');
  signal.throwIfAborted();
  // Do not cancel an in-flight allocation: its response is needed to release it.
  const lease = await api.acquire(AbortSignal.timeout(10000));
  let owned: ReturnType<typeof ownMoqSession> | undefined;
  let player: Player | undefined;
  let expiry: ReturnType<typeof setTimeout> | undefined;
  let watchdog: ReturnType<typeof setInterval> | undefined;
  const origin = new Origin.Producer();
  try {
    signal.throwIfAborted();
    if (!await moqSupported(lease, audio)) throw new MoqUnavailable('MoQ codec unavailable');
    const remaining = Date.parse(lease.expires_at) - Date.now();
    if (!Number.isFinite(remaining) || remaining <= 0 || remaining > 2147483647) throw new Error('Invalid MoQ lease expiry');
    const session = await connectMoq(new URL(lease.url), AbortSignal.any([signal, AbortSignal.timeout(10000)]), origin);
    owned = ownMoqSession(session, signal, () => api.release(lease));
    if (session.transport !== 'webtransport') throw new Error('Unexpected MoQ transport');
    signal.throwIfAborted();
    expiry = setTimeout(() => { void owned?.close().catch(() => { /* release failure remains observable by owner */ }); }, remaining);
    const { Player } = await import('@moq/watch');
    signal.throwIfAborted();
    player = new Player({ origin, name: Path.from(lease.broadcast), canvas, enabled: true,
      announced: false, paused: false, muted: !audio, volume: 1, visible: 'always' });
    let lastTimestamp: number | undefined;
    let advancedAt = Date.now();
    watchdog = setInterval(() => {
      const timestamp = player?.renderer.out.timestamp.peek();
      if (timestamp !== undefined && timestamp !== lastTimestamp) {
        lastTimestamp = timestamp;
        advancedAt = Date.now();
      } else if (Date.now() - advancedAt > 10000) {
        void owned?.close().catch(() => { /* owner observes release failure */ });
      }
    }, 1000);
    onActive();
    await session.closed;
    if (!signal.aborted) throw new Error('MoQ session ended');
  } catch (error) {
    if (!signal.aborted) throw error;
  } finally {
    if (expiry !== undefined) clearTimeout(expiry);
    if (watchdog !== undefined) clearInterval(watchdog);
    player?.close();
    origin.close();
    // A failed close or release must prevent fallback and any replacement lease.
    if (owned) await owned.close();
    else await api.release(lease);
  }
}
