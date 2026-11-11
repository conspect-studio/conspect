// Shared wall store (issue #250): one lease, one session, one decoder, latest frame.
//
// A single provider owns ONE wall MoQ lease (one WebTransport session, one VideoDecoder),
// keeps the latest decoded frame, and notifies canvas subscribers per frame without any
// React re-render. Each surface falls back to its own still when the wall is not live.
import { startMoqPlaybackWithRefresh } from './moqPlayback';
import { createWallLeaseApi, fetchWall, type WallGeometry } from './wallApi';
import { playWallWhep, type WallWhepOptions } from './wallWhep';

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
  readonly playWhep?: (options: WallWhepOptions) => Promise<void>;
  readonly createLeaseApi: typeof createWallLeaseApi;
}
const DEFAULT_DEPS: WallDeps = { fetchWall, play: startMoqPlaybackWithRefresh, playWhep: playWallWhep, createLeaseApi: createWallLeaseApi };

export interface WallSnapshot {
  /** Geometry is only exposed while frames are flowing; undefined means "use stills". */
  readonly geometry: WallGeometry | undefined;
  /** Why surfaces show stills (undefined while live or still connecting). */
  readonly reason?: string | undefined;
  readonly transport: 'moq' | 'webrtc' | 'stills';
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
  private snapshot: WallSnapshot = { geometry: undefined, transport: 'stills' };
  private geometry: WallGeometry | undefined;
  /**
   * The current wall picture, drawn ONCE per decoded frame. Every surface crops
   * from this canvas. Drawing a `VideoFrame` converts the whole frame each time
   * (measured: about 8 ms per call in a browser without a GPU, whatever the crop),
   * so thirteen crops straight from the frame cost thirteen conversions.
   */
  private picture: HTMLCanvasElement | undefined;
  private pictureContext: CanvasRenderingContext2D | undefined;
  private hasPicture = false;
  private lastFrameAt: number | undefined;
  private readonly frameListeners = new Set<(picture: HTMLCanvasElement) => void>();
  private readonly stateListeners = new Set<() => void>();
  private root: AbortController | undefined;
  private session: AbortController | undefined;
  private wake: (() => void) | undefined;
  private watch: ReturnType<typeof setInterval> | undefined;
  private failures = 0;
  private reason: string | undefined;
  private transport: 'moq' | 'webrtc' | 'stills' = 'stills';
  private moqFailed = false;
  private moqReason: string | undefined;
  readonly stats: WallStats = { frames: 0, lastFrameAt: 0, sessions: 0, decoders: 0, gapsOver500ms: 0, maxGapMs: 0 };

  constructor(private readonly deps: WallDeps = DEFAULT_DEPS) {}

  getSnapshot = (): WallSnapshot => this.snapshot;
  subscribeState = (listener: () => void): (() => void) => {
    this.stateListeners.add(listener);
    return () => { this.stateListeners.delete(listener); };
  };
  /** Per-frame callback with the current wall picture; also invoked once now, if there is one. */
  subscribeFrames(listener: (picture: HTMLCanvasElement) => void): () => void {
    this.frameListeners.add(listener);
    if (this.hasPicture && this.picture) listener(this.picture);
    return () => { this.frameListeners.delete(listener); };
  }

  /** Convert the decoded frame once. False when no 2D canvas is available. */
  private paint(frame: VideoFrame | HTMLVideoElement): boolean {
    const picture = this.picture ?? (this.picture = document.createElement('canvas'));
    const width = frame instanceof HTMLVideoElement ? frame.videoWidth : frame.displayWidth;
    const height = frame instanceof HTMLVideoElement ? frame.videoHeight : frame.displayHeight;
    if (!width || !height) return false;
    // WHEP may downscale the wall to its preview encode cap. Restore the
    // authored geometry before subscribers crop in authored cell coordinates.
    const outputWidth = frame instanceof HTMLVideoElement ? this.geometry?.width ?? width : width;
    const outputHeight = frame instanceof HTMLVideoElement ? this.geometry?.height ?? height : height;
    if (picture.width !== outputWidth || picture.height !== outputHeight) {
      picture.width = outputWidth;
      picture.height = outputHeight;
      this.pictureContext = undefined;
    }
    const context = this.pictureContext ?? (this.pictureContext = picture.getContext('2d') ?? undefined);
    if (!context) return false;
    context.drawImage(frame, 0, 0, outputWidth, outputHeight);
    this.hasPicture = true;
    return true;
  }

