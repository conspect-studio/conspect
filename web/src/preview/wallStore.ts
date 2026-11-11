// Shared wall store (issue #250): one lease, one session, one decoder, latest frame.
//
// A single provider owns ONE wall MoQ lease (one WebTransport session, one VideoDecoder),
// keeps the latest decoded frame, and notifies canvas subscribers per frame without any
// React re-render. Each surface falls back to its own still when the wall is not live.
import { startMoqPlaybackWithRefresh } from './moqPlayback';
import { createWallLeaseApi, fetchWall, type WallGeometry } from './wallApi';

export const WALL_POLL_MS = 2000;
export const WALL_STALE_MS = 2000;
export const WALL_BACKOFF_MS: readonly number[] = [5000, 10000, 30000];

export interface WallStats {
  frames: number;
  lastFrameAt: number;
  sessions: number;
  decoders: number;
  gapsOver500ms: number;
  maxGapMs: number;
  /** Last session failure, for diagnostics only (never shown as an operator error). */
  lastError?: string | undefined;
}

declare global {
  interface Window { __wallStats?: WallStats }
}

export type WallPlay = typeof startMoqPlaybackWithRefresh;
export interface WallDeps {
  readonly fetchWall: typeof fetchWall;
  readonly play: WallPlay;
  readonly createLeaseApi: typeof createWallLeaseApi;
}
const DEFAULT_DEPS: WallDeps = { fetchWall, play: startMoqPlaybackWithRefresh, createLeaseApi: createWallLeaseApi };

export interface WallSnapshot {
  /** Geometry is only exposed while frames are flowing; undefined means "use stills". */
  readonly geometry: WallGeometry | undefined;
  /** Why surfaces show stills (undefined while live or still connecting). */
  readonly reason?: string | undefined;
}

export function isAborted(s: AbortSignal): boolean { return s.aborted; }

function sleep(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    if (signal.aborted) { resolve(); return; }
    const done = (): void => { clearTimeout(timer); signal.removeEventListener('abort', done); resolve(); };
    const timer = setTimeout(done, ms);
    signal.addEventListener('abort', done, { once: true });
  });
}

export class WallStore {
  private snapshot: WallSnapshot = { geometry: undefined };
  private geometry: WallGeometry | undefined;
  private frame: VideoFrame | undefined;
  private lastFrameAt: number | undefined;
  private readonly frameListeners = new Set<(frame: VideoFrame) => void>();
  private readonly stateListeners = new Set<() => void>();
  private root: AbortController | undefined;
  private session: AbortController | undefined;
  private wake: (() => void) | undefined;
  private watch: ReturnType<typeof setInterval> | undefined;
  private failures = 0;
  private reason: string | undefined;
  readonly stats: WallStats = { frames: 0, lastFrameAt: 0, sessions: 0, decoders: 0, gapsOver500ms: 0, maxGapMs: 0 };

  constructor(private readonly deps: WallDeps = DEFAULT_DEPS) {}

  getSnapshot = (): WallSnapshot => this.snapshot;
  subscribeState = (listener: () => void): (() => void) => {
    this.stateListeners.add(listener);
    return () => { this.stateListeners.delete(listener); };
  };
  /** Per-frame callback; also invoked once with the current frame, if any. */
  subscribeFrames(listener: (frame: VideoFrame) => void): () => void {
    this.frameListeners.add(listener);
    if (this.frame) listener(this.frame);
    return () => { this.frameListeners.delete(listener); };
  }

  start(): void {
    if (this.root) return;
    const root = new AbortController();
    this.root = root;
    window.__wallStats = this.stats;
    this.watch = setInterval(() => { this.publish(); }, 500);
    void this.pollLoop(root.signal);
    void this.playLoop(root.signal);
  }

  stop(): void {
    this.root?.abort();
    this.root = undefined;
    this.session?.abort();
    if (this.watch !== undefined) clearInterval(this.watch);
    this.watch = undefined;
    this.frame?.close();
    this.frame = undefined;
    this.geometry = undefined;
    this.lastFrameAt = undefined;
    this.publish();
  }

  /** Re-derive the snapshot: live only with geometry and a frame no older than WALL_STALE_MS. */
  private publish(): void {
    const live = this.geometry !== undefined && this.lastFrameAt !== undefined && performance.now() - this.lastFrameAt <= WALL_STALE_MS;
    const geometry = live ? this.geometry : undefined;
    const reason = live ? undefined : this.reason;
    if (geometry === this.snapshot.geometry && reason === this.snapshot.reason) return;
    this.snapshot = { geometry, reason };
    for (const listener of [...this.stateListeners]) listener();
  }

