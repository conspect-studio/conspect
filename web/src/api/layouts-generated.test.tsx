// TDD red: verify layouts CRUD goes through the typed openapi-fetch client
// (POST/PUT/DELETE on /api/v1/layouts/{id}) not a hand-written fetch.
//
// These tests intercept the spec-correct paths (`POST /api/v1/layouts/{id}`,
// `PUT /api/v1/layouts/{id}`, `DELETE /api/v1/layouts/{id}`) — the paths the
// generated schema declares. They FAIL until `useSaveLayout`/`useDeleteLayout`
// are rewritten to call the typed client instead of the bespoke fetch helpers.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { renderHook } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import type { ReactNode } from 'react';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';

import { createApiClient } from './client';
import {
  queryKeys,
  readEtags,
  useDeleteLayout,
  useSaveLayout,
} from './queries';
import type { Layout } from './queries';

const BASE = 'http://localhost';

let store: Layout[] = [];
let lastIfMatch: string | null = null;
let lastCreatedId: string | null = null;

// These handlers match the spec paths (`/api/v1/layouts/{id}` for write ops).
// The old bespoke helpers POST to `/api/v1/layouts` (collection), so any test
// that reaches this server before the refactor will get an unhandled-request
// error (the server is strict), confirming the test is genuinely red.
const server = setupServer(
  http.get(`${BASE}/api/v1/layouts`, () => HttpResponse.json(store)),

  // CREATE — spec: POST /api/v1/layouts/{id}  (id supplied by the caller)
  http.post(`${BASE}/api/v1/layouts/:id`, async ({ request, params }) => {
    const body = (await request.json()) as { name: string; body: unknown };
    const id = String(params.id);
    lastCreatedId = id;
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
  // Strict: any request not matched above fails the test, proving the typed
  // client is calling the right paths.
  server.listen({ onUnhandledRequest: 'error' });
});
afterEach(() => {
  server.resetHandlers();
  store = [];
  lastIfMatch = null;
  lastCreatedId = null;
});
afterAll(() => {
  server.close();
});

describe('useSaveLayout via generated typed client', () => {
  it('creates a layout at POST /api/v1/layouts/{id} (spec path) and stores its ETag', async () => {
    const qc = newClient();
    const api = createApiClient({ baseUrl: BASE });
    const { result } = renderHook(() => useSaveLayout({ api }), {
      wrapper: wrapper(qc),
    });
    const saved = await result.current.mutateAsync({
      id: 'new-layout-1',
      input: { name: 'Studio A', body: { schema_version: 1 } },
    });
    expect(saved.id).toBe('new-layout-1');
    expect(lastCreatedId).toBe('new-layout-1');
    expect(readEtags(qc)[saved.id]).toBe('"v1"');
  });

  it('sends the stored ETag as If-Match on update via PUT /api/v1/layouts/{id}', async () => {
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

  it('rolls back the optimistic list when the server rejects a create', async () => {
    server.use(
      http.post(`${BASE}/api/v1/layouts/:id`, () =>
        HttpResponse.json(
          { type: '/problems/conflict', title: 'Conflict', status: 409 },
          { status: 409 },
        ),
      ),
    );
    const qc = newClient();
    qc.setQueryData(queryKeys.layouts, [{ id: 'existing', name: 'Keep', body: {} }]);
    const api = createApiClient({ baseUrl: BASE });
    const { result } = renderHook(() => useSaveLayout({ api }), {
      wrapper: wrapper(qc),
    });
    await expect(
      result.current.mutateAsync({
        id: 'doomed',
        input: { name: 'Doomed', body: {} },
      }),
    ).rejects.toMatchObject({ status: 409 });
    const list = qc.getQueryData<Layout[]>(queryKeys.layouts);
    expect(list).toHaveLength(1);
    expect(list?.[0]?.name).toBe('Keep');
  });
});

describe('useDeleteLayout via generated typed client', () => {
  it('sends DELETE /api/v1/layouts/{id} with If-Match and removes optimistically', async () => {
    const qc = newClient();
    qc.setQueryData(queryKeys.layouts, [
      { id: 'keep', name: 'Keep', body: {} },
      { id: 'drop', name: 'Drop', body: {} },
    ]);
    qc.setQueryData(queryKeys.etags, { drop: '"v9"' });
    store = [
      { id: 'keep', name: 'Keep', body: {} },
      { id: 'drop', name: 'Drop', body: {} },
    ];
    const api = createApiClient({ baseUrl: BASE });
    const { result } = renderHook(() => useDeleteLayout({ api }), {
      wrapper: wrapper(qc),
    });
    await result.current.mutateAsync('drop');
    expect(lastIfMatch).toBe('"v9"');
    const list = qc.getQueryData<Layout[]>(queryKeys.layouts);
    expect(list?.map((l) => l.id)).toEqual(['keep']);
  });

  it('treats 404 as idempotent success on delete', async () => {
    server.use(
      http.delete(`${BASE}/api/v1/layouts/:id`, () =>
        HttpResponse.json(
          { type: '/problems/not-found', title: 'Not found', status: 404 },
          { status: 404 },
        ),
      ),
    );
    const qc = newClient();
    qc.setQueryData(queryKeys.layouts, [{ id: 'gone', name: 'Gone', body: {} }]);
    const api = createApiClient({ baseUrl: BASE });
    const { result } = renderHook(() => useDeleteLayout({ api }), {
      wrapper: wrapper(qc),
    });
    // 404 on delete should resolve (idempotent), not reject.
    await expect(result.current.mutateAsync('gone')).resolves.toBeUndefined();
  });
});
