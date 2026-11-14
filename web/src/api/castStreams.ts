// Cast streams: the token URLs a Google Cast receiver plays (browser sender).
//   * `GET    /api/v1/cast/config` — receiver app id, namespace, cast outputs.
//   * `POST   /api/v1/cast/streams` — `{feed?|output?}` → `201` with `path`
//     (`/cast/<token>/master.m3u8`, relative to this origin; shown only here
//     and in the owner's GET). No `Idempotency-Key`.
//   * `GET    /api/v1/cast/streams` — own streams (never with `path`).
//   * `GET    /api/v1/cast/streams/{id}` — one stream (`ready`, `segments`,
//     `fetches`, `last_report`).
//   * `DELETE /api/v1/cast/streams/{id}` — `204`, revokes the token at once.
// Refusals are RFC 9457 problems whose `type` is `/problems/<reason>`;
// `castRefusalSentence` turns them into operator sentences.
import { t } from '@lingui/core/macro';

import { apiUrl, buildHeaders, OperationApiError, readProblem } from './operations';
import type { RequestOptions } from './operations';
import type { OutputVideoFeed } from './outputRouting';
import type { components } from './types';

export type CastConfig = components['schemas']['CastConfigDoc'];
export type CastOutput = components['schemas']['CastOutputDoc'];
export type CastStream = components['schemas']['CastStreamDoc'];
export type CastFetches = components['schemas']['CastFetches'];

/** `POST /cast/streams` body (`feed` typed as the take route's feed). */
export interface CreateCastStreamRequest {
  readonly feed?: OutputVideoFeed;
  readonly output?: string;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

function isFetches(value: unknown): value is CastFetches {
  return (
    isRecord(value) &&
    typeof value.playlists === 'number' &&
    typeof value.segments === 'number' &&
    typeof value.cast_device === 'boolean'
  );
}

function isCastStream(value: unknown): value is CastStream {
  return (
    isRecord(value) &&
    typeof value.id === 'string' &&
    typeof value.output === 'string' &&
    typeof value.content_type === 'string' &&
    typeof value.hls_segment_format === 'string' &&
    typeof value.hls_video_segment_format === 'string' &&
    typeof value.ready === 'boolean' &&
    typeof value.segments === 'number' &&
    typeof value.expires_unix_ms === 'number' &&
    isFetches(value.fetches)
  );
}

function isCastOutput(value: unknown): value is CastOutput {
  return (
    isRecord(value) &&
    typeof value.id === 'string' &&
    typeof value.active === 'boolean' &&
    typeof value.ready === 'boolean' &&
    typeof value.streams === 'number'
  );
}

function isCastConfig(value: unknown): value is CastConfig {
  return (
    isRecord(value) &&
    typeof value.namespace === 'string' &&
    typeof value.receiver_path === 'string' &&
    (value.receiver_app_id === undefined ||
      value.receiver_app_id === null ||
      typeof value.receiver_app_id === 'string') &&
    Array.isArray(value.outputs) &&
    value.outputs.every(isCastOutput)
  );
}

const STREAMS = '/api/v1/cast/streams';

function streamPath(id: string): string {
  return `${STREAMS}/${encodeURIComponent(id)}`;
}

async function readJson<T>(response: Response, guard: (value: unknown) => value is T, what: string): Promise<T> {
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!guard(body)) {
    throw new OperationApiError(`The server returned an unexpected ${what}.`);
  }
  return body;
}

/** `GET /api/v1/cast/config`. */
export async function getCastConfig(options: RequestOptions = {}): Promise<CastConfig> {
  const response = await fetch(apiUrl(options, '/api/v1/cast/config'), {
    headers: buildHeaders(options, false),
  });
  return readJson(response, isCastConfig, 'cast configuration');
}

/** `POST /api/v1/cast/streams` (`201`). */
export async function createCastStream(
  request: CreateCastStreamRequest,
  options: RequestOptions = {},
): Promise<CastStream> {
  const response = await fetch(apiUrl(options, STREAMS), {
    method: 'POST',
    headers: buildHeaders(options, true),
    body: JSON.stringify(request),
  });
  return readJson(response, isCastStream, 'cast stream');
}

