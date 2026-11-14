// Everything the browser sender listens to while one cast stream plays on one
// Cast session, behind one `detach()` so no listener or timer can outlive it:
//   * SESSION_STATE_CHANGED → SESSION_ENDED (CAF: `errorCode` null on a clean end);
//   * the custom namespace (the Conspect receiver's reports; `ping` asks for one);
//   * the media status (`chrome.cast.media.Media` update listener: CAF calls it
//     with `isAlive`, false for EVERY idle state, so read the properties);
//   * the session volume (VOLUME_CHANGED);
//   * a poll of `GET /cast/streams/{id}` (fetch counters + the last report the
//     receiver POSTed; the only state source with Google's default receiver).
import type { CastStream } from '../api/castStreams';
import { OperationApiError } from '../api/operations';
import { parseReceiverReport } from './castReport';
import type { ReceiverReport } from './castReport';
import type {
  CastContext,
  CastMessageListener,
  CastSession,
  CastSessionStateEventData,
  CastVolumeEventData,
  ChromeCastMedia,
} from './castTypes';

export interface WatchHandlers {
  readonly report: (report: ReceiverReport) => void;
  readonly media: (playerState: string, idleReason: string | null) => void;
  readonly volume: (level: number | null, muted: boolean | null) => void;
  readonly stream: (doc: CastStream) => void;
  /** The stream is gone on the server (expired or deleted elsewhere). */
  readonly streamGone: () => void;
  readonly ended: (errorCode: string | null) => void;
}

export interface WatchApi {
  readonly getStream: (id: string) => Promise<CastStream>;
}

export class SessionWatch {
  private detached = false;
  private stateHandler: ((event: CastSessionStateEventData) => void) | null = null;
  private messageListener: CastMessageListener | null = null;
  private volumeHandler: ((event: CastVolumeEventData) => void) | null = null;
  private media: ChromeCastMedia | null = null;
  private mediaListener: (() => void) | null = null;
  private timer: ReturnType<typeof setInterval> | null = null;
  private lastReportAt = 0;

  constructor(
    private readonly context: CastContext,
    readonly session: CastSession,
    readonly streamId: string,
    private readonly namespace: string,
    private readonly api: WatchApi,
    private readonly handlers: WatchHandlers,
    private readonly pollMs: number,
  ) {}

  /** Attach the session-level listeners and start the poll. */
  start(): void {
    const framework = window.cast?.framework;
    const stateType = framework?.CastContextEventType.SESSION_STATE_CHANGED;
    if (stateType !== undefined) {
      const handler = (event: CastSessionStateEventData): void => {
        if (!this.detached && event.sessionState === 'SESSION_ENDED') {
          this.handlers.ended(typeof event.errorCode === 'string' && event.errorCode !== '' ? event.errorCode : null);
        }
      };
      this.stateHandler = handler;
      this.context.addEventListener(stateType, handler);
    }
    const message: CastMessageListener = (_namespace, data) => {
      const report = parseReceiverReport(data);
      if (!this.detached && report !== null) this.handlers.report(report);
    };
    this.messageListener = message;
    this.session.addMessageListener(this.namespace, message);
    const volumeType = framework?.SessionEventType.VOLUME_CHANGED;
    if (volumeType !== undefined) {
      const volume = (): void => {
        if (!this.detached) this.handlers.volume(this.session.getVolume(), this.session.isMute());
      };
      this.volumeHandler = volume;
      this.session.addEventListener(volumeType, volume);
    }
    this.handlers.volume(this.session.getVolume(), this.session.isMute());
    this.timer = setInterval(() => {
      void this.poll();
    }, this.pollMs);
  }

  /** Ask the Conspect receiver for a report now (ignored by other receivers). */
  ping(): void {
    this.session.sendMessage(this.namespace, { type: 'ping' }).catch(() => undefined);
  }

  /** Listen to the media the last LOAD created (call after `loadMedia` resolves). */
  attachMedia(): void {
    this.detachMedia();
    if (this.detached) return;
    const media = this.session.getMediaSession();
    this.media = media;
    if (media === null) return;
    const listener = (): void => {
      if (!this.detached) this.handlers.media(media.playerState ?? 'IDLE', media.idleReason ?? null);
    };
    this.mediaListener = listener;
    media.addUpdateListener(listener);
  }

  private detachMedia(): void {
    if (this.media !== null && this.mediaListener !== null) {
      this.media.removeUpdateListener(this.mediaListener);
    }
    this.media = null;
    this.mediaListener = null;
  }

  private async poll(): Promise<void> {
    try {
      const doc = await this.api.getStream(this.streamId);
      if (this.detached) return;
      this.handlers.stream(doc);
      const last = doc.last_report;
      if (last !== undefined && last !== null && last.received_unix_ms > this.lastReportAt) {
        this.lastReportAt = last.received_unix_ms;
        const report = parseReceiverReport(last.report);
        if (report !== null) this.handlers.report(report);
      }
    } catch (error) {
      if (!this.detached && error instanceof OperationApiError && error.status === 404) {
        this.handlers.streamGone();
      }
      // Anything else: best effort, the next poll tries again.
    }
  }

  /** Remove every listener and the poll; idempotent. */
  detach(): void {
    if (this.detached) return;
    this.detached = true;
    this.detachMedia();
    if (this.timer !== null) clearInterval(this.timer);
    this.timer = null;
    const framework = window.cast?.framework;
    const stateType = framework?.CastContextEventType.SESSION_STATE_CHANGED;
    if (stateType !== undefined && this.stateHandler !== null) {
      this.context.removeEventListener(stateType, this.stateHandler);
    }
    this.stateHandler = null;
    if (this.messageListener !== null) {
      this.session.removeMessageListener(this.namespace, this.messageListener);
    }
    this.messageListener = null;
    const volumeType = framework?.SessionEventType.VOLUME_CHANGED;
    if (volumeType !== undefined && this.volumeHandler !== null) {
      this.session.removeEventListener(volumeType, this.volumeHandler);
    }
    this.volumeHandler = null;
  }
}
