// Test-only mocks of the Cast Web Sender SDK surface this app calls. Install
// with `installMockCastSdk()` in a test's `beforeEach`/`beforeAll`, and remove
// with `window.cast = window.chrome = undefined` style cleanup — tests do
// their own teardown so this stays a pure factory.
import { vi } from 'vitest';
import type { Mock } from 'vitest';
import type {
  CastContext,
  CastDevice,
  CastSession,
  CastSessionStateEventData,
  CastVolumeEventData,
  ChromeCastMedia,
  ChromeCastMediaNamespace,
  ChromeCastNamespace,
} from './castTypes';

// `castTypes.ts` declares the SDK's media constructors as `declare class`
// (type-only — there is no runtime SDK to load in tests). These are minimal
// REAL runtime stand-ins so `new chromeCast.media.MediaInfo(...)` etc. work
// exactly like the production code path, just without the real SDK.
class MockMediaInfo {
  contentId: string;
  contentUrl?: string;
  contentType: string;
  streamType = '';
  customData?: Record<string, unknown>;
  hlsSegmentFormat?: string;
  hlsVideoSegmentFormat?: string;
  metadata?: { metadataType: number; title?: string };
  constructor(contentId: string, contentType: string) {
    this.contentId = contentId;
    this.contentType = contentType;
  }
}

class MockGenericMediaMetadata {
  metadataType = 0;
  title?: string;
}

class MockLoadRequest {
  media: MockMediaInfo;
  constructor(media: MockMediaInfo) {
    this.media = media;
  }
}

export interface MockCastSdk {
  readonly context: CastContext;
  readonly session: CastSession;
  readonly mediaSession: ChromeCastMedia;
  /**
   * Simulate SESSION_STATE_CHANGED → SESSION_ENDED exactly as CAF builds it:
   * `errorCode` is `null` on a clean end, a code string on a failure.
   */
  emitSessionEnded: (errorCode?: string) => void;
  /**
   * Simulate a media status update as CAF delivers it: the properties change,
   * then the listener is called with `isAlive` (false for every IDLE state).
   */
  emitMediaUpdate: (status: { playerState: string; idleReason?: string }) => void;
  /** Deliver a custom-namespace message exactly as CAF does: `(namespace, jsonString)`. */
  emitMessage: (namespace: string, message: unknown) => void;
  /** Simulate a receiver volume change (properties change, then VOLUME_CHANGED). */
  emitVolume: (level: number, muted?: boolean) => void;
  /** Active custom-namespace listeners (leak checks). */
  readonly messageListenerCount: () => number;
  /** Whether a SESSION_STATE_CHANGED handler is registered (leak checks). */
  readonly hasSessionStateHandler: () => boolean;
  readonly requestSession: Mock<() => Promise<void>>;
  readonly setVolume: Mock<(level: number) => Promise<void>>;
  readonly setMute: Mock<(muted: boolean) => Promise<void>>;
  readonly sendMessage: Mock<(namespace: string, data: unknown) => Promise<void>>;
  readonly loadMedia: Mock<(request: unknown) => Promise<void>>;
  readonly endSession: Mock<(stopCasting: boolean) => void>;
}

/**
 * Install a mock `window.cast` / `window.chrome.cast` and trigger
 * `__onGCastApiAvailable(true)` so `loadCastSdk()` resolves immediately
 * without injecting a real `<script>` tag.
 */
