// Tests for the alarm hooks, with the HTTP layer mocked by MSW. These assert
// real behaviour: the list read sends the bearer token + the server-side filters,
// and an acknowledge sends `If-Match` and, on a 412 conflict, re-reads the
// current version from the problem `detail` and retries.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { renderHook, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import type { ReactNode } from 'react';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';

import { useAckAlarm, useAlarms } from './alarmsQueries';
import type { AlarmRecord } from './alarmsQueries';

const BASE = 'http://localhost';

function unacked(id: string): AlarmRecord {
  return {
    id,
    kind: 'Black',
    severity: 'Major',
    scope: { kind: 'tile', index: 0 },
    raised_at: 0,
    dwell: 0,
    latched: true,
    ack: { state: 'Unacked' },
  };
}

let lastAuth: string | null = null;
let lastUrl = '';
let ackAttempts: (string | null)[] = [];

const server = setupServer(
  http.get(`${BASE}/api/v1/alarms`, ({ request }) => {
    lastAuth = request.headers.get('Authorization');
    lastUrl = request.url;
    return HttpResponse.json([unacked('a1')]);
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
  lastAuth = null;
  lastUrl = '';
  ackAttempts = [];
});
afterAll(() => {
  server.close();
});

describe('useAlarms', () => {
  it('GETs /api/v1/alarms with the bearer token and severity filter', async () => {
    const qc = newClient();
    const { result } = renderHook(
      () => useAlarms({ severity: 'Major', active: true }, { baseUrl: BASE, token: 'tok-123' }),
      { wrapper: wrapper(qc) },
    );
    await waitFor(() => {
      expect(result.current.isSuccess).toBe(true);
    });
    expect(result.current.data?.[0]?.id).toBe('a1');
    expect(lastAuth).toBe('Bearer tok-123');
    const url = new URL(lastUrl);
    expect(url.pathname).toBe('/api/v1/alarms');
    expect(url.searchParams.get('severity')).toBe('major');
    expect(url.searchParams.get('active')).toBe('true');
  });
});

describe('useAckAlarm', () => {
  it('POSTs the ack with If-Match and the token', async () => {
    server.use(
      http.post(`${BASE}/api/v1/alarms/:id/ack`, ({ request }) => {
        ackAttempts.push(request.headers.get('If-Match'));
        lastAuth = request.headers.get('Authorization');
        const acked: AlarmRecord = {
          ...unacked('a1'),
          ack: { state: 'Acked', who: 'op', when: 1 },
        };
        return HttpResponse.json(acked, { headers: { ETag: 'W/"2"' } });
      }),
    );
    const qc = newClient();
    const { result } = renderHook(() => useAckAlarm({ baseUrl: BASE, token: 'tok-xyz' }), {
      wrapper: wrapper(qc),
    });
    const acked = await result.current.mutateAsync('a1');
    expect(acked.ack.state).toBe('Acked');
    expect(ackAttempts).toEqual(['W/"1"']);
    expect(lastAuth).toBe('Bearer tok-xyz');
  });

  it('retries with the current version parsed from a 412 detail', async () => {
    server.use(
      http.post(`${BASE}/api/v1/alarms/:id/ack`, ({ request }) => {
        const ifMatch = request.headers.get('If-Match');
        ackAttempts.push(ifMatch);
        if (ifMatch === 'W/"1"') {
          return HttpResponse.json(
            {
              type: '/problems/version-conflict',
              title: 'Precondition failed',
              status: 412,
              detail: 'alarm "a1" was modified: expected version 1, current is 4',
            },
            { status: 412 },
          );
        }
        const acked: AlarmRecord = {
          ...unacked('a1'),
          ack: { state: 'Acked', who: 'op', when: 1 },
        };
        return HttpResponse.json(acked, { headers: { ETag: 'W/"5"' } });
      }),
    );
    const qc = newClient();
    const { result } = renderHook(() => useAckAlarm({ baseUrl: BASE }), {
      wrapper: wrapper(qc),
    });
    const acked = await result.current.mutateAsync('a1');
    expect(acked.ack.state).toBe('Acked');
    // First attempt with the optimistic version, retry with the parsed current.
    expect(ackAttempts).toEqual(['W/"1"', 'W/"4"']);
  });
});
