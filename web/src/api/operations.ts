// Shared HTTP helpers for the operator surfaces (alarms, salvos, tally, audit).
//
// SCHEMA STATUS (read me)
// -----------------------
// Most of these endpoints ARE modelled in the generated `paths` of `./schema.ts`
// and the read paths go through the typed `openapi-fetch` client. A few mutations
// are not in the spec even though the control plane wires them (notably `DELETE`
// on `/api/v1/salvos/{id}` and `/api/v1/tally/profiles/{id}` -- only their `PUT`
// is in `paths`), and the others need `If-Match` / 202-body handling the typed
// client does not express ergonomically. So -- exactly as `./layouts.ts` already
// does for the layout write ops -- those calls go through `fetch` with
// EXPLICITLY-TYPED request/response shapes that reuse the generated
// `components['schemas']` types (NOT hand-written shapes, NOT untyped `as`-casts).
// The token is read from `getStoredToken()`, mirroring `createApiClient`, so
// every call authenticates with the operator's bearer token.
//
// Long-running commands (arm/take/cancel, tally override) return `202 Accepted`
// with an operation id; the result arrives later on the realtime stream
// (conventions section 6, ADR-W008). `submitOperation` returns that body so the
// caller can surface the id. `ETag`/`If-Match` optimistic concurrency is honoured
// on the version-stamped mutations (alarm ack, salvo/profile replace + delete).
import type { components } from './schema';
import { isCorrelatedKind, REPLAY_KIND } from '../realtime/correlatedCommands';
import { correlateOperation } from '../realtime/operations';
import type { OperationOutcome } from '../realtime/operations';
import { getStoredToken } from './token';

/** An RFC 9457 problem document, as modelled in the spec. */
export type Problem = components['schemas']['Problem'];

/** The `202 Accepted` body the command endpoints return. */
export type AcceptedBody = components['schemas']['AcceptedBody'];

/**
 * An accepted command plus the authoritative completion: resolves when the
 * realtime envelope whose `corr` equals `operation_id` arrives (or from the
 * early-outcome cache), rejects on timeout. Ignoring it is safe.
 */
export type AcceptedOperation = AcceptedBody & {
  /**
   * Present only when the server correlates this command's outcome (see
   * `../realtime/correlatedCommands`) or the acceptance is an Idempotency-Key
   * replay (the original kind is unknown). Absent = acceptance-only.
   */
  readonly completion?: Promise<OperationOutcome>;
};

/** Register an accepted body for correlation and attach its completion promise. */
export function trackAccepted(body: AcceptedBody): AcceptedOperation {
  if (!isCorrelatedKind(body.kind) && body.kind !== REPLAY_KIND) {
    return { ...body };
  }
  const completion = correlateOperation(body);
  // A caller that never awaits `completion` must not trigger an unhandled rejection.
  completion.catch(() => undefined);
  return { ...body, completion };
}

/** A failed operator-surface call, normalized to a message + status (+ detail). */
export class OperationApiError extends Error {
  /** The HTTP status code, when one was returned. */
  readonly status: number | undefined;

  /** The RFC 9457 `detail`, when the failure carried a problem body. */
  readonly detail: string | undefined;

  constructor(message: string, status?: number, detail?: string) {
    super(message);
    this.name = 'OperationApiError';
    this.status = status;
    this.detail = detail;
  }
}

