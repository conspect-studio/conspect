// useEngineEvents — the conflated `devices` topic (ADR-M008/ADR-RT007).
//
// `device.status` is a latest-wins lane scoped by the envelope `id`: each frame
// REPLACES that device's cached snapshot (the connect-time snapshot frames and
// later deltas are the same wire shape). `device.removed` drops the entry.
// Lossless lifecycle events (adopted/mode/error/removed) feed a bounded,
// newest-first session ring for the Events tab; `device.discovered` rows are
// correlated by the envelope `corr` (the scan's operation id). `$resync`
// rebuilds: every devices cache is discarded, never merged.
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { act, renderHook } from '@testing-library/react';
import type { ReactNode } from 'react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import {
  DEVICE_EVENTS_QUERY_KEY,
  DEVICE_STATUS_QUERY_KEY,
  DISCOVERED_LIVE_QUERY_KEY,
  ENGINE_CLOCK_QUERY_KEY,
  useEngineEvents,
} from './useEngineEvents';
import type {
  DeviceEventEntry,
  EngineClockRef,
} from './useEngineEvents';
import type { DeviceDiscovered, DeviceStatus } from './generated-types';

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

  emit(text: string): void {
    this.onmessage?.(new MessageEvent<unknown>('message', { data: text }));
  }
}

function frame(
  seq: number,
  t: string,
  data: unknown,
  extra: Record<string, unknown> = {},
): string {
  return JSON.stringify({ v: 1, topic: 'devices', seq, ts: seq * 10, t, data, ...extra });
}

