// Component tests for <WhepPlayer> against a scripted fake RTCPeerConnection
// (ADR-W023 §7). jsdom has no WebRTC, so the player's injected `pcFactory` and
// `fetchImpl` seams let us drive the whole negotiate → connect → play → fail and
// teardown state space without a browser — exactly the property the factory
// injection exists for.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act } from '@testing-library/react';

import { renderWithProviders } from '../test/render';
import { WhepPlayer } from './WhepPlayer';

// jsdom has no MediaStream; a minimal shim is enough — the player only adds
// tracks to it and assigns it to video.srcObject.
class FakeMediaStream {
  readonly tracks: MediaStreamTrack[] = [];
  addTrack(track: MediaStreamTrack): void {
    this.tracks.push(track);
  }
}

/** A scripted fake RTCPeerConnection the tests advance step by step. */
class FakePeerConnection extends EventTarget {
  iceGatheringState: RTCIceGatheringState = 'complete';
  connectionState: RTCPeerConnectionState = 'new';
  iceConnectionState: RTCIceConnectionState = 'checking';
  localDescription: RTCSessionDescription | null = null;
  remoteSdp: string | undefined;
  closed = false;
  statsBytes = 0;
  statsRows: Record<string, unknown>[] | undefined;

  ontrack: ((event: RTCTrackEvent) => void) | null = null;
  onconnectionstatechange: (() => void) | null = null;

  static last: FakePeerConnection | undefined;

  constructor() {
    super();
    FakePeerConnection.last = this;
  }

  addTransceiver(): void {
    // no-op; the offer m-lines are not inspected by these tests.
  }

  createOffer(): Promise<RTCSessionDescriptionInit> {
    return Promise.resolve({ type: 'offer', sdp: 'v=0\r\na=offer' });
  }

  setLocalDescription(desc: RTCSessionDescriptionInit): Promise<void> {
    const local = { type: 'offer', sdp: desc.sdp ?? '' } as unknown as RTCSessionDescription;
    this.localDescription = local;
    return Promise.resolve();
  }

  setRemoteDescription(desc: RTCSessionDescriptionInit): Promise<void> {
    this.remoteSdp = desc.sdp;
    return Promise.resolve();
  }

  getStats(): Promise<RTCStatsReport> {
    if (this.statsRows) return Promise.resolve(new Map(this.statsRows.map((row, i) => [String(i), row])));
    const map = new Map<string, unknown>();
    map.set('in', {
      id: 'in',
      type: 'inbound-rtp',
      timestamp: Date.now(),
      bytesReceived: this.statsBytes,
    });
    return Promise.resolve(map as unknown as RTCStatsReport);
  }

  close(): void {
    this.closed = true;
  }

  /** Test helper: drive a connection-state transition + fire the handler. */
  emitConnectionState(state: RTCPeerConnectionState): void {
    this.connectionState = state;
    this.onconnectionstatechange?.();
  }

  /** Test helper: deliver an inbound track. */
  emitTrack(kind: 'audio' | 'video' = 'video'): void {
    const event = { track: { kind } } as unknown as RTCTrackEvent;
    this.ontrack?.(event);
  }
}

/** A `typeof fetch` mock that returns a scripted Response per call. */
function makeFetch(responder: () => Response): ReturnType<typeof vi.fn<typeof fetch>> {
  return vi.fn<typeof fetch>().mockImplementation(() => Promise.resolve(responder()));
}

function sdpResponse(status: number, location?: string, body = 'v=0\r\na=answer'): Response {
  const headers = new Headers();
  if (location !== undefined) {
    headers.set('Location', location);
  }
  const resp = {
    ok: status >= 200 && status < 300,
    status,
    url: 'https://host.example/api/v1/preview/program/whep',
    headers,
    text: (): Promise<string> => Promise.resolve(body),
    clone(): Response {
      return { json: (): Promise<unknown> => Promise.resolve({}) } as unknown as Response;
    },
  };
  return resp as unknown as Response;
}

beforeEach(() => {
  vi.stubGlobal('MediaStream', FakeMediaStream);
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
  FakePeerConnection.last = undefined;
});

