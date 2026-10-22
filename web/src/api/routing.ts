// Routing surface: classify and apply a crosspoint take.
//
// A "take" routes one source elementary stream onto a destination (a video
// cell, an audio program-bus channel, an audio discrete track, or a subtitle
// layer). The control plane classifies every take (invariant #11):
//   * `POST /api/v1/routing/plan` is a dry run — it returns a `RoutePlan`
//     ({class, coerced}) WITHOUT applying anything.
//   * `POST /api/v1/routing/{kind}/take` applies it. A hot (Class-1 /
//     reset-lite) take returns `200` with a `TakeApplied` body; a Class-2
//     migration returns `202 Accepted` with an operation id (the outcome lands
//     on the realtime stream). Both share the `RouteTakeRequest` body, and the
//     take carries an `Idempotency-Key` so a replay reserves the same operation.
import {
  apiUrl,
  buildHeaders,
  OperationApiError,
  readProblem,
} from './operations';
import type { AcceptedBody, RequestOptions } from './operations';
import type { components } from './schema';

/** The body of a plan or take request (a source → target crosspoint). */
export type RouteTakeRequest = components['schemas']['RouteTakeRequest'];

/** The dry-run classification a plan returns. */
export type RoutePlan = components['schemas']['RoutePlan'];

/** The body a hot (Class-1 / reset-lite) take returns. */
export type TakeApplied = components['schemas']['TakeApplied'];

/** The class a take resolves to (`class1` hot, `reset_lite`, `class2` migration). */
export type RouteClass = components['schemas']['RouteClass'];

/** The destination of a take (an internally-tagged 4-arm union). */
export type RouteTarget = components['schemas']['RouteTargetDoc'];

/** A reference to a source elementary stream. */
export type StreamRef = components['schemas']['StreamRefDoc'];

export { OperationApiError } from './operations';
export type { AcceptedBody, RequestOptions } from './operations';

/** The take kinds the `{kind}` path segment accepts. */
export type RouteKind = 'video' | 'audio' | 'subtitle';

/** The take kinds, in display order. */
export const ROUTE_KINDS: readonly RouteKind[] = ['video', 'audio', 'subtitle'];

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

function isRoutePlan(value: unknown): value is RoutePlan {
  return (
    isRecord(value) &&
    typeof value.class === 'string' &&
    typeof value.coerced === 'boolean'
  );
}

function isTakeApplied(value: unknown): value is TakeApplied {
  return (
    isRecord(value) &&
    typeof value.applied === 'boolean' &&
    typeof value.class === 'string' &&
    typeof value.coerced === 'boolean' &&
    typeof value.operation_id === 'string'
  );
}

function isAcceptedBody(value: unknown): value is AcceptedBody {
  return (
    isRecord(value) &&
    typeof value.operation_id === 'string' &&
    typeof value.kind === 'string'
  );
}

/** Classify a take without applying it (`POST /api/v1/routing/plan`). */
export async function planRoute(
  request: RouteTakeRequest,
  options: RequestOptions = {},
): Promise<RoutePlan> {
  const response = await fetch(apiUrl(options, '/api/v1/routing/plan'), {
    method: 'POST',
    headers: buildHeaders(options, true),
    body: JSON.stringify(request),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isRoutePlan(body)) {
    throw new OperationApiError('The server returned an unexpected route plan.');
  }
  return body;
}

/** The outcome of a take: applied hot, or accepted as a migration. */
export type TakeOutcome =
  | { readonly status: 'applied'; readonly applied: TakeApplied }
  | { readonly status: 'accepted'; readonly accepted: AcceptedBody };

/**
 * Apply a take (`POST /api/v1/routing/{kind}/take`). A hot take resolves to
 * `{ status: 'applied' }` (HTTP `200`); a Class-2 migration resolves to
 * `{ status: 'accepted' }` (HTTP `202`) whose operation id correlates the
 * eventual outcome on the realtime stream.
 */
export async function takeRoute(
  kind: RouteKind,
  request: RouteTakeRequest,
  options: RequestOptions = {},
): Promise<TakeOutcome> {
  const response = await fetch(
    apiUrl(options, `/api/v1/routing/${encodeURIComponent(kind)}/take`),
    {
      method: 'POST',
      headers: buildHeaders(options, true),
      body: JSON.stringify(request),
    },
  );
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (response.status === 202) {
    if (!isAcceptedBody(body)) {
      throw new OperationApiError('The server returned an unexpected command body.');
    }
    return { status: 'accepted', accepted: body };
  }
  if (!isTakeApplied(body)) {
    throw new OperationApiError('The server returned an unexpected take result.');
  }
  return { status: 'applied', applied: body };
}
