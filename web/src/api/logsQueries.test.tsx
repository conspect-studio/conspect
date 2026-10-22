// Tests for the logs hook (MSW-mocked HTTP). These assert real behaviour: the
// read GETs /api/v1/logs with the bearer token, threads the level + resource
// filters into the query string, and surfaces the returned records.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { renderHook, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import type { ReactNode } from 'react';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';

import { useLogs } from './logsQueries';
import type { LogRecord } from './logsQueries';

const BASE = 'http://localhost';

let lastAuth: string | null = null;
let lastUrl = '';

const record: LogRecord = {
  level: 'warn',
  message: 'tile fell back to last-good frame',
  seq: 42,
  target: 'multiview_engine',
  timestamp_ms: 1_700_000_000_000,
  resource_id: 'cam-north',
  resource_kind: 'source',
};

const server = setupServer(
  http.get(`${BASE}/api/v1/logs`, ({ request }) => {
    lastAuth = request.headers.get('Authorization');
    lastUrl = request.url;
    return HttpResponse.json([record]);
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
});
afterAll(() => {
  server.close();
});

describe('useLogs', () => {
  it('GETs /api/v1/logs with the bearer token', async () => {
    const qc = newClient();
    const { result } = renderHook(
      () => useLogs({}, { baseUrl: BASE, token: 'tok-a', refetchInterval: false }),
      { wrapper: wrapper(qc) },
    );
    await waitFor(() => {
      expect(result.current.isSuccess).toBe(true);
    });
    expect(result.current.data?.[0]?.message).toBe(
      'tile fell back to last-good frame',
    );
    expect(result.current.data?.[0]?.resource_id).toBe('cam-north');
    expect(lastAuth).toBe('Bearer tok-a');
    expect(new URL(lastUrl).pathname).toBe('/api/v1/logs');
  });

  it('threads the level + resource filters into the query string', async () => {
    const qc = newClient();
    const { result } = renderHook(
      () =>
        useLogs(
          { level: 'warn', kind: 'source', resourceId: 'cam-north', limit: 100 },
          { baseUrl: BASE, refetchInterval: false },
        ),
      { wrapper: wrapper(qc) },
    );
    await waitFor(() => {
      expect(result.current.isSuccess).toBe(true);
    });
    const url = new URL(lastUrl);
    expect(url.searchParams.get('level')).toBe('warn');
    expect(url.searchParams.get('kind')).toBe('source');
    expect(url.searchParams.get('resource_id')).toBe('cam-north');
    expect(url.searchParams.get('limit')).toBe('100');
  });
});
