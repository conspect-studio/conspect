// Media-player (VT) transport surface: list / fetch the configured players and
// drive each through its transport verbs.
//
// A media player is a pre-declared, bus-selectable channel (config
// `media_players[]`; ADR-0057 + ADR-0097) that rolls an asset with VT-style
// transport: load an asset, cue to the in-point or a frame, play / pause / stop,
// seek, and — for the vamp/loop feature — arm / take / cancel a clean exit at the
// next vamp boundary. The control plane exposes them under
// `/api/v1/media/players`:
//   * `GET  /api/v1/media/players`            lists `Resource[]` (config-as-code).
//   * `GET  /api/v1/media/players/{id}`       returns one `Resource`.
//   * `POST /api/v1/media/players/{id}/load`  body `{ asset }`   -> 202 + op id.
//   * `POST /api/v1/media/players/{id}/cue`   body `{ frame? }`  -> 202 + op id.
//   * `POST /api/v1/media/players/{id}/seek`  body `{ frame? }`  -> 202 + op id.
//   * `POST /api/v1/media/players/{id}/play|pause|stop`         -> 202 + op id.
//   * `POST /api/v1/media/players/{id}/exit/arm|take|cancel`    -> 202 + op id.
// Every command returns `202 Accepted` with an operation id; the outcome (and the
// player's new transport state) lands later on the realtime stream as a
// `media.player_state` event (conventions section 6, ADR-RT008).
import {
  apiUrl,
  buildHeaders,
  OperationApiError,
  readProblem,
  submitOperation,
} from './operations';
import type { AcceptedBody, RequestOptions } from './operations';
import type { components } from './schema';

/** A configured media player, exactly as the control plane returns it. */
export type MediaPlayer = components['schemas']['Resource'];

/** The optional JSON body a transport verb may carry (`asset` / `frame`). */
export type TransportBody = components['schemas']['TransportBody'];

export { OperationApiError } from './operations';
export type { AcceptedBody, RequestOptions } from './operations';

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

function isMediaPlayer(value: unknown): value is MediaPlayer {
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

const COLLECTION = '/api/v1/media/players';

function itemPath(id: string): string {
  return `${COLLECTION}/${encodeURIComponent(id)}`;
}

/** List all configured media players (`GET /api/v1/media/players`). */
export async function listMediaPlayers(
  options: RequestOptions = {},
): Promise<MediaPlayer[]> {
  const response = await fetch(apiUrl(options, COLLECTION), {
    method: 'GET',
    headers: buildHeaders(options, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!Array.isArray(body) || !body.every(isMediaPlayer)) {
    throw new OperationApiError('The server returned an unexpected player list.');
  }
  return body;
}

/** Fetch one configured media player (`GET /api/v1/media/players/{id}`). */
export async function getMediaPlayer(
  id: string,
  options: RequestOptions = {},
): Promise<MediaPlayer> {
  const response = await fetch(apiUrl(options, itemPath(id)), {
    method: 'GET',
    headers: buildHeaders(options, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isMediaPlayer(body)) {
    throw new OperationApiError('The server returned an unexpected player body.');
  }
  return body;
}

/**
 * `POST` a transport verb that carries a `TransportBody` and return its `202`
 * body. Used by `load` (asset), `cue`, and `seek` (frame). An empty `body` is
 * still sent as `{}` so the server reads the verb's default (e.g. cue/seek to the
 * in-point). The outcome arrives later on the realtime stream.
 */
async function submitTransport(
  path: string,
  body: TransportBody,
  options: RequestOptions = {},
): Promise<AcceptedBody> {
  const response = await fetch(apiUrl(options, path), {
    method: 'POST',
    headers: buildHeaders(options, true),
    body: JSON.stringify(body),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const parsed: unknown = await response.json();
  if (!isAcceptedBody(parsed)) {
    throw new OperationApiError('The server returned an unexpected command body.');
  }
  return parsed;
}

/** Load an asset into a player (`POST .../load`, body `{ asset }`). */
export function loadMediaPlayer(
  id: string,
  asset: string,
  options: RequestOptions = {},
): Promise<AcceptedBody> {
  return submitTransport(`${itemPath(id)}/load`, { asset }, options);
}

/**
 * Cue a player to its in-point, or to `frame` when given
 * (`POST .../cue`, body `{ frame? }`).
 */
export function cueMediaPlayer(
  id: string,
  frame: number | undefined,
  options: RequestOptions = {},
): Promise<AcceptedBody> {
  return submitTransport(
    `${itemPath(id)}/cue`,
    frame === undefined ? {} : { frame },
    options,
  );
}

/** Seek a player to a frame (`POST .../seek`, body `{ frame? }`). */
export function seekMediaPlayer(
  id: string,
  frame: number | undefined,
  options: RequestOptions = {},
): Promise<AcceptedBody> {
  return submitTransport(
    `${itemPath(id)}/seek`,
    frame === undefined ? {} : { frame },
    options,
  );
}

/** Play forward (`POST .../play`). */
export function playMediaPlayer(
  id: string,
  options: RequestOptions = {},
): Promise<AcceptedBody> {
  return submitOperation(`${itemPath(id)}/play`, options);
}

/** Pause, holding the current frame (`POST .../pause`). */
export function pauseMediaPlayer(
  id: string,
  options: RequestOptions = {},
): Promise<AcceptedBody> {
  return submitOperation(`${itemPath(id)}/pause`, options);
}

/** Stop / re-cue (`POST .../stop`). */
export function stopMediaPlayer(
  id: string,
  options: RequestOptions = {},
): Promise<AcceptedBody> {
  return submitOperation(`${itemPath(id)}/stop`, options);
}

/** Arm the vamp exit — fire at the next vamp boundary (`POST .../exit/arm`). */
export function armMediaPlayerExit(
  id: string,
  options: RequestOptions = {},
): Promise<AcceptedBody> {
  return submitOperation(`${itemPath(id)}/exit/arm`, options);
}

/** Take the vamp exit — arm + fire at the soonest boundary (`POST .../exit/take`). */
export function takeMediaPlayerExit(
  id: string,
  options: RequestOptions = {},
): Promise<AcceptedBody> {
  return submitOperation(`${itemPath(id)}/exit/take`, options);
}

/** Cancel an armed vamp exit (`POST .../exit/cancel`). */
export function cancelMediaPlayerExit(
  id: string,
  options: RequestOptions = {},
): Promise<AcceptedBody> {
  return submitOperation(`${itemPath(id)}/exit/cancel`, options);
}
