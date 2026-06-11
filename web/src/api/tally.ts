// Tally surface: resolved tally state, tally profiles (CRUD), and the manual
// override.
//
//   * `GET  /api/v1/tally`              -> resolved `TallyEntryDoc[]` per target.
//   * `GET  /api/v1/tally/profiles`     -> `TallyProfileDoc[]` (no per-item ETag).
//   * `GET  /api/v1/tally/profiles/{id}`-> one profile + its `ETag` (used to read
//        the current version before a replace/delete; wired by the control plane
//        but not yet in the generated `paths`, so it goes through `fetch` here).
//   * `PUT  /api/v1/tally/profiles/{id}`-> create (201) / replace (200, If-Match).
//   * `DELETE /api/v1/tally/profiles/{id}` -> delete (If-Match).
//   * `PUT  /api/v1/tally/override`     -> force a target's lamp (202 + op id).
//   * `DELETE /api/v1/tally/override`   -> clear a target's override (202 + op id;
//        the clear carries a JSON body naming the target).
//
// The override endpoints submit an engine command and report `202`; the resolved
// lamp arrives later on the realtime stream as a `tally.state` event.
import {
  apiUrl,
  buildHeaders,
  OperationApiError,
  readProblem,
} from './operations';
import type { AcceptedBody, RequestOptions } from './operations';
import type { components } from './schema';

/** One resolved tally entry (target + lamp state). */
export type TallyEntry = components['schemas']['TallyEntryDoc'];

/** A tally profile definition. */
export type TallyProfile = components['schemas']['TallyProfileDoc'];

/** A tally target (a tile index or a named element). */
export type TallyTarget = components['schemas']['TallyTargetDoc'];

/** The TSL UMD lamp palette. */
export type TallyColor = components['schemas']['TallyColorDoc'];

/** A profile + the ETag the control plane stamped it with. */
export interface ProfileWithEtag {
  /** The stored profile. */
  readonly profile: TallyProfile;
  /** The ETag for `If-Match`, when present. */
  readonly etag: string | undefined;
}

export { OperationApiError } from './operations';
export type { AcceptedBody, RequestOptions } from './operations';

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

function isTallyEntry(value: unknown): value is TallyEntry {
  return isRecord(value) && isRecord(value.state) && isRecord(value.target);
}

function isProfile(value: unknown): value is TallyProfile {
  return isRecord(value) && typeof value.id === 'string';
}

function isAcceptedBody(value: unknown): value is AcceptedBody {
  return (
    isRecord(value) &&
    typeof value.operation_id === 'string' &&
    typeof value.kind === 'string'
  );
}

/** List resolved tally state (`GET /api/v1/tally`). */
export async function listTally(options: RequestOptions = {}): Promise<TallyEntry[]> {
  const response = await fetch(apiUrl(options, '/api/v1/tally'), {
    method: 'GET',
    headers: buildHeaders(options, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!Array.isArray(body) || !body.every(isTallyEntry)) {
    throw new OperationApiError('The server returned an unexpected tally list.');
  }
  return body;
}

/** List tally profiles (`GET /api/v1/tally/profiles`). */
export async function listProfiles(options: RequestOptions = {}): Promise<TallyProfile[]> {
  const response = await fetch(apiUrl(options, '/api/v1/tally/profiles'), {
    method: 'GET',
    headers: buildHeaders(options, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!Array.isArray(body) || !body.every(isProfile)) {
    throw new OperationApiError('The server returned an unexpected profile list.');
  }
  return body;
}

function profilePath(id: string): string {
  return `/api/v1/tally/profiles/${encodeURIComponent(id)}`;
}

/** Fetch one profile + its ETag (`GET /api/v1/tally/profiles/{id}`). */
export async function getProfile(
  id: string,
  options: RequestOptions = {},
): Promise<ProfileWithEtag> {
  const response = await fetch(apiUrl(options, profilePath(id)), {
    method: 'GET',
    headers: buildHeaders(options, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isProfile(body)) {
    throw new OperationApiError('The server returned an unexpected profile body.');
  }
  const etag = response.headers.get('ETag');
  return { profile: body, etag: etag ?? undefined };
}

/** Create (201) or replace (200, `If-Match`) a tally profile. */
export async function putProfile(
  id: string,
  profile: TallyProfile,
  options: RequestOptions = {},
): Promise<ProfileWithEtag> {
  const response = await fetch(apiUrl(options, profilePath(id)), {
    method: 'PUT',
    headers: buildHeaders(options, true),
    body: JSON.stringify({ ...profile, id }),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isProfile(body)) {
    throw new OperationApiError('The server returned an unexpected profile body.');
  }
  const etag = response.headers.get('ETag');
  return { profile: body, etag: etag ?? undefined };
}

/** Delete a tally profile (`DELETE /api/v1/tally/profiles/{id}`, `If-Match`). */
export async function deleteProfile(
  id: string,
  options: RequestOptions = {},
): Promise<void> {
  const response = await fetch(apiUrl(options, profilePath(id)), {
    method: 'DELETE',
    headers: buildHeaders(options, false),
  });
  if (!response.ok && response.status !== 404) {
    throw await readProblem(response);
  }
}

async function submitOverride(
  method: 'PUT' | 'DELETE',
  payload: Record<string, unknown>,
  options: RequestOptions,
): Promise<AcceptedBody> {
  const response = await fetch(apiUrl(options, '/api/v1/tally/override'), {
    method,
    headers: buildHeaders(options, true),
    body: JSON.stringify(payload),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isAcceptedBody(body)) {
    throw new OperationApiError('The server returned an unexpected command body.');
  }
  return body;
}

/** Force a target's lamp (`PUT /api/v1/tally/override` -> 202 + op id). */
export function setOverride(
  target: TallyTarget,
  color: TallyColor,
  options: RequestOptions = {},
): Promise<AcceptedBody> {
  return submitOverride('PUT', { target, color }, options);
}

/** Clear a target's override (`DELETE /api/v1/tally/override` -> 202 + op id). */
export function clearOverride(
  target: TallyTarget,
  options: RequestOptions = {},
): Promise<AcceptedBody> {
  return submitOverride('DELETE', { target }, options);
}
