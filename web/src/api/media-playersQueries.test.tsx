// Tests for the media-player transport hooks (MSW-mocked HTTP). These assert
// real behaviour against the regenerated OpenAPI surface (`/api/v1/media/players`):
//   * the list read sends the bearer token and returns the configured players;
//   * `load`/`cue`/`seek` POST to the right verb path with a JSON `TransportBody`
//     (`asset` / `frame`) and surface the `202` operation id;
//   * `play`/`pause`/`stop` and `exit/arm|take|cancel` POST to their verb path
//     with NO body and surface the `202` operation id.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { renderHook, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import type { ReactNode } from 'react';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';

import {
  useMediaPlayerTransport,
  useMediaPlayers,
} from './media-playersQueries';
import type { MediaPlayer } from './media-playersQueries';

const BASE = 'http://localhost';

let lastAuth: string | null = null;
let lastPath = '';
let lastBody: unknown = undefined;

const PLAYER: MediaPlayer = {
  id: 'vt-1',
  name: 'VT 1',
  body: { id: 'vt-1', default_asset: 'opener' },
};

const server = setupServer(
  http.get(`${BASE}/api/v1/media/players`, ({ request }) => {
    lastAuth = request.headers.get('Authorization');
    return HttpResponse.json([PLAYER]);
  }),
);

function wrapper(client: QueryClient): (props: { children: ReactNode }) => ReactNode {
  return function Wrapper({ children }: { children: ReactNode }): ReactNode {
    return <QueryClientProvider client={client}>{children}</QueryClientProvider>;
  };
}

function newClient(): QueryClient {
  return new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
}

/** Register a 202-returning handler for one media-player verb sub-path. */
function acceptVerb(suffix: string, kind: string): void {
  server.use(
    http.post(`${BASE}/api/v1/media/players/:id/${suffix}`, async ({ request }) => {
      lastPath = new URL(request.url).pathname;
      lastAuth = request.headers.get('Authorization');
      const text = await request.text();
      lastBody = text === '' ? undefined : (JSON.parse(text) as unknown);
      return HttpResponse.json({ operation_id: `op-${kind}`, kind }, { status: 202 });
    }),
  );
}

beforeAll(() => {
  server.listen({ onUnhandledRequest: 'error' });
});
afterEach(() => {
  server.resetHandlers();
  lastAuth = null;
  lastPath = '';
  lastBody = undefined;
});
afterAll(() => {
  server.close();
});

describe('useMediaPlayers', () => {
  it('GETs /api/v1/media/players with the bearer token', async () => {
    const qc = newClient();
    const { result } = renderHook(
      () => useMediaPlayers({ baseUrl: BASE, token: 'tok-1' }),
      { wrapper: wrapper(qc) },
    );
    await waitFor(() => {
      expect(result.current.isSuccess).toBe(true);
    });
    expect(result.current.data?.[0]?.id).toBe('vt-1');
    expect(result.current.data?.[0]?.name).toBe('VT 1');
    expect(lastAuth).toBe('Bearer tok-1');
  });
});

describe('useMediaPlayerTransport — bodyless verbs', () => {
  it.each([
    ['play', 'play', 'play_player'],
    ['pause', 'pause', 'pause_player'],
    ['stop', 'stop', 'stop_player'],
    ['arm-exit', 'exit/arm', 'arm_exit'],
    ['take-exit', 'exit/take', 'take_exit'],
    ['cancel-exit', 'exit/cancel', 'cancel_exit'],
  ] as const)(
    'POSTs %s to the right path with no body and returns the 202 op id',
    async (action, suffix, kind) => {
      acceptVerb(suffix, kind);
      const qc = newClient();
      const { result } = renderHook(
        () => useMediaPlayerTransport({ baseUrl: BASE, token: 'tok-2' }),
        { wrapper: wrapper(qc) },
      );
      const accepted = await result.current.mutateAsync({ id: 'vt-1', action });
      expect(accepted.operation_id).toBe(`op-${kind}`);
      expect(accepted.kind).toBe(kind);
      expect(lastPath).toBe(`/api/v1/media/players/vt-1/${suffix}`);
      expect(lastAuth).toBe('Bearer tok-2');
      // Bodyless verbs send no JSON body.
      expect(lastBody).toBeUndefined();
    },
  );
});

describe('useMediaPlayerTransport — load carries an asset', () => {
  it('POSTs .../load with the asset in the TransportBody', async () => {
    acceptVerb('load', 'load_player');
    const qc = newClient();
    const { result } = renderHook(
      () => useMediaPlayerTransport({ baseUrl: BASE }),
      { wrapper: wrapper(qc) },
    );
    const accepted = await result.current.mutateAsync({
      id: 'vt-1',
      action: 'load',
      asset: 'opener',
    });
    expect(accepted.operation_id).toBe('op-load_player');
    expect(lastPath).toBe('/api/v1/media/players/vt-1/load');
    expect(lastBody).toEqual({ asset: 'opener' });
  });
});

describe('useMediaPlayerTransport — cue/seek carry a frame', () => {
  it('POSTs .../cue with the frame in the TransportBody', async () => {
    acceptVerb('cue', 'cue_player');
    const qc = newClient();
    const { result } = renderHook(
      () => useMediaPlayerTransport({ baseUrl: BASE }),
      { wrapper: wrapper(qc) },
    );
    await result.current.mutateAsync({ id: 'vt-1', action: 'cue', frame: 120 });
    expect(lastPath).toBe('/api/v1/media/players/vt-1/cue');
    expect(lastBody).toEqual({ frame: 120 });
  });

  it('POSTs .../cue with an EMPTY body when no frame is given (cue to in-point)', async () => {
    acceptVerb('cue', 'cue_player');
    const qc = newClient();
    const { result } = renderHook(
      () => useMediaPlayerTransport({ baseUrl: BASE }),
      { wrapper: wrapper(qc) },
    );
    await result.current.mutateAsync({ id: 'vt-1', action: 'cue' });
    expect(lastPath).toBe('/api/v1/media/players/vt-1/cue');
    // An absent frame cues to the in-point: an empty JSON object, no `frame`.
    expect(lastBody).toEqual({});
  });

  it('POSTs .../seek with the frame in the TransportBody', async () => {
    acceptVerb('seek', 'seek_player');
    const qc = newClient();
    const { result } = renderHook(
      () => useMediaPlayerTransport({ baseUrl: BASE }),
      { wrapper: wrapper(qc) },
    );
    await result.current.mutateAsync({ id: 'vt-1', action: 'seek', frame: 0 });
    expect(lastPath).toBe('/api/v1/media/players/vt-1/seek');
    expect(lastBody).toEqual({ frame: 0 });
  });
});
