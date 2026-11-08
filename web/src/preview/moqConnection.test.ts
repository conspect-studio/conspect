import { afterEach, describe, expect, it, vi } from 'vitest';
import { connectMoq } from './moqConnection';

afterEach(() => vi.unstubAllGlobals());

describe('QUIC-only MoQ connection', () => {
  it('disables WebSocket on the real pinned client before any dial', async () => {
    const websocket = vi.fn(() => { throw new Error('WebSocket must not open'); });
    vi.stubGlobal('WebSocket', websocket);
    vi.stubGlobal('WebTransport', undefined);
    await expect(connectMoq(new URL('https://relay.example/preview'), AbortSignal.timeout(100)))
      .rejects.toThrow();
    expect(websocket).not.toHaveBeenCalled();
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
