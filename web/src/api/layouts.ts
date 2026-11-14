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
import type { components } from './types';

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
  /**
   * The RFC 9457 problem `detail`, when the server returned one — the
   * specific reason behind a generic `title` (e.g. the `routed-layout` 403's
   * explanation of which outputs/sources are missing authorization).
   */
  readonly detail: string | undefined;

  constructor(message: string, status?: number, detail?: string) {
    super(message);
    this.name = 'LayoutApiError';
    this.status = status;
    this.detail = detail;
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
    throw new LayoutApiError(error.title, error.status, error.detail ?? undefined);
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
    throw new LayoutApiError(error.title, error.status, error.detail ?? undefined);
  }
  const etag = response.headers.get('ETag');
  return { layout: data, etag: etag ?? undefined };
}

/**
 * Read a layout's current `ETag` via `GET /api/v1/layouts/{id}`. Returns
 * `undefined` when the layout does not exist (404) — the caller then creates it.
 * Used when a save targets an existing layout whose ETag this session never
 * cached (e.g. after a page reload: the list response carries no ETags).
 */
export async function fetchLayoutEtag(
  client: MultiviewApiClient,
  id: string,
): Promise<string | undefined> {
  const { error, response } = await client.GET('/api/v1/layouts/{id}', {
    params: { path: { id } },
  });
  if (response.status === 404) {
    return undefined;
  }
  if (error !== undefined) {
    throw new LayoutApiError(error.title, error.status, error.detail ?? undefined);
  }
  return response.headers.get('ETag') ?? undefined;
}

/** A single layout read together with its current `ETag`, as ONE response. */
export interface LayoutWithEtag {
  /** The layout exactly as the control plane returned it just now. */
  readonly layout: Layout;
  /** The `ETag` from that same response, when present. */
  readonly etag: string | undefined;
}

/**
 * Read a single layout AND its `ETag` from the same `GET /api/v1/layouts/{id}`
 * response. A save that edits only part of a layout (e.g. the Layers dialog)
 * must base its edit on this fresh read — never on a list-derived snapshot,
 * whose body/ETag can already be stale relative to a concurrent editor save —
 * and must send exactly this `ETag` as `If-Match` so a lost-update is a
 * `412`, not a silent overwrite.
 */
export async function fetchLayoutWithEtag(
  client: MultiviewApiClient,
  id: string,
): Promise<LayoutWithEtag> {
  const { data, error, response } = await client.GET('/api/v1/layouts/{id}', {
    params: { path: { id } },
  });
  if (error !== undefined) {
    throw new LayoutApiError(error.title, error.status, error.detail ?? undefined);
  }
  return { layout: data, etag: response.headers.get('ETag') ?? undefined };
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
    throw new LayoutApiError(error.title, error.status, error.detail ?? undefined);
  }
}
