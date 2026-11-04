import { act, renderHook } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { useJpegPreview } from './useJpegPreview';

vi.mock('../api/token', () => ({ getStoredToken: () => undefined }));

interface PendingRequest {
  signal: AbortSignal | undefined;
  resolve: (response: { ok: boolean; blob: () => Promise<Blob> }) => void;
  finishBlob: (() => void) | undefined;
}

let requests: PendingRequest[];
let createdUrls: string[];
let revokedUrls: string[];
let nextUrl: number;

function deferredResponse(signal: AbortSignal | undefined): Promise<{ ok: boolean; blob: () => Promise<Blob> }> {
  return new Promise((resolve) => {
    requests.push({ signal, resolve, finishBlob: undefined });
  });
}

function resolveResponse(request: PendingRequest | undefined, value: string): void {
  request?.resolve({
    ok: true,
    blob: () => new Promise((resolve) => {
      request.finishBlob = () => {
        resolve(new Blob([value]));
      };
    }),
  });
}

beforeEach(() => {
  vi.useFakeTimers();
  requests = [];
  createdUrls = [];
  revokedUrls = [];
  nextUrl = 0;
  vi.stubGlobal('fetch', vi.fn((_path: string, options: RequestInit) =>
    deferredResponse(options.signal as AbortSignal | undefined),
  ));
  vi.stubGlobal('URL', {
    createObjectURL: vi.fn(() => {
      const url = `blob:preview-${String(nextUrl++)}`;
      createdUrls.push(url);
      return url;
    }),
    revokeObjectURL: vi.fn((url: string) => revokedUrls.push(url)),
  });
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe('useJpegPreview', () => {
  it('keeps at most one request in flight and resumes after completion', async () => {
    const { result } = renderHook(() => useJpegPreview('/preview/a.jpg', 1000));
    expect(requests).toHaveLength(1);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(5000);
    });
    expect(requests).toHaveLength(1);

    await act(async () => {
      resolveResponse(requests[0], 'first');
      await Promise.resolve();
      requests[0]?.finishBlob?.();
      await Promise.resolve();
    });
    expect(result.current).toBe('blob:preview-0');

    await act(async () => {
      await vi.advanceTimersByTimeAsync(1000);
    });
    expect(requests).toHaveLength(2);
  });

  it('aborts pending work and revokes its URL on unmount', async () => {
    const { unmount } = renderHook(() => useJpegPreview('/preview/a.jpg', 1000));
    const pending = requests[0];
    expect(pending?.signal?.aborted).toBe(false);

    unmount();
    expect(pending?.signal?.aborted).toBe(true);

    await act(async () => {
      resolveResponse(pending, 'late');
      await Promise.resolve();
      pending?.finishBlob?.();
      await Promise.resolve();
    });
    expect(createdUrls).toEqual(revokedUrls);
  });

  it('aborts the old source and ignores its late completion', async () => {
    const { result, rerender } = renderHook(
      ({ path }) => useJpegPreview(path, 1000),
      { initialProps: { path: '/preview/a.jpg' } },
    );
    const oldRequest = requests[0];

    rerender({ path: '/preview/b.jpg' });
    expect(oldRequest?.signal?.aborted).toBe(true);
    expect(requests).toHaveLength(2);

    await act(async () => {
      resolveResponse(oldRequest, 'stale');
      await Promise.resolve();
      oldRequest?.finishBlob?.();
      await Promise.resolve();
    });
    expect(createdUrls).toEqual(revokedUrls);
    expect(result.current).toBeUndefined();

    await act(async () => {
      resolveResponse(requests[1], 'current');
      await Promise.resolve();
      requests[1]?.finishBlob?.();
      await Promise.resolve();
    });
    expect(result.current).toBe('blob:preview-1');
  });
});
