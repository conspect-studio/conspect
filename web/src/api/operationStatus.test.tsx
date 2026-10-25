// Operator-facing operation status: acceptance toast first, then the
// authoritative outcome (success, failure with reason, or a separate timeout).
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act, renderHook } from '@testing-library/react';

import { dismissToast, useToasts } from '../components/ui/use-toast';
import { resolveOperationEnvelope } from '../realtime/operations';
import type { Envelope } from '../realtime/envelope';
import { trackAccepted } from './operations';
import { followOperation } from './operationStatus';
import '../test/render';

function env(corr: string, t: string, data: unknown = {}): Envelope {
  return { v: 1, t, topic: 'jobs', seq: 1, ts: 1, corr, data };
}

function toasts(): ReturnType<typeof useToasts> {
  return renderHook(() => useToasts()).result.current;
}

describe('followOperation', () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    for (const item of toasts()) dismissToast(item.id);
    vi.useRealTimers();
  });

  it('toasts acceptance first, then the success outcome', async () => {
    const accepted = trackAccepted({ operation_id: 'st-1', kind: 'take' });
    followOperation(accepted, 'Salvo take');
    expect(toasts().map((x) => x.title)).toEqual(['Command accepted']);
    expect(toasts()[0]?.description).toContain('st-1');
    await act(async () => {
      resolveOperationEnvelope(env('st-1', 'job.result'));
      await vi.advanceTimersByTimeAsync(0);
    });
    const list = toasts();
    expect(list).toHaveLength(2);
    expect(list[1]?.title).toContain('Salvo take');
    expect(list[1]?.variant).not.toBe('destructive');
  });

  it('reports a failed outcome destructively with its reason', async () => {
    const accepted = trackAccepted({ operation_id: 'st-2', kind: 'take' });
    followOperation(accepted, 'Salvo take');
    await act(async () => {
      resolveOperationEnvelope(env('st-2', 'job.failed', { message: 'layout missing' }));
      await vi.advanceTimersByTimeAsync(0);
    });
    const failure = toasts()[1];
    expect(failure?.variant).toBe('destructive');
    expect(failure?.description).toContain('layout missing');
  });

  it('reports a timeout separately from a failure', async () => {
    const accepted = trackAccepted({ operation_id: 'st-3', kind: 'take' });
    followOperation(accepted, 'Salvo take');
    await act(async () => {
      await vi.advanceTimersByTimeAsync(61_000);
    });
    const last = toasts()[1];
    expect(last?.title).toBe('Outcome not received');
    expect(last?.description).toContain('st-3');
  });
});
