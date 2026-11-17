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
  newIdempotencyKey,
  OperationApiError,
  readProblem,
  submitOperation,
} from './operations';
import type { AcceptedBody, RequestOptions } from './operations';
import type { components } from './types';
import { isPatchPlanResponse } from './patch';
import type { PatchPlanResponse } from './patch';

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

/** The patch salvo `202` (#308): no `kind` field, so it does not fit {@link AcceptedBody}. */
export interface PatchSalvoAccepted {
  readonly operation_id: string;
  readonly status: string;
  /** Present when the salvo also recalls layout/sources/tally/UMD. */
  readonly recall_operation_id?: string | null;
}

function isPatchSalvoAccepted(value: unknown): value is PatchSalvoAccepted {
  return (
    isRecord(value) &&
    typeof value.operation_id === 'string' &&
    typeof value.status === 'string'
  );
}

/**
 * Plan a salvo's patch crosspoints (`POST /api/v1/salvos/{id}/plan`, #308): the
 * same `PatchPlan`/`SetPlan` answer as `POST /patch/plan`, without applying.
 */
export async function planSalvo(
  id: string,
  options: RequestOptions = {},
): Promise<PatchPlanResponse> {
  const response = await fetch(apiUrl(options, `${itemPath(id)}/plan`), {
    method: 'POST',
    headers: buildHeaders(options, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  // The salvo plan route answers the identical shape as `/patch/plan`; share
  // one guard so the two never drift.
  if (!isPatchPlanResponse(body)) {
    throw new OperationApiError('The server returned an unexpected salvo plan.');
  }
  return body;
}

/**
 * Take a patch salvo (`POST /api/v1/salvos/{id}/take`, #308): its crosspoints
 * apply atomically through the same set path as `POST /patch/take` (outcome
 * `patch.take`, one destination too); a legacy recall, when also present, is
 * submitted too (`recall_operation_id`). A fresh `Idempotency-Key` per attempt,
 * matching `takePatch`.
 */
export async function takePatchSalvo(
  id: string,
  options: RequestOptions & { readonly head?: string } = {},
): Promise<PatchSalvoAccepted> {
  const requestOptions = { ...options, idempotencyKey: options.idempotencyKey ?? newIdempotencyKey() };
  const query = options.head !== undefined ? `?head=${encodeURIComponent(options.head)}` : '';
  const response = await fetch(apiUrl(requestOptions, `${itemPath(id)}/take${query}`), {
    method: 'POST',
    headers: buildHeaders(requestOptions, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isPatchSalvoAccepted(body)) {
    throw new OperationApiError('The server returned an unexpected salvo take body.');
  }
  return body;
}