export function installMockCastSdk(options: { deviceName?: string } = {}): MockCastSdk {
  const deviceName = options.deviceName ?? 'Living Room TV';
  let mediaUpdateListener: ((isAlive?: boolean) => void) | null = null;
  // The real `chrome.cast.media.Media` exposes its state as properties.
  const mediaSession: ChromeCastMedia & { playerState?: string; idleReason?: string | null } = {
    playerState: 'PLAYING',
    idleReason: null,
    addUpdateListener: (listener: (isAlive?: boolean) => void) => {
      mediaUpdateListener = listener;
    },
    removeUpdateListener: () => {
      mediaUpdateListener = null;
    },
    stop: vi.fn((_request, successCallback: () => void) => {
      successCallback();
    }),
  };

  const device: CastDevice = { friendlyName: deviceName };

  const loadMedia = vi.fn(() => Promise.resolve());
  // Real CAF: endSession(true) stops the receiver, then the context fires
  // SESSION_ENDED with errorCode null.
  const endSession = vi.fn((stopCasting: boolean) => {
    void stopCasting;
    hasSession = false;
    sessionStateHandler?.({ sessionState: 'SESSION_ENDED', errorCode: null });
  });

  let volume = 0.5;
  let muted = false;
  const messageListeners = new Map<string, Set<(namespace: string, message: string) => void>>();
  const volumeHandlers = new Set<(event: CastVolumeEventData) => void>();
  const notifyVolume = (): void => {
    volumeHandlers.forEach((handler) => { handler({ volume, isMute: muted }); });
  };
  const setVolume = vi.fn((level: number) => {
    volume = level;
    notifyVolume();
    return Promise.resolve();
  });
  const setMute = vi.fn((isMute: boolean) => {
    muted = isMute;
    notifyVolume();
    return Promise.resolve();
  });
  const sendMessage = vi.fn((namespace: string, data: unknown) => {
    void namespace;
    void data;
    return Promise.resolve();
  });

  const session: CastSession = {
    sessionId: 'mock-session-1',
    getCastDevice: () => device,
    loadMedia,
    getMediaSession: () => mediaSession,
    endSession,
    getVolume: () => volume,
    isMute: () => muted,
    setVolume,
    setMute,
    // Real CAF: session.addEventListener(cast.framework.SessionEventType.*, handler).
    addEventListener: (_eventType: string, handler: (event: CastVolumeEventData) => void) => {
      volumeHandlers.add(handler);
    },
    removeEventListener: (_eventType: string, handler: (event: CastVolumeEventData) => void) => {
      volumeHandlers.delete(handler);
    },
    addMessageListener: (namespace, listener) => {
      const set = messageListeners.get(namespace) ?? new Set();
      set.add(listener);
      messageListeners.set(namespace, set);
    },
    removeMessageListener: (namespace, listener) => {
      messageListeners.get(namespace)?.delete(listener);
    },
    sendMessage,
  };

  let sessionStateHandler: ((event: CastSessionStateEventData) => void) | null = null;

  const requestSession = vi.fn(() => {
    hasSession = true;
    return Promise.resolve();
  });

  let hasSession = false;

  const context: CastContext = {
    setOptions: vi.fn(),
    requestSession,
    getCurrentSession: () => (hasSession ? session : null),
    addEventListener: (_eventType: string, handler: (event: CastSessionStateEventData) => void) => {
      sessionStateHandler = handler;
    },
    removeEventListener: () => {
      sessionStateHandler = null;
    },
  };

  const media: ChromeCastMediaNamespace = {
    MediaInfo: MockMediaInfo,
    GenericMediaMetadata: MockGenericMediaMetadata,
    LoadRequest: MockLoadRequest,
    StreamType: { LIVE: 'LIVE', BUFFERED: 'BUFFERED', OTHER: 'OTHER' },
    HlsSegmentFormat: { TS: 'TS', FMP4: 'FMP4' },
    HlsVideoSegmentFormat: { MPEG2_TS: 'MPEG2_TS', FMP4: 'FMP4' },
    MetadataType: { GENERIC: 0 },
    PlayerState: { IDLE: 'IDLE', PLAYING: 'PLAYING', PAUSED: 'PAUSED', BUFFERING: 'BUFFERING' },
    DEFAULT_MEDIA_RECEIVER_APP_ID: 'CC1AD845',
  };

  const chromeCast: ChromeCastNamespace = {
    AutoJoinPolicy: { ORIGIN_SCOPED: 'origin_scoped', TAB_AND_ORIGIN_SCOPED: 'tab_and_origin_scoped', PAGE_SCOPED: 'page_scoped' },
    media,
    ErrorCode: {},
  };

  window.chrome = { cast: chromeCast };
  window.cast = {
    framework: {
      CastContext: { getInstance: () => context },
      CastContextEventType: { SESSION_STATE_CHANGED: 'sessionstatechanged' },
      SessionEventType: { VOLUME_CHANGED: 'volumechanged' },
      SessionState: {
        NO_SESSION: 'NO_SESSION',
        SESSION_STARTING: 'SESSION_STARTING',
        SESSION_STARTED: 'SESSION_STARTED',
        SESSION_START_FAILED: 'SESSION_START_FAILED',
        SESSION_ENDING: 'SESSION_ENDING',
        SESSION_ENDED: 'SESSION_ENDED',
        SESSION_RESUMED: 'SESSION_RESUMED',
      },
    },
  };
  // Fire the availability callback the loader awaits, exactly as the real
  // gstatic script does once loaded — but synchronously since no script tag
  // was actually injected in tests (the loader short-circuits via the
  // pre-existing `window.cast`).
  window.__onGCastApiAvailable?.(true);

  return {
    context,
    session,
    mediaSession,
    emitSessionEnded: (errorCode?: string): void => {
      sessionStateHandler?.({
        sessionState: 'SESSION_ENDED',
        errorCode: errorCode ?? null,
      });
    },
    emitMediaUpdate: (status): void => {
      const mutable = mediaSession as { playerState?: string; idleReason?: string | null };
      mutable.playerState = status.playerState;
      mutable.idleReason = status.idleReason ?? null;
      mediaUpdateListener?.(status.playerState !== 'IDLE');
    },
    emitMessage: (namespace: string, message: unknown): void => {
      // CAF hands the listener the raw JSON text.
      const text = typeof message === 'string' ? message : JSON.stringify(message);
      messageListeners.get(namespace)?.forEach((listener) => { listener(namespace, text); });
    },
    emitVolume: (level: number, isMute = false): void => {
      volume = level;
      muted = isMute;
      notifyVolume();
    },
    messageListenerCount: (): number =>
      [...messageListeners.values()].reduce((n, set) => n + set.size, 0),
    hasSessionStateHandler: (): boolean => sessionStateHandler !== null,
    requestSession,
    loadMedia,
    endSession,
    setVolume,
    setMute,
    sendMessage,
  };
}
