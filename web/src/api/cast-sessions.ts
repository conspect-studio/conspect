// Cast surface: ad-hoc Cast (Chromecast / Google Cast) device sessions.
//
// An operator can cast a running output's rendition to a Cast receiver on the
// network as an EPHEMERAL session, then optionally promote it to a permanent
// managed device. The control plane wires:
//   * `GET  /api/v1/cast/sessions` — the live ephemeral sessions.
//   * `POST /api/v1/cast/sessions` — start an ad-hoc session (`201`).
//   * `GET  /api/v1/cast/sessions/{id}` — one session.
//   * `DELETE /api/v1/cast/sessions/{id}` — stop a session (`204`).
//   * `POST /api/v1/cast/sessions/{id}/save` — promote ephemeral → device
//     (`201`, returns the registered device `Resource`).
//   * `POST /api/v1/cast/sessions/{id}/volume` — set the receiver volume
//     (`202` + operation id; the level lands on the realtime stream).
// Candidate Cast receivers come from the shared discovery inventory
// (`useDiscoveredInventory`, driver_kind === 'cast'), not from this surface.
import {
  apiUrl,
  buildHeaders,
  OperationApiError,
  readProblem,
} from './operations';
import type { AcceptedBody, RequestOptions } from './operations';
import type { components } from './schema';

/** One live cast session, exactly as the control plane returns it. */
export type CastSession = components['schemas']['CastSessionDoc'];

/** The body of a start-cast-session request. */
export type StartCastSessionRequest = components['schemas']['StartCastSessionRequest'];

/** The body of a save (promote-to-device) request. */
export type SaveCastSessionRequest = components['schemas']['SaveCastSessionRequest'];

/** The body of a set-volume request. */
export type CastVolumeRequest = components['schemas']['CastVolumeRequest'];

/** A registered resource (the device a promoted session becomes). */
export type Resource = components['schemas']['Resource'];

export { OperationApiError } from './operations';
export type { AcceptedBody, RequestOptions } from './operations';

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

function isCastSession(value: unknown): value is CastSession {
  return (
    isRecord(value) &&
    typeof value.address === 'string' &&
    typeof value.id === 'string' &&
    typeof value.media_url === 'string' &&
    typeof value.output === 'string' &&
    typeof value.state === 'string'
  );
}

function isResource(value: unknown): value is Resource {
  return (
    isRecord(value) &&
    typeof value.id === 'string' &&
    typeof value.name === 'string' &&
    'body' in value
  );
}

function isAcceptedBody(value: unknown): value is AcceptedBody {
  return (
    isRecord(value) &&
    typeof value.operation_id === 'string' &&
    typeof value.kind === 'string'
  );
}

const COLLECTION = '/api/v1/cast/sessions';

function itemPath(id: string): string {
  return `${COLLECTION}/${encodeURIComponent(id)}`;
}

/** List the live ephemeral cast sessions (`GET /api/v1/cast/sessions`). */
export async function listCastSessions(
  options: RequestOptions = {},
): Promise<CastSession[]> {
  const response = await fetch(apiUrl(options, COLLECTION), {
    method: 'GET',
    headers: buildHeaders(options, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!Array.isArray(body) || !body.every(isCastSession)) {
    throw new OperationApiError('The server returned an unexpected cast session list.');
  }
  return body;
}

/** Start an ad-hoc cast session (`POST /api/v1/cast/sessions`, `201`). */
export async function startCastSession(
  request: StartCastSessionRequest,
  options: RequestOptions = {},
): Promise<CastSession> {
  const response = await fetch(apiUrl(options, COLLECTION), {
    method: 'POST',
    headers: buildHeaders(options, true),
    body: JSON.stringify(request),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isCastSession(body)) {
    throw new OperationApiError('The server returned an unexpected cast session.');
  }
  return body;
}

/** Stop a cast session (`DELETE /api/v1/cast/sessions/{id}`, `204`). */
export async function stopCastSession(
  id: string,
  options: RequestOptions = {},
): Promise<void> {
  const response = await fetch(apiUrl(options, itemPath(id)), {
    method: 'DELETE',
    headers: buildHeaders(options, false),
  });
  // 404 is idempotent-success for a stop (the session is already gone).
  if (!response.ok && response.status !== 404) {
    throw await readProblem(response);
  }
}

/**
 * Promote an ephemeral session to a permanent managed device
 * (`POST /api/v1/cast/sessions/{id}/save`, `201`). Returns the registered
 * device resource.
 */
export async function saveCastSession(
  id: string,
  request: SaveCastSessionRequest,
  options: RequestOptions = {},
): Promise<Resource> {
  const response = await fetch(apiUrl(options, `${itemPath(id)}/save`), {
    method: 'POST',
    headers: buildHeaders(options, true),
    body: JSON.stringify(request),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isResource(body)) {
    throw new OperationApiError('The server returned an unexpected device resource.');
  }
  return body;
}

/**
 * Set the receiver volume for a session (`POST /api/v1/cast/sessions/{id}/volume`,
 * `202`). The applied level lands on the realtime stream, not in this response;
 * the returned operation id correlates it.
 */
export async function setCastVolume(
  id: string,
  request: CastVolumeRequest,
  options: RequestOptions = {},
): Promise<AcceptedBody> {
  const response = await fetch(apiUrl(options, `${itemPath(id)}/volume`), {
    method: 'POST',
    headers: buildHeaders(options, true),
    body: JSON.stringify(request),
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
