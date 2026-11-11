import { act, renderHook } from '@testing-library/react';
import { afterEach, expect, it, vi } from 'vitest';

import { useProducerNowMs } from './producerNow';

afterEach(() => vi.useRealTimers());

it('refreshes the first subscriber after a delayed first mount and restarts after the last unmount', () => {
  vi.useFakeTimers();
  vi.setSystemTime(new Date('2026-01-01T12:00:00Z'));
  const first = renderHook(() => useProducerNowMs());
  expect(first.result.current).toBe(Date.now());
  const second = renderHook(() => useProducerNowMs());
  void act(() => vi.advanceTimersByTime(1000));
  expect(first.result.current).toBe(Date.now());
  expect(second.result.current).toBe(Date.now());
  first.unmount();
  second.unmount();
  vi.setSystemTime(new Date('2026-01-01T12:10:00Z'));
  const restarted = renderHook(() => useProducerNowMs());
  expect(restarted.result.current).toBe(Date.now());
  restarted.unmount();
});
