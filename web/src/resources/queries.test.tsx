// Tests for the Sources/Outputs/Overlays resource hooks, with the HTTP layer
// mocked by MSW. These assert real behaviour: list reads project the opaque
// `{id,name,body}` records onto the typed view-models; create/update/delete call
// the right method + path with the per-kind config body the engine can load;
// update AND delete echo a fetched ETag as `If-Match` (the delete handler 428s
// without it, so a regression fails); every request carries the stored bearer
// token; and a server error surfaces as an `ApiError` with its status.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { renderHook, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import type { ReactNode } from 'react';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';

import { setStoredToken, clearStoredToken } from '../api/token';
import {
  ApiError,
  useDeleteResource,
  useOutputs,
  useOverlays,
  useSaveResource,
  useSources,
} from './queries';

const BASE = 'http://localhost';

interface Stored {
  id: string;
  name: string;
  body: Record<string, unknown>;
}

let sources: Stored[] = [];
let lastMethod: string | null = null;
let lastPath: string | null = null;
let lastIfMatch: string | null = null;
let lastBody: unknown = null;
let lastAuth: string | null = null;

const server = setupServer(
  // Sources collection + item.
  http.get(`${BASE}/api/v1/sources`, ({ request }) => {
    lastAuth = request.headers.get('Authorization');
    return HttpResponse.json(sources);
  }),
  http.get(`${BASE}/api/v1/sources/:id`, ({ request, params }) => {
    lastAuth = request.headers.get('Authorization');
    const found = sources.find((s) => s.id === String(params.id));
    if (found === undefined) {
      return HttpResponse.json(
        { type: '/problems/not-found', title: 'Not found', status: 404 },
        { status: 404 },
      );
    }
    return HttpResponse.json(found, { headers: { ETag: '"src-v1"' } });
  }),
  http.post(`${BASE}/api/v1/sources/:id`, async ({ request, params }) => {
    lastMethod = 'POST';
    lastPath = new URL(request.url).pathname;
    lastAuth = request.headers.get('Authorization');
    lastBody = await request.json();
    const id = String(params.id);
    const body = lastBody as { name: string; body: Record<string, unknown> };
    const created: Stored = { id, name: body.name, body: body.body };
    sources = [...sources, created];
    return HttpResponse.json(created, { status: 201, headers: { ETag: '"src-v2"' } });
  }),
  http.put(`${BASE}/api/v1/sources/:id`, async ({ request, params }) => {
    lastMethod = 'PUT';
    lastPath = new URL(request.url).pathname;
    lastIfMatch = request.headers.get('If-Match');
    lastAuth = request.headers.get('Authorization');
    lastBody = await request.json();
    const id = String(params.id);
    const body = lastBody as { name: string; body: Record<string, unknown> };
    const updated: Stored = { id, name: body.name, body: body.body };
    sources = sources.map((s) => (s.id === id ? updated : s));
    return HttpResponse.json(updated, { headers: { ETag: '"src-v3"' } });
  }),
  http.delete(`${BASE}/api/v1/sources/:id`, ({ request, params }) => {
    lastMethod = 'DELETE';
    lastPath = new URL(request.url).pathname;
    lastIfMatch = request.headers.get('If-Match');
    lastAuth = request.headers.get('Authorization');
    // The control plane requires `If-Match` on delete: 428 without it. The hook
    // must fetch the ETag first, so a missing header is a regression.
    if (lastIfMatch === null) {
      return HttpResponse.json(
        { type: '/problems/precondition-required', title: 'Precondition required', status: 428 },
        { status: 428 },
      );
    }
    const id = String(params.id);
    sources = sources.filter((s) => s.id !== id);
    return new HttpResponse(null, { status: 204 });
  }),
  // Outputs + overlays list endpoints (projection coverage).
  http.get(`${BASE}/api/v1/outputs`, () =>
    HttpResponse.json([
      { id: 'prog', name: 'Program', body: { kind: 'rtsp_server', mount: '/mv', codec: 'h264' } },
      { id: 'hls', name: 'HLS', body: { kind: 'll_hls', path: '/var/hls', codec: 'hevc' } },
    ]),
  ),
  http.post(`${BASE}/api/v1/outputs/:id`, async ({ request, params }) => {
    lastMethod = 'POST';
    lastPath = new URL(request.url).pathname;
    lastBody = await request.json();
    const id = String(params.id);
    const body = lastBody as { name: string; body: Record<string, unknown> };
    return HttpResponse.json(
      { id, name: body.name, body: body.body },
      { status: 201, headers: { ETag: '"out-v1"' } },
    );
  }),
  http.get(`${BASE}/api/v1/overlays`, () =>
    HttpResponse.json([
      { id: 'clk', name: 'Clock', body: { kind: 'clock', target: 'canvas', z: 100 } },
    ]),
  ),
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
  clearStoredToken();
  sources = [];
  lastMethod = null;
  lastPath = null;
  lastIfMatch = null;
  lastBody = null;
  lastAuth = null;
});
afterAll(() => {
  server.close();
});

