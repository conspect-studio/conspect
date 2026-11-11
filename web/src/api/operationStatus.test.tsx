import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act, renderHook } from '@testing-library/react';
import { dismissToast, useToasts } from '../components/ui/use-toast';
import { followOperation } from './operationStatus';
import { trackAccepted } from './operations';
import '../test/render';

function toasts(): ReturnType<typeof useToasts> {
  return renderHook(() => useToasts()).result.current;
}

describe('followOperation', () => {
  beforeEach(() => { vi.useFakeTimers(); });
  afterEach(() => {
    const current = toasts();
    act(() => { for (const item of current) dismissToast(item.id); });
    vi.useRealTimers();
  });

  it.each(['take_salvo', 'media_play', 'replay', 'media_load'])(
    'toasts acceptance, never a generic completion or timeout for %s', async (kind) => {
      await act(async () => {
        followOperation(trackAccepted({ operation_id: `op-${kind}`, kind }), 'Command');
        await vi.advanceTimersByTimeAsync(120_000);
      });
      expect(toasts().map((item) => item.title)).toEqual(['Command accepted']);
      expect(toasts()[0]?.description).toContain(`op-${kind}`);
    },
  );
});
