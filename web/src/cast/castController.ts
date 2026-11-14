// The browser-cast state machine (framework-agnostic — no React here). One
// instance lives for the app's lifetime (see `CastProvider.tsx`) so a cast
// survives page navigation, matching the Cast SDK's own one-session-per-tab
// model.
//
// idle → preparing (POST /cast/streams) → connecting (picker, while the
// stream warms up, bounded 15 s) → loading (LOAD) → buffering ⇄ playing →
// stopping → idle; error {text, detail} from any step.
//
// Gesture: the Cast picker (`requestSession`) needs the click's transient user
// activation (Chrome: about 5 s). The SDK is preloaded when the page learns the
// receiver app id, the stream POST is bounded by the engine's 3 s take wait, and
// the picker opens as soon as the POST answers — BEFORE the ready poll — so a
// refused stream never opens the picker and a slow warm-up never eats the
// gesture. A cancelled picker deletes the stream.
//
// A closed tab does NOT end a cast (the receiver keeps the token alive);
// `dispose()` only detaches. Stop = `endSession(true)` + DELETE; a DELETE still
// in flight when the page is left is re-sent with `keepalive` on `pagehide`.
import { t } from '@lingui/core/macro';

import { createCastStream, deleteCastStream, getCastStream, listCastStreams, castRefusalSentence } from '../api/castStreams';
import type { CastStream } from '../api/castStreams';
import { OperationApiError } from '../api/operations';
import type { OutputVideoFeed } from '../api/outputRouting';
import { receiverErrorCodes, receiverErrorSentence } from './castReport';
import type { ReceiverReport } from './castReport';
import { buildLoadRequest, describeCastError, isUserCancellation, waitPlayable } from './castLoad';
import { SessionWatch } from './castSessionWatch';
import type { CastContext, CastSession } from './castTypes';
import { loadCastSdk } from './sdkLoader';

export { describeCastError } from './castLoad';

export type CastStatus =
  | 'preparing'
  | 'connecting'
  | 'loading'
  | 'buffering'
  | 'playing'
  | 'stopping'
  | 'error';

/** What to cast: a stable key, a display name, and the feed (or a cast output as it is). */
export interface CastTarget {
  readonly key: string;
  readonly name: string;
  readonly feed?: OutputVideoFeed;
  readonly output?: string;
}

/** Which receiver app the controller uses (from `GET /cast/config`). */
export interface CastReceiverConfig {
  /** The registered Conspect receiver; `null` = Google's Default Media Receiver. */
  readonly appId: string | null;
  readonly namespace: string;
}

export interface CastSnapshot {
  readonly status: CastStatus | 'idle';
  readonly targetKey: string | null;
  readonly targetName: string | null;
  readonly deviceName: string | null;
  readonly errorText: string | null;
  /** Receiver error codes or a server detail, shown under the sentence. */
  readonly errorDetail: string | null;
  readonly streamId: string | null;
  /** The cast output the stream plays from. */
  readonly output: string | null;
  /** `default` = Google's Default Media Receiver (no reports, no branded screen). */
  readonly receiver: 'conspect' | 'default' | null;
  readonly report: ReceiverReport | null;
  /** 0..1, `null` before the first volume status. */
  readonly volume: number | null;
  readonly muted: boolean | null;
  /** 10 s after LOAD the TV has fetched no playlist from this server. */
  readonly tvUnreachable: boolean;
}

export const IDLE_SNAPSHOT: CastSnapshot = {
  status: 'idle',
  targetKey: null,
  targetName: null,
  deviceName: null,
  errorText: null,
  errorDetail: null,
  streamId: null,
  output: null,
  receiver: null,
  report: null,
  volume: null,
  muted: null,
  tvUnreachable: false,
};

/** The API the controller calls (injectable for tests). */
export interface CastStreamApi {
  readonly create: typeof createCastStream;
  readonly get: typeof getCastStream;
  readonly list: typeof listCastStreams;
  readonly remove: typeof deleteCastStream;
}

const DEFAULT_API: CastStreamApi = {
  create: createCastStream,
  get: getCastStream,
  list: listCastStreams,
  remove: deleteCastStream,
};

