// Component tests for the WHEP→JPEG fallback ladder (ADR-W023 §2): probe-gated
// transport selection, the honest fallback badge, and the retry affordance.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act, fireEvent, waitFor } from '@testing-library/react';

import { renderWithProviders } from '../test/render';
import { PreviewSurface } from './PreviewSurface';
import { capacityRetryDelayMs } from './capacityBackoff';
import type { PreviewCapabilities } from './capabilities';

function caps(over: Partial<PreviewCapabilities> = {}): PreviewCapabilities {
  return {
    webrtc: true,
    moq: false,
    fallback: 'jpeg',
    scopes: {
      program: { whep: true, fidelity: 'real-encoded-output' },
      inputs: { whep: true },
      outputs: { whep: true },
    },
    ...over,
  };
}

// A minimal local description for the idle fake (jsdom has no WebRTC types).
const IDLE_LOCAL_DESCRIPTION = { type: 'offer', sdp: 'v=0' } as unknown as RTCSessionDescription;

// A no-op PC factory: never emits a track or transitions, so a mounted player
// just sits in "connecting" unless the test drives a fatal error via fetch.
class IdlePeerConnection extends EventTarget {
  iceGatheringState: RTCIceGatheringState = 'complete';
  connectionState: RTCPeerConnectionState = 'new';
  localDescription: RTCSessionDescription = IDLE_LOCAL_DESCRIPTION;
  ontrack: ((event: RTCTrackEvent) => void) | null = null;
  onconnectionstatechange: (() => void) | null = null;
  addTransceiver(): void {
    // no-op
  }
  createOffer(): Promise<RTCSessionDescriptionInit> {
    return Promise.resolve({ type: 'offer', sdp: 'v=0' });
  }
  setLocalDescription(): Promise<void> {
    return Promise.resolve();
  }
  setRemoteDescription(): Promise<void> {
    return Promise.resolve();
  }
  getStats(): Promise<RTCStatsReport> {
    return Promise.resolve(new Map() as unknown as RTCStatsReport);
  }
  close(): void {
    // no-op
  }
}

function okSdp(): Response {
  const headers = new Headers();
  headers.set('Location', 'sessions/abc');
  return {
    ok: true,
    status: 201,
    url: 'https://h/api/v1/preview/program/whep',
    headers,
    text: (): Promise<string> => Promise.resolve('v=0\r\na=answer'),
    clone(): Response {
      return { json: (): Promise<unknown> => Promise.resolve({}) } as unknown as Response;
    },
  } as unknown as Response;
}

function rejectSdp(): Response {
  return {
    ok: false,
    status: 503,
    url: 'https://h/api/v1/preview/program/whep',
    headers: new Headers(),
    text: (): Promise<string> => Promise.resolve(''),
    clone(): Response {
      return {
        json: (): Promise<unknown> => Promise.resolve({ fallback: 'jpeg' }),
      } as unknown as Response;
    },
  } as unknown as Response;
}

