// Layouts CRUD bindings over the generated, typed openapi-fetch client.
//
// The control-plane spec declares:
//   GET    /api/v1/layouts          — list
//   GET    /api/v1/layouts/{id}     — fetch one
//   POST   /api/v1/layouts/{id}     — create (id supplied by caller)
//   PUT    /api/v1/layouts/{id}     — replace (If-Match → 412)
//   DELETE /api/v1/layouts/{id}     — delete  (If-Match → 412)
//
// All five are now in `paths` of the generated schema (`schema.ts`), so every
// request/response shape is compile-checked against the spec. No hand-written
// fetch calls; no bespoke URL/header helpers.
//
// ETag/If-Match: the control plane version-stamps each layout (conventions §6,
// RFC 9457 on conflict). The caller passes the stored ETag via `ifMatch` and it
// is forwarded as a raw `If-Match` request header (not a typed spec parameter —
// the spec omits it from `parameters`, so we pass it via `headers` in the
// openapi-fetch options).
import type { MultiviewApiClient } from './client';
import type { components } from './schema';

/** A persisted layout resource, exactly as the control plane returns it. */
export type Layout = components['schemas']['Layout'];

/** The create/update payload accepted by the control plane. */
export type LayoutInput = components['schemas']['LayoutInput'];

/** An RFC 9457 problem document, as modelled in the spec. */
export type Problem = components['schemas']['Problem'];

/** A failed layouts API call, normalized to a message + HTTP status. */
export class LayoutApiError extends Error {
  /** The HTTP status code, when one was returned. */
  readonly status: number | undefined;

  constructor(message: string, status?: number) {
    super(message);
    this.name = 'LayoutApiError';
    this.status = status;
  }
}

/** The result of a successful write: the stored layout and its new ETag. */
export interface LayoutWriteResult {
  /** The stored resource as the control plane returned it. */
  readonly layout: Layout;
  /** The new ETag from the response header, when present. */
  readonly etag: string | undefined;
}

/**
 * Create a layout via `POST /api/v1/layouts/{id}` (id is caller-supplied, per
 * the spec). Returns the created layout and its initial ETag.
 */
export async function createLayout(
  client: MultiviewApiClient,
  id: string,
  input: LayoutInput,
): Promise<LayoutWriteResult> {
  const { data, error, response } = await client.POST('/api/v1/layouts/{id}', {
    params: { path: { id } },
    body: input,
  });
  if (error !== undefined) {
    throw new LayoutApiError(error.title, error.status);
  }
  const etag = response.headers.get('ETag');
  return { layout: data, etag: etag ?? undefined };
}

/**
 * Replace a layout via `PUT /api/v1/layouts/{id}`. Sends `If-Match` when an
 * ETag is provided for optimistic concurrency (412 when the version has moved).
 */
export async function updateLayout(
  client: MultiviewApiClient,
  id: string,
  input: LayoutInput,
  ifMatch?: string,
): Promise<LayoutWriteResult> {
  const extraHeaders: Record<string, string> = {};
  if (ifMatch !== undefined && ifMatch !== '') {
    extraHeaders['If-Match'] = ifMatch;
  }
  const { data, error, response } = await client.PUT('/api/v1/layouts/{id}', {
    params: { path: { id } },
    body: input,
    headers: extraHeaders,
  });
  if (error !== undefined) {
    throw new LayoutApiError(error.title, error.status);
  }
  const etag = response.headers.get('ETag');
  return { layout: data, etag: etag ?? undefined };
}

/**
 * Delete a layout via `DELETE /api/v1/layouts/{id}`. Sends `If-Match` when an
 * ETag is provided. A 404 response is treated as idempotent success (the
 * resource is already gone).
 */
export async function deleteLayoutById(
  client: MultiviewApiClient,
  id: string,
  ifMatch?: string,
): Promise<void> {
  const extraHeaders: Record<string, string> = {};
  if (ifMatch !== undefined && ifMatch !== '') {
    extraHeaders['If-Match'] = ifMatch;
  }
  const { error, response } = await client.DELETE('/api/v1/layouts/{id}', {
    params: { path: { id } },
    headers: extraHeaders,
  });
  // 404 is idempotent-success: the resource is already absent.
  if (response.status === 404) {
    return;
  }
  if (error !== undefined) {
    throw new LayoutApiError(error.title, error.status);
  }
}
