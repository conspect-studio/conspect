// Tests for the audit hook (MSW-mocked HTTP). These assert real behaviour: the
// read GETs /api/v1/audit with the bearer token and threads an `object_id`
// filter through when given.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { renderHook, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import type { ReactNode } from 'react';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';

import { useAudit } from './auditQueries';
import type { AuditEntry } from './auditQueries';

const BASE = 'http://localhost';

let lastAuth: string | null = null;
let lastUrl = '';

const entry: AuditEntry = {
  action: 'create',
  actor: 'admin',
  at_nanos: 12_000_000,
  object_id: 'wall',
  object_kind: 'layout',
};

const server = setupServer(
  http.get(`${BASE}/api/v1/audit`, ({ request }) => {
    lastAuth = request.headers.get('Authorization');
    lastUrl = request.url;
    return HttpResponse.json([entry]);
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

describe('useAudit', () => {
  it('GETs /api/v1/audit with the bearer token', async () => {
    const qc = newClient();
    const { result } = renderHook(() => useAudit(undefined, { baseUrl: BASE, token: 'tok-a' }), {
      wrapper: wrapper(qc),
    });
    await waitFor(() => {
      expect(result.current.isSuccess).toBe(true);
    });
    expect(result.current.data?.[0]?.object_id).toBe('wall');
    expect(lastAuth).toBe('Bearer tok-a');
    expect(new URL(lastUrl).pathname).toBe('/api/v1/audit');
  });

  it('threads an object_id filter into the query string', async () => {
    const qc = newClient();
    const { result } = renderHook(() => useAudit('wall', { baseUrl: BASE }), {
      wrapper: wrapper(qc),
    });
    await waitFor(() => {
      expect(result.current.isSuccess).toBe(true);
    });
    expect(new URL(lastUrl).searchParams.get('object_id')).toBe('wall');
  });
});