describe('useSources', () => {
  it('projects the opaque body onto the source view-model', async () => {
    sources = [
      { id: 'cam-north', name: 'North', body: { kind: 'rtsp', url: 'rtsp://h/s' } },
      { id: 'studio', name: 'Studio', body: { kind: 'ndi', name: 'STUDIO (CAM 1)' } },
      { id: 'clip', name: 'Clip', body: { kind: 'file', path: '/media/clip.mp4' } },
      { id: 'bars', name: 'Bars', body: { kind: 'test' } },
    ];
    const qc = newClient();
    const { result } = renderHook(() => useSources({ baseUrl: BASE }), {
      wrapper: wrapper(qc),
    });
    await waitFor(() => {
      expect(result.current.isSuccess).toBe(true);
    });
    expect(result.current.data).toHaveLength(4);
    expect(result.current.data?.[0]).toEqual({
      id: 'cam-north',
      name: 'North',
      kind: 'rtsp',
      rawKind: 'rtsp',
      editable: true,
      locator: 'rtsp://h/s',
    });
    // NDI binds by source name; file by path — both projected onto `locator`.
    expect(result.current.data?.[1]?.locator).toBe('STUDIO (CAM 1)');
    expect(result.current.data?.[2]?.locator).toBe('/media/clip.mp4');
    expect(result.current.data?.[3]?.kind).toBe('test');
    expect(result.current.data?.[3]?.locator).toBeUndefined();
  });

  it('surfaces an RFC 9457 problem as an ApiError with its status', async () => {
    server.use(
      http.get(`${BASE}/api/v1/sources`, () =>
        HttpResponse.json(
          { type: '/problems/unauthorized', title: 'Unauthorized', status: 401 },
          { status: 401 },
        ),
      ),
    );
    const qc = newClient();
    const { result } = renderHook(() => useSources({ baseUrl: BASE }), {
      wrapper: wrapper(qc),
    });
    await waitFor(() => {
      expect(result.current.isError).toBe(true);
    });
    expect(result.current.error).toBeInstanceOf(ApiError);
    expect(result.current.error?.status).toBe(401);
    expect(result.current.error?.message).toBe('Unauthorized');
  });

  it('attaches the stored bearer token to a list request', async () => {
    setStoredToken('secret-token');
    const qc = newClient();
    const { result } = renderHook(() => useSources({ baseUrl: BASE }), {
      wrapper: wrapper(qc),
    });
    await waitFor(() => {
      expect(result.current.isSuccess).toBe(true);
    });
    expect(lastAuth).toBe('Bearer secret-token');
  });
});

