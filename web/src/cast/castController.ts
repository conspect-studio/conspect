// The browser-cast state machine (framework-agnostic — no React here). One
// instance lives for the app's lifetime (see `CastProvider.tsx`) so a cast
// survives page navigation, matching the Cast SDK's own one-session-per-tab
// model: starting a new target while already casting replaces the loaded
// media in the SAME Cast session rather than opening a second one.
//
// PR 1 scope: there is no browser-cast backend endpoint yet. This casts an
// EXISTING HLS output's own public playlist URL directly
// (`window.location.origin + path`, where `path` is one of the `/hls/...`
// mounts from `hlsPlaybackPaths` — already CORS-enabled for the Cast receiver
// origin). There is no POST/renew/DELETE call; "stop" only ends the Cast
// session.
//
// States: idle → connecting (requestSession / device picker) → loading
// (loadMedia) → playing; error (with a sentence). A user-cancelled picker
// returns to idle with no error. The session/media lifecycle (SESSION_ENDED,
// or media IDLE with FINISHED/ERROR) tears down and returns to idle/error on
// its own.
import { t } from '@lingui/core/macro';

import { loadCastSdk } from './sdkLoader';
import type {
  CastError,
  CastSession,
  CastSessionStateEventData,
  ChromeCastMedia,
} from './castTypes';

export type CastStatus = 'connecting' | 'loading' | 'playing' | 'error';

/** One castable target: a stable key (for "is this the active target"), a
 * display name, and the HLS playlist path to cast. */
export interface CastTarget {
  readonly key: string;
  readonly name: string;
  readonly path: string;
}

export interface CastSnapshot {
  readonly status: CastStatus | 'idle';
  readonly targetKey: string | null;
  readonly targetName: string | null;
  readonly deviceName: string | null;
  readonly errorText: string | null;
}

const IDLE_SNAPSHOT: CastSnapshot = {
  status: 'idle',
  targetKey: null,
  targetName: null,
  deviceName: null,
  errorText: null,
};

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

function isCastErrorLike(value: unknown): value is CastError {
  return isRecord(value) && typeof value.code === 'string';
}

/**
 * The CAF sender's promises (`requestSession`, `loadMedia`) reject with a bare
 * `chrome.cast.ErrorCode` string (e.g. `"cancel"`); the older callback API
 * passes a `chrome.cast.Error` object. Normalise both to the code.
 */
function errorCode(error: unknown): string | null {
  if (typeof error === 'string') {
    return error;
  }
  return isCastErrorLike(error) ? error.code : null;
}

function isUserCancellation(error: unknown): boolean {
  return errorCode(error) === 'cancel';
}

/** A human-actionable sentence for a failed connect/load. */
export function describeCastError(error: unknown): string {
  const code = errorCode(error);
  if (code !== null) {
    switch (code) {
      case 'session_error':
        return t`Cast session error.`;
      case 'timeout':
        return t`The Cast request timed out.`;
      case 'receiver_unavailable':
        return t`No Cast receiver is available on this network.`;
      case 'load_media_failed':
        return t`The Cast receiver could not load the media.`;
      case 'media_error':
        return t`The Cast receiver could not play this stream. Check that the address uses HTTPS with a valid certificate.`;
      default:
        return (isCastErrorLike(error) ? error.description : undefined) ?? t`Could not cast to this device.`;
    }
  }
  return error instanceof Error ? error.message : t`Could not cast to this device.`;
}

/** The framework-agnostic browser-cast controller; one per tab (see `CastProvider`). */
export class CastController {
  private snapshot: CastSnapshot = IDLE_SNAPSHOT;
  private readonly listeners = new Set<() => void>();
  private session: CastSession | null = null;
  private mediaSession: ChromeCastMedia | null = null;
  private mediaListener: ((isAlive?: boolean) => void) | null = null;
  private sessionStateHandler: ((event: CastSessionStateEventData) => void) | null = null;
  /** Bumped by every start/stop: a stale async step never overwrites newer state. */
  private generation = 0;
  /** Set while our own `stop()` ends the session, so its SESSION_ENDED is ours. */
  private stopping = false;
  /** Set by `dispose()` until the provider mounts again (`activate()`). */
  private disposed = false;

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

