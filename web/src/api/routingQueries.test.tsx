// Tests for the routing hooks (MSW-mocked HTTP). These assert real behaviour:
// plan POSTs the crosspoint to /routing/plan, take POSTs to /routing/{kind}/take
// with a fresh Idempotency-Key, and a take branches on the 200 (hot) vs 202
// (migration) status.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { renderHook, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import type { ReactNode } from 'react';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';

import { usePlanRoute, useTakeRoute } from './routingQueries';
import type { RouteTakeRequest } from './routingQueries';

const BASE = 'http://localhost';

let lastPlanBody: unknown = null;
let lastTakeUrl = '';
let lastIdempotencyKey: string | null = null;

const request: RouteTakeRequest = {
  source: { input_id: 'cam-north', kind: { kind: 'video' } },
  target: { kind: 'video_cell', cell: 'cell-a' },
};

// The take handler returns a hot (200) result by default; one test flips it to a
// 202 migration to exercise the accepted branch.
let takeStatus: 200 | 202 = 200;

const server = setupServer(
  http.post(`${BASE}/api/v1/routing/plan`, async ({ request: req }) => {
    lastPlanBody = await req.json();
    return HttpResponse.json({ class: 'class2', coerced: false });
  }),
  http.post(`${BASE}/api/v1/routing/:kind/take`, ({ request: req }) => {
    lastTakeUrl = req.url;
    lastIdempotencyKey = req.headers.get('Idempotency-Key');
    if (takeStatus === 202) {
      return HttpResponse.json(
        { kind: 'take', operation_id: 'op-take-1' },
        { status: 202 },
      );
    }
    return HttpResponse.json(
      { applied: true, class: 'class1', coerced: false, operation_id: 'op-take-0' },
      { status: 200 },
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
  lastPlanBody = null;
  lastTakeUrl = '';
  lastIdempotencyKey = null;
  takeStatus = 200;
});
afterAll(() => {
  server.close();
});

describe('usePlanRoute', () => {
  it('POSTs the crosspoint to /routing/plan and returns the class', async () => {
    const qc = newClient();
    const { result } = renderHook(() => usePlanRoute({ baseUrl: BASE }), {
      wrapper: wrapper(qc),
    });
    result.current.mutate(request);
    await waitFor(() => {
      expect(result.current.isSuccess).toBe(true);
    });
    expect(result.current.data?.class).toBe('class2');
    expect(lastPlanBody).toEqual(request);
  });
});

describe('useTakeRoute', () => {
  it('POSTs to /routing/{kind}/take with an Idempotency-Key and reports a hot take', async () => {
    const qc = newClient();
    const { result } = renderHook(() => useTakeRoute({ baseUrl: BASE }), {
      wrapper: wrapper(qc),
    });
    result.current.mutate({ kind: 'video', request });
    await waitFor(() => {
      expect(result.current.isSuccess).toBe(true);
    });
    expect(new URL(lastTakeUrl).pathname).toBe('/api/v1/routing/video/take');
    expect(lastIdempotencyKey).not.toBeNull();
    expect(lastIdempotencyKey).not.toBe('');
    const outcome = result.current.data;
    expect(outcome?.status).toBe('applied');
    if (outcome?.status === 'applied') {
      expect(outcome.applied.class).toBe('class1');
    }
  });

  it('reports a Class-2 migration as accepted with an operation id (202)', async () => {
    takeStatus = 202;
    const qc = newClient();
    const { result } = renderHook(() => useTakeRoute({ baseUrl: BASE }), {
      wrapper: wrapper(qc),
    });
    result.current.mutate({ kind: 'audio', request });
    await waitFor(() => {
      expect(result.current.isSuccess).toBe(true);
    });
    const outcome = result.current.data;
    expect(outcome?.status).toBe('accepted');
    if (outcome?.status === 'accepted') {
      expect(outcome.accepted.operation_id).toBe('op-take-1');
    }
  });
});