/** How long after LOAD a TV that fetched nothing is reported as unable to reach us. */
export const TV_UNREACHABLE_MS = 10_000;
const POLL_MS = 2_000;
const STORAGE_KEY = 'conspect.cast.active';

interface StoredCast {
  readonly sessionId: string;
  readonly streamId: string;
  readonly key: string;
  readonly name: string;
}

function readStored(): StoredCast | null {
  try {
    const raw = window.localStorage.getItem(STORAGE_KEY);
    if (raw === null) return null;
    const value: unknown = JSON.parse(raw);
    if (
      typeof value === 'object' &&
      value !== null &&
      'sessionId' in value &&
      'streamId' in value &&
      'key' in value &&
      'name' in value &&
      typeof value.sessionId === 'string' &&
      typeof value.streamId === 'string' &&
      typeof value.key === 'string' &&
      typeof value.name === 'string'
    ) {
      return { sessionId: value.sessionId, streamId: value.streamId, key: value.key, name: value.name };
    }
  } catch {
    // Unreadable storage: nothing to restore.
  }
  return null;
}

function writeStored(value: StoredCast | null): void {
  try {
    if (value === null) window.localStorage.removeItem(STORAGE_KEY);
    else window.localStorage.setItem(STORAGE_KEY, JSON.stringify(value));
  } catch {
    // Storage is optional (private mode): re-attach just will not happen.
  }
}

/** The framework-agnostic browser-cast controller; one per tab (see `CastProvider`). */
export class CastController {
  private snapshot: CastSnapshot = IDLE_SNAPSHOT;
  private readonly listeners = new Set<() => void>();
  private receiver: CastReceiverConfig | null = null;
  private watch: SessionWatch | null = null;
  /** Bumped by every start/stop/dispose: a stale async step never overwrites newer state. */
  private generation = 0;
  private disposed = false;
  private loadedAt: number | null = null;
  /** A Stop's DELETE not yet answered: re-sent with keepalive on pagehide. */
  private pendingDelete: string | null = null;
  private readonly onPageHide = (): void => {
    if (this.pendingDelete !== null) {
      void this.api.remove(this.pendingDelete, { keepalive: true }).catch(() => undefined);
    }
  };

