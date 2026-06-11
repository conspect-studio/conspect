// React Query bindings for the Sources / Outputs / Overlays resource lists.
//
// Each read hook fetches `GET /api/v1/{kind}` and projects every `{id,name,body}`
// record onto its display view-model (see `./api.ts`). The engine is isolated
// (invariant #10): these are best-effort reads and degrade to loading / error
// states rather than assume a response.
//
// Writes (create / update / delete) go through the CRUD hooks below. Update and
// delete read the per-resource ETag the list/get response carried and echo it as
// `If-Match` for optimistic concurrency. Mutations invalidate the affected list
// on settle so the projected views re-read authoritative server state.
import {
  useMutation,
  useQuery,
  useQueryClient,
} from '@tanstack/react-query';
import type {
  QueryClient,
  UseMutationResult,
  UseQueryResult,
} from '@tanstack/react-query';

import {
  deleteResource,
  getResource,
  listResources,
  ResourceApiError,
  toOutputView,
  toOverlayView,
  toProbeView,
  toSourceView,
  writeResource,
} from './api';
import type { ResourceWithEtag } from './api';
import type {
  OutputView,
  OverlayView,
  ProbeView,
  ResourceInput,
  ResourceKind,
  ResourceRecord,
  SourceView,
} from './types';

export type { ResourceInput, ResourceKind, ResourceRecord } from './types';

/** A failed resource call, normalized to a single human-readable message. */
export class ApiError extends Error {
  /** The HTTP status code, when the failure carried an RFC 9457 problem body. */
  readonly status: number | undefined;

  constructor(message: string, status?: number) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
  }
}

function toApiError(error: unknown): ApiError {
  if (error instanceof ResourceApiError) {
    return new ApiError(error.message, error.status);
  }
  if (error instanceof Error) {
    return new ApiError(error.message);
  }
  return new ApiError('Unknown error');
}

/** Stable React Query keys for the resource lists and their ETag maps. */
export const resourceKeys = {
  list: (kind: ResourceKind): readonly string[] => ['resources', kind],
  etags: (kind: ResourceKind): readonly string[] => ['resources', kind, 'etags'],
};

/** Per-resource ETag map, mirrored into the cache for `If-Match`. */
export type EtagMap = Readonly<Record<string, string>>;

/** Read the cached ETag map for a resource kind (empty when nothing stored). */
export function readResourceEtags(queryClient: QueryClient, kind: ResourceKind): EtagMap {
  return queryClient.getQueryData<EtagMap>(resourceKeys.etags(kind)) ?? {};
}

/** Connection options threaded into the read + write helpers. */
export interface ResourceContext {
  /** Base URL for the helpers (defaults to same-origin). */
  readonly baseUrl?: string;
  /** Optional bearer token. */
  readonly token?: string;
}

function requestOptions(context: ResourceContext): { baseUrl?: string; token?: string } {
  return {
    ...(context.baseUrl !== undefined ? { baseUrl: context.baseUrl } : {}),
    ...(context.token !== undefined ? { token: context.token } : {}),
  };
}

function rememberEtag(
  queryClient: QueryClient,
  kind: ResourceKind,
  id: string,
  etag: string | undefined,
): void {
  if (etag === undefined) {
    return;
  }
  queryClient.setQueryData<EtagMap>(resourceKeys.etags(kind), (current) => ({
    ...(current ?? {}),
    [id]: etag,
  }));
}

/**
 * Build the read hook for a resource kind: list `GET /api/v1/{kind}` and project
 * each record onto its view-model. The factory keeps Sources/Outputs/Overlays
 * DRY while preserving their distinct, fully-typed view shapes.
 */
function makeListHook<View>(
  kind: ResourceKind,
  project: (record: ResourceRecord) => View,
): (context?: ResourceContext) => UseQueryResult<readonly View[], ApiError> {
  return function useList(
    context: ResourceContext = {},
  ): UseQueryResult<readonly View[], ApiError> {
    const queryClient = useQueryClient();
    return useQuery<readonly View[], ApiError>({
      queryKey: resourceKeys.list(kind),
      queryFn: async (): Promise<readonly View[]> => {
        let records: ResourceRecord[];
        try {
          records = await listResources(kind, requestOptions(context));
        } catch (error) {
          throw toApiError(error);
        }
        // The list response does not carry per-item ETags; clear any stale map
        // so an update re-fetches the current ETag via GET before its PUT.
        queryClient.setQueryData<EtagMap>(resourceKeys.etags(kind), {});
        return records.map(project);
      },
    });
  };
}

