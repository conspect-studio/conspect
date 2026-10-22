// React Query bindings for the routing (crosspoint take) surface.
//
// `usePlanRoute()` classifies a take as a dry run (Class-1 hot vs Class-2
// migration) so the UI can warn before applying. `useTakeRoute()` applies it,
// minting a fresh `Idempotency-Key` per take so a network replay reserves the
// same operation rather than taking twice. A hot take resolves immediately; a
// Class-2 migration returns a `202` operation id whose outcome lands on the
// realtime stream. There is no read cache to invalidate — a take's effect is
// observed on the engine's realtime stream, not refetched here.
import { useMutation } from '@tanstack/react-query';
import type { UseMutationResult } from '@tanstack/react-query';

import { planRoute, takeRoute } from './routing';
import type {
  OperationApiError,
  RequestOptions,
  RouteKind,
  RoutePlan,
  RouteTakeRequest,
  TakeOutcome,
} from './routing';

export type {
  RouteClass,
  RouteKind,
  RoutePlan,
  RouteTakeRequest,
  RouteTarget,
  StreamRef,
  TakeApplied,
  TakeOutcome,
} from './routing';
export { ROUTE_KINDS, OperationApiError } from './routing';

/** Connection options threaded into the routing hooks. */
export interface RoutingContext {
  /** Base URL (defaults to same-origin). */
  readonly baseUrl?: string;
  /** Optional bearer token (defaults to the operator's stored token). */
  readonly token?: string;
}

function options(context: RoutingContext): RequestOptions {
  return {
    ...(context.baseUrl !== undefined ? { baseUrl: context.baseUrl } : {}),
    ...(context.token !== undefined ? { token: context.token } : {}),
  };
}

/** Classify a take without applying it. */
export function usePlanRoute(
  context: RoutingContext = {},
): UseMutationResult<RoutePlan, OperationApiError, RouteTakeRequest> {
  return useMutation<RoutePlan, OperationApiError, RouteTakeRequest>({
    mutationFn: (request): Promise<RoutePlan> => planRoute(request, options(context)),
  });
}

/** Variables passed to {@link useTakeRoute}. */
export interface TakeRouteVars {
  /** The `{kind}` path segment (`video` / `audio` / `subtitle`). */
  readonly kind: RouteKind;
  /** The source → target crosspoint to apply. */
  readonly request: RouteTakeRequest;
}

/** A fresh idempotency key for a take (a random UUID per attempt). */
function freshIdempotencyKey(): string {
  return crypto.randomUUID();
}

/** Apply a take, minting a fresh `Idempotency-Key`. */
export function useTakeRoute(
  context: RoutingContext = {},
): UseMutationResult<TakeOutcome, OperationApiError, TakeRouteVars> {
  return useMutation<TakeOutcome, OperationApiError, TakeRouteVars>({
    mutationFn: ({ kind, request }): Promise<TakeOutcome> =>
      takeRoute(kind, request, {
        ...options(context),
        idempotencyKey: freshIdempotencyKey(),
      }),
  });
}
