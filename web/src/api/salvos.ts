// Salvos surface: list / create / replace / delete salvo definitions and
// arm / take / cancel them on the engine.
//
// A salvo is a named recall (layout + source/tally/UMD rebindings). The control
// plane stores it version-stamped:
//   * `GET /api/v1/salvos` lists `SalvoDoc[]` (no per-item ETag).
//   * `GET /api/v1/salvos/{id}` returns one salvo + its `ETag` (used to read the
//     current version before a replace/delete -- this path is wired by the
//     control plane but not yet in the generated `paths`, so it goes through
//     `fetch` here, mirroring `./layouts.ts`).
//   * `PUT /api/v1/salvos/{id}` creates (201, no `If-Match`) or replaces (200,
//     `If-Match` required).
//   * `DELETE /api/v1/salvos/{id}` deletes (`If-Match` required).
//   * `POST /api/v1/salvos/{id}/arm|take|cancel` submit engine commands and
//     return `202 Accepted` + an operation id (outcome on the realtime stream).
import {
  apiUrl,
  buildHeaders,
  OperationApiError,
  readProblem,
  submitOperation,
} from './operations';
import type { AcceptedBody, RequestOptions } from './operations';
import type { components } from './schema';

/** A salvo definition, exactly as the control plane returns it. */
export type Salvo = components['schemas']['SalvoDoc'];

/** A salvo + the ETag the control plane stamped it with. */
export interface SalvoWithEtag {
  /** The stored salvo. */
  readonly salvo: Salvo;
  /** The ETag for `If-Match`, when the response carried one. */
  readonly etag: string | undefined;
}

export { OperationApiError } from './operations';
export type { AcceptedBody, RequestOptions } from './operations';

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

function isSalvo(value: unknown): value is Salvo {
  return isRecord(value) && typeof value.id === 'string';
}

const COLLECTION = '/api/v1/salvos';

function itemPath(id: string): string {
  return `${COLLECTION}/${encodeURIComponent(id)}`;
}

/** List all salvo definitions (`GET /api/v1/salvos`). */
export async function listSalvos(options: RequestOptions = {}): Promise<Salvo[]> {
  const response = await fetch(apiUrl(options, COLLECTION), {
    method: 'GET',
    headers: buildHeaders(options, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!Array.isArray(body) || !body.every(isSalvo)) {
    throw new OperationApiError('The server returned an unexpected salvo list.');
  }
  return body;
}

/** Fetch one salvo + its ETag (`GET /api/v1/salvos/{id}`). */
export async function getSalvo(
  id: string,
  options: RequestOptions = {},
): Promise<SalvoWithEtag> {
  const response = await fetch(apiUrl(options, itemPath(id)), {
    method: 'GET',
    headers: buildHeaders(options, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isSalvo(body)) {
    throw new OperationApiError('The server returned an unexpected salvo body.');
  }
  const etag = response.headers.get('ETag');
  return { salvo: body, etag: etag ?? undefined };
}

/**
 * Create-or-replace a salvo via `PUT /api/v1/salvos/{id}`. The path id is
 * authoritative; the body's `id` is aligned to it server-side. A replace must
 * carry the current ETag as `If-Match` (pass it in `options.etag`); a create
 * omits it (the control plane returns 201).
 */
export async function putSalvo(
  id: string,
  salvo: Salvo,
  options: RequestOptions = {},
): Promise<SalvoWithEtag> {
  const response = await fetch(apiUrl(options, itemPath(id)), {
    method: 'PUT',
    headers: buildHeaders(options, true),
    body: JSON.stringify({ ...salvo, id }),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isSalvo(body)) {
    throw new OperationApiError('The server returned an unexpected salvo body.');
  }
  const etag = response.headers.get('ETag');
  return { salvo: body, etag: etag ?? undefined };
}

/** Delete a salvo (`DELETE /api/v1/salvos/{id}`), sending `If-Match`. */
export async function deleteSalvo(
  id: string,
  options: RequestOptions = {},
): Promise<void> {
  const response = await fetch(apiUrl(options, itemPath(id)), {
    method: 'DELETE',
    headers: buildHeaders(options, false),
  });
  // 404 is idempotent-success for a delete (the salvo is already gone).
  if (!response.ok && response.status !== 404) {
    throw await readProblem(response);
  }
}

/** Arm (stage) a salvo (`POST /api/v1/salvos/{id}/arm`). */
export function armSalvo(
  id: string,
  options: RequestOptions = {},
): Promise<AcceptedBody> {
  return submitOperation(`${itemPath(id)}/arm`, options);
}

/** Take (apply) a salvo (`POST /api/v1/salvos/{id}/take`). */
export function takeSalvo(
  id: string,
  options: RequestOptions = {},
): Promise<AcceptedBody> {
  return submitOperation(`${itemPath(id)}/take`, options);
}

/** Cancel an armed salvo (`POST /api/v1/salvos/{id}/cancel`). */
export function cancelSalvo(
  id: string,
  options: RequestOptions = {},
): Promise<AcceptedBody> {
  return submitOperation(`${itemPath(id)}/cancel`, options);
}