beforeEach(() => {
  vi.stubGlobal('MediaStream', class {
    addTrack(): void {
      // no-op
    }
  });
  // Stub the JPEG-poll fetch so the JpegRung does not hit the network.
  vi.stubGlobal(
    'fetch',
    vi.fn((): Promise<Response> => Promise.reject(new Error('jpeg fetch suppressed in test'))),
  );
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe('<PreviewSurface>', () => {
  it('renders the JPEG rung with NO badge when the build has no WebRTC (honest primary)', () => {
    const { getByTestId, queryByTestId } = renderWithProviders(
      <PreviewSurface
        scope="program"
        whepEndpoint="/api/v1/preview/program/whep"
        jpegPath="/api/v1/preview/program.jpg"
        label="Program"
        capabilities={caps({ webrtc: false })}
      />,
    );
    expect(getByTestId('jpeg-rung')).toBeInTheDocument();
    // A primary still preview is labeled too; it is not live video.
    expect(getByTestId('still-preview-label')).toHaveTextContent('Still preview (~1 fps)');
    expect(queryByTestId('fallback-badge')).toBeNull();
    // The WHEP player was never mounted.
    expect(queryByTestId('whep-player')).toBeNull();
  });

  it('renders the JPEG rung with NO badge for an input scope that advertises no whep (honest primary)', () => {
    const { getByTestId, queryByTestId } = renderWithProviders(
      <PreviewSurface
        scope="input"
        whepEndpoint="/api/v1/preview/inputs/cam1/whep"
        jpegPath="/api/v1/preview/inputs/cam1.jpg"
        label="Input cam1"
        capabilities={caps({
          scopes: {
            program: { whep: true },
            inputs: { whep: false },
            outputs: { whep: true },
          },
        })}
      />,
    );
    expect(getByTestId('jpeg-rung')).toBeInTheDocument();
    expect(getByTestId('still-preview-label')).toHaveTextContent('Still preview (~1 fps)');
    expect(queryByTestId('input-preview-unavailable')).toBeNull();
    expect(queryByTestId('fallback-badge')).toBeNull();
  });

  it('mounts the WHEP player when the scope advertises whep', () => {
    const { getByTestId, queryByTestId } = renderWithProviders(
      <PreviewSurface
        scope="program"
        whepEndpoint="/api/v1/preview/program/whep"
        jpegPath="/api/v1/preview/program.jpg"
        label="Program"
        capabilities={caps()}
        pcFactory={() => new IdlePeerConnection() as unknown as RTCPeerConnection}
        fetchImpl={(): Promise<Response> => Promise.resolve(okSdp())}
      />,
    );
    expect(getByTestId('whep-player')).toBeInTheDocument();
    expect(queryByTestId('jpeg-rung')).toBeNull();
  });

  it('renders fallback diagnostics into the supplied host behind one disclosure, not beside the picture', async () => {
    const host = document.createElement('div');
    document.body.appendChild(host);
    const { getByTestId } = renderWithProviders(
      <PreviewSurface
        scope="program"
        whepEndpoint="/api/v1/preview/program/whep"
        jpegPath="/api/v1/preview/program.jpg"
        label="Program"
        capabilities={caps()}
        diagnosticsHost={host}
        pcFactory={() => new IdlePeerConnection() as unknown as RTCPeerConnection}
        fetchImpl={(): Promise<Response> => Promise.resolve(rejectSdp())}
      />,
    );
    await waitFor(() => { expect(getByTestId('fallback-badge')).toBeInTheDocument(); });
    await waitFor(() => { expect(host.querySelector('[data-testid="fallback-diagnostics"]')).not.toBeNull(); });
    expect(host.querySelectorAll('details.ps-diagnostics')).toHaveLength(1);
    expect(getByTestId('jpeg-rung').querySelector('[data-testid="fallback-diagnostics"]')).toBeNull();
    host.remove();
  });

  it('degrades WHEP→JPEG WITH the honest badge on a fatal session error, and retries', async () => {
    const { getByTestId, queryByTestId, getByRole } = renderWithProviders(
      <PreviewSurface
        scope="program"
        whepEndpoint="/api/v1/preview/program/whep"
        jpegPath="/api/v1/preview/program.jpg"
        label="Program"
        capabilities={caps()}
        pcFactory={() => new IdlePeerConnection() as unknown as RTCPeerConnection}
        fetchImpl={(): Promise<Response> => Promise.resolve(rejectSdp())}
      />,
    );
    // The WHEP POST is rejected ⇒ the surface degrades to JPEG with the badge.
    await waitFor(() => {
      expect(getByTestId('fallback-badge')).toBeInTheDocument();
    });
    expect(getByTestId('jpeg-rung')).toBeInTheDocument();

    // Wait for the rejected POST and player cleanup before retrying.
    await waitFor(() => expect(getByRole('button', { name: /retry live preview/i })).toBeInTheDocument());
    act(() => {
      fireEvent.click(getByRole('button', { name: /retry live preview/i }));
    });
    expect(getByTestId('whep-player')).toBeInTheDocument();
    expect(queryByTestId('fallback-badge')).toBeNull();
  });

  function capacityRefusal(): Response {
    return {
      ok: false,
      status: 503,
      url: 'https://h/api/v1/preview/program/whep',
      headers: new Headers(),
      text: (): Promise<string> => Promise.resolve(''),
      clone(): Response {
        return { json: (): Promise<unknown> => Promise.resolve({ type: 'urn:conspect:problem:whep-capacity', fallback: 'jpeg' }) } as unknown as Response;
      },
    } as unknown as Response;
  }

  it('says why live video is unavailable on a whep-capacity refusal, then retries on a ~10 s backoff, not while hidden', async () => {
    vi.useFakeTimers();
    try {
      const fetchImpl = vi.fn((): Promise<Response> => Promise.resolve(capacityRefusal()));
      const { getByTestId } = renderWithProviders(
        <PreviewSurface
          scope="program"
          whepEndpoint="/api/v1/preview/program/whep"
          jpegPath="/api/v1/preview/program.jpg"
          label="Program"
          capabilities={caps()}
          pcFactory={() => new IdlePeerConnection() as unknown as RTCPeerConnection}
          fetchImpl={fetchImpl}
        />,
      );
      await act(async () => { await vi.advanceTimersByTimeAsync(100); });
      expect(getByTestId('fallback-reason')).toHaveTextContent('Live video in use by another viewer — showing stills');
      expect(fetchImpl).toHaveBeenCalledTimes(1);
      await act(async () => { await vi.advanceTimersByTimeAsync(9000); });
      expect(fetchImpl).toHaveBeenCalledTimes(1);
      await act(async () => { await vi.advanceTimersByTimeAsync(1500); });
      expect(fetchImpl).toHaveBeenCalledTimes(2); // ~10 s
      // Second refusal backs off further (15 s) and is capped at 30 s.
      await act(async () => { await vi.advanceTimersByTimeAsync(100); });
      let visibility: DocumentVisibilityState = 'hidden';
      vi.spyOn(document, 'visibilityState', 'get').mockImplementation(() => visibility);
      await act(async () => { await vi.advanceTimersByTimeAsync(60_000); });
      expect(fetchImpl).toHaveBeenCalledTimes(2); // hidden: no retry storm
      visibility = 'visible';
      await act(async () => { document.dispatchEvent(new Event('visibilitychange')); await vi.advanceTimersByTimeAsync(100); });
      expect(fetchImpl).toHaveBeenCalledTimes(3);
    } finally {
      vi.useRealTimers();
    }
  });

  it('caps the capacity backoff', () => {
    expect(capacityRetryDelayMs(0)).toBe(10_000);
    expect(capacityRetryDelayMs(1)).toBe(15_000);
    expect(capacityRetryDelayMs(20)).toBe(30_000);
  });
});
