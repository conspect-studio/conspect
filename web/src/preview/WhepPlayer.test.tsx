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
  localDescription: RTCSessionDescription | null = null;
  remoteSdp: string | undefined;
  closed = false;
  statsBytes = 0;

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
  emitTrack(): void {
    const event = { track: { kind: 'video' } } as unknown as RTCTrackEvent;
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
    // Let the negotiate microtasks settle (createOffer → POST → setRemote).
    await act(async () => {
      await vi.runOnlyPendingTimersAsync();
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

  it('calls onFatal when the WHEP POST is rejected (non-2xx)', async () => {
    const fetchImpl = makeFetch(() => sdpResponse(503));
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
});
