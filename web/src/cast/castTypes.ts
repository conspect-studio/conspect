// Minimal hand-written TypeScript declarations for the parts of the Google
// Cast Web Sender SDK (`cast.framework` / `chrome.cast`) this module actually
// uses. The SDK itself is loaded at runtime from Google's CDN (see
// `sdkLoader.ts`) — this is NOT an npm dependency, so there is no upstream
// `@types` package to lean on. Keep this file to exactly the surface we call.
//
// Google Cast and Chromecast are trademarks of Google LLC; this project is
// independent and not affiliated with or endorsed by Google.

/** `chrome.cast.ErrorCode` values this app branches on (subset; the SDK defines more). */
export type CastErrorCode = string;

export interface CastError {
  readonly code: CastErrorCode;
  readonly description?: string;
}

/** `chrome.cast.media.MediaInfo` constructor + the fields this app sets. */
export declare class ChromeCastMediaInfo {
  constructor(contentId: string, contentType: string);
  contentId: string;
  contentUrl?: string;
  contentType: string;
  streamType: string;
  customData?: Record<string, unknown>;
  hlsSegmentFormat?: string;
  hlsVideoSegmentFormat?: string;
  metadata?: ChromeCastMediaMetadata;
  /** The media's audio/text/video tracks, as published by the receiver (`chrome.cast.media.MediaInfo.tracks`). */
  tracks?: readonly ChromeCastTrack[] | null;
}

export declare class ChromeCastGenericMediaMetadata {
  metadataType: number;
  title?: string;
}

export type ChromeCastMediaMetadata = ChromeCastGenericMediaMetadata;

// eslint-disable-next-line @typescript-eslint/no-extraneous-class -- declares the SDK's `chrome.cast.media.LoadRequest` constructor shape only.
export declare class ChromeCastLoadRequest {
  constructor(media: ChromeCastMediaInfo);
}

/** `chrome.cast.media.Track` (the subset this app reads). */
export interface ChromeCastTrack {
  readonly trackId: number;
  /** `chrome.cast.media.TrackType` (`AUDIO`, `TEXT`, `VIDEO`). */
  readonly type: string;
  readonly name?: string | null;
  readonly language?: string | null;
}

export interface ChromeCastMedia {
  /** The loaded `MediaInfo`; its `tracks` field carries the audio/text/video tracks (`chrome.cast.media.Media.media`). */
  readonly media?: ChromeCastMediaInfo;
  /** `chrome.cast.media.PlayerState` (`IDLE`, `PLAYING`, `PAUSED`, `BUFFERING`). */
  readonly playerState?: string;
  /** `chrome.cast.media.IdleReason` when `playerState` is `IDLE`. */
  readonly idleReason?: string | null;
  /** The currently active track ids (one per kind that has a selection); lives on `Media` itself, not `Media.media`. */
  readonly activeTrackIds?: readonly number[] | null;
  addUpdateListener: (listener: (isAlive?: boolean) => void) => void;
  removeUpdateListener: (listener: (isAlive?: boolean) => void) => void;
  stop: (
    request: unknown,
    successCallback: () => void,
    errorCallback: (error: CastError) => void,
  ) => void;
  /** `chrome.cast.media.Media.editTracksInfo`: switch active audio/text tracks. */
  editTracksInfo: (
    request: ChromeCastEditTracksInfoRequest,
    successCallback: () => void,
    errorCallback: (error: CastError) => void,
  ) => void;
}

export declare class ChromeCastEditTracksInfoRequest {
  constructor(activeTrackIds: readonly number[]);
  activeTrackIds: readonly number[];
}

export interface CastDevice {
  readonly friendlyName?: string;
}

/** `cast.framework.VolumeEventData` (VOLUME_CHANGED / MUTE_CHANGED... share it). */
export interface CastVolumeEventData {
  readonly volume?: number;
  readonly isMute?: boolean;
}

/** A custom-namespace message listener: `(namespace, message)`; CAF passes the JSON as a string. */
export type CastMessageListener = (namespace: string, message: string) => void;

