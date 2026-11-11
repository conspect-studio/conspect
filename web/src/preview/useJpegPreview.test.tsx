import { act, renderHook } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { staleAfterMs, useJpegPreview, useJpegStill } from './useJpegPreview';

vi.mock('../api/token', () => ({ getStoredToken: () => undefined }));

interface FakeResp { ok: boolean; status?: number; headers?: Headers; blob: () => Promise<Blob> }

interface PendingRequest {
  signal: AbortSignal | undefined;
  resolve: (response: FakeResp) => void;
  finishBlob: (() => void) | undefined;
}

let requests: PendingRequest[];
let createdUrls: string[];
let revokedUrls: string[];
let nextUrl: number;

function deferredResponse(signal: AbortSignal | undefined): Promise<FakeResp> {
  return new Promise((resolve) => {
    requests.push({ signal, resolve, finishBlob: undefined });
  });
}

function resolveResponse(request: PendingRequest | undefined, value: string): void {
  request?.resolve({
    ok: true,
    headers: new Headers({ 'content-type': 'image/jpeg' }),
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
  it('does not poll while disabled and aborts when the focus player opens', () => {
    const { rerender } = renderHook(({ enabled }) => useJpegPreview('/preview/a.jpg', 1000, enabled), { initialProps: { enabled: false } });
    expect(requests).toHaveLength(0);
    rerender({ enabled: true });
    expect(requests).toHaveLength(1);
    rerender({ enabled: false });
    expect(requests[0]?.signal?.aborted).toBe(true);
  });
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

  it('does not show a previous source frame under a new source path', async () => {
    const { result, rerender } = renderHook(({ path }) => useJpegPreview(path, 1000), { initialProps: { path: '/preview/a.jpg' } });
    await act(async () => {
      resolveResponse(requests[0], 'first');
      await Promise.resolve();
      requests[0]?.finishBlob?.();
      await Promise.resolve();
    });
    expect(result.current).toBe('blob:preview-0');
    rerender({ path: '/preview/b.jpg' });
    expect(result.current).toBeUndefined();
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

  async function settle(i: number, value: string): Promise<void> {
    await act(async () => {
      resolveResponse(requests[i], value);
      await Promise.resolve();
      requests[i]?.finishBlob?.();
      await Promise.resolve();
    });
  }
  async function fail(i: number, status: number, contentType?: string): Promise<void> {
    await act(async () => {
      requests[i]?.resolve({
        ok: status >= 200 && status < 300,
        status,
        headers: new Headers(contentType ? { 'content-type': contentType } : {}),
        blob: () => Promise.resolve(new Blob(['<html>'])),
      });
      await Promise.resolve();
    });
  }

  it('treats a 200 text/html (SPA index) as unavailable and never exposes a frame', async () => {
    const { result } = renderHook(() => useJpegStill('/preview/a.jpg', 1000));
    await fail(0, 200, 'text/html');
    expect(result.current.url).toBeUndefined();
    expect(result.current.state).toBe('unavailable');
    expect(createdUrls).toHaveLength(0);
  });

  it('drops a previous frame when the route turns out not to be served', async () => {
    const { result } = renderHook(() => useJpegStill('/preview/a.jpg', 1000));
    await settle(0, 'good');
    expect(result.current.state).toBe('fresh');
    await act(async () => { await vi.advanceTimersByTimeAsync(1000); });
    await fail(1, 200, 'text/html');
    expect(result.current).toEqual({ url: undefined, state: 'unavailable' });
  });

  it('keeps the last good frame across failed polls and goes stale only after the bounded age', async () => {
    const { result } = renderHook(() => useJpegStill('/preview/a.jpg', 2000));
    await settle(0, 'good');
    expect(result.current).toEqual({ url: 'blob:preview-0', state: 'fresh' });
    // 503 polls every 2 s: the frame stays, still fresh within the bound.
    for (let i = 1; i <= 3; i += 1) {
      await act(async () => { await vi.advanceTimersByTimeAsync(2000); });
      await fail(i, 503, 'application/problem+json');
      expect(result.current).toEqual({ url: 'blob:preview-0', state: 'fresh' });
    }
    expect(staleAfterMs(2000)).toBe(10_000);
    await act(async () => { await vi.advanceTimersByTimeAsync(4000); });
    expect(result.current.state).toBe('stale');
    expect(result.current.url).toBe('blob:preview-0');
    // A new good frame clears staleness.
    const next = requests.length - 1;
    await settle(next, 'again');
    expect(result.current.state).toBe('fresh');
  });
});