  constructor(
    private readonly api: CastStreamApi = DEFAULT_API,
    private readonly now: () => number = Date.now,
  ) {}

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return (): void => {
      this.listeners.delete(listener);
    };
  };

  getSnapshot = (): CastSnapshot => this.snapshot;

  private emit(partial: Partial<CastSnapshot>): void {
    this.snapshot = { ...this.snapshot, ...partial };
    this.listeners.forEach((listener) => {
      listener();
    });
  }

  /** Whether a start or stop is in flight (a new start is ignored then). */
  get busy(): boolean {
    const s = this.snapshot.status;
    return s === 'preparing' || s === 'connecting' || s === 'loading' || s === 'stopping';
  }

  /**
   * The page learned the receiver app (`GET /cast/config`): remember it,
   * preload the SDK (so the click's gesture is not spent loading it) and
   * re-attach to a cast this tab started before a reload.
   */
  configure(receiver: CastReceiverConfig): void {
    const changed = this.receiver?.appId !== receiver.appId || this.receiver.namespace !== receiver.namespace;
    this.receiver = receiver;
    if (!changed || this.disposed) return;
    void loadCastSdk(receiver.appId)
      .then(() => this.reattach())
      .catch(() => undefined);
  }

  /** The receiver the controller uses now (`null` before `configure`). */
  get receiverConfig(): CastReceiverConfig | null {
    return this.receiver;
  }

  /**
   * Cast `target`. While casting, the new stream replaces the old one in the
   * SAME Cast session (the old stream is deleted after the new LOAD).
   */
  async start(target: CastTarget): Promise<void> {
    const receiver = this.receiver;
    if (this.busy || this.disposed) return;
    if (receiver === null) {
      this.emit({ ...IDLE_SNAPSHOT, status: 'error', targetKey: target.key, targetName: target.name, errorText: t`The cast settings are still loading. Try again in a moment.` });
      return;
    }
    const generation = ++this.generation;
    const current = (): boolean => generation === this.generation && !this.disposed;
    const previous = this.watch;
    const before = this.snapshot;
    let swapped = false;
    const custom = receiver.appId !== null;
    this.emit({
      ...(previous === null ? IDLE_SNAPSHOT : this.snapshot),
      status: 'preparing',
      targetKey: target.key,
      targetName: target.name,
      errorText: null,
      errorDetail: null,
      receiver: custom ? 'conspect' : 'default',
      tvUnreachable: false,
    });
    let stream: CastStream | null = null;
    let openedSession: CastSession | null = null;
    const abandon = (): void => {
      if (stream !== null) void this.api.remove(stream.id).catch(() => undefined);
    };
    try {
      const context = await loadCastSdk(receiver.appId);
      if (!current()) return;
      try {
        stream = await this.api.create({
          ...(target.feed !== undefined ? { feed: target.feed } : {}),
          ...(target.output !== undefined ? { output: target.output } : {}),
        });
      } catch (error) {
        if (current() && previous !== null) {
          // The running cast is untouched: keep showing it, with the refusal.
          this.emit({ ...before, errorText: castRefusalSentence(error), errorDetail: null });
        } else if (current()) {
          this.emit({
            status: 'error',
            errorText: castRefusalSentence(error),
            errorDetail: error instanceof OperationApiError ? (error.detail ?? null) : null,
          });
        }
        return;
      }
      if (!current()) {
        abandon();
        return;
      }
      this.emit({ streamId: stream.id, output: stream.output });
      // The picker and the warm-up run together; LOAD waits for both.
      const created = stream;
      const ready = waitPlayable(created, custom, current, this.now, this.api.get);
      ready.catch(() => undefined);
      let session = previous?.session ?? context.getCurrentSession();
      if (session === null) {
        this.emit({ status: 'connecting' });
        try {
          await context.requestSession();
        } catch (error) {
          if (isUserCancellation(error)) {
            abandon();
            if (current()) {
              this.generation += 1;
              this.emit(IDLE_SNAPSHOT);
            }
            return;
          }
          throw error;
        }
        session = context.getCurrentSession();
        if (session === null) throw new Error('No Cast session is available after requestSession.');
        openedSession = session;
      }
      if (!current()) {
        abandon();
        return;
      }
      this.emit({ status: 'connecting', deviceName: session.getCastDevice?.()?.friendlyName ?? null });
      const playable = await ready;
      if (!current()) {
        abandon();
        return;
      }
      this.emit({ status: 'loading' });
      swapped = true;
      previous?.detach();
      this.watch = null;
      const watch = this.makeWatch(context, session, playable.id, receiver.namespace);
      this.watch = watch;
      watch.start();
      await session.loadMedia(buildLoadRequest(playable, target.name));
      if (!current()) {
        watch.detach();
        if (this.watch === watch) this.watch = null;
        abandon();
        return;
      }
      watch.attachMedia();
      watch.ping();
      this.loadedAt = this.now();
      writeStored({ sessionId: session.sessionId ?? '', streamId: playable.id, key: target.key, name: target.name });
      if (previous !== null && previous.streamId !== playable.id) {
        void this.api.remove(previous.streamId).catch(() => undefined);
      }
      if (this.snapshot.status === 'loading') this.emit({ status: 'buffering' });
    } catch (error) {
      if (!current()) {
        abandon();
        return;
      }
      this.generation += 1;
      abandon();
      if (previous !== null && !swapped) {
        // The new stream never reached the TV: the old cast still plays.
        this.emit({ ...before, errorText: describeCastError(error), errorDetail: null });
        return;
      }
      this.watch?.detach();
      this.watch = null;
      if (previous !== null) {
        void this.api.remove(previous.streamId).catch(() => undefined);
      }
      // A session this start opened is ended; one it found is left alone.
      openedSession?.endSession(true);
      writeStored(null);
      this.emit({ ...IDLE_SNAPSHOT, status: 'error', targetKey: target.key, targetName: target.name, errorText: describeCastError(error) });
    }
  }

  private makeWatch(
    context: CastContext,
    session: CastSession,
    streamId: string,
    namespace: string,
  ): SessionWatch {
    const watch: SessionWatch = new SessionWatch(context, session, streamId, namespace, { getStream: this.api.get }, {
      report: (report) => {
        if (this.watch === watch) this.applyReport(report);
      },
      media: (playerState, idleReason) => {
        if (this.watch === watch) this.applyMedia(playerState, idleReason);
      },
      volume: (level, muted) => {
        if (this.watch === watch) this.emit({ volume: level, muted });
      },
      stream: (doc) => {
        if (this.watch === watch) this.applyStream(doc);
      },
      streamGone: () => {
        if (this.watch === watch) this.finish(watch, t`The cast stream ended on the server (expired or stopped elsewhere).`);
      },
      ended: (code) => {
        if (this.watch === watch) this.finish(watch, code === null ? null : describeCastError(code), false);
      },
    }, POLL_MS);
    return watch;
  }

  private applyReport(report: ReceiverReport): void {
    const busy = this.snapshot.status === 'stopping' || this.snapshot.status === 'preparing' || this.snapshot.status === 'connecting';
    if (busy) {
      this.emit({ report });
      return;
    }
    if (report.state === 'error') {
      this.emit({
        report,
        status: 'error',
        errorText: receiverErrorSentence(report.error),
        errorDetail: receiverErrorCodes(report.error),
      });
      return;
    }
    // `idle` before the receiver has the media: still loading.
    const status: CastStatus =
      report.state === 'playing' ? 'playing' : report.state === 'buffering' ? 'buffering' : 'loading';
    this.emit({
      report,
      status,
      ...(status === 'playing' ? { errorText: null, errorDetail: null, tvUnreachable: false } : {}),
    });
  }

  private applyMedia(playerState: string, idleReason: string | null): void {
    const s = this.snapshot.status;
    if (s === 'stopping' || s === 'preparing' || s === 'connecting') return;
    // With the Conspect receiver its reports are richer; media status only
    // fills in until the first report arrives.
    if (playerState === 'PLAYING' && this.snapshot.report === null) {
      this.emit({ status: 'playing', tvUnreachable: false });
    } else if (playerState === 'BUFFERING' && this.snapshot.report === null) {
      this.emit({ status: 'buffering' });
    } else if (playerState === 'IDLE' && idleReason === 'ERROR') {
      // The receiver gave up on this media. CANCELLED/INTERRUPTED (a stop or
      // a replacing LOAD) are not failures of this cast.
      const report = this.snapshot.report;
      this.emit({
        status: 'error',
        errorText: report?.state === 'error' ? receiverErrorSentence(report.error) : describeCastError('media_error'),
        errorDetail: report?.state === 'error' ? receiverErrorCodes(report.error) : null,
      });
    } else if (playerState === 'IDLE' && idleReason === 'FINISHED' && this.watch !== null) {
      this.finish(this.watch, null);
    }
  }

  private applyStream(doc: CastStream): void {
    const loadedAt = this.loadedAt;
    const unreachable =
      loadedAt !== null &&
      doc.fetches.playlists === 0 &&
      this.now() - loadedAt >= TV_UNREACHABLE_MS &&
      this.snapshot.status !== 'playing';
    if (unreachable !== this.snapshot.tvUnreachable) this.emit({ tvUnreachable: unreachable });
  }

  /** The cast ended (TV, server or receiver): detach, delete the stream, show `errorText` or idle. */
  private finish(watch: SessionWatch, errorText: string | null, endSession = true): void {
    this.generation += 1;
    watch.detach();
    this.watch = null;
    this.loadedAt = null;
    void this.api.remove(watch.streamId).catch(() => undefined);
    if (endSession) watch.session.endSession(true);
    writeStored(null);
    this.emit(errorText === null ? IDLE_SNAPSHOT : { ...IDLE_SNAPSHOT, status: 'error', targetKey: this.snapshot.targetKey, targetName: this.snapshot.targetName, errorText });
  }

  /** Operator Stop: end the Cast session (the TV stops) and delete the stream. */
  async stop(): Promise<void> {
    this.generation += 1;
    const watch = this.watch;
    const streamId = watch?.streamId ?? this.snapshot.streamId;
    this.watch = null;
    this.loadedAt = null;
    this.emit({ status: 'stopping' });
    watch?.detach();
    writeStored(null);
    try {
      watch?.session.endSession(true);
    } catch {
      // The session may already be gone; the DELETE still revokes the token.
    }
    if (streamId !== null) {
      this.pendingDelete = streamId;
      try {
        await this.api.remove(streamId);
      } catch {
        // Best effort: the token idles out within 60 s once the TV stops.
      } finally {
        if (this.pendingDelete === streamId) this.pendingDelete = null;
      }
    }
    if (this.snapshot.status === 'stopping') this.emit(IDLE_SNAPSHOT);
  }

  /** Clear an error shown with no cast running. */
  dismissError(): void {
    if (this.snapshot.status === 'error' && this.watch === null) this.emit(IDLE_SNAPSHOT);
    else if (this.snapshot.errorText !== null) this.emit({ errorText: null, errorDetail: null });
  }

  /** Receiver volume, 0..1. */
  async setVolume(level: number): Promise<void> {
    const session = this.watch?.session;
    if (session === undefined) return;
    const clamped = Math.min(1, Math.max(0, level));
    this.emit({ volume: clamped });
    await session.setVolume(clamped).catch(() => undefined);
  }

  async setMuted(muted: boolean): Promise<void> {
    const session = this.watch?.session;
    if (session === undefined) return;
    this.emit({ muted });
    await session.setMute(muted).catch(() => undefined);
  }

  /**
   * Re-attach after a reload: the SDK auto-joins the tab's Cast session
   * (origin-scoped); if this tab's stream is still listed, show it again.
   */
  async reattach(): Promise<void> {
    const receiver = this.receiver;
    const stored = readStored();
    if (receiver === null || stored === null || this.disposed || this.snapshot.status !== 'idle') return;
    const generation = ++this.generation;
    const current = (): boolean => generation === this.generation && !this.disposed;
    try {
      const context = await loadCastSdk(receiver.appId);
      const session = context.getCurrentSession();
      const streams = await this.api.list();
      // A start may have begun while the list was in flight.
      if (!current() || this.getSnapshot().status !== 'idle') return;
      const stream = streams.find((s) => s.id === stored.streamId);
      if (session === null || stream === undefined) {
        if (stream === undefined) writeStored(null);
        return;
      }
      const watch = this.makeWatch(context, session, stream.id, receiver.namespace);
      this.watch = watch;
      this.loadedAt = null;
      this.emit({
        ...IDLE_SNAPSHOT,
        status: 'buffering',
        targetKey: stored.key,
        targetName: stored.name,
        deviceName: session.getCastDevice?.()?.friendlyName ?? null,
        streamId: stream.id,
        output: stream.output,
        receiver: receiver.appId === null ? 'default' : 'conspect',
      });
      watch.start();
      watch.attachMedia();
      watch.ping();
    } catch {
      // Nothing to restore.
    }
  }

  /** Provider mount: (re-)arm the controller (StrictMode keeps the same instance). */
  activate(): void {
    this.disposed = false;
    window.addEventListener('pagehide', this.onPageHide);
    const receiver = this.receiver;
    if (receiver !== null && this.snapshot.status === 'idle') void this.reattach();
  }

  /**
   * Provider teardown (app unmount, or StrictMode's dev re-mount): invalidate
   * in-flight work and detach every listener. The cast itself keeps playing
   * (a closed tab must not end it); `activate()` re-attaches.
   */
  dispose(): void {
    this.disposed = true;
    this.generation += 1;
    window.removeEventListener('pagehide', this.onPageHide);
    this.watch?.detach();
    this.watch = null;
    this.loadedAt = null;
    this.emit(IDLE_SNAPSHOT);
  }
}
