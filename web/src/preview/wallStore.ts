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
    if (geometry === this.snapshot.geometry) return;
    this.snapshot = { geometry };
    for (const listener of [...this.stateListeners]) listener();
  }

  private onFrame = (incoming: VideoFrame): void => {
    const now = performance.now();
    if (this.lastFrameAt !== undefined) {
      const gap = now - this.lastFrameAt;
      if (gap > 500) this.stats.gapsOver500ms += 1;
      if (gap > this.stats.maxGapMs) this.stats.maxGapMs = gap;
    }
    this.lastFrameAt = now;
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

  private async pollLoop(signal: AbortSignal): Promise<void> {
    while (!signal.aborted) {
      try {
        const next = await this.deps.fetchWall(signal);
        if (isAborted(signal)) return;
        const changed = this.isPlaying() && next.generation !== this.playingGeneration;
        this.geometry = next;
        this.wake?.();
        if (changed) this.session?.abort();
        this.publish();
      } catch {
        if (isAborted(signal)) return;
        // Wall unavailable: surfaces fall back to stills; the session loop is stopped.
        this.geometry = undefined;
        this.session?.abort();
        this.publish();
      }
      await sleep(WALL_POLL_MS, signal);
    }
  }

  private playingGeneration: number | undefined;
  private isPlaying(): boolean { return this.session !== undefined && this.playingGeneration !== undefined; }

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
      } catch {
        failed = !isAborted(aborted);
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