/** `GET /api/v1/cast/streams/{id}`. */
export async function getCastStream(id: string, options: RequestOptions = {}): Promise<CastStream> {
  const response = await fetch(apiUrl(options, streamPath(id)), {
    headers: buildHeaders(options, false),
  });
  return readJson(response, isCastStream, 'cast stream');
}

function isCastStreamList(value: unknown): value is CastStream[] {
  return Array.isArray(value) && value.every(isCastStream);
}

/** `GET /api/v1/cast/streams` (own streams; never with `path`). */
export async function listCastStreams(options: RequestOptions = {}): Promise<CastStream[]> {
  const response = await fetch(apiUrl(options, STREAMS), {
    headers: buildHeaders(options, false),
  });
  return readJson(response, isCastStreamList, 'cast stream list');
}

/**
 * `DELETE /api/v1/cast/streams/{id}` (`204`; a `404` means it is already
 * gone). `keepalive` lets the request outlive a page that is being left.
 */
export async function deleteCastStream(
  id: string,
  options: RequestOptions & { readonly keepalive?: boolean } = {},
): Promise<void> {
  const response = await fetch(apiUrl(options, streamPath(id)), {
    method: 'DELETE',
    headers: buildHeaders(options, false),
    ...(options.keepalive === true ? { keepalive: true } : {}),
  });
  if (!response.ok && response.status !== 404) {
    throw await readProblem(response);
  }
}

/** The problem `reason` (`/problems/<reason>` → `<reason>`), if any. */
export function problemReason(error: unknown): string | undefined {
  if (!(error instanceof OperationApiError) || error.problemType === undefined) {
    return undefined;
  }
  const slash = error.problemType.lastIndexOf('/');
  return error.problemType.slice(slash + 1);
}

/**
 * An operator sentence for a failed cast-stream call: every documented
 * 403/409/422/429/503 of `POST /cast/streams`. Unknown refusals fall back to
 * the server's own `detail`.
 */
export function castRefusalSentence(error: unknown): string {
  const reason = problemReason(error);
  const status = error instanceof OperationApiError ? error.status : undefined;
  switch (reason) {
    case 'cast-unavailable':
      return t`Casting is not available right now: the engine has no usable cast output.`;
    case 'cast-capacity':
      return t`Every cast output is in use, or you have too many casts. Stop a cast and try again.`;
    case 'cast-no-encoder':
      return t`No free encoder is left for this cast. Stop another output or cast what a cast output already shows.`;
    case 'switcher-required':
      return t`Preview and Clean need a switcher in this run. Cast Program or a source instead.`;
    case 'aux-capacity':
      return t`Too many layouts are on air at once. Cast something else or free a layout output.`;
    case 'output-kind-unroutable':
      return t`This cast output cannot be routed to another feed.`;
    case 'routing-unavailable':
      return t`Routing is not available in this run, so the cast output cannot change what it shows.`;
    case 'unknown-feed':
      return t`That feed does not exist (any more). Choose another one.`;
    case 'layout-not-admissible':
      return t`That layout cannot be cast right now.`;
    case 'engine-busy':
      return t`The engine is busy and did not switch the cast output in time. Try again.`;
    default:
      break;
  }
  if (reason?.startsWith('refused-') === true) {
    return t`The engine refused to switch the cast output (${reason.slice(8)}).`;
  }
  if (status === 403) {
    return t`You are not allowed to cast this feed or use this cast output.`;
  }
  if (status === 429) {
    return t`Too many cast requests. Wait a moment and try again.`;
  }
  if (status === 503) {
    return t`The engine is busy and did not switch the cast output in time. Try again.`;
  }
  if (error instanceof OperationApiError) {
    return error.detail ?? error.message;
  }
  return error instanceof Error ? error.message : t`Could not prepare the cast stream.`;
}