describe('useEngineEvents devices topic', () => {
  let client: QueryClient;

  beforeEach(() => {
    FakeWebSocket.instances = [];
    vi.stubGlobal('WebSocket', FakeWebSocket);
    // The realtime auth ticket mint (`POST /api/v1/ws/ticket`, ADR-RT011): the
    // connect fetches a single-use ticket before opening the socket.
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
    return <QueryClientProvider client={client}>{children}</QueryClientProvider>;
  }

  async function start(): Promise<{ socket: FakeWebSocket; unmount: () => void }> {
    const { unmount } = renderHook(() => useEngineEvents(), { wrapper });
    // The connect mints a single-use ticket (async) before opening the socket
    // (ADR-RT011); wait for the socket under `act` so connect-driven state
    // updates flush.
    await act(async () => {
      await vi.waitFor(() => {
        expect(FakeWebSocket.instances.length).toBeGreaterThan(0);
      });
    });
    const socket = FakeWebSocket.instances[0];
    if (socket === undefined) {
      throw new Error('no socket');
    }
    act(() => {
      socket.onopen?.();
    });
    return { socket, unmount };
  }

  it('device.status upserts the per-device snapshot, latest wins', async () => {
    const { socket, unmount } = await start();
    act(() => {
      socket.emit(
        frame(
          1,
          'device.status',
          {
            device_id: 'dev-a',
            state: 'ONLINE',
            mode: 'decoder',
            temperature_c: 41.5,
            last_seen_ts: 90,
          },
          { id: 'dev-a' },
        ),
      );
      socket.emit(
        frame(
          2,
          'device.status',
          { device_id: 'dev-a', state: 'DEGRADED', mode: 'decoder' },
          { id: 'dev-a' },
        ),
      );
      socket.emit(
        frame(3, 'device.status', { device_id: 'dev-b', state: 'ADOPTING' }, { id: 'dev-b' }),
      );
    });
    const map = client.getQueryData<Record<string, DeviceStatus>>(DEVICE_STATUS_QUERY_KEY);
    expect(map?.['dev-a']?.state).toBe('DEGRADED');
    expect(map?.['dev-b']?.state).toBe('ADOPTING');
    unmount();
  });

  it('device.removed drops the snapshot and records a lifecycle event', async () => {
    const { socket, unmount } = await start();
    act(() => {
      socket.emit(
        frame(1, 'device.status', { device_id: 'dev-a', state: 'ONLINE' }, { id: 'dev-a' }),
      );
      socket.emit(frame(2, 'device.removed', { device_id: 'dev-a' }, { id: 'dev-a' }));
    });
    const map = client.getQueryData<Record<string, DeviceStatus>>(DEVICE_STATUS_QUERY_KEY);
    expect(map?.['dev-a']).toBeUndefined();
    const ring = client.getQueryData<readonly DeviceEventEntry[]>(DEVICE_EVENTS_QUERY_KEY);
    expect(ring?.[0]?.event).toEqual({ kind: 'removed', deviceId: 'dev-a' });
    unmount();
  });

  it('lifecycle events stack newest-first in the session ring', async () => {
    const { socket, unmount } = await start();
    act(() => {
      socket.emit(
        frame(
          1,
          'device.adopted',
          { device_id: 'dev-a', driver: 'zowietek', name: 'Foyer' },
          { id: 'dev-a' },
        ),
      );
      socket.emit(
        frame(
          2,
          'device.mode',
          {
            device_id: 'dev-a',
            mode: 'encoder',
            phase: 'started',
            impact: 'dev',
            detail: 'restarting',
          },
          { id: 'dev-a' },
        ),
      );
      socket.emit(
        frame(
          3,
          'device.error',
          { device_id: 'dev-a', message: 'decode stalled', code: '00004' },
          { id: 'dev-a' },
        ),
      );
    });
    const ring = client.getQueryData<readonly DeviceEventEntry[]>(DEVICE_EVENTS_QUERY_KEY);
    expect(ring).toHaveLength(3);
    expect(ring?.[0]?.event.kind).toBe('error');
    expect(ring?.[1]?.event).toEqual({
      kind: 'mode',
      deviceId: 'dev-a',
      mode: 'encoder',
      phase: 'started',
      impact: 'dev',
      detail: 'restarting',
    });
    expect(ring?.[2]?.event.kind).toBe('adopted');
    unmount();
  });

  it('device.discovered rows correlate to their scan via the envelope corr', async () => {
    const { socket, unmount } = await start();
    act(() => {
      socket.emit(
        frame(
          1,
          'device.discovered',
          { address: '[fd00::42]:80', driver: 'zowietek', family: 'ipv6', name: 'box' },
          { corr: 'op-scan' },
        ),
      );
    });
    const byCorr = client.getQueryData<Record<string, readonly DeviceDiscovered[]>>(
      DISCOVERED_LIVE_QUERY_KEY,
    );
    expect(byCorr?.['op-scan']).toHaveLength(1);
    expect(byCorr?.['op-scan']?.[0]?.address).toBe('[fd00::42]:80');
    unmount();
  });

  it('throttles engine-clock writes: a burst within 500 ms writes once', async () => {
    const { socket, unmount } = await start();
    act(() => {
      socket.emit(
        frame(1, 'device.status', { device_id: 'dev-a', state: 'ONLINE' }, {
          id: 'dev-a',
          ts: 1_000_000_000,
        }),
      );
    });
    const first = client.getQueryData<EngineClockRef>(ENGINE_CLOCK_QUERY_KEY);
    expect(first?.engineTs).toBe(1_000_000_000);
    act(() => {
      // A burst advancing < 500 ms: NO new cache write (same object identity),
      // so pages holding the clock ref are not re-rendered at envelope rate.
      socket.emit(
        frame(2, 'device.status', { device_id: 'dev-a', state: 'ONLINE' }, {
          id: 'dev-a',
          ts: 1_200_000_000,
        }),
      );
      socket.emit(
        frame(3, 'device.status', { device_id: 'dev-a', state: 'ONLINE' }, {
          id: 'dev-a',
          ts: 1_499_999_999,
        }),
      );
    });
    expect(client.getQueryData<EngineClockRef>(ENGINE_CLOCK_QUERY_KEY)).toBe(first);
    act(() => {
      // Advancing ≥ 500 ms writes again.
      socket.emit(
        frame(4, 'device.status', { device_id: 'dev-a', state: 'ONLINE' }, {
          id: 'dev-a',
          ts: 1_500_000_000,
        }),
      );
    });
    expect(
      client.getQueryData<EngineClockRef>(ENGINE_CLOCK_QUERY_KEY)?.engineTs,
    ).toBe(1_500_000_000);
    unmount();
  });

  it('device.adopted invalidates the devices list (another operator adopted)', async () => {
    const { socket, unmount } = await start();
    client.setQueryData(['resources', 'devices'], []);
    act(() => {
      socket.emit(
        frame(
          1,
          'device.mode',
          { device_id: 'dev-a', mode: 'encoder', phase: 'started', impact: 'dev' },
          { id: 'dev-a' },
        ),
      );
    });
    // mode/error churn must NOT refetch the list…
    expect(client.getQueryState(['resources', 'devices'])?.isInvalidated).toBe(false);
    act(() => {
      socket.emit(
        frame(
          2,
          'device.adopted',
          { device_id: 'dev-x', driver: 'zowietek', name: 'New box' },
          { id: 'dev-x' },
        ),
      );
    });
    // …but an adoption elsewhere must surface without an unrelated refetch.
    expect(client.getQueryState(['resources', 'devices'])?.isInvalidated).toBe(true);
    unmount();
  });

  it('device.removed invalidates the devices list', async () => {
    const { socket, unmount } = await start();
    client.setQueryData(['resources', 'devices'], []);
    act(() => {
      socket.emit(frame(1, 'device.removed', { device_id: 'dev-a' }, { id: 'dev-a' }));
    });
    expect(client.getQueryState(['resources', 'devices'])?.isInvalidated).toBe(true);
    unmount();
  });

  it("a new scan's rows prune previous scans' discovered rows", async () => {
    const { socket, unmount } = await start();
    act(() => {
      socket.emit(
        frame(
          1,
          'device.discovered',
          { address: '[fd00::42]:80', driver: 'zowietek', family: 'ipv6' },
          { corr: 'op-1' },
        ),
      );
      socket.emit(
        frame(
          2,
          'device.discovered',
          { address: '[fd00::43]:80', driver: 'zowietek', family: 'ipv6' },
          { corr: 'op-1' },
        ),
      );
      socket.emit(
        frame(
          3,
          'device.discovered',
          { address: '[fd00::44]:80', driver: 'cast', family: 'ipv6' },
          { corr: 'op-2' },
        ),
      );
    });
    // Only the active scan's rows remain: the per-corr map is bounded by the
    // newest scan, not by session length.
    const byCorr = client.getQueryData<Record<string, readonly DeviceDiscovered[]>>(
      DISCOVERED_LIVE_QUERY_KEY,
    );
    expect(Object.keys(byCorr ?? {})).toEqual(['op-2']);
    expect(byCorr?.['op-2']).toHaveLength(1);
    expect(byCorr?.['op-2']?.[0]?.address).toBe('[fd00::44]:80');
    unmount();
  });

  it('tracks an engine-monotonic clock reference from envelope timestamps', async () => {
    const { socket, unmount } = await start();
    act(() => {
      socket.emit(
        frame(5, 'device.status', { device_id: 'dev-a', state: 'ONLINE' }, { id: 'dev-a' }),
      );
    });
    const clock = client.getQueryData<EngineClockRef>(ENGINE_CLOCK_QUERY_KEY);
    expect(clock?.engineTs).toBe(50);
    expect(typeof clock?.wallMs).toBe('number');
    unmount();
  });

  it('$resync discards every devices cache (rebuild, never merge)', async () => {
    const { socket, unmount } = await start();
    act(() => {
      socket.emit(
        frame(1, 'device.status', { device_id: 'dev-a', state: 'ONLINE' }, { id: 'dev-a' }),
      );
      socket.emit(
        frame(
          2,
          'device.discovered',
          { address: '[fd00::42]:80', driver: 'zowietek', family: 'ipv6' },
          { corr: 'op-scan' },
        ),
      );
      socket.emit(
        JSON.stringify({
          v: 1,
          topic: '$control',
          seq: 0,
          ts: 99,
          t: '$resync',
          data: { topics: ['devices'] },
        }),
      );
    });
    expect(
      client.getQueryData<Record<string, DeviceStatus>>(DEVICE_STATUS_QUERY_KEY),
    ).toBeUndefined();
    expect(
      client.getQueryData<Record<string, readonly DeviceDiscovered[]>>(
        DISCOVERED_LIVE_QUERY_KEY,
      ),
    ).toBeUndefined();
    unmount();
  });
});