export interface CastSession {
  readonly sessionId?: string;
  getCastDevice?: () => CastDevice | null | undefined;
  loadMedia: (request: ChromeCastLoadRequest) => Promise<void>;
  getMediaSession: () => ChromeCastMedia | null;
  endSession: (stopCasting: boolean) => void;
  /** Receiver volume, 0..1 (`null` before the first status). */
  getVolume: () => number | null;
  isMute: () => boolean | null;
  /** Resolves on success; rejects with an error-code string. */
  setVolume: (level: number) => Promise<void>;
  setMute: (isMute: boolean) => Promise<void>;
  addEventListener: (eventType: string, handler: (event: CastVolumeEventData) => void) => void;
  removeEventListener: (eventType: string, handler: (event: CastVolumeEventData) => void) => void;
  addMessageListener: (namespace: string, listener: CastMessageListener) => void;
  removeMessageListener: (namespace: string, listener: CastMessageListener) => void;
  sendMessage: (namespace: string, data: unknown) => Promise<void>;
}

/** `cast.framework.RemotePlayer` session states this app matches on. */
export type CastSessionState =
  | 'NO_SESSION'
  | 'SESSION_STARTING'
  | 'SESSION_STARTED'
  | 'SESSION_START_FAILED'
  | 'SESSION_ENDING'
  | 'SESSION_ENDED'
  | 'SESSION_RESUMED';

export interface CastSessionStateEventData {
  readonly sessionState: CastSessionState;
  /** CAF sets this to `null` on a clean end; a code string on a failure. */
  readonly errorCode?: CastErrorCode | null;
}

export interface CastContext {
  setOptions: (options: {
    receiverApplicationId: string;
    autoJoinPolicy: string;
  }) => void;
  requestSession: () => Promise<void>;
  getCurrentSession: () => CastSession | null;
  addEventListener: (
    eventType: string,
    handler: (event: CastSessionStateEventData) => void,
  ) => void;
  removeEventListener: (
    eventType: string,
    handler: (event: CastSessionStateEventData) => void,
  ) => void;
}

export interface CastFrameworkNamespace {
  CastContext: { getInstance: () => CastContext };
  CastContextEventType: { SESSION_STATE_CHANGED: string };
  SessionEventType: { VOLUME_CHANGED: string };
  SessionState: Record<string, CastSessionState>;
}

export interface ChromeCastMediaNamespace {
  MediaInfo: typeof ChromeCastMediaInfo;
  GenericMediaMetadata: typeof ChromeCastGenericMediaMetadata;
  LoadRequest: typeof ChromeCastLoadRequest;
  EditTracksInfoRequest: typeof ChromeCastEditTracksInfoRequest;
  StreamType: { LIVE: string; BUFFERED: string; OTHER: string };
  HlsSegmentFormat: { TS: string; FMP4: string };
  HlsVideoSegmentFormat: { MPEG2_TS: string; FMP4: string };
  MetadataType: { GENERIC: number };
  PlayerState: { IDLE: string; PLAYING: string; PAUSED: string; BUFFERING: string };
  TrackType: { AUDIO: string; TEXT: string; VIDEO: string };
  DEFAULT_MEDIA_RECEIVER_APP_ID: string;
}

export interface ChromeCastNamespace {
  AutoJoinPolicy: { ORIGIN_SCOPED: string; TAB_AND_ORIGIN_SCOPED: string; PAGE_SCOPED: string };
  media: ChromeCastMediaNamespace;
  ErrorCode: Record<string, CastErrorCode>;
}

declare global {
  interface Window {
    cast?: { framework: CastFrameworkNamespace };
    // `window.chrome` is not in the standard DOM lib; this augmentation only
    // asserts the shape this module needs (and its mere presence is also the
    // Chrome/Edge browser-support signal — see `support.ts`).
    chrome?: { cast?: ChromeCastNamespace };
    __onGCastApiAvailable?: (isAvailable: boolean) => void;
  }
}

// Re-export nothing extra; this file's purpose is the ambient declarations
// plus the few constructor/type exports other cast/ modules import directly.
export {};
