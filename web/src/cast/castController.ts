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

  /**
   * Start casting `target`'s HLS playlist. If already playing, replaces the
   * loaded media in the same Cast session rather than opening a second one.
   */
  async start(target: CastTarget): Promise<void> {
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
              this.detachSessionListeners();
              this.emit(IDLE_SNAPSHOT);
              return;
            }
            throw error;
          }
          session = context.getCurrentSession();
        }
        if (session === null) {
          throw new Error('No Cast session is available after requestSession.');
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

      this.emit({
        status: 'playing',
        deviceName: activeSession.getCastDevice?.()?.friendlyName ?? null,
      });
    } catch (error) {
      if (!replacing) {
        this.detachSessionListeners();
        this.session = null;
      }
      this.emit({ status: 'error', errorText: describeCastError(error) });
    }
  }

  /** Operator-initiated stop: ends the Cast session. */
  async stop(): Promise<void> {
    const session = this.session;
    const mediaSession = this.mediaSession;
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
    this.detachSessionListeners();
    this.session = null;
    this.emit(IDLE_SNAPSHOT);
  }

  /** Release resources on provider teardown (test/HMR hygiene); never calls the network. */
  dispose(): void {
    this.detachSessionListeners();
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
    await session.loadMedia(request);
    this.attachMediaListeners(session);
  }

  private attachMediaListeners(session: CastSession): void {
    const previous = this.mediaSession;
    if (previous !== null && this.mediaListener !== null) {
      previous.removeUpdateListener(this.mediaListener);
    }
    const mediaSession = session.getMediaSession();
    this.mediaSession = mediaSession;
    if (mediaSession === null) {
      this.mediaListener = null;
      return;
    }
    const listener = (isAlive?: boolean): void => {
      // `chrome.cast.media.Media` carries its state as properties; the update
      // listener receives `isAlive` (false once the media session is gone).
      const { playerState, idleReason } = mediaSession;
      if (playerState === 'IDLE' && idleReason === 'ERROR') {
        void this.stop().then(() => {
          this.emit({
            status: 'error',
            errorText: describeCastError('media_error'),
          });
        });
      } else if ((playerState === 'IDLE' && idleReason === 'FINISHED') || isAlive === false) {
        void this.stop();
      }
    };
    this.mediaListener = listener;
    mediaSession.addUpdateListener(listener);
  }

  private attachSessionListeners(
    context: { addEventListener: (eventType: string, handler: (event: CastSessionStateEventData) => void) => void },
    session: CastSession,
  ): void {
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
    if (this.mediaSession !== null && this.mediaListener !== null) {
      this.mediaSession.removeUpdateListener(this.mediaListener);
    }
    this.mediaSession = null;
    this.mediaListener = null;
    const eventType = window.cast?.framework.CastContextEventType.SESSION_STATE_CHANGED;
    if (eventType !== undefined && this.sessionStateHandler !== null) {
      // The session object owns no removeEventListener; the CastContext does.
      window.cast?.framework.CastContext.getInstance().removeEventListener(eventType, this.sessionStateHandler);
    }
    this.sessionStateHandler = null;
  }

  private handleSessionEnded(errorCode: string | undefined): void {
    this.session = null;
    this.mediaSession = null;
    this.mediaListener = null;
    this.sessionStateHandler = null;
    if (errorCode !== undefined && errorCode !== '') {
      this.emit({ status: 'error', errorText: describeCastError({ code: errorCode }) });
    } else {
      this.emit(IDLE_SNAPSHOT);
    }
  }
}
