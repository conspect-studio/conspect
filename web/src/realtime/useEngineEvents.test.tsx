// useEngineEvents — connect-time tiles seeding. A fresh client must show the
// CURRENT per-tile lifecycle state from the `tiles` `$snapshot` frame the
// server emits right after `$hello`, WITHOUT waiting for a `tile.state` delta
// (the bug: a page connecting between transitions showed no badge at all).
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { act, renderHook } from '@testing-library/react';
import type { ReactNode } from 'react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { TILES_QUERY_KEY, useEngineEvents } from './useEngineEvents';
import type { LiveTile } from './useEngineEvents';

/** A controllable WebSocket double the hook connects to. */
class FakeWebSocket {
  static instances: FakeWebSocket[] = [];
  onopen: (() => void) | null = null;
  onmessage: ((event: MessageEvent<unknown>) => void) | null = null;
  onerror: (() => void) | null = null;
  onclose: (() => void) | null = null;
  readonly url: string;

  constructor(url: string) {
    this.url = url;
    FakeWebSocket.instances.push(this);
  }

  send(): void {
    // The hook never sends in these tests.
  }

  close(): void {
    // Closed by the hook on unmount; nothing to emit.
  }

  /** Deliver a raw text frame as the server would. */
  emit(text: string): void {
    this.onmessage?.(new MessageEvent<unknown>('message', { data: text }));
  }
}

// The exact wire frames the Rust control plane emits at connect (see
// crates/multiview-control/src/realtime.rs): `$hello` on `$control`, then the
// tiles `$snapshot` baseline on topic `tiles` (docs/api/realtime.md §5).
const HELLO_FRAME = JSON.stringify({
  v: 1,
  topic: '$control',
  seq: 0,
  ts: 7,
  t: '$hello',
  data: {
    session_id: 'sess-test',
    server_v: [1],
    heartbeat_ms: 15000,
    min_rate_hz: 1,
    max_rate_hz: 60,
    default_rate_hz: 30,
    replay_ring: 64,
  },
});

const TILES_SNAPSHOT_FRAME = JSON.stringify({
  v: 1,
  topic: 'tiles',
  seq: 1,
  ts: 7,
  t: '$snapshot',
  data: {
    as_of_seq: 7,
    tiles: [
      { id: 'cam1', state: 'LIVE', input: 'cam1' },
      { id: 'cam2', state: 'NO_SIGNAL', input: 'cam2' },
    ],
  },
});

/**
 * The connection mints a single-use ticket (ADR-RT011) before opening the
 * socket, so the WebSocket is created after a microtask. Wait for it (under
 * `act`, so the connect-driven state updates are flushed).
 */
async function firstSocket(): Promise<FakeWebSocket> {
  await act(async () => {
    await vi.waitFor(() => {
      expect(FakeWebSocket.instances.length).toBeGreaterThan(0);
    });
  });
  const socket = FakeWebSocket.instances[0];
  if (socket === undefined) {
    throw new Error('no socket');
  }
  return socket;
}

describe('useEngineEvents connect-time tiles snapshot', () => {
  let client: QueryClient;

  beforeEach(() => {
    FakeWebSocket.instances = [];
    vi.stubGlobal('WebSocket', FakeWebSocket);
    // The realtime auth ticket mint (`POST /api/v1/ws/ticket`, ADR-RT011).
    vi.stubGlobal(
      'fetch',
      vi.fn(() =>
        Promise.resolve({
          ok: true,
          json: () =>
            Promise.resolve({ ticket: 'test-ticket', expires_in_secs: 30 }),
        }),
      ),
    );
    client = new QueryClient();
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    client.clear();
  });

  function wrapper({ children }: { children: ReactNode }): ReactNode {
    return (
      <QueryClientProvider client={client}>{children}</QueryClientProvider>
    );
  }

  it('seeds the tiles cache from the $snapshot frame with no delta', async () => {
    const { unmount } = renderHook(() => useEngineEvents(), { wrapper });
    const socket = await firstSocket();

    act(() => {
      socket.onopen?.();
      socket.emit(HELLO_FRAME);
      socket.emit(TILES_SNAPSHOT_FRAME);
    });

    const tiles = client.getQueryData<Record<string, LiveTile>>(
      TILES_QUERY_KEY,
    );
    expect(tiles).toBeDefined();
    expect(tiles?.cam1?.state).toBe('LIVE');
    expect(tiles?.cam1?.input).toBe('cam1');
    expect(tiles?.cam2?.state).toBe('NO_SIGNAL');
    unmount();
  });

  it('a later snapshot REBUILDS (replaces) the tile cache, never merges', async () => {
    const { unmount } = renderHook(() => useEngineEvents(), { wrapper });
    const socket = await firstSocket();

    act(() => {
      socket.onopen?.();
      socket.emit(HELLO_FRAME);
      socket.emit(TILES_SNAPSHOT_FRAME);
      // A fresh snapshot with only cam1 (now STALE): cam2 must drop out.
      socket.emit(
        JSON.stringify({
          v: 1,
          topic: 'tiles',
          seq: 2,
          ts: 9,
          t: '$snapshot',
          data: {
            as_of_seq: 9,
            tiles: [{ id: 'cam1', state: 'STALE', input: 'cam1' }],
          },
        }),
      );
    });

    const tiles = client.getQueryData<Record<string, LiveTile>>(
      TILES_QUERY_KEY,
    );
    expect(tiles?.cam1?.state).toBe('STALE');
    expect(tiles?.cam2).toBeUndefined();
    unmount();
  });
});
