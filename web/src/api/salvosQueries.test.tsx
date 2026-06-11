// Tests for the salvo hooks (MSW-mocked HTTP). These assert real behaviour: the
// list read sends the token; an arm POSTs to .../arm and returns the 202
// operation id; and a replace pre-reads the ETag via GET, then sends it as
// `If-Match` on the PUT.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { renderHook, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import type { ReactNode } from 'react';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';

import {
  useSalvoOperation,
  useSalvos,
  useSaveSalvo,
} from './salvosQueries';
import type { Salvo } from './salvosQueries';

const BASE = 'http://localhost';

let lastAuth: string | null = null;
let armPath = '';
let putIfMatch: string | null = null;
let getCount = 0;

const server = setupServer(
  http.get(`${BASE}/api/v1/salvos`, ({ request }) => {
    lastAuth = request.headers.get('Authorization');
    const salvo: Salvo = { id: 's1', display_name: 'Wide' };
    return HttpResponse.json([salvo]);
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
  armPath = '';
  putIfMatch = null;
  getCount = 0;
});
afterAll(() => {
  server.close();
});

describe('useSalvos', () => {
  it('GETs /api/v1/salvos with the bearer token', async () => {
    const qc = newClient();
    const { result } = renderHook(() => useSalvos({ baseUrl: BASE, token: 'tok-1' }), {
      wrapper: wrapper(qc),
    });
    await waitFor(() => {
      expect(result.current.isSuccess).toBe(true);
    });
    expect(result.current.data?.[0]?.id).toBe('s1');
    expect(lastAuth).toBe('Bearer tok-1');
  });
});

describe('useSalvoOperation', () => {
  it('POSTs .../arm and returns the 202 operation id', async () => {
    server.use(
      http.post(`${BASE}/api/v1/salvos/:id/arm`, ({ request }) => {
        armPath = new URL(request.url).pathname;
        lastAuth = request.headers.get('Authorization');
        return HttpResponse.json(
          { operation_id: 'op-77', kind: 'arm_salvo' },
          { status: 202 },
        );
      }),
    );
    const qc = newClient();
    const { result } = renderHook(() => useSalvoOperation({ baseUrl: BASE, token: 'tok-2' }), {
      wrapper: wrapper(qc),
    });
    const accepted = await result.current.mutateAsync({ id: 's1', action: 'arm' });
    expect(accepted.operation_id).toBe('op-77');
    expect(armPath).toBe('/api/v1/salvos/s1/arm');
    expect(lastAuth).toBe('Bearer tok-2');
  });
});

describe('useSaveSalvo', () => {
  it('pre-reads the ETag then sends it as If-Match on a replace PUT', async () => {
    server.use(
      http.get(`${BASE}/api/v1/salvos/:id`, () => {
        getCount += 1;
        return HttpResponse.json({ id: 's1', display_name: 'Wide' }, {
          headers: { ETag: 'W/"3"' },
        });
      }),
      http.put(`${BASE}/api/v1/salvos/:id`, async ({ request }) => {
        putIfMatch = request.headers.get('If-Match');
        const body = (await request.json()) as Salvo;
        return HttpResponse.json(body, { headers: { ETag: 'W/"4"' } });
      }),
    );
    const qc = newClient();
    const { result } = renderHook(() => useSaveSalvo({ baseUrl: BASE }), {
      wrapper: wrapper(qc),
    });
    await result.current.mutateAsync({
      salvo: { id: 's1', display_name: 'Wider' },
      create: false,
    });
    expect(getCount).toBe(1);
    expect(putIfMatch).toBe('W/"3"');
  });
});