describe('useOutputs / useOverlays projection', () => {
  it('folds wire kinds and projects target + codec for outputs', async () => {
    const qc = newClient();
    const { result } = renderHook(() => useOutputs({ baseUrl: BASE }), {
      wrapper: wrapper(qc),
    });
    await waitFor(() => {
      expect(result.current.isSuccess).toBe(true);
    });
    expect(result.current.data?.[0]).toEqual({
      id: 'prog',
      name: 'Program',
      kind: 'rtsp',
      rawKind: 'rtsp_server',
      editable: true,
      target: '/mv',
      codec: 'h264',
    });
    expect(result.current.data?.[1]).toEqual({
      id: 'hls',
      name: 'HLS',
      kind: 'll-hls',
      rawKind: 'll_hls',
      editable: true,
      target: '/var/hls',
      codec: 'hevc',
    });
  });

  it('reads the overlay target + stacking order from the body', async () => {
    const qc = newClient();
    const { result } = renderHook(() => useOverlays({ baseUrl: BASE }), {
      wrapper: wrapper(qc),
    });
    await waitFor(() => {
      expect(result.current.isSuccess).toBe(true);
    });
    expect(result.current.data?.[0]).toEqual({
      id: 'clk',
      name: 'Clock',
      kind: 'clock',
      rawKind: 'clock',
      editable: true,
      target: 'canvas',
      z: 100,
    });
  });
});

describe('useSaveResource', () => {
  it('creates a source with the per-kind config body (POST to the item path)', async () => {
    const qc = newClient();
    const { result } = renderHook(() => useSaveResource('sources', { baseUrl: BASE }), {
      wrapper: wrapper(qc),
    });
    const saved = await result.current.mutateAsync({
      id: 'cam-1',
      create: true,
      input: { name: 'Cam 1', body: { kind: 'rtsp', url: 'rtsp://h/1' } },
    });
    expect(lastMethod).toBe('POST');
    expect(lastPath).toBe('/api/v1/sources/cam-1');
    expect(saved.id).toBe('cam-1');
    expect(lastBody).toEqual({ name: 'Cam 1', body: { kind: 'rtsp', url: 'rtsp://h/1' } });
  });

  it('writes the NDI source name and the file path into the body', async () => {
    const qc = newClient();
    const { result } = renderHook(() => useSaveResource('sources', { baseUrl: BASE }), {
      wrapper: wrapper(qc),
    });
    await result.current.mutateAsync({
      id: 'studio',
      create: true,
      input: { name: 'Studio', body: { kind: 'ndi', name: 'STUDIO (CAM 1)' } },
    });
    expect(lastBody).toEqual({
      name: 'Studio',
      body: { kind: 'ndi', name: 'STUDIO (CAM 1)' },
    });
    await result.current.mutateAsync({
      id: 'clip',
      create: true,
      input: { name: 'Clip', body: { kind: 'file', path: '/media/clip.mp4' } },
    });
    expect(lastBody).toEqual({
      name: 'Clip',
      body: { kind: 'file', path: '/media/clip.mp4' },
    });
  });

  it('creates an output with {kind: wire, target, codec} and no enabled field', async () => {
    const qc = newClient();
    const { result } = renderHook(() => useSaveResource('outputs', { baseUrl: BASE }), {
      wrapper: wrapper(qc),
    });
    await result.current.mutateAsync({
      id: 'program-rtsp',
      create: true,
      input: {
        name: 'Program RTSP',
        body: { kind: 'rtsp_server', mount: '/multiview', codec: 'h264' },
      },
    });
    expect(lastMethod).toBe('POST');
    expect(lastPath).toBe('/api/v1/outputs/program-rtsp');
    expect(lastBody).toEqual({
      name: 'Program RTSP',
      body: { kind: 'rtsp_server', mount: '/multiview', codec: 'h264' },
    });
    const sentBody = (lastBody as { body: Record<string, unknown> }).body;
    expect('enabled' in sentBody).toBe(false);
  });

  it('updates with PUT and echoes a fetched ETag as If-Match', async () => {
    sources = [{ id: 'cam-1', name: 'Old', body: { kind: 'rtsp', url: 'rtsp://h/0' } }];
    const qc = newClient();
    const { result } = renderHook(() => useSaveResource('sources', { baseUrl: BASE }), {
      wrapper: wrapper(qc),
    });
    await result.current.mutateAsync({
      id: 'cam-1',
      create: false,
      input: { name: 'New', body: { kind: 'rtsp', url: 'rtsp://h/0' } },
    });
    expect(lastMethod).toBe('PUT');
    expect(lastPath).toBe('/api/v1/sources/cam-1');
    // No cached ETag → the hook GETs the record first (which returns "src-v1").
    expect(lastIfMatch).toBe('"src-v1"');
  });

  it('attaches the stored bearer token to a create request', async () => {
    setStoredToken('secret-token');
    const qc = newClient();
    const { result } = renderHook(() => useSaveResource('sources', { baseUrl: BASE }), {
      wrapper: wrapper(qc),
    });
    await result.current.mutateAsync({
      id: 'cam-2',
      create: true,
      input: { name: 'Cam 2', body: { kind: 'test' } },
    });
    expect(lastAuth).toBe('Bearer secret-token');
  });

  it('surfaces a create conflict as an ApiError', async () => {
    server.use(
      http.post(`${BASE}/api/v1/sources/:id`, () =>
        HttpResponse.json(
          { type: '/problems/conflict', title: 'Conflict', status: 409 },
          { status: 409 },
        ),
      ),
    );
    const qc = newClient();
    const { result } = renderHook(() => useSaveResource('sources', { baseUrl: BASE }), {
      wrapper: wrapper(qc),
    });
    await expect(
      result.current.mutateAsync({
        id: 'dupe',
        create: true,
        input: { name: 'Dupe', body: { kind: 'test' } },
      }),
    ).rejects.toMatchObject({ status: 409 });
  });
});

