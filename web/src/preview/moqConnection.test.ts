import { afterEach, describe, expect, it, vi } from 'vitest';
import { connectMoq, ownMoqSession } from './moqConnection';

afterEach(() => vi.unstubAllGlobals());

describe('established session ownership', () => {
  it('releases after rejected termination confirmation', async () => {
    const release = vi.fn(() => Promise.resolve());
    const owned = ownMoqSession({ close: vi.fn(), closed: Promise.reject(new Error('remote reset')) }, new AbortController().signal, release);
    await owned.released;
    await owned.close();
    expect(release).toHaveBeenCalledOnce();
  });

  it('does not throw from pre-aborted ownership when close throws', async () => {
    let finish!: () => void;
    const closed = new Promise<null>((resolve) => { finish = () => { resolve(null); }; });
    const release = vi.fn(() => Promise.resolve());
    const owned = ownMoqSession({ close: () => { throw new Error('close boom'); }, closed }, AbortSignal.abort(), release);
    expect(release).not.toHaveBeenCalled();
    finish();
    await owned.released;
    expect(release).toHaveBeenCalledOnce();
  });

  it('waits for transport closure before releasing the lease exactly once', async () => {
    const events: string[] = [];
    let finish!: () => void;
    const closed = new Promise<null>((resolve) => { finish = () => { resolve(null); }; });
    const controller = new AbortController();
    const release = vi.fn(() => { events.push('release'); return Promise.resolve(); });
    const owned = ownMoqSession({ close: () => { events.push('close'); }, closed }, controller.signal, release);
    controller.abort();
    expect(events).toEqual(['close']);
    const teardown = owned.close();
    finish();
    await teardown;
    await owned.close();
    expect(events).toEqual(['close', 'release']);
    expect(release).toHaveBeenCalledOnce();
  });

  it('bounds a stalled close and permits retry after transport confirmation', async () => {
    vi.useFakeTimers();
    try {
      let finish!: () => void;
      const closed = new Promise<null>((resolve) => { finish = () => { resolve(null); }; });
      const release = vi.fn(() => Promise.resolve());
      const owned = ownMoqSession({ close: vi.fn(), closed }, new AbortController().signal, release, 50);
      const failed = expect(owned.close()).rejects.toThrow('closure deadline');
      await vi.advanceTimersByTimeAsync(50);
      await failed;
      expect(release).not.toHaveBeenCalled();
      finish();
      await owned.close();
      expect(release).toHaveBeenCalledOnce();
    } finally { vi.useRealTimers(); }
  });

  it('retries failed idempotent lease release without duplicate concurrent requests', async () => {
    const release = vi.fn<() => Promise<void>>()
      .mockRejectedValueOnce(new Error('control unavailable'))
      .mockResolvedValue(undefined);
    const owned = ownMoqSession({ close: vi.fn(), closed: Promise.resolve(null) }, new AbortController().signal, release);
    await expect(owned.released).rejects.toThrow('control unavailable');
    await Promise.all([owned.close(), owned.close()]);
    expect(release).toHaveBeenCalledTimes(2);
  });

  it('releases after remote termination without waiting for unmount', async () => {
    const close = vi.fn();
    const release = vi.fn(() => Promise.resolve());
    const owned = ownMoqSession({ close, closed: Promise.resolve(null) }, new AbortController().signal, release);
    await owned.released;
    await owned.close();
    expect(release).toHaveBeenCalledOnce();
    expect(close).not.toHaveBeenCalled();
  });
});

describe('QUIC-only MoQ connection', () => {
  it('disables WebSocket on the real pinned client before any dial', async () => {
    const websocket = vi.fn(() => { throw new Error('WebSocket must not open'); });
    vi.stubGlobal('WebSocket', websocket);
    vi.stubGlobal('WebTransport', undefined);
    await expect(connectMoq(new URL('https://relay.example/preview'), AbortSignal.timeout(100)))
      .rejects.toThrow();
    expect(websocket).not.toHaveBeenCalled();
  });

  it('never races WebSocketStream while WebTransport handshake hangs', async () => {
    const socket = vi.fn(() => { throw new Error('unexpected socket'); });
    const close = vi.fn();
    const transport = vi.fn(class {
      ready = new Promise<void>(() => { /* Deliberately stalled handshake. */ });
      closed = new Promise<void>(() => { /* Deliberately stalled transport. */ });
      close = close;
    });
    vi.stubGlobal('WebTransport', transport);
    vi.stubGlobal('WebSocketStream', socket);
    vi.stubGlobal('WebSocket', socket);
    await expect(connectMoq(new URL('https://relay.example/hanging'), AbortSignal.timeout(750)))
      .rejects.toThrow();
    expect(transport).toHaveBeenCalledOnce();
    expect(socket).not.toHaveBeenCalled();
    expect(close).toHaveBeenCalled();
  });

  it('rejects non-HTTPS endpoints without a dial', async () => {
    await expect(connectMoq(new URL('http://relay.example/preview'), AbortSignal.timeout(100)))
      .rejects.toThrow('HTTPS');
  });

  it('honors cancellation before opening any transport', async () => {
    const websocket = vi.fn();
    const transport = vi.fn();
    vi.stubGlobal('WebSocket', websocket);
    vi.stubGlobal('WebTransport', transport);
    await expect(connectMoq(new URL('https://relay.example/preview'), AbortSignal.abort()))
      .rejects.toThrow();
    expect(websocket).not.toHaveBeenCalled();
    expect(transport).not.toHaveBeenCalled();
  });
});
