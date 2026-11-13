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
  contentType: string;
  streamType = '';
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
  /** Simulate the SDK firing SESSION_STATE_CHANGED with SESSION_ENDED. */
  emitSessionEnded: (errorCode?: string) => void;
  /** Simulate the media session's update listener firing (e.g. after IDLE/FINISHED). */
  emitMediaUpdate: (status: { playerState: string; idleReason?: string }) => void;
  readonly requestSession: Mock<() => Promise<void>>;
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
  const endSession = vi.fn();

  const session: CastSession = {
    sessionId: 'mock-session-1',
    getCastDevice: () => device,
    loadMedia,
    getMediaSession: () => mediaSession,
    endSession,
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
        ...(errorCode !== undefined ? { errorCode } : {}),
      });
    },
    emitMediaUpdate: (status): void => {
      const mutable = mediaSession as { playerState?: string; idleReason?: string | null };
      mutable.playerState = status.playerState;
      mutable.idleReason = status.idleReason ?? null;
      mediaUpdateListener?.();
    },
    requestSession,
    loadMedia,
    endSession,
  };
}