  /** Whether a connect or load is in flight (a new start is ignored then). */
  get busy(): boolean {
    return this.snapshot.status === 'connecting' || this.snapshot.status === 'loading' || this.stopping;
  }

  /**
   * Start casting `target`'s HLS playlist. If already playing, replaces the
   * loaded media in the same Cast session rather than opening a second one.
   * Ignored while another connect/load/stop is in flight.
   */
  async start(target: CastTarget): Promise<void> {
    if (this.busy || this.disposed) {
      return;
    }
    const generation = ++this.generation;
    const current = (): boolean => generation === this.generation;
    const replacing = this.snapshot.status === 'playing' && this.session !== null;
    this.emit({
      status: replacing ? 'loading' : 'connecting',
      targetKey: target.key,
      targetName: target.name,
      errorText: null,
    });
    try {
      let session = this.session;
      if (!replacing) {
        const context = await loadCastSdk();
        session = context.getCurrentSession();
        if (session === null) {
          try {
            await context.requestSession();
          } catch (error) {
            if (isUserCancellation(error)) {
              if (current()) {
                this.emit(IDLE_SNAPSHOT);
              }
              return;
            }
            throw error;
          }
          session = context.getCurrentSession();
        }
        if (session === null) {
          throw new Error('No Cast session is available after requestSession.');
        }
        if (!current()) {
          return;
        }
        this.session = session;
        this.attachSessionListeners(context, session);
      }

      const activeSession = session;
      if (activeSession === null) {
        throw new Error('No Cast session is available.');
      }
      this.emit({ status: 'loading' });
      await this.loadMedia(activeSession, target);
      if (!current()) {
        return;
      }
      this.emit({
        status: 'playing',
        deviceName: activeSession.getCastDevice?.()?.friendlyName ?? null,
      });
    } catch (error) {
      if (!current()) {
        return;
      }
      if (!replacing) {
        this.detachSessionListeners();
        this.session = null;
      }
      this.emit({ status: 'error', errorText: describeCastError(error) });
    }
  }

  /** Operator-initiated stop: ends the Cast session (the TV stops too). */
  async stop(): Promise<void> {
    await this.end(null);
  }

  /** End the session; afterwards show `errorText`, or idle when `null`. */
  private async end(errorText: string | null): Promise<void> {
    this.generation += 1;
    const session = this.session;
    const mediaSession = this.mediaSession;
    this.stopping = true;
    this.detachMediaListener();
    try {
      if (mediaSession !== null) {
        await new Promise<void>((resolve) => {
          mediaSession.stop(
            {},
            () => {
              resolve();
            },
            () => {
              resolve();
            },
          );
        });
      }
      session?.endSession(true);
    } finally {
      this.detachSessionListeners();
      this.session = null;
      this.stopping = false;
    }
    this.emit(errorText === null ? IDLE_SNAPSHOT : { ...IDLE_SNAPSHOT, status: 'error', errorText });
  }

  /**
   * Provider mount: (re-)arm the controller. React StrictMode keeps the same
   * controller across its dev unmount/re-mount, so `dispose()` is not final.
   */
  activate(): void {
    this.disposed = false;
  }

  /**
   * Provider teardown (the app unmounts, or React StrictMode's dev
   * re-mount): invalidate any in-flight start so it can never attach
   * listeners or load media on behalf of a dead controller, and end an
   * active cast rather than leaving the TV playing with no owner here.
   */
  dispose(): void {
    this.disposed = true;
    this.generation += 1;
    if (this.session !== null) {
      void this.stop();
    } else {
      this.detachSessionListeners();
    }
  }

