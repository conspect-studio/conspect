// Web-capture (VT fill) transport surface: discover honest capture
// capabilities, submit one bounded Windy capture job, poll/cancel it, and
// (separately, via `media-players.ts`) load the finished artifact into a
// player with an explicit `artifactRef`.
//
// A fill is an authorized, bounded public Windy view (crates/multiview-control
// src/web_capture.rs, routes/web_capture.rs,
// src/web_capture/provider.rs::capabilities). The control plane exposes it
// under `/api/v1/media/captures`:
//   * `GET  /api/v1/media/captures/capabilities` -> honest capability flags
//     (whether capture is provisioned, whether Premium/URL-fetch are
//     available). No request body, no auth-sensitive fields beyond the flags.
//   * `POST /api/v1/media/captures` (required `Idempotency-Key`) body
//     `CaptureBody` -> `202 { job_id, status_url }` once accepted.
//   * `GET  /api/v1/media/captures/{id}` -> `JobView` (poll until terminal).
//   * `POST /api/v1/media/captures/{id}/cancel` (required `Idempotency-Key`).
//
// `capabilities` is not yet modelled as a typed JSON object in the generated
// spec (`operations['capture_capabilities']` declares `200` with no typed
// content), so its shape is read defensively here, exactly like the other
// `fetch`-based helpers in `operations.ts`/`media-players.ts` — never a
// hand-written request/response type for a route the generated client
// already models structurally (`CaptureBody`/`JobView`/`Problem` all come
// from `./schema.ts`).
import {
  apiUrl,
  buildHeaders,
  newIdempotencyKey,
  readProblem,
  sendCommand,
  OperationApiError,
} from './operations';
import type { RequestOptions } from './operations';
import type { components } from './types';

export { OperationApiError, newIdempotencyKey } from './operations';
export type { RequestOptions } from './operations';

/** The bounded Windy view a capture submits (`CaptureBody.view`). */
export type CaptureView = components['schemas']['View'];

/** Terms acceptance, checked against the current template version server-side. */
export type CaptureTerms = components['schemas']['Terms'];

/** The `POST /api/v1/media/captures` request body. */
export type CaptureBody = components['schemas']['CaptureBody'];

/** Public job status projection (`GET /api/v1/media/captures/{id}`). */
export type JobView = components['schemas']['JobView'];

/** The current Windy terms-of-use template version the server checks. */
export const WINDY_TERMS_VERSION = 'windy-2023-09-01';

/**
 * `vt-<playerId>-windy` — a stable, player-derived `fill_id`. Passes
 * `authorize_object` (crates/multiview-control/src/auth.rs): the default
 * (unscoped) principal permits any object id, and a scoped principal can
 * allow-list this exact id per player, same as any other object id.
 */
export function windyFillId(playerId: string): string {
  return `vt-${playerId}-windy`;
}

/**
 * Honest capture capabilities (`GET /api/v1/media/captures/capabilities`):
 * whether capture is provisioned at all, whether authenticated Premium
 * capture is configured, and whether arbitrary-URL capture is enabled. A
 * server that answers with a flag this type does not recognise still parses
 * cleanly — unknown flags are ignored, never assumed false or true.
 */
export interface CaptureCapabilities {
  readonly capture_available: boolean;
  readonly premium_available: boolean;
  readonly url_fetch_available: boolean;
  readonly url_fetch_reason?: string | null;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

function isCaptureCapabilities(value: unknown): value is CaptureCapabilities {
  return (
    isRecord(value) &&
    typeof value.capture_available === 'boolean' &&
    typeof value.premium_available === 'boolean' &&
    typeof value.url_fetch_available === 'boolean'
  );
}

/** The `202` body of `POST /api/v1/media/captures`. */
export interface CreateCaptureAccepted {
  readonly job_id: number;
  readonly status_url?: string;
}

function isCreateCaptureAccepted(value: unknown): value is CreateCaptureAccepted {
  return isRecord(value) && typeof value.job_id === 'number';
}

function isJobView(value: unknown): value is JobView {
  return (
    isRecord(value) &&
    typeof value.fill_id === 'string' &&
    typeof value.job_id === 'number' &&
    typeof value.state === 'string'
  );
}

const CAPTURES = '/api/v1/media/captures';

function jobPath(id: number): string {
  return `${CAPTURES}/${encodeURIComponent(String(id))}`;
}

/**
 * `GET /api/v1/media/captures/capabilities` — whether this server is
 * provisioned to run captures at all, and whether Premium/URL-fetch are
 * enabled. Always call this before offering the fill dialog: a server
 * without a configured capture service refuses every submission, and the
 * dialog must say so honestly instead of just failing on submit.
 */
export async function getCaptureCapabilities(
  options: RequestOptions = {},
): Promise<CaptureCapabilities> {
  const response = await fetch(apiUrl(options, `${CAPTURES}/capabilities`), {
    method: 'GET',
    headers: buildHeaders(options, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isCaptureCapabilities(body)) {
    throw new OperationApiError('The server returned an unexpected capabilities body.');
  }
  return body;
}

/**
 * `POST /api/v1/media/captures` — submit one bounded capture job. A fresh
 * `Idempotency-Key` is minted unless `options.idempotencyKey` is supplied
 * (re-used on a retry of the SAME attempt so a replay reserves the same job
 * rather than starting a second one). Returns the `202` body (`job_id` +
 * `status_url`); the job's progress is read separately via `getCaptureJob`.
 */
export async function createCapture(
  body: CaptureBody,
  options: RequestOptions = {},
): Promise<CreateCaptureAccepted> {
  const idempotencyKey = options.idempotencyKey ?? newIdempotencyKey();
  const response = await sendCommand(CAPTURES, { ...options, idempotencyKey }, { body });
  const parsed: unknown = await response.json();
  if (!isCreateCaptureAccepted(parsed)) {
    throw new OperationApiError('The server returned an unexpected capture-accept body.');
  }
  return parsed;
}

/**
 * `GET /api/v1/media/captures/{id}` — one job's current status projection.
 * Poll this (e.g. every ~2s) until `state` is terminal
 * (`succeeded`/`failed`/`cancelled`/`uncertain`).
 */
export async function getCaptureJob(
  id: number,
  options: RequestOptions = {},
): Promise<JobView> {
  const response = await fetch(apiUrl(options, jobPath(id)), {
    method: 'GET',
    headers: buildHeaders(options, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isJobView(body)) {
    throw new OperationApiError('The server returned an unexpected job body.');
  }
  return body;
}

/**
 * `POST /api/v1/media/captures/{id}/cancel` — cancel a queued/running job. A
 * fresh `Idempotency-Key` is minted unless supplied.
 */
export async function cancelCapture(
  id: number,
  options: RequestOptions = {},
): Promise<void> {
  const idempotencyKey = options.idempotencyKey ?? newIdempotencyKey();
  await sendCommand(`${jobPath(id)}/cancel`, { ...options, idempotencyKey }, { method: 'POST' });
}

/** The job states the server reports (`JobView.state`). */
export type CaptureJobState =
  | 'queued'
  | 'running'
  | 'succeeded'
  | 'failed'
  | 'cancelled'
  | 'uncertain';

/** Whether `state` is one the poller should stop on. */
export function isTerminalCaptureState(state: string): boolean {
  return state === 'succeeded'
    || state === 'failed'
    || state === 'cancelled'
    || state === 'uncertain';
}