/** List the managed ingest sources, projected to {@link SourceView}. */
export const useSources = makeListHook<SourceView>('sources', toSourceView);

/** List the configured outputs, projected to {@link OutputView}. */
export const useOutputs = makeListHook<OutputView>('outputs', toOutputView);

/** List the configured overlays, projected to {@link OverlayView}. */
export const useOverlays = makeListHook<OverlayView>('overlays', toOverlayView);

/** List the configured fail-state probes, projected to {@link ProbeView}. */
export const useProbes = makeListHook<ProbeView>('probes', toProbeView);

/** The variables passed to a save mutation. */
export interface SaveResourceVars {
  /** The target resource id (the path is authoritative). */
  readonly id: string;
  /** The create/update payload. */
  readonly input: ResourceInput;
  /** Create (`POST`) when true; update (`PUT` + `If-Match`) when false. */
  readonly create: boolean;
}

/**
 * Create or update a resource of the given kind. On update the stored ETag is
 * read first (or fetched via GET when absent) and echoed as `If-Match`; the
 * response ETag is remembered for the next write. The affected list is
 * invalidated on settle so the projected view re-reads server state.
 */
export function useSaveResource(
  kind: ResourceKind,
  context: ResourceContext = {},
): UseMutationResult<ResourceRecord, ApiError, SaveResourceVars> {
  const queryClient = useQueryClient();
  return useMutation<ResourceRecord, ApiError, SaveResourceVars>({
    mutationFn: async ({ id, input, create }): Promise<ResourceRecord> => {
      try {
        let etag = readResourceEtags(queryClient, kind)[id];
        if (!create && etag === undefined) {
          // No cached ETag (e.g. first edit after a list read): fetch the
          // current one so the conditional PUT carries an `If-Match`.
          const current = await getResource(kind, id, requestOptions(context));
          etag = current.etag;
        }
        const result: ResourceWithEtag = await writeResource(
          kind,
          id,
          input,
          create,
          {
            ...requestOptions(context),
            ...(etag !== undefined ? { etag } : {}),
          },
        );
        rememberEtag(queryClient, kind, result.record.id, result.etag);
        return result.record;
      } catch (error) {
        throw toApiError(error);
      }
    },
    onSettled: (): void => {
      void queryClient.invalidateQueries({ queryKey: resourceKeys.list(kind) });
    },
  });
}

/**
 * Delete a resource of the given kind. The control plane requires `If-Match` on
 * delete (`428` without it, `412` when stale), and the list response carries no
 * per-item ETags — so, mirroring {@link useSaveResource}, the stored ETag is
 * read first and, when uncached, fetched via GET before the conditional DELETE.
 * The list is invalidated on settle.
 */
export function useDeleteResource(
  kind: ResourceKind,
  context: ResourceContext = {},
): UseMutationResult<undefined, ApiError, string> {
  const queryClient = useQueryClient();
  return useMutation<undefined, ApiError, string>({
    mutationFn: async (id): Promise<undefined> => {
      try {
        let etag = readResourceEtags(queryClient, kind)[id];
        if (etag === undefined) {
          // No cached ETag: fetch the current one so the DELETE carries an
          // `If-Match` (the backend rejects an unconditional delete with 428).
          const current = await getResource(kind, id, requestOptions(context));
          etag = current.etag;
        }
        await deleteResource(kind, id, {
          ...requestOptions(context),
          ...(etag !== undefined ? { etag } : {}),
        });
      } catch (error) {
        throw toApiError(error);
      }
      return undefined;
    },
    onSettled: (): void => {
      void queryClient.invalidateQueries({ queryKey: resourceKeys.list(kind) });
    },
  });
}