  start(): void {
    if (this.root) return;
    const root = new AbortController();
    this.root = root;
    window.__wallStats = this.stats;
    this.watch = setInterval(() => { this.publish(); }, 500);
    window.addEventListener('pagehide', this.onPageHide);
    window.addEventListener('pageshow', this.onPageShow);
    void this.pollLoop(root.signal);
    void this.playLoop(root.signal);
  }

  private readonly onPageHide = (): void => { this.stop(); };
  private readonly onPageShow = (event: PageTransitionEvent): void => { if (event.persisted) this.start(); };

  stop(): void {
    window.removeEventListener('pagehide', this.onPageHide);
    this.root?.abort();
    this.root = undefined;
    this.session?.abort();
    if (this.watch !== undefined) clearInterval(this.watch);
    this.watch = undefined;
    this.hasPicture = false;
    this.geometry = undefined;
    this.lastFrameAt = undefined;
    this.transport = 'stills';
    this.publish();
  }

  /** Remove bfcache lifecycle listeners when the desk itself unmounts. */
  dispose(): void {
    this.stop();
    window.removeEventListener('pageshow', this.onPageShow);
  }

  /** Re-derive the snapshot: live only with geometry and a frame no older than WALL_STALE_MS. */
  private publish(): void {
    const live = this.geometry !== undefined && this.lastFrameAt !== undefined && performance.now() - this.lastFrameAt <= WALL_STALE_MS;
    const geometry = live ? this.geometry : undefined;
    const reason = live ? undefined : this.reason;
    const transport = geometry ? this.transport : 'stills';
    if (geometry === this.snapshot.geometry && reason === this.snapshot.reason && transport === this.snapshot.transport) return;
    this.snapshot = { geometry, reason, transport };
    for (const listener of [...this.stateListeners]) listener();
  }

  private onFrame = (incoming: VideoFrame | HTMLVideoElement): void => {
    // A frame from a session of another layout generation is never shown.
    if (this.playingGeneration === undefined || this.playingGeneration !== this.geometry?.generation) return;
    const now = performance.now();
    if (this.lastFrameAt !== undefined) {
      const gap = now - this.lastFrameAt;
      if (gap > 500) this.stats.gapsOver500ms += 1;
      if (gap > this.stats.maxGapMs) this.stats.maxGapMs = gap;
    }
    // The source owns the frame or video element: copy pixels synchronously.
    if (!this.paint(incoming)) return;
    this.lastFrameAt = now;
    this.reason = undefined;
    this.stats.frames += 1;
    this.stats.lastFrameAt = Date.now();
    this.failures = 0;
    this.publish();
    const picture = this.picture;
    if (picture) for (const listener of [...this.frameListeners]) listener(picture);
  };

  private dropFrame(): void {
    // A picture of another layout must never be cropped with the new geometry.
    this.hasPicture = false;
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
      const sessionFrame = (frame: VideoFrame | HTMLVideoElement): void => {
        if (!aborted.aborted && this.session === session) this.onFrame(frame);
      };
      let failed = false;
      const rung = this.moqFailed ? 'webrtc' : 'moq';
      this.transport = rung;
      try {
        if (rung === 'moq') {
          await this.deps.play({
            api: this.deps.createLeaseApi(), canvas: document.createElement('canvas'), signal: aborted, audio: false,
            onActive: () => { this.stats.sessions += 1; this.stats.decoders += 1; },
            onFrame: sessionFrame,
          });
        } else {
          await (this.deps.playWhep ?? playWallWhep)({
            signal: aborted, onFrame: sessionFrame,
            onActive: () => { this.stats.sessions += 1; this.stats.decoders += 1; },
          });
        }
      } catch (error) {
        failed = !isAborted(aborted);
        if (failed) {
          if (rung === 'moq') { this.moqFailed = true; this.moqReason = error instanceof Error ? error.message : String(error); }
          this.stats.lastError = error instanceof Error ? `${error.name}: ${error.message}` : String(error);
          this.reason = rung === 'moq' ? `MoQ failed (${this.stats.lastError})` : `${this.moqReason ? `MoQ failed (${this.moqReason}); ` : ''}WebRTC failed (${this.stats.lastError})`;
        }
      }
      this.session = undefined;
      this.playingGeneration = undefined;
      if (isAborted(signal)) return;
      if (failed || !this.isAborted(session)) {
        // First MoQ failure falls through immediately; later WHEP failures back off.
        const delay = rung === 'moq' && failed ? 0 : WALL_BACKOFF_MS[Math.min(this.failures++, WALL_BACKOFF_MS.length - 1)] ?? 30000;
        this.dropFrame();
        this.publish();
        if (delay) await sleep(delay, signal);
      }
      // A generation change (session aborted by the poller) reopens immediately.
    }
  }
}

