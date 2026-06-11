// Tests for the tally hooks (MSW-mocked HTTP). These assert real behaviour: the
// resolved-state read sends the token; setting an override PUTs the target+color
// and returns the 202 operation id; and a profile replace pre-reads the ETag and
// sends it as `If-Match`.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { renderHook, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import type { ReactNode } from 'react';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';

import {
  useSaveProfile,
  useTally,
  useTallyOverride,
} from './tallyQueries';
import type { TallyEntry, TallyProfile } from './tallyQueries';

const BASE = 'http://localhost';

let lastAuth: string | null = null;
let overrideMethod = '';
let overrideBody: unknown = null;
let putIfMatch: string | null = null;

const entry: TallyEntry = {
  target: { kind: 'tile', index: 0 },
  state: { color: 'Red', brightness: 3, source: { kind: 'program' } },
};

const server = setupServer(
  http.get(`${BASE}/api/v1/tally`, ({ request }) => {
    lastAuth = request.headers.get('Authorization');
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
  overrideMethod = '';
  overrideBody = null;
  putIfMatch = null;
});
afterAll(() => {
  server.close();
});

describe('useTally', () => {
  it('GETs /api/v1/tally with the bearer token', async () => {
    const qc = newClient();
    const { result } = renderHook(() => useTally({ baseUrl: BASE, token: 'tok-t' }), {
      wrapper: wrapper(qc),
    });
    await waitFor(() => {
      expect(result.current.isSuccess).toBe(true);
    });
    expect(result.current.data?.[0]?.state.color).toBe('Red');
    expect(lastAuth).toBe('Bearer tok-t');
  });
});

describe('useTallyOverride', () => {
  it('PUTs the target + colour and returns the 202 operation id', async () => {
    server.use(
      http.put(`${BASE}/api/v1/tally/override`, async ({ request }) => {
        overrideMethod = request.method;
        overrideBody = await request.json();
        return HttpResponse.json(
          { operation_id: 'op-tally', kind: 'set_override' },
          { status: 202 },
        );
      }),
    );
    const qc = newClient();
    const { result } = renderHook(() => useTallyOverride({ baseUrl: BASE }), {
      wrapper: wrapper(qc),
    });
    const accepted = await result.current.mutateAsync({
      action: 'set',
      target: { kind: 'tile', index: 2 },
      color: 'Amber',
    });
    expect(accepted.operation_id).toBe('op-tally');
    expect(overrideMethod).toBe('PUT');
    expect(overrideBody).toEqual({
      target: { kind: 'tile', index: 2 },
      color: 'Amber',
    });
  });
});

describe('useSaveProfile', () => {
  it('pre-reads the ETag then sends it as If-Match on a replace', async () => {
    server.use(
      http.get(`${BASE}/api/v1/tally/profiles/:id`, () =>
        HttpResponse.json({ id: 'p1' } satisfies TallyProfile, {
          headers: { ETag: 'W/"7"' },
        }),
      ),
      http.put(`${BASE}/api/v1/tally/profiles/:id`, async ({ request }) => {
        putIfMatch = request.headers.get('If-Match');
        const body = (await request.json()) as TallyProfile;
        return HttpResponse.json(body, { headers: { ETag: 'W/"8"' } });
      }),
    );
    const qc = newClient();
    const { result } = renderHook(() => useSaveProfile({ baseUrl: BASE }), {
      wrapper: wrapper(qc),
    });
    await result.current.mutateAsync({ profile: { id: 'p1' }, create: false });
    expect(putIfMatch).toBe('W/"7"');
  });
});
