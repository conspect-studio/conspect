// Tests for the input-streams hook (MSW-mocked HTTP). These assert real
// behaviour: the read GETs /api/v1/inputs/{id}/streams with the bearer token,
// stays disabled until an id is given, and surfaces the inventory.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { renderHook, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import type { ReactNode } from 'react';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';

import { useInputStreams } from './input-streamsQueries';
import type { StreamInventory } from './input-streamsQueries';

const BASE = 'http://localhost';

let lastUrl = '';
let lastAuth: string | null = null;

const inventory: StreamInventory = {
  input_id: 'cam-north',
  streams: [
    {
      kind: 'video',
      codec: 'h264',
      default: true,
      detail: { detail: 'video', params: { width: 1920, height: 1080 } },
      id: { key: 'v/pid:256', kind_scope: 'v', tier: 'hard' },
    },
    {
      kind: 'audio',
      codec: 'aac',
      default: true,
      detail: { detail: 'audio', params: { channels: 2, sample_rate: 48_000 } },
      id: { key: 'a/pid:257', kind_scope: 'a', tier: 'hard' },
      language: 'eng',
    },
  ],
};

const server = setupServer(
  http.get(`${BASE}/api/v1/inputs/:id/streams`, ({ request }) => {
    lastUrl = request.url;
    lastAuth = request.headers.get('Authorization');
    return HttpResponse.json(inventory);
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
  lastUrl = '';
  lastAuth = null;
});
afterAll(() => {
  server.close();
});

describe('useInputStreams', () => {
  it('GETs /api/v1/inputs/{id}/streams with the bearer token', async () => {
    const qc = newClient();
    const { result } = renderHook(
      () => useInputStreams('cam-north', { baseUrl: BASE, token: 'tok-a' }),
      { wrapper: wrapper(qc) },
    );
    await waitFor(() => {
      expect(result.current.isSuccess).toBe(true);
    });
    expect(result.current.data?.streams).toHaveLength(2);
    expect(result.current.data?.streams[0]?.codec).toBe('h264');
    expect(new URL(lastUrl).pathname).toBe('/api/v1/inputs/cam-north/streams');
    expect(lastAuth).toBe('Bearer tok-a');
  });

  it('stays disabled (does not fetch) until an input id is given', () => {
    const qc = newClient();
    const { result } = renderHook(() => useInputStreams(undefined, { baseUrl: BASE }), {
      wrapper: wrapper(qc),
    });
    // An empty/undefined id leaves the query disabled; it never fires a request.
    expect(result.current.fetchStatus).toBe('idle');
    expect(result.current.isPending).toBe(true);
    expect(lastUrl).toBe('');
  });
});