describe('useDeleteResource', () => {
  it('deletes with DELETE and a fetched If-Match (the backend 428s without it)', async () => {
    sources = [{ id: 'cam-1', name: 'Drop', body: { kind: 'test' } }];
    const qc = newClient();
    const { result } = renderHook(() => useDeleteResource('sources', { baseUrl: BASE }), {
      wrapper: wrapper(qc),
    });
    await result.current.mutateAsync('cam-1');
    expect(lastMethod).toBe('DELETE');
    expect(lastPath).toBe('/api/v1/sources/cam-1');
    // The hook fetched the ETag via GET and sent it as `If-Match`; without it the
    // mocked backend would have returned 428 and the mutation would have thrown.
    expect(lastIfMatch).toBe('"src-v1"');
    expect(sources).toHaveLength(0);
  });

  it('surfaces a 428 (missing If-Match) as an ApiError', async () => {
    // Force the GET that supplies the ETag to omit it, so the DELETE goes out
    // unconditionally and the backend rejects it with 428.
    sources = [{ id: 'cam-1', name: 'Drop', body: { kind: 'test' } }];
    server.use(
      http.get(`${BASE}/api/v1/sources/:id`, ({ params }) => {
        const found = sources.find((s) => s.id === String(params.id));
        return found === undefined
          ? HttpResponse.json(
              { type: '/problems/not-found', title: 'Not found', status: 404 },
              { status: 404 },
            )
          : HttpResponse.json(found);
      }),
    );
    const qc = newClient();
    const { result } = renderHook(() => useDeleteResource('sources', { baseUrl: BASE }), {
      wrapper: wrapper(qc),
    });
    await expect(result.current.mutateAsync('cam-1')).rejects.toMatchObject({ status: 428 });
    expect(sources).toHaveLength(1);
  });

  it('attaches the stored bearer token to the delete request', async () => {
    setStoredToken('secret-token');
    sources = [{ id: 'cam-1', name: 'Drop', body: { kind: 'test' } }];
    const qc = newClient();
    const { result } = renderHook(() => useDeleteResource('sources', { baseUrl: BASE }), {
      wrapper: wrapper(qc),
    });
    await result.current.mutateAsync('cam-1');
    expect(lastAuth).toBe('Bearer secret-token');
  });
});
