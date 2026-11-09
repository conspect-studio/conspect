import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { submitOperation } from '../../api/operations';
import { QueryClient } from '@tanstack/react-query';
import { OperationApiError } from '../../api/operations';
import { getOperationTracker } from './operationTracker';

const player = (corr: string, kind: string, topic = 'switcher', id = 'p1') => ({
  v: 1, t: 'media.player_state', topic, id, seq: 1, ts: 1, corr,
  data: { player: id, position_frames: 0, state: { kind } },
});
const expectation = { topic: 'switcher' as const, resource: 'p1', event: 'media.player_state' as const,
  terminal: { kind: 'player-state' as const, state: 'playing' as const } };
const accepted = { operation_id: 'op1', kind: 'media.play' };
const server = setupServer();
beforeAll(() => { server.listen({ onUnhandledRequest: 'error' }); });
afterEach(() => { server.resetHandlers(); });
afterAll(() => { server.close(); });

describe('actual operation tracker', () => {
  it('joins the real submitted HTTP 202 with an early correlated socket frame', async () => {
    const tracker = getOperationTracker(new QueryClient());
    let count = 0;
    server.use(http.post('*/api/v1/media/players/p1/play', () => {
      count += 1;
      tracker.onEnvelope(player('op1', 'playing'));
      return HttpResponse.json(accepted, { status: 202 });
    }));
    const result = await tracker.submit(() => submitOperation('/api/v1/media/players/p1/play'), expectation);
    expect(result).toEqual({ kind: 'tracked', operationId: 'op1' });
    expect(tracker.snapshot().records.get('op1')?.phase).toBe('applied');
    expect(count).toBe(1);
  });

  it.each(['before', 'after'])('joins actual 202 and exact terminal event %s admission', async (order) => {
    const tracker = getOperationTracker(new QueryClient());
    if (order === 'before') tracker.onEnvelope(player('op1', 'playing'));
    const result = await tracker.submit(() => Promise.resolve(accepted), expectation);
    if (order === 'after') tracker.onEnvelope(player('op1', 'playing'));
    expect(result).toEqual({ kind: 'tracked', operationId: 'op1' });
    expect(tracker.snapshot().records.get('op1')?.phase).toBe('applied');
  });

  it('ignores loading, wrong topic, wrong resource and unrelated progress even in early cache', async () => {
    const tracker = getOperationTracker(new QueryClient());
    tracker.onEnvelope(player('op1', 'playing', 'devices'));
    tracker.onEnvelope(player('op1', 'playing', 'switcher', 'p2'));
    tracker.onEnvelope({ ...player('op1', 'playing'), t: 'job.progress', topic: 'jobs', data: { phase: 'done', pct: 100 } });
    tracker.onEnvelope(player('op1', 'loading'));
    await tracker.submit(() => Promise.resolve(accepted), expectation);
    expect(tracker.snapshot().records.get('op1')?.phase).toBe('accepted');
    tracker.onEnvelope(player('op1', 'playing'));
    expect(tracker.snapshot().records.get('op1')?.phase).toBe('applied');
  });

  it('accepts output idle as typed terminal, not starting or unrelated states', async () => {
    const tracker = getOperationTracker(new QueryClient());
    await tracker.submit(() => Promise.resolve(accepted), { topic: 'outputs', resource: 'out1', event: 'output.status',
      terminal: { kind: 'output-state', state: 'idle' } });
    tracker.onEnvelope({ ...player('op1', 'playing', 'outputs', 'out1'), t: 'output.status', data: { state: 'starting' } });
    expect(tracker.snapshot().records.get('op1')?.phase).toBe('accepted');
    tracker.onEnvelope({ ...player('op1', 'playing', 'outputs', 'out1'), t: 'output.status', data: { state: 'idle' } });
    expect(tracker.snapshot().records.get('op1')?.phase).toBe('applied');
  });

  it('marks pending unknown on gap, accepts late exact event and never calls another POST', async () => {
    const tracker = getOperationTracker(new QueryClient());
    const post = vi.fn().mockResolvedValue(accepted);
    await tracker.submit(post, expectation);
    tracker.onGap();
    expect(tracker.snapshot().records.get('op1')?.phase).toBe('unknown');
    tracker.onEnvelope(player('op1', 'playing'));
    expect(tracker.snapshot().records.get('op1')?.phase).toBe('applied');
    expect(post).toHaveBeenCalledTimes(1);
  });

  it('deadline yields unknown, exact late event can reconcile', async () => {
    vi.useFakeTimers();
    try {
      const tracker = getOperationTracker(new QueryClient());
      await tracker.submit(() => Promise.resolve(accepted), expectation, { deadlineMs: 100 });
      vi.advanceTimersByTime(101);
      expect(tracker.snapshot().records.get('op1')?.phase).toBe('unknown');
      tracker.onEnvelope(player('op1', 'playing'));
      expect(tracker.snapshot().records.get('op1')?.phase).toBe('applied');
    } finally { vi.useRealTimers(); }
  });

  it('preserves anonymous uncertainty on network ambiguity but frees definitive HTTP rejection', async () => {
    const tracker = getOperationTracker(new QueryClient());
    await expect(tracker.submit(() => Promise.reject(new Error('network')), expectation)).rejects.toThrow('network');
    expect([...tracker.snapshot().provisional.values()].map((seat) => seat.phase)).toEqual(['unknown']);
    await expect(tracker.submit(() => Promise.reject(new OperationApiError('bad', 422)), expectation)).rejects.toThrow('bad');
    expect(tracker.snapshot().provisional.size).toBe(1);
  });

  it('full capacity refuses before running caller request', async () => {
    const tracker = getOperationTracker(new QueryClient());
    for (let i = 0; i < 256; i++) {
      await expect(tracker.submit(() => Promise.reject(new Error('network')), expectation)).rejects.toThrow();
    }
    const post = vi.fn();
    await expect(tracker.submit(post, expectation)).rejects.toMatchObject({ code: 'capacity' });
    expect(post).not.toHaveBeenCalled();
  });

  it('does not share a tracker across clients and resets on changed identity', async () => {
    const client = new QueryClient();
    const tracker = getOperationTracker(client);
    await tracker.submit(() => Promise.resolve(accepted), expectation);
    expect(getOperationTracker(new QueryClient()).snapshot().records.size).toBe(0);
    tracker.resetIdentity('other-operator');
    expect(tracker.snapshot().records.size).toBe(0);
  });

  it('does not infer failure from output error without an explicit command failure contract', async () => {
    const tracker = getOperationTracker(new QueryClient());
    await tracker.submit(() => Promise.resolve(accepted), { topic: 'outputs', resource: 'o1', event: 'output.status',
      terminal: { kind: 'output-state', state: 'running' } });
    tracker.onEnvelope({ ...player('op1', 'playing', 'outputs', 'o1'), t: 'output.status', data: { state: 'error' } });
    expect(tracker.snapshot().records.get('op1')?.phase).toBe('accepted');
  });

  it('distinguishes vamp arm and disarm despite the same state kind', async () => {
    const tracker = getOperationTracker(new QueryClient());
    await tracker.submit(() => Promise.resolve(accepted), { topic: 'switcher', resource: 'p1', event: 'media.player_state',
      terminal: { kind: 'player-state', state: 'vamping', exitArmed: true } });
    tracker.onEnvelope({ ...player('op1', 'vamping'), data: { player: 'p1', position_frames: 0,
      state: { kind: 'vamping', exit_armed: false } } });
    expect(tracker.snapshot().records.get('op1')?.phase).toBe('accepted');
    tracker.onEnvelope({ ...player('op1', 'vamping'), data: { player: 'p1', position_frames: 0,
      state: { kind: 'vamping', exit_armed: true } } });
    expect(tracker.snapshot().records.get('op1')?.phase).toBe('applied');
  });

  it('unsupported commands remain accepted-unobserved without a fabricated correlation', async () => {
    const tracker = getOperationTracker(new QueryClient());
    const result = await tracker.submit(() => Promise.resolve(accepted), undefined);
    expect(result).toEqual({ kind: 'accepted-unobserved', operationId: 'op1' });
    expect(tracker.snapshot().records.size).toBe(0);
    expect([...tracker.snapshot().provisional.values()].map((seat) => seat.phase)).toEqual(['unknown']);
  });
});
