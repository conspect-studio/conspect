// Tests for the cast-session hooks (MSW-mocked HTTP). These assert real
// behaviour: the list GETs /api/v1/cast/sessions; start POSTs (201); stop
// DELETEs (204); save POSTs and returns the device resource (201); volume POSTs
// and returns the 202 operation id.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { renderHook, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import type { ReactNode } from 'react';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';

import {
  useCastSessions,
  useSaveCastSession,
  useSetCastVolume,
  useStartCastSession,
  useStopCastSession,
} from './cast-sessionsQueries';
import type { CastSession } from './cast-sessionsQueries';

const BASE = 'http://localhost';

let lastStartBody: unknown = null;
let lastSaveBody: unknown = null;
let lastVolumeBody: unknown = null;
let lastDeletedId: string | null = null;

const session: CastSession = {
  address: '[2001:db8::5]:8009',
  id: 'cast-session-1',
  media_url: 'http://[2001:db8::1]:8080/hls/program.m3u8',
  name: 'Lobby TV',
  output: 'hls-main',
  state: 'ONLINE',
};

const server = setupServer(
  http.get(`${BASE}/api/v1/cast/sessions`, () => HttpResponse.json([session])),
  http.post(`${BASE}/api/v1/cast/sessions`, async ({ request }) => {
    lastStartBody = await request.json();
    return HttpResponse.json(session, { status: 201 });
  }),
  http.delete(`${BASE}/api/v1/cast/sessions/:id`, ({ params }) => {
    lastDeletedId = String(params.id);
    return new HttpResponse(null, { status: 204 });
  }),
  http.post(`${BASE}/api/v1/cast/sessions/:id/save`, async ({ request }) => {
    lastSaveBody = await request.json();
    return HttpResponse.json(
      { id: 'cast-lobby', name: 'Lobby TV', body: {} },
      { status: 201 },
    );
  }),
  http.post(`${BASE}/api/v1/cast/sessions/:id/volume`, async ({ request }) => {
    lastVolumeBody = await request.json();
    return HttpResponse.json(
      { kind: 'cast-volume', operation_id: 'op-vol-1' },
      { status: 202 },
    );
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

beforeAll(() => {
  server.listen({ onUnhandledRequest: 'error' });
});
afterEach(() => {
  server.resetHandlers();
  lastStartBody = null;
  lastSaveBody = null;
  lastVolumeBody = null;
  lastDeletedId = null;
});
afterAll(() => {
  server.close();
});

describe('useCastSessions', () => {
  it('GETs the live session list', async () => {
    const qc = newClient();
    const { result } = renderHook(
      () => useCastSessions({ baseUrl: BASE, refetchInterval: false }),
      { wrapper: wrapper(qc) },
    );
    await waitFor(() => {
      expect(result.current.isSuccess).toBe(true);
    });
    expect(result.current.data?.[0]?.id).toBe('cast-session-1');
    expect(result.current.data?.[0]?.state).toBe('ONLINE');
  });
});

describe('cast mutations', () => {
  it('starts a session with POST (201) and echoes the body', async () => {
    const qc = newClient();
    const { result } = renderHook(() => useStartCastSession({ baseUrl: BASE }), {
      wrapper: wrapper(qc),
    });
    result.current.mutate({ address: '[2001:db8::5]:8009', name: 'Lobby TV', output: 'hls-main' });
    await waitFor(() => {
      expect(result.current.isSuccess).toBe(true);
    });
    expect(lastStartBody).toEqual({
      address: '[2001:db8::5]:8009',
      name: 'Lobby TV',
      output: 'hls-main',
    });
    expect(result.current.data?.id).toBe('cast-session-1');
  });

  it('stops a session with DELETE (204)', async () => {
    const qc = newClient();
    const { result } = renderHook(() => useStopCastSession({ baseUrl: BASE }), {
      wrapper: wrapper(qc),
    });
    result.current.mutate('cast-session-1');
    await waitFor(() => {
      expect(result.current.isSuccess).toBe(true);
    });
    expect(lastDeletedId).toBe('cast-session-1');
  });

  it('promotes a session to a device with POST save (201)', async () => {
    const qc = newClient();
    const { result } = renderHook(() => useSaveCastSession({ baseUrl: BASE }), {
      wrapper: wrapper(qc),
    });
    result.current.mutate({ id: 'cast-session-1', request: { device_id: 'cast-lobby' } });
    await waitFor(() => {
      expect(result.current.isSuccess).toBe(true);
    });
    expect(lastSaveBody).toEqual({ device_id: 'cast-lobby' });
    expect(result.current.data?.id).toBe('cast-lobby');
  });

  it('sets the volume with POST (202) and surfaces the operation id', async () => {
    const qc = newClient();
    const { result } = renderHook(() => useSetCastVolume({ baseUrl: BASE }), {
      wrapper: wrapper(qc),
    });
    result.current.mutate({ id: 'cast-session-1', request: { level_percent: 40 } });
    await waitFor(() => {
      expect(result.current.isSuccess).toBe(true);
    });
    expect(lastVolumeBody).toEqual({ level_percent: 40 });
    expect(result.current.data?.operation_id).toBe('op-vol-1');
  });
});