/** Connection options shared by every operator-surface call. */
export interface RequestOptions {
  /** Base URL (defaults to same-origin, matching the typed client). */
  readonly baseUrl?: string;
  /** Optional bearer token; falls back to the operator's stored token. */
  readonly token?: string;
  /**
   * The current ETag for `If-Match` on a version-stamped mutation. Omit when the
   * endpoint does not require optimistic concurrency.
   */
  readonly etag?: string;
  /**
   * An `Idempotency-Key` for a start/stop/swap-style command so a replayed
   * request reserves the same operation rather than starting a second one
   * (conventions section 6). Omit when the endpoint does not reserve by key.
   */
  readonly idempotencyKey?: string;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

function isProblem(value: unknown): value is Problem {
  return (
    isRecord(value) &&
    typeof value.status === 'number' &&
    typeof value.title === 'string'
  );
}

/** Parse an RFC 9457 problem out of a non-2xx response (status-only fallback). */
export async function readProblem(response: Response): Promise<OperationApiError> {
  try {
    const body: unknown = await response.json();
    if (isProblem(body)) {
      return new OperationApiError(
        body.title,
        body.status,
        body.detail ?? undefined,
      );
    }
  } catch {
    // Fall through to a status-only error when the body is absent/unparseable.
  }
  return new OperationApiError(
    `Request failed (${String(response.status)})`,
    response.status,
  );
}

/** Build the request headers, injecting the bearer token and `If-Match`. */
export function buildHeaders(options: RequestOptions, withJsonBody: boolean): Headers {
  const headers = new Headers();
  if (withJsonBody) {
    headers.set('Content-Type', 'application/json');
  }
  // An explicit token wins; otherwise fall back to the operator's stored token so
  // every call authenticates without threading the token through each page.
  const token = options.token ?? getStoredToken();
  if (token !== undefined && token !== '') {
    headers.set('Authorization', `Bearer ${token}`);
  }
  if (options.etag !== undefined && options.etag !== '') {
    headers.set('If-Match', options.etag);
  }
  if (options.idempotencyKey !== undefined && options.idempotencyKey !== '') {
    headers.set('Idempotency-Key', options.idempotencyKey);
  }
  return headers;
}

/** Join the (optional) base URL with an absolute API path. */
export function apiUrl(options: RequestOptions, path: string): string {
  return `${options.baseUrl ?? ''}${path}`;
}

function isAcceptedBody(value: unknown): value is AcceptedBody {
  return (
    isRecord(value) &&
    typeof value.operation_id === 'string' &&
    typeof value.kind === 'string'
  );
}

/** Options for {@link sendCommand}. */
export interface CommandRequest {
  /** HTTP method (defaults to `POST`). */
  readonly method?: 'POST' | 'PUT' | 'DELETE';
  /** JSON body; omitted for body-less commands. */
  readonly body?: unknown;
}

/**
 * A fresh RFC 4122 v4 UUID. `crypto.randomUUID` only exists in secure contexts, so
 * over plain HTTP on a non-loopback host it is undefined; fall back to building
 * one from `crypto.getRandomValues` (never `Math.random`).
 */
export function newIdempotencyKey(): string {
  if (typeof crypto.randomUUID === 'function') {
    return crypto.randomUUID();
  }
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  bytes[6] = ((bytes[6] ?? 0) & 0x0f) | 0x40;
  bytes[8] = ((bytes[8] ?? 0) & 0x3f) | 0x80;
  const hex = Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

/**
 * The single place every 202 command request is sent: mints the default
 * `crypto.randomUUID` `Idempotency-Key` (an explicit `options.idempotencyKey`
 * wins) and applies the JSON body, if any. Returns the raw response so callers
 * with a non-standard success shape (route take 200/202, set-mode, scan) can
 * parse it themselves; non-2xx responses throw the RFC 9457 problem.
 */
export async function sendCommand(
  path: string,
  options: RequestOptions = {},
  request: CommandRequest = {},
): Promise<Response> {
  const idempotencyKey = options.idempotencyKey ?? newIdempotencyKey();
  const hasBody = request.body !== undefined;
  const response = await fetch(apiUrl(options, path), {
    method: request.method ?? 'POST',
    headers: buildHeaders({ ...options, idempotencyKey }, hasBody),
    ...(hasBody ? { body: JSON.stringify(request.body) } : {}),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  return response;
}

/**
 * Send a long-running command (optionally with a JSON body) and return its `202
 * Accepted` body. The outcome arrives later on the realtime stream, never in this
 * response (conventions section 6); completion tracking is attached only for
 * commands the server correlates.
 */
export async function submitOperation(
  path: string,
  options: RequestOptions = {},
  request: CommandRequest = {},
): Promise<AcceptedOperation> {
  const response = await sendCommand(path, options, request);
  const body: unknown = await response.json();
  if (!isAcceptedBody(body)) {
    throw new OperationApiError('The server returned an unexpected command body.');
  }
  return trackAccepted(body);
}
