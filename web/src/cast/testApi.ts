// Test-only fake of the cast-stream API the controller calls (`CastStreamApi`).
import { vi } from 'vitest';
import type { Mock } from 'vitest';

import type { CastStream, CreateCastStreamRequest } from '../api/castStreams';
import { OperationApiError } from '../api/operations';
import type { CastStreamApi } from './castController';

export const TOKEN_PATH = '/cast/AbCdEfGhIjKlMnOpQrStUvWxYz0123456789-_abcde/master.m3u8';

export function streamDoc(overrides: Partial<CastStream> = {}): CastStream {
  return {
    id: 'cs-1',
    output: 'cast-1',
    // The generated schema types the feed as an opaque object (`Record<string, never>`).
    feed: {},
    path: TOKEN_PATH,
    content_type: 'application/vnd.apple.mpegurl',
    hls_segment_format: 'fmp4',
    hls_video_segment_format: 'fmp4',
    ready: true,
    segments: 12,
    expires_unix_ms: 0,
    fetches: { playlists: 0, segments: 0, cast_device: false },
    ...overrides,
  };
}

export interface FakeApi extends CastStreamApi {
  readonly create: Mock<(request: CreateCastStreamRequest) => Promise<CastStream>>;
  readonly get: Mock<(id: string) => Promise<CastStream>>;
  readonly list: Mock<() => Promise<CastStream[]>>;
  readonly remove: Mock<(id: string, options?: { keepalive?: boolean }) => Promise<void>>;
}

/** A fake API: `create` answers `created`, `get` answers `current()`. */
export function fakeApi(created: CastStream = streamDoc(), current: () => CastStream = () => created): FakeApi {
  return {
    create: vi.fn(() => Promise.resolve(created)),
    get: vi.fn(() => Promise.resolve({ ...current(), path: null })),
    list: vi.fn(() => Promise.resolve([{ ...current(), path: null }])),
    remove: vi.fn(() => Promise.resolve()),
  };
}

/** A problem+json refusal as `readProblem` builds it. */
export function refusal(status: number, reason: string, detail = 'detail'): OperationApiError {
  return new OperationApiError('Cast refused', status, detail, `/problems/${reason}`);
}

/** An in-memory `localStorage` (this jsdom build has none) installed on `window`. */
export function installMemoryStorage(): void {
  const data = new Map<string, string>();
  const storage = {
    getItem: (key: string): string | null => data.get(key) ?? null,
    setItem: (key: string, value: string): void => {
      data.set(key, value);
    },
    removeItem: (key: string): void => {
      data.delete(key);
    },
    clear: (): void => {
      data.clear();
    },
  };
  Object.defineProperty(window, 'localStorage', { value: storage, configurable: true });
}