  private async loadMedia(session: CastSession, target: CastTarget): Promise<void> {
    const chromeCast = window.chrome?.cast;
    if (chromeCast === undefined) {
      throw new Error('chrome.cast is unavailable.');
    }
    const url = `${window.location.origin}${target.path}`;
    const media = new chromeCast.media.MediaInfo(url, 'application/vnd.apple.mpegurl');
    media.streamType = chromeCast.media.StreamType.LIVE;
    media.hlsSegmentFormat = chromeCast.media.HlsSegmentFormat.TS;
    media.hlsVideoSegmentFormat = chromeCast.media.HlsVideoSegmentFormat.MPEG2_TS;
    const metadata = new chromeCast.media.GenericMediaMetadata();
    metadata.metadataType = chromeCast.media.MetadataType.GENERIC;
    metadata.title = target.name;
    media.metadata = metadata;
    const request = new chromeCast.media.LoadRequest(media);
    // A replacing LOAD makes the old media report IDLE/INTERRUPTED; detach
    // its listener first so that is never read as "the cast ended".
    this.detachMediaListener();
    // CAF resolves on success and rejects with a chrome.cast.ErrorCode string.
    await session.loadMedia(request);
    this.attachMediaListener(session);
  }

  private attachMediaListener(session: CastSession): void {
    this.detachMediaListener();
    const mediaSession = session.getMediaSession();
    this.mediaSession = mediaSession;
    if (mediaSession === null) {
      return;
    }
    // CAF calls this as listener(isAlive), with isAlive false for EVERY idle
    // state; only the idle reason says why. CANCELLED/INTERRUPTED (a stop or
    // a replacing load from any sender) are not failures of this cast, so
    // only ERROR and FINISHED end it here; a session that really ends is
    // reported by SESSION_ENDED.
    const listener = (): void => {
      if (mediaSession.playerState !== 'IDLE') {
        return;
      }
      if (mediaSession.idleReason === 'ERROR') {
        void this.end(describeCastError('media_error'));
      } else if (mediaSession.idleReason === 'FINISHED') {
        void this.end(null);
      }
    };
    this.mediaListener = listener;
    mediaSession.addUpdateListener(listener);
  }

  private detachMediaListener(): void {
    if (this.mediaSession !== null && this.mediaListener !== null) {
      this.mediaSession.removeUpdateListener(this.mediaListener);
    }
    this.mediaSession = null;
    this.mediaListener = null;
  }

  private attachSessionListeners(
    context: { addEventListener: (eventType: string, handler: (event: CastSessionStateEventData) => void) => void },
    session: CastSession,
  ): void {
    this.detachSessionListeners();
    const eventType = window.cast?.framework.CastContextEventType.SESSION_STATE_CHANGED;
    if (eventType === undefined) {
      return;
    }
    const handler = (event: CastSessionStateEventData): void => {
      if (event.sessionState === 'SESSION_ENDED') {
        this.handleSessionEnded(event.errorCode);
      }
    };
    this.sessionStateHandler = handler;
    context.addEventListener(eventType, handler);
    this.session = session;
  }

  private detachSessionListeners(): void {
    this.detachMediaListener();
    const eventType = window.cast?.framework.CastContextEventType.SESSION_STATE_CHANGED;
    if (eventType !== undefined && this.sessionStateHandler !== null) {
      // The session object owns no removeEventListener; the CastContext does.
      window.cast?.framework.CastContext.getInstance().removeEventListener(eventType, this.sessionStateHandler);
    }
    this.sessionStateHandler = null;
  }

  private handleSessionEnded(errorCode: string | null | undefined): void {
    // Our own stop() provoked this event: it already decides the end state.
    if (this.stopping) {
      return;
    }
    this.generation += 1;
    this.detachSessionListeners();
    this.session = null;
    // CAF sets errorCode to null on a clean end (TV stopped, other sender).
    if (typeof errorCode === 'string' && errorCode !== '') {
      this.emit({ ...IDLE_SNAPSHOT, status: 'error', errorText: describeCastError(errorCode) });
    } else {
      this.emit(IDLE_SNAPSHOT);
    }
  }
}
