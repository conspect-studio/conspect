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
  sendCommand,
  OperationApiError,
  readProblem,
  submitOperation,
} from './operations';
import type { AcceptedBody, RequestOptions } from './operations';
import type { components } from './types';

/** A configured media player, exactly as the control plane returns it. */
export type MediaPlayer = components['schemas']['Resource'];

/** The optional JSON body a transport verb may carry (`asset` / `frame`). */
export type TransportBody = components['schemas']['TransportBody'];

/** Exact immutable completed-capture selector for an `artifactRef` load. */
export type ArtifactRef = components['schemas']['ArtifactRef'];

/** Commit landing intent for an `artifactRef` load: cue or resume. */
export type LoadThen = components['schemas']['LoadThen'];

/** Bounded native-load operation view (`GET .../load-operations/{operation}`). */
export type LoadOperation = components['schemas']['LoadOperation'];

/** Native commit acknowledgement (duration, geometry, audio presence). */
export type NativeLoadAck = components['schemas']['NativeLoadAck'];

/**
 * Wire response of an `artifactRef` load: the legacy `202 AcceptedBody` shape
 * when the server has not adopted the artifact branch, or a `LoadOperation`
 * once it has. Distinguished below by the presence of `operation`+`state`.
 */
export type LoadResponse = components['schemas']['LoadResponse'];

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
  const response = await sendCommand(path, options, { body });
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

function isLoadOperation(value: unknown): value is LoadOperation {
  return (
    isRecord(value) &&
    typeof value.operation === 'number' &&
    typeof value.state === 'string'
  );
}

/**
 * Load a completed capture artifact into a player
 * (`POST .../load`, body `{ artifactRef, then? }`). The server replies either
 * the legacy `202 AcceptedBody` or a `LoadOperation` whose completion
 * requires native video (and audio) adoption — poll
 * `getPlayerLoadOperation` on the latter to its terminal state. A fresh
 * `Idempotency-Key` is minted unless `options.idempotencyKey` is supplied —
 * reuse the SAME key for a retry of the same load attempt so a replay
 * reserves the one operation rather than starting a second commit.
 */
export async function loadMediaPlayerArtifact(
  id: string,
  artifactRef: ArtifactRef,
  then: LoadThen | undefined,
  options: RequestOptions = {},
): Promise<LoadResponse> {
  const response = await sendCommand(
    `${itemPath(id)}/load`,
    options,
    { body: then === undefined ? { artifactRef } : { artifactRef, then } },
  );
  const parsed: unknown = await response.json();
  if (isLoadOperation(parsed)) {
    return parsed;
  }
  if (isAcceptedBody(parsed)) {
    return parsed;
  }
  throw new OperationApiError('The server returned an unexpected artifact-load body.');
}

/**
 * `GET /api/v1/media/players/{id}/load-operations/{operation}` — read one
 * actor- and player-scoped native load operation to its (possibly still
 * pending) current state. Poll this until `state` is terminal; the native
 * ack (duration/geometry/audio) arrives only once committed.
 */
export async function getPlayerLoadOperation(
  id: string,
  operation: number,
  options: RequestOptions = {},
): Promise<LoadOperation> {
  const path = `${itemPath(id)}/load-operations/${encodeURIComponent(String(operation))}`;
  const response = await fetch(apiUrl(options, path), {
    method: 'GET',
    headers: buildHeaders(options, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isLoadOperation(body)) {
    throw new OperationApiError('The server returned an unexpected load-operation body.');
  }
  return body;
}

/**
 * The native load operation states this server actually reports
 * (`crates/multiview-cli/src/player/provider.rs::classify`): the pending
 * in-flight state, and three terminal outcomes. A different `PlayerLoadProvider`
 * is free to report a `state` outside this set — {@link isTerminalLoadOperationState}
 * treats anything it does not recognise as still pending (never assumes an
 * unrecognised value is already terminal, which would stop polling early and
 * mislabel the operation as stuck).
 */
export type LoadOperationState = 'pending' | 'completed' | 'failed' | 'superseded';

/** Whether `state` is one of this provider's three terminal load outcomes. */
export function isTerminalLoadOperationState(state: string): boolean {
  return state === 'completed' || state === 'failed' || state === 'superseded';
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
