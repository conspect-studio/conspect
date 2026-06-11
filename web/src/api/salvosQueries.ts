// React Query bindings for the salvos surface.
//
// `useSalvos()` lists definitions; `useSaveSalvo()` creates/replaces one (reading
// the current ETag via GET before a replace, since the list carries none);
// `useDeleteSalvo()` deletes (same ETag pre-read). `useSalvoOperation()` arms /
// takes / cancels and returns the `202` operation id so the page can surface it.
// All mutations invalidate the list on settle. The engine is isolated (invariant
// #10): every read degrades to loading / error states.
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
  armSalvo,
  cancelSalvo,
  deleteSalvo,
  getSalvo,
  listSalvos,
  putSalvo,
  takeSalvo,
} from './salvos';
import type {
  AcceptedBody,
  OperationApiError,
  RequestOptions,
  Salvo,
} from './salvos';

export type { Salvo } from './salvos';
export { OperationApiError } from './operations';

/** Connection options threaded into the salvo hooks. */
export interface SalvoContext {
  /** Base URL (defaults to same-origin). */
  readonly baseUrl?: string;
  /** Optional bearer token (defaults to the operator's stored token). */
  readonly token?: string;
}

function options(context: SalvoContext): RequestOptions {
  return {
    ...(context.baseUrl !== undefined ? { baseUrl: context.baseUrl } : {}),
    ...(context.token !== undefined ? { token: context.token } : {}),
  };
}

/** Stable React Query key for the salvo list. */
export const salvoKeys = {
  list: ['salvos'] as const,
};

/** List all salvo definitions. */
export function useSalvos(
  context: SalvoContext = {},
): UseQueryResult<Salvo[], OperationApiError> {
  return useQuery<Salvo[], OperationApiError>({
    queryKey: salvoKeys.list,
    queryFn: (): Promise<Salvo[]> => listSalvos(options(context)),
  });
}

/** Variables passed to {@link useSaveSalvo}. */
export interface SaveSalvoVars {
  /** The salvo definition (its `id` is authoritative). */
  readonly salvo: Salvo;
  /** Create (`PUT` without `If-Match`) when true; replace (with `If-Match`). */
  readonly create: boolean;
}

/** Create or replace a salvo; a replace pre-reads the current ETag. */
export function useSaveSalvo(
  context: SalvoContext = {},
): UseMutationResult<Salvo, OperationApiError, SaveSalvoVars> {
  const queryClient = useQueryClient();
  return useMutation<Salvo, OperationApiError, SaveSalvoVars>({
    mutationFn: async ({ salvo, create }): Promise<Salvo> => {
      const base = options(context);
      let etag: string | undefined;
      if (!create) {
        // Replace: read the live ETag so the conditional PUT carries `If-Match`.
        const current = await getSalvo(salvo.id, base);
        etag = current.etag;
      }
      const result = await putSalvo(salvo.id, salvo, {
        ...base,
        ...(etag !== undefined ? { etag } : {}),
      });
      return result.salvo;
    },
    onSettled: (): void => {
      void queryClient.invalidateQueries({ queryKey: salvoKeys.list });
    },
  });
}

/** Delete a salvo; pre-reads the current ETag for the conditional DELETE. */
export function useDeleteSalvo(
  context: SalvoContext = {},
): UseMutationResult<undefined, OperationApiError, string> {
  const queryClient = useQueryClient();
  return useMutation<undefined, OperationApiError, string>({
    mutationFn: async (id): Promise<undefined> => {
      const base = options(context);
      const current = await getSalvo(id, base);
      await deleteSalvo(id, {
        ...base,
        ...(current.etag !== undefined ? { etag: current.etag } : {}),
      });
      return undefined;
    },
    onSettled: (): void => {
      void queryClient.invalidateQueries({ queryKey: salvoKeys.list });
    },
  });
}

/** The arm / take / cancel command kinds. */
export type SalvoAction = 'arm' | 'take' | 'cancel';

/** Variables passed to {@link useSalvoOperation}. */
export interface SalvoOperationVars {
  /** The salvo id to act on. */
  readonly id: string;
  /** Which command to submit. */
  readonly action: SalvoAction;
}

/** Arm / take / cancel a salvo, returning the `202` operation id body. */
export function useSalvoOperation(
  context: SalvoContext = {},
): UseMutationResult<AcceptedBody, OperationApiError, SalvoOperationVars> {
  return useMutation<AcceptedBody, OperationApiError, SalvoOperationVars>({
    mutationFn: ({ id, action }): Promise<AcceptedBody> => {
      const base = options(context);
      switch (action) {
        case 'arm':
          return armSalvo(id, base);
        case 'take':
          return takeSalvo(id, base);
        case 'cancel':
          return cancelSalvo(id, base);
      }
    },
  });
}
