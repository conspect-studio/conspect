import { act, renderHook } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import type { ReactNode } from 'react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { Envelope } from './envelope';
import { useEngineEvents } from './useEngineEvents';
import { getOperationTracker } from '../components/broadcast/operationTracker';

const socket = vi.hoisted(() => ({ subscribe: vi.fn() }));
vi.mock('./shared', () => ({ subscribeRealtimeAll: socket.subscribe }));

describe('shell realtime operation adapter', () => {
  beforeEach(() => { socket.subscribe.mockReset(); });
  it('joins actual 202 using the existing shell subscription, preserving handlers and lifecycle', async () => {
    const client = new QueryClient();
    const unsubscribe = vi.fn();
    let envelope: ((message: Envelope) => void) | undefined;
    let status: ((state: 'reconnecting' | 'open') => void) | undefined;
    let gap: (() => void) | undefined;
    socket.subscribe.mockImplementation((onEnvelope, onStatus, onGap) => {
      envelope = onEnvelope; status = onStatus; gap = onGap; return unsubscribe;
    });
    const wrapper = ({ children }: { children: ReactNode }) =>
      <QueryClientProvider client={client}>{children}</QueryClientProvider>;
    const hook = renderHook(() => useEngineEvents(), { wrapper });
    const tracker = getOperationTracker(client);
    await tracker.submit(() => Promise.resolve({ operation_id: 'op1', kind: 'media_play' }), {
      topic: 'switcher', resource: 'p1', event: 'media.player_state',
      terminal: { kind: 'player-state', state: 'playing' },
    });
    const event: Envelope = { v: 1, t: 'media.player_state', topic: 'switcher', id: 'p1',
      seq: 1, ts: 1, corr: 'op1', data: { player: 'p1', position_frames: 0, state: { kind: 'playing' } } };
    act(() => { envelope?.(event); });
    expect(tracker.snapshot().records.get('op1')?.phase).toBe('applied');
    expect(client.getQueryData(['realtime', 'switcher', 'players'])).toHaveProperty('p1');
    expect(socket.subscribe).toHaveBeenCalledTimes(1);
    act(() => { status?.('reconnecting'); gap?.(); });
    expect(hook.result.current.gaps).toBe(1);
    hook.unmount();
    expect(unsubscribe).toHaveBeenCalledTimes(1);
  });
});