  private onFrame = (incoming: VideoFrame): void => {
    // A frame from a session of another layout generation is never shown.
    if (this.playingGeneration === undefined || this.playingGeneration !== this.geometry?.generation) return;
    const now = performance.now();
    if (this.lastFrameAt !== undefined) {
      const gap = now - this.lastFrameAt;
      if (gap > 500) this.stats.gapsOver500ms += 1;
      if (gap > this.stats.maxGapMs) this.stats.maxGapMs = gap;
    }
    this.lastFrameAt = now;
    this.reason = undefined;
    this.stats.frames += 1;
    this.stats.lastFrameAt = Date.now();
    this.failures = 0;
    // The library owns `incoming`; keep our own reference and close the previous one.
    const next = incoming.clone();
    const previous = this.frame;
    this.frame = next;
    previous?.close();
    this.publish();
    for (const listener of [...this.frameListeners]) listener(next);
  };

  private dropFrame(): void {
    this.frame?.close();
    this.frame = undefined;
    this.lastFrameAt = undefined;
  }

  private async pollLoop(signal: AbortSignal): Promise<void> {
    let unavailable = 0;
    while (!signal.aborted) {
      try {
        const next = await this.deps.fetchWall(signal);
        if (isAborted(signal)) return;
        unavailable = 0;
        if (this.reason?.startsWith('wall unavailable')) this.reason = undefined;
        if (this.geometry !== undefined && next.generation !== this.geometry.generation) {
          // New layout: never crop a frame of the old layout with new cells. Drop
          // the old frame and session first; surfaces show stills until a frame
          // of the new session arrives.
          this.session?.abort();
          this.dropFrame();
        }
        this.geometry = next;
        this.wake?.();
        this.publish();
      } catch (error) {
        if (isAborted(signal)) return;
        this.reason = error instanceof Error ? error.message : 'wall unavailable';
        // Wall unavailable (off, scoped, starting): stills, and back off so a
        // server without a wall is not polled every two seconds forever.
        this.geometry = undefined;
        this.session?.abort();
        this.dropFrame();
        this.publish();
        unavailable += 1;
      }
      const backoff = unavailable === 0 ? WALL_POLL_MS : WALL_BACKOFF_MS[Math.min(unavailable - 1, WALL_BACKOFF_MS.length - 1)] ?? 30000;
      await sleep(backoff, signal);
    }
  }

  private playingGeneration: number | undefined;

  private isAborted(c: AbortController): boolean { return c.signal.aborted; }
  private currentGeometry(): WallGeometry | undefined { return this.geometry; }

  private async playLoop(signal: AbortSignal): Promise<void> {
    while (!signal.aborted) {
      const geometry = this.currentGeometry();
      if (!geometry) {
        await new Promise<void>((resolve) => {
          this.wake = resolve;
          signal.addEventListener('abort', () => { resolve(); }, { once: true });
        });
        this.wake = undefined;
        continue;
      }
      const session = new AbortController();
      this.session = session;
      this.playingGeneration = geometry.generation;
      const aborted = AbortSignal.any([signal, session.signal]);
      let failed = false;
      try {
        await this.deps.play({
          api: this.deps.createLeaseApi(), canvas: document.createElement('canvas'), signal: aborted, audio: false,
          onActive: () => { this.stats.sessions += 1; this.stats.decoders += 1; },
          onFrame: this.onFrame,
        });
      } catch (error) {
        failed = !isAborted(aborted);
        if (failed) {
          this.stats.lastError = error instanceof Error ? `${error.name}: ${error.message}` : String(error);
          this.reason = `live stream failed (${this.stats.lastError})`;
        }
      }
      this.session = undefined;
      this.playingGeneration = undefined;
      if (isAborted(signal)) return;
      if (failed || !this.isAborted(session)) {
        // Session failed or ended on its own: bounded backoff, never a hot loop.
        const delay = WALL_BACKOFF_MS[Math.min(this.failures, WALL_BACKOFF_MS.length - 1)] ?? 30000;
        this.failures += 1;
        this.geometry = undefined;
        this.publish();
        await sleep(delay, signal);
        this.wake?.();
      }
      // A generation change (session aborted by the poller) reopens immediately.
    }
  }
}