describe('<WhepPlayer>', () => {
  it('reports an ICE failure immediately and stops callbacks after cleanup', async () => {
    const onFatal = vi.fn();
    const { unmount } = renderWithProviders(<WhepPlayer endpoint="/private-token" label="Input" pcFactory={() => new FakePeerConnection() as unknown as RTCPeerConnection} fetchImpl={makeFetch(() => sdpResponse(201, 'sessions/abc'))} onFatal={onFatal} />);
    await act(async () => { await vi.advanceTimersByTimeAsync(0); });
    const pc = FakePeerConnection.last!;
    act(() => { pc.iceConnectionState = 'failed'; pc.dispatchEvent(new Event('iceconnectionstatechange')); });
    expect(onFatal).toHaveBeenCalledWith(expect.objectContaining({ stage: 'ice-failed', httpStatus: 201 }));
    unmount();
    await act(async () => { await vi.advanceTimersByTimeAsync(20000); });
    expect(onFatal).toHaveBeenCalledTimes(1);
    expect(pc.closed).toBe(true);
  });

  it('reports connection timeout without claiming blocked UDP', async () => {
    const onFatal = vi.fn();
    renderWithProviders(<WhepPlayer endpoint="/private-token" label="Input" pcFactory={() => new FakePeerConnection() as unknown as RTCPeerConnection} fetchImpl={makeFetch(() => sdpResponse(201, 'sessions/abc'))} onFatal={onFatal} />);
    await act(async () => { await vi.advanceTimersByTimeAsync(16000); });
    expect(onFatal).toHaveBeenCalledWith(expect.objectContaining({ stage: 'ice-timeout', httpStatus: 201, ice: 'checking' }));
    expect(JSON.stringify(onFatal.mock.calls)).not.toMatch(/private-token|blocked|sessions/);
  });

  it.each([
    ['dtls-failed', [{ type: 'transport', dtlsState: 'failed' }]],
    ['video-no-rtp', [{ type: 'inbound-rtp', kind: 'video', bytesReceived: 0, framesDecoded: 0 }]],
    ['video-no-decode', [{ type: 'inbound-rtp', kind: 'video', bytesReceived: 100, framesDecoded: 0 }]],
    ['media-unknown', []],
  ] as const)('reports %s from safe mocked stats', async (stage, rows) => {
    const onFatal = vi.fn();
    const play = vi.spyOn(HTMLMediaElement.prototype, 'play').mockResolvedValue();
    renderWithProviders(<WhepPlayer endpoint="/private-token" label="Input" pcFactory={() => new FakePeerConnection() as unknown as RTCPeerConnection} fetchImpl={makeFetch(() => sdpResponse(201, 'sessions/abc'))} onFatal={onFatal} />);
    await act(async () => { await vi.advanceTimersByTimeAsync(0); });
    const pc = FakePeerConnection.last!;
    pc.statsRows = [...rows];
    if (stage !== 'dtls-failed') act(() => { pc.emitConnectionState('connected'); });
    await act(async () => { await vi.advanceTimersByTimeAsync(8000); });
    expect(onFatal).toHaveBeenCalledWith(expect.objectContaining({ stage }));
    play.mockRestore();
  });

  it('reports playback rejection without exposing the browser error', async () => {
    const play = vi.spyOn(HTMLMediaElement.prototype, 'play').mockRejectedValue(new Error('secret SDP'));
    const onFatal = vi.fn();
    renderWithProviders(<WhepPlayer endpoint="/private-token" label="Input" pcFactory={() => new FakePeerConnection() as unknown as RTCPeerConnection} fetchImpl={makeFetch(() => sdpResponse(201, 'sessions/abc'))} onFatal={onFatal} />);
    await act(async () => { await vi.advanceTimersByTimeAsync(0); FakePeerConnection.last?.emitConnectionState('connected'); });
    expect(onFatal).toHaveBeenCalledWith(expect.objectContaining({ stage: 'media-play-error' }));
    expect(JSON.stringify(onFatal.mock.calls)).not.toContain('secret');
    play.mockRestore();
  });

  it.each((['checking', 'connected', 'completed'] as const).flatMap((ice) => ['connecting', 'unknown', 'failed'].map((dtls) => ({ ice, dtls }))))('classifies timeout with ICE $ice and DTLS $dtls truthfully', async ({ ice, dtls }) => {
    const onFatal = vi.fn();
    renderWithProviders(<WhepPlayer endpoint="/preview" label="Input" pcFactory={() => new FakePeerConnection() as unknown as RTCPeerConnection} fetchImpl={makeFetch(() => sdpResponse(201, 'sessions/abc'))} onFatal={onFatal} />);
    FakePeerConnection.last!.iceConnectionState = ice;
    FakePeerConnection.last!.statsRows = [{ type: 'transport', dtlsState: dtls }];
    await act(async () => { await vi.advanceTimersByTimeAsync(16000); });
    expect(onFatal).toHaveBeenCalledWith(expect.objectContaining({ stage: dtls === 'failed' ? 'dtls-failed' : ice === 'checking' ? 'ice-timeout' : 'connection-timeout' }));
  });

  it('rejects stale preconnect stats and keeps only one request outstanding', async () => {
    const onFatal = vi.fn();
    const play = vi.spyOn(HTMLMediaElement.prototype, 'play').mockResolvedValue();
    const { unmount } = renderWithProviders(<WhepPlayer endpoint="/preview" label="Input" pcFactory={() => new FakePeerConnection() as unknown as RTCPeerConnection} fetchImpl={makeFetch(() => sdpResponse(201, 'sessions/abc'))} onFatal={onFatal} />);
    const pc = FakePeerConnection.last!;
    let resolve: ((report: RTCStatsReport) => void) | undefined;
    const stats = vi.spyOn(pc, 'getStats').mockImplementation(() => new Promise((done) => { resolve = done; }));
    await act(async () => { await vi.advanceTimersByTimeAsync(3000); });
    expect(stats).toHaveBeenCalledTimes(1);
    act(() => { pc.emitConnectionState('connected'); });
    await act(async () => { resolve?.(new Map([['transport', { type: 'transport', dtlsState: 'failed' }]])); await Promise.resolve(); });
    expect(onFatal).not.toHaveBeenCalled();
    await act(async () => { await vi.advanceTimersByTimeAsync(1000); });
    expect(stats).toHaveBeenCalledTimes(2);
    unmount();
    await act(async () => { resolve?.(new Map([['transport', { type: 'transport', dtlsState: 'failed' }]])); await vi.advanceTimersByTimeAsync(10000); });
    expect(onFatal).not.toHaveBeenCalled();
    expect(stats).toHaveBeenCalledTimes(2);
    play.mockRestore();
  });

  it.each(['frozen', 'advancing', 'absent'] as const)('handles %s decoded counters while RTP grows', async (mode) => {
    const onFatal = vi.fn();
    const play = vi.spyOn(HTMLMediaElement.prototype, 'play').mockResolvedValue();
    renderWithProviders(<WhepPlayer endpoint="/preview" label="Input" pcFactory={() => new FakePeerConnection() as unknown as RTCPeerConnection} fetchImpl={makeFetch(() => sdpResponse(201, 'sessions/abc'))} onFatal={onFatal} />);
    const pc = FakePeerConnection.last!;
    await act(async () => { await vi.advanceTimersByTimeAsync(0); pc.emitConnectionState('connected'); });
    for (let i = 1; i <= 9; i++) {
      pc.statsRows = [{ type: 'inbound-rtp', kind: 'video', bytesReceived: i * 100, ...(mode === 'absent' ? {} : { framesDecoded: mode === 'frozen' ? 1 : i }) }];
      await act(async () => { await vi.advanceTimersByTimeAsync(1000); });
    }
    if (mode === 'frozen') expect(onFatal).toHaveBeenCalledWith(expect.objectContaining({ stage: 'video-stalled' }));
    else expect(onFatal).not.toHaveBeenCalled();
    play.mockRestore();
  });

  it('negotiates: POSTs the gathered offer, applies the answer, reaches playing', async () => {
    const fetchImpl = makeFetch(() => sdpResponse(201, 'sessions/abc'));
    const onStatus = vi.fn();
    const { getByTestId } = renderWithProviders(
      <WhepPlayer
        endpoint="/api/v1/preview/program/whep"
        label="Program"
        pcFactory={() => new FakePeerConnection() as unknown as RTCPeerConnection}
        fetchImpl={fetchImpl}
        onStatus={onStatus}
      />,
    );
    // Settle negotiation without advancing the new 15-second connection deadline.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(fetchImpl).toHaveBeenCalledTimes(1);
    const init = fetchImpl.mock.calls[0]?.[1];
    expect(init?.method).toBe('POST');

    const pc = FakePeerConnection.last;
    expect(pc?.remoteSdp).toBe('v=0\r\na=answer');

    // Drive the connection to connected ⇒ the surface reports playing.
    act(() => {
      pc?.emitTrack();
      pc?.emitConnectionState('connected');
    });
    expect(getByTestId('whep-player').getAttribute('data-status')).toBe('playing');
    expect(onStatus).toHaveBeenCalledWith('playing');
  });

  it.each([['video', 'audio'], ['audio', 'video']] as const)(
    'keeps video attached when %s arrives before %s on separate streams',
    (first, second) => {
      const { container } = renderWithProviders(
        <WhepPlayer
          endpoint="/api/v1/preview/inputs/au-abc-news/whep"
          label="Input au-abc-news"
          pcFactory={() => new FakePeerConnection() as unknown as RTCPeerConnection}
          fetchImpl={makeFetch(() => sdpResponse(201, 'sessions/abc'))}
        />,
      );
      const video = container.querySelector('video');
      const pc = FakePeerConnection.last;
      expect(video).not.toBeNull();
      act(() => { pc?.emitTrack(first); });
      const attached = video?.srcObject as unknown as FakeMediaStream;
      expect(attached).toBeInstanceOf(FakeMediaStream);
      act(() => { pc?.emitTrack(second); });
      expect(video?.srcObject).toBe(attached);
      expect(attached.tracks.map((track) => track.kind)).toEqual([first, second]);
    },
  );

  it('waits for program DELETE before it signals the next focus', async () => {
    let finishDelete: ((response: Response) => void) | undefined;
    const fetchImpl = vi.fn<typeof fetch>().mockImplementation((_url, init) =>
      init?.method === 'DELETE'
        ? new Promise<Response>((resolve) => { finishDelete = resolve; })
        : Promise.resolve(sdpResponse(201, 'sessions/abc')),
    );
    const onSessionReleased = vi.fn();
    const { unmount } = renderWithProviders(
      <WhepPlayer
        endpoint="/api/v1/preview/program/whep"
        label="Program"
        pcFactory={() => new FakePeerConnection() as unknown as RTCPeerConnection}
        fetchImpl={fetchImpl}
        onSessionReleased={onSessionReleased}
      />,
    );
    await act(async () => { await vi.runOnlyPendingTimersAsync(); });
    unmount();
    expect(fetchImpl.mock.calls.some(([, init]) => init?.method === 'DELETE')).toBe(true);
    expect(onSessionReleased).not.toHaveBeenCalled();
    await act(async () => { finishDelete?.(sdpResponse(204)); await Promise.resolve(); });
    expect(onSessionReleased).toHaveBeenCalledTimes(1);
  });

  it('does not release focus until a late POST has its DELETE response', async () => {
    let finishPost: ((response: Response) => void) | undefined;
    let finishDelete: ((response: Response) => void) | undefined;
    const fetchImpl = vi.fn<typeof fetch>().mockImplementation((_url, init) =>
      init?.method === 'DELETE'
        ? new Promise<Response>((resolve) => { finishDelete = resolve; })
        : new Promise<Response>((resolve) => { finishPost = resolve; }),
    );
    const onSessionReleased = vi.fn();
    const { unmount } = renderWithProviders(
      <WhepPlayer
        endpoint="/api/v1/preview/program/whep"
        label="Program"
        pcFactory={() => new FakePeerConnection() as unknown as RTCPeerConnection}
        fetchImpl={fetchImpl}
        onSessionReleased={onSessionReleased}
      />,
    );
    await act(async () => { await vi.runOnlyPendingTimersAsync(); });
    expect(fetchImpl.mock.calls.filter(([, init]) => init?.method === 'POST')).toHaveLength(1);
    unmount();
    expect(onSessionReleased).not.toHaveBeenCalled();
    await act(async () => { finishPost?.(sdpResponse(201, 'sessions/late')); await Promise.resolve(); });
    expect(fetchImpl.mock.calls.filter(([, init]) => init?.method === 'DELETE')).toHaveLength(1);
    expect(onSessionReleased).not.toHaveBeenCalled();
    await act(async () => { finishDelete?.(sdpResponse(204)); await Promise.resolve(); });
    expect(onSessionReleased).toHaveBeenCalledTimes(1);
  });

  it('calls onFatal when the connection state goes to failed', async () => {
    const fetchImpl = makeFetch(() => sdpResponse(201, 'sessions/abc'));
    const onFatal = vi.fn();
    renderWithProviders(
      <WhepPlayer
        endpoint="/api/v1/preview/program/whep"
        label="Program"
        pcFactory={() => new FakePeerConnection() as unknown as RTCPeerConnection}
        fetchImpl={fetchImpl}
        onFatal={onFatal}
      />,
    );
    await act(async () => {
      await vi.runOnlyPendingTimersAsync();
    });
    act(() => {
      FakePeerConnection.last?.emitConnectionState('failed');
    });
    expect(onFatal).toHaveBeenCalledTimes(1);
  });

  it.each([503, 401, 500])('reports safe HTTP status %s on WHEP rejection', async (status) => {
    const fetchImpl = makeFetch(() => sdpResponse(status));
    const onFatal = vi.fn();
    renderWithProviders(
      <WhepPlayer
        endpoint="/api/v1/preview/program/whep"
        label="Program"
        pcFactory={() => new FakePeerConnection() as unknown as RTCPeerConnection}
        fetchImpl={fetchImpl}
        onFatal={onFatal}
      />,
    );
    // The negotiate promise chain (createOffer → POST reject → fail) settles
    // across microtasks; flush them under fake timers.
    await act(async () => {
      await vi.runOnlyPendingTimersAsync();
      await Promise.resolve();
    });
    expect(onFatal).toHaveBeenCalledTimes(1);
    expect(onFatal).toHaveBeenCalledWith(expect.objectContaining({ httpStatus: status, stage: status === 503 ? 'http-capacity' : 'http-error' }));
  });

  it('fails when inbound bytes never advance (the stats stall watchdog)', async () => {
    const fetchImpl = makeFetch(() => sdpResponse(201, 'sessions/abc'));
    const onFatal = vi.fn();
    renderWithProviders(
      <WhepPlayer
        endpoint="/api/v1/preview/program/whep"
        label="Program"
        pcFactory={() => new FakePeerConnection() as unknown as RTCPeerConnection}
        fetchImpl={fetchImpl}
        onFatal={onFatal}
      />,
    );
    await act(async () => {
      await vi.runOnlyPendingTimersAsync();
    });
    act(() => {
      FakePeerConnection.last?.emitConnectionState('connected');
    });
    // statsBytes stays 0 forever; after the stall window the watchdog fails.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(8000);
    });
    expect(onFatal).toHaveBeenCalled();
  });

  it('DELETEs the session and closes the PC on unmount', async () => {
    const fetchImpl = makeFetch(() => sdpResponse(201, 'sessions/abc'));
    const { unmount } = renderWithProviders(
      <WhepPlayer
        endpoint="/api/v1/preview/program/whep"
        label="Program"
        pcFactory={() => new FakePeerConnection() as unknown as RTCPeerConnection}
        fetchImpl={fetchImpl}
      />,
    );
    await act(async () => {
      await vi.runOnlyPendingTimersAsync();
    });
    const pc = FakePeerConnection.last;
    act(() => {
      unmount();
    });
    expect(pc?.closed).toBe(true);
    // A DELETE on the resolved session url was issued (POST + DELETE = 2 calls).
    const deleteCall = fetchImpl.mock.calls.find((call) => call[1]?.method === 'DELETE');
    expect(deleteCall).toBeDefined();
  });

  const pcf = (): RTCPeerConnection => new FakePeerConnection() as unknown as RTCPeerConnection;

  it('DELETEs with keepalive on pagehide, once, and not again on unmount', async () => {
    const fetchImpl = makeFetch(() => sdpResponse(201, 'sessions/abc'));
    const { unmount } = renderWithProviders(
      <WhepPlayer endpoint="/api/v1/preview/program/whep" label="Program" pcFactory={pcf} fetchImpl={fetchImpl} />,
    );
    await act(async () => { await vi.runOnlyPendingTimersAsync(); });
    await act(async () => { window.dispatchEvent(new Event('pagehide')); await Promise.resolve(); });
    const deletes = (): unknown[][] => fetchImpl.mock.calls.filter((c) => c[1]?.method === 'DELETE');
    expect(deletes()).toHaveLength(1);
    expect(deletes()[0]?.[1]).toMatchObject({ method: 'DELETE', keepalive: true });
    unmount();
    expect(deletes()).toHaveLength(1);
  });

  it('DELETEs with keepalive on unmount', async () => {
    const fetchImpl = makeFetch(() => sdpResponse(201, 'sessions/abc'));
    const { unmount } = renderWithProviders(
      <WhepPlayer endpoint="/api/v1/preview/program/whep" label="Program" pcFactory={pcf} fetchImpl={fetchImpl} />,
    );
    await act(async () => { await vi.runOnlyPendingTimersAsync(); });
    unmount();
    const del = fetchImpl.mock.calls.find((c) => c[1]?.method === 'DELETE');
    expect(del?.[0]).toBe('https://host.example/api/v1/preview/program/sessions/abc');
    expect(del?.[1]).toMatchObject({ keepalive: true });
  });

  it('never POSTs a new offer while the previous session is still being released', async () => {
    let finishDelete: (() => void) | undefined;
    const fetchImpl = vi.fn<typeof fetch>().mockImplementation((_url, init) =>
      init?.method === 'DELETE'
        ? new Promise<Response>((resolve) => { finishDelete = () => { resolve(sdpResponse(204)); }; })
        : Promise.resolve(sdpResponse(201, 'sessions/abc')),
    );
    const first = renderWithProviders(
      <WhepPlayer endpoint="/api/v1/preview/program/whep" label="Program" pcFactory={pcf} fetchImpl={fetchImpl} />,
    );
    await act(async () => { await vi.runOnlyPendingTimersAsync(); });
    first.unmount();
    renderWithProviders(
      <WhepPlayer endpoint="/api/v1/preview/program/whep" label="Program" pcFactory={pcf} fetchImpl={fetchImpl} />,
    );
    await act(async () => { await vi.advanceTimersByTimeAsync(4000); });
    const posts = (): number => fetchImpl.mock.calls.filter((c) => c[1]?.method === 'POST').length;
    expect(posts()).toBe(1); // the second offer is held back
    await act(async () => { finishDelete?.(); await vi.advanceTimersByTimeAsync(10); });
    expect(posts()).toBe(2);
  });

  it('offers anyway after a bounded wait if the earlier release never completes', async () => {
    const fetchImpl = vi.fn<typeof fetch>().mockImplementation((_url, init) =>
      init?.method === 'DELETE' ? new Promise<Response>(() => undefined) : Promise.resolve(sdpResponse(201, 'sessions/abc')),
    );
    const first = renderWithProviders(
      <WhepPlayer endpoint="/api/v1/preview/program/whep" label="Program" pcFactory={pcf} fetchImpl={fetchImpl} />,
    );
    await act(async () => { await vi.runOnlyPendingTimersAsync(); });
    first.unmount();
    renderWithProviders(
      <WhepPlayer endpoint="/api/v1/preview/program/whep" label="Program" pcFactory={pcf} fetchImpl={fetchImpl} />,
    );
    await act(async () => { await vi.advanceTimersByTimeAsync(6000); });
    expect(fetchImpl.mock.calls.filter((c) => c[1]?.method === 'POST')).toHaveLength(2);
  });
});
