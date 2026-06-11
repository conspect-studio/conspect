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
import { getStoredToken } from './token';

/** An RFC 9457 problem document, as modelled in the spec. */
export type Problem = components['schemas']['Problem'];

/** The `202 Accepted` body the command endpoints return. */
export type AcceptedBody = components['schemas']['AcceptedBody'];

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

/**
 * `POST` a long-running command and return its `202 Accepted` body. The
 * command's outcome arrives later on the realtime stream, never in this response
 * (conventions section 6).
 */
export async function submitOperation(
  path: string,
  options: RequestOptions = {},
): Promise<AcceptedBody> {
  const response = await fetch(apiUrl(options, path), {
    method: 'POST',
    headers: buildHeaders(options, false),
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
