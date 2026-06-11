// React Query bindings for the tally surface.
//
// Reads: `useTally()` (resolved state) and `useTallyProfiles()`. Writes:
// `useSaveProfile()` / `useDeleteProfile()` (ETag pre-read for the conditional
// PUT/DELETE, since neither list carries per-item ETags), and `useTallyOverride()`
// which sets/clears a manual override and returns the `202` operation id body.
// The engine is isolated (invariant #10): reads degrade to loading / error.
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
  clearOverride,
  deleteProfile,
  getProfile,
  listProfiles,
  listTally,
  putProfile,
  setOverride,
} from './tally';
import type {
  AcceptedBody,
  OperationApiError,
  RequestOptions,
  TallyColor,
  TallyEntry,
  TallyProfile,
  TallyTarget,
} from './tally';

export type {
  TallyColor,
  TallyEntry,
  TallyProfile,
  TallyTarget,
} from './tally';
export { OperationApiError } from './operations';

/** Connection options threaded into the tally hooks. */
export interface TallyContext {
  /** Base URL (defaults to same-origin). */
  readonly baseUrl?: string;
  /** Optional bearer token (defaults to the operator's stored token). */
  readonly token?: string;
}

function options(context: TallyContext): RequestOptions {
  return {
    ...(context.baseUrl !== undefined ? { baseUrl: context.baseUrl } : {}),
    ...(context.token !== undefined ? { token: context.token } : {}),
  };
}

/** Stable React Query keys for the tally reads. */
export const tallyKeys = {
  state: ['tally', 'state'] as const,
  profiles: ['tally', 'profiles'] as const,
};

/** List the resolved tally state per target. */
export function useTally(
  context: TallyContext = {},
): UseQueryResult<TallyEntry[], OperationApiError> {
  return useQuery<TallyEntry[], OperationApiError>({
    queryKey: tallyKeys.state,
    queryFn: (): Promise<TallyEntry[]> => listTally(options(context)),
  });
}

/** List the tally profiles. */
export function useTallyProfiles(
  context: TallyContext = {},
): UseQueryResult<TallyProfile[], OperationApiError> {
  return useQuery<TallyProfile[], OperationApiError>({
    queryKey: tallyKeys.profiles,
    queryFn: (): Promise<TallyProfile[]> => listProfiles(options(context)),
  });
}

/** Variables passed to {@link useSaveProfile}. */
export interface SaveProfileVars {
  /** The profile definition (its `id` is authoritative). */
  readonly profile: TallyProfile;
  /** Create (no `If-Match`) when true; replace (with `If-Match`) otherwise. */
  readonly create: boolean;
}

/** Create or replace a tally profile; a replace pre-reads the current ETag. */
export function useSaveProfile(
  context: TallyContext = {},
): UseMutationResult<TallyProfile, OperationApiError, SaveProfileVars> {
  const queryClient = useQueryClient();
  return useMutation<TallyProfile, OperationApiError, SaveProfileVars>({
    mutationFn: async ({ profile, create }): Promise<TallyProfile> => {
      const base = options(context);
      let etag: string | undefined;
      if (!create) {
        const current = await getProfile(profile.id, base);
        etag = current.etag;
      }
      const result = await putProfile(profile.id, profile, {
        ...base,
        ...(etag !== undefined ? { etag } : {}),
      });
      return result.profile;
    },
    onSettled: (): void => {
      void queryClient.invalidateQueries({ queryKey: tallyKeys.profiles });
    },
  });
}

/** Delete a tally profile; pre-reads the current ETag for the DELETE. */
export function useDeleteProfile(
  context: TallyContext = {},
): UseMutationResult<undefined, OperationApiError, string> {
  const queryClient = useQueryClient();
  return useMutation<undefined, OperationApiError, string>({
    mutationFn: async (id): Promise<undefined> => {
      const base = options(context);
      const current = await getProfile(id, base);
      await deleteProfile(id, {
        ...base,
        ...(current.etag !== undefined ? { etag: current.etag } : {}),
      });
      return undefined;
    },
    onSettled: (): void => {
      void queryClient.invalidateQueries({ queryKey: tallyKeys.profiles });
    },
  });
}

/** Variables passed to {@link useTallyOverride}. */
export type OverrideVars =
  | { readonly action: 'set'; readonly target: TallyTarget; readonly color: TallyColor }
  | { readonly action: 'clear'; readonly target: TallyTarget };

/**
 * Set or clear a manual tally override. Returns the `202` operation id body; the
 * resolved lamp arrives later on the realtime stream. Refetches the resolved
 * state on settle so the table re-reads server state when it lands.
 */
export function useTallyOverride(
  context: TallyContext = {},
): UseMutationResult<AcceptedBody, OperationApiError, OverrideVars> {
  const queryClient = useQueryClient();
  return useMutation<AcceptedBody, OperationApiError, OverrideVars>({
    mutationFn: (vars): Promise<AcceptedBody> => {
      const base = options(context);
      return vars.action === 'set'
        ? setOverride(vars.target, vars.color, base)
        : clearOverride(vars.target, base);
    },
    onSettled: (): void => {
      void queryClient.invalidateQueries({ queryKey: tallyKeys.state });
    },
  });
}
