// React Query bindings for the cast-session surface.
//
// `useCastSessions()` lists the live ephemeral sessions, POLLED on a modest
// cadence (the cast.session.started/.removed realtime events live on a separate
// lane; this surface refetches instead, and a live-WS upgrade is a later
// drop-in). `useStartCastSession()` / `useStopCastSession()` / `useSaveCastSession()`
// mutate the session set and invalidate the list on settle. `useSetCastVolume()`
// returns the `202` operation id so the page can surface it (the applied level
// arrives on the realtime stream). The engine is isolated (invariant #10): every
// read degrades to loading / error states.
import {
  useMutation,
  useQuery,
  useQueryClient,
} from '@tanstack/react-query';
import type {
  UseMutationResult,
  UseQueryResult,
} from '@tanstack/react-query';

import {
  listCastSessions,
  saveCastSession,
  setCastVolume,
  startCastSession,
  stopCastSession,
} from './cast-sessions';
import type {
  AcceptedBody,
  CastSession,
  CastVolumeRequest,
  OperationApiError,
  RequestOptions,
  Resource,
  SaveCastSessionRequest,
  StartCastSessionRequest,
} from './cast-sessions';

export type {
  CastSession,
  CastVolumeRequest,
  Resource,
  SaveCastSessionRequest,
  StartCastSessionRequest,
} from './cast-sessions';
export { OperationApiError } from './operations';

/** Connection options threaded into the cast hooks. */
export interface CastContext {
  /** Base URL (defaults to same-origin). */
  readonly baseUrl?: string;
  /** Optional bearer token (defaults to the operator's stored token). */
  readonly token?: string;
  /**
   * Poll interval in milliseconds for the session list. Defaults to 4s; pass
   * `false` to disable polling (e.g. in a test).
   */
  readonly refetchInterval?: number | false;
}

function options(context: CastContext): RequestOptions {
  return {
    ...(context.baseUrl !== undefined ? { baseUrl: context.baseUrl } : {}),
    ...(context.token !== undefined ? { token: context.token } : {}),
  };
}

/** Stable React Query key for the cast session list. */
export const castSessionKeys = {
  list: ['cast', 'sessions'] as const,
};

/** Default poll cadence for the session list. */
const DEFAULT_CAST_POLL_MS = 4_000;

/** List the live ephemeral cast sessions (polled). */
export function useCastSessions(
  context: CastContext = {},
): UseQueryResult<CastSession[], OperationApiError> {
  return useQuery<CastSession[], OperationApiError>({
    queryKey: castSessionKeys.list,
    queryFn: (): Promise<CastSession[]> => listCastSessions(options(context)),
    refetchInterval: context.refetchInterval ?? DEFAULT_CAST_POLL_MS,
  });
}

/** Start an ad-hoc cast session; invalidates the list on settle. */
export function useStartCastSession(
  context: CastContext = {},
): UseMutationResult<CastSession, OperationApiError, StartCastSessionRequest> {
  const queryClient = useQueryClient();
  return useMutation<CastSession, OperationApiError, StartCastSessionRequest>({
    mutationFn: (request): Promise<CastSession> =>
      startCastSession(request, options(context)),
    onSettled: (): void => {
      void queryClient.invalidateQueries({ queryKey: castSessionKeys.list });
    },
  });
}

/** Stop a cast session by id; invalidates the list on settle. */
export function useStopCastSession(
  context: CastContext = {},
): UseMutationResult<undefined, OperationApiError, string> {
  const queryClient = useQueryClient();
  return useMutation<undefined, OperationApiError, string>({
    mutationFn: async (id): Promise<undefined> => {
      await stopCastSession(id, options(context));
      return undefined;
    },
    onSettled: (): void => {
      void queryClient.invalidateQueries({ queryKey: castSessionKeys.list });
    },
  });
}

/** Variables passed to {@link useSaveCastSession}. */
export interface SaveCastSessionVars {
  /** The ephemeral session id to promote. */
  readonly id: string;
  /** The save request (device id + optional display name). */
  readonly request: SaveCastSessionRequest;
}

/** Promote an ephemeral session to a managed device; returns the device. */
export function useSaveCastSession(
  context: CastContext = {},
): UseMutationResult<Resource, OperationApiError, SaveCastSessionVars> {
  const queryClient = useQueryClient();
  return useMutation<Resource, OperationApiError, SaveCastSessionVars>({
    mutationFn: ({ id, request }): Promise<Resource> =>
      saveCastSession(id, request, options(context)),
    onSettled: (): void => {
      // A promoted device may leave the ephemeral list; refresh it.
      void queryClient.invalidateQueries({ queryKey: castSessionKeys.list });
    },
  });
}

/** Variables passed to {@link useSetCastVolume}. */
export interface SetCastVolumeVars {
  /** The session id to set the volume on. */
  readonly id: string;
  /** The volume request (`level_percent`, 0–100). */
  readonly request: CastVolumeRequest;
}

/** Set the receiver volume; returns the `202` operation id body. */
export function useSetCastVolume(
  context: CastContext = {},
): UseMutationResult<AcceptedBody, OperationApiError, SetCastVolumeVars> {
  return useMutation<AcceptedBody, OperationApiError, SetCastVolumeVars>({
    mutationFn: ({ id, request }): Promise<AcceptedBody> =>
      setCastVolume(id, request, options(context)),
  });
}
