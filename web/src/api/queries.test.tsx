// Tests for the layouts query + mutation hooks, with the HTTP layer mocked by
// MSW. These assert real behaviour: list reads decode the typed body, saves and
// deletes apply optimistic cache updates and roll back on error, and update/
// delete echo the stored ETag as `If-Match`.
//
// Write ops go through the typed openapi-fetch client and hit the spec-correct
// paths: `POST /api/v1/layouts/{id}` (create), `PUT /api/v1/layouts/{id}`
// (update), `DELETE /api/v1/layouts/{id}` (delete).  The id is caller-supplied
// on create, matching the control-plane spec.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { renderHook, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import type { ReactNode } from 'react';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';

import { createApiClient } from './client';
import {
  queryKeys,
  readEtags,
  useDeleteLayout,
  useLayouts,
  useSaveLayout,
} from './queries';
import type { Layout } from './queries';

const BASE = 'http://localhost';

let store: Layout[] = [];
let lastIfMatch: string | null = null;

const server = setupServer(
  http.get(`${BASE}/api/v1/layouts`, () => HttpResponse.json(store)),

  // CREATE — spec: POST /api/v1/layouts/{id} (id in path, caller-supplied)
  http.post(`${BASE}/api/v1/layouts/:id`, async ({ request, params }) => {
    const body = (await request.json()) as { name: string; body: unknown };
    const id = String(params.id);
    const created: Layout = { id, name: body.name, body: body.body };
    store = [...store, created];
    return HttpResponse.json(created, { status: 201, headers: { ETag: '"v1"' } });
  }),

  // UPDATE — spec: PUT /api/v1/layouts/{id}
  http.put(`${BASE}/api/v1/layouts/:id`, async ({ request, params }) => {
    lastIfMatch = request.headers.get('If-Match');
    const body = (await request.json()) as { name: string; body: unknown };
    const id = String(params.id);
    const updated: Layout = { id, name: body.name, body: body.body };
    store = store.map((l) => (l.id === id ? updated : l));
    return HttpResponse.json(updated, { headers: { ETag: '"v2"' } });
  }),

  // DELETE — spec: DELETE /api/v1/layouts/{id}
  http.delete(`${BASE}/api/v1/layouts/:id`, ({ request, params }) => {
    lastIfMatch = request.headers.get('If-Match');
    const id = String(params.id);
    store = store.filter((l) => l.id !== id);
    return new HttpResponse(null, { status: 204 });
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
  store = [];
  lastIfMatch = null;
});
afterAll(() => {
  server.close();
});

describe('useLayouts', () => {
  it('reads the typed layouts list', async () => {
    store = [{ id: 'a', name: 'Studio A', body: {} }];
    const qc = newClient();
    const api = createApiClient({ baseUrl: BASE });
    const { result } = renderHook(() => useLayouts(api), { wrapper: wrapper(qc) });
    await waitFor(() => {
      expect(result.current.isSuccess).toBe(true);
    });
    expect(result.current.data).toHaveLength(1);
    expect(result.current.data?.[0]?.name).toBe('Studio A');
  });

  it('surfaces an RFC 9457 problem as an ApiError', async () => {
    server.use(
      http.get(`${BASE}/api/v1/layouts`, () =>
        HttpResponse.json(
          { type: '/problems/unauthorized', title: 'Unauthorized', status: 401 },
          { status: 401 },
        ),
      ),
    );
    const qc = newClient();
    const api = createApiClient({ baseUrl: BASE });
    const { result } = renderHook(() => useLayouts(api), { wrapper: wrapper(qc) });
    await waitFor(() => {
      expect(result.current.isError).toBe(true);
    });
    expect(result.current.error?.status).toBe(401);
    expect(result.current.error?.message).toBe('Unauthorized');
  });
});

describe('useSaveLayout', () => {
  it('creates a layout (POST /api/v1/layouts/{id}) and stores its ETag', async () => {
    const qc = newClient();
    const api = createApiClient({ baseUrl: BASE });
    const { result } = renderHook(() => useSaveLayout({ api }), {
      wrapper: wrapper(qc),
    });
    // The spec requires the id on create — the caller nominates it.
    const saved = await result.current.mutateAsync({
      id: 'srv-0',
      input: { name: 'New', body: { schema_version: 1 } },
    });
    expect(saved.id).toBe('srv-0');
    expect(readEtags(qc)[saved.id]).toBe('"v1"');
  });

  it('sends the stored ETag as If-Match on update', async () => {
    const qc = newClient();
    qc.setQueryData(queryKeys.layouts, [{ id: 'a', name: 'Old', body: {} }]);
    qc.setQueryData(queryKeys.etags, { a: '"v1"' });
    store = [{ id: 'a', name: 'Old', body: {} }];
    const api = createApiClient({ baseUrl: BASE });
    const { result } = renderHook(() => useSaveLayout({ api }), {
      wrapper: wrapper(qc),
    });
    await result.current.mutateAsync({ id: 'a', input: { name: 'Renamed', body: {} } });
    expect(lastIfMatch).toBe('"v1"');
    expect(readEtags(qc).a).toBe('"v2"');
  });

  it('rolls back the optimistic list when the server rejects the save', async () => {
    server.use(
      http.post(`${BASE}/api/v1/layouts/:id`, () =>
        HttpResponse.json(
          { type: '/problems/conflict', title: 'Conflict', status: 409 },
          { status: 409 },
        ),
      ),
    );
    const qc = newClient();
    qc.setQueryData(queryKeys.layouts, [{ id: 'a', name: 'Existing', body: {} }]);
    const api = createApiClient({ baseUrl: BASE });
    const { result } = renderHook(() => useSaveLayout({ api }), {
      wrapper: wrapper(qc),
    });
    await expect(
      result.current.mutateAsync({ id: 'new-id', input: { name: 'Doomed', body: {} } }),
    ).rejects.toMatchObject({ status: 409 });
    const list = qc.getQueryData<Layout[]>(queryKeys.layouts);
    expect(list).toHaveLength(1);
    expect(list?.[0]?.name).toBe('Existing');
  });
});

describe('useDeleteLayout', () => {
  it('optimistically removes a layout and sends If-Match', async () => {
    const qc = newClient();
    qc.setQueryData(queryKeys.layouts, [
      { id: 'a', name: 'Keep', body: {} },
      { id: 'b', name: 'Drop', body: {} },
    ]);
    qc.setQueryData(queryKeys.etags, { b: '"v9"' });
    store = [
      { id: 'a', name: 'Keep', body: {} },
      { id: 'b', name: 'Drop', body: {} },
    ];
    const api = createApiClient({ baseUrl: BASE });
    const { result } = renderHook(() => useDeleteLayout({ api }), {
      wrapper: wrapper(qc),
    });
    await result.current.mutateAsync('b');
    expect(lastIfMatch).toBe('"v9"');
    const list = qc.getQueryData<Layout[]>(queryKeys.layouts);
    expect(list?.map((l) => l.id)).toEqual(['a']);
  });
});
