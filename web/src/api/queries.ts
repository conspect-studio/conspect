// React Query bindings over the typed API client.
//
// Each read hook calls the generated, typed client (see `client.ts`) so the
// query data shapes are exactly the control-plane response schemas. The engine
// is isolated (invariant #10): the UI is a best-effort reader and must degrade
// to loading / error states rather than assume a response.
//
// Writes go through the typed client functions in `./layouts.ts`
// (`createLayout`/`updateLayout`/`deleteLayoutById`), which call the spec-
// correct paths (`POST /api/v1/layouts/{id}`, `PUT`, `DELETE`). Mutations
// apply optimistic cache updates and roll back on error, and track per-layout
// ETags for `If-Match` optimistic concurrency (conventions §6).
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

import type { MultiviewApiClient } from './client';
import {
  createLayout,
  deleteLayoutById,
  LayoutApiError,
  updateLayout,
  fetchLayoutEtag,
  fetchLayoutWithEtag,
} from './layouts';
import type { Layout, LayoutInput, LayoutWithEtag } from './layouts';

export type { Layout, LayoutInput, LayoutWithEtag } from './layouts';

/** A failed API call, normalized to a single human-readable message. */
export class ApiError extends Error {
  /** The HTTP status code, when the failure carried an RFC 9457 problem body. */
  readonly status: number | undefined;
  /** The RFC 9457 problem `detail`, when the server returned one. */
  readonly detail: string | undefined;

  constructor(message: string, status?: number, detail?: string) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
    this.detail = detail;
  }
}

/** Stable React Query keys for the control-plane resources. */
export const queryKeys = {
  layouts: ['layouts'] as const,
  etags: ['layouts', 'etags'] as const,
};

/** Per-layout ETag map, mirrored into the cache for `If-Match`. */
export type EtagMap = Readonly<Record<string, string>>;

/** Read the cached ETag map (empty when nothing has been stored yet). */
export function readEtags(queryClient: QueryClient): EtagMap {
  return queryClient.getQueryData<EtagMap>(queryKeys.etags) ?? {};
}

/**
 * Fetch all layouts via `GET /api/v1/layouts`. The result type is inferred from
 * the OpenAPI schema, so a drift between the Rust API and this client is a
 * compile error.
 */
export function useLayouts(client: MultiviewApiClient): UseQueryResult<Layout[], ApiError> {
  return useQuery<Layout[], ApiError>({
    queryKey: queryKeys.layouts,
    queryFn: async (): Promise<Layout[]> => {
      // openapi-fetch returns a discriminated `{ data } | { error }`: when
      // `error` is absent, `data` is the typed success body (the Layout array).
      const { data, error } = await client.GET('/api/v1/layouts');
      if (error !== undefined) {
        throw new ApiError(error.title, error.status);
      }
      return data;
    },
  });
}

/** Connection context for the mutation hooks — just the typed client. */
export interface MutationContext {
  /** The typed API client (base URL + auth already baked in). */
  readonly api: MultiviewApiClient;
}

function toApiError(error: unknown): ApiError {
  if (error instanceof LayoutApiError) {
    return new ApiError(error.message, error.status, error.detail);
  }
  if (error instanceof Error) {
    return new ApiError(error.message);
  }
  return new ApiError('Unknown error');
}

/**
 * Read a single layout AND its `ETag` from the SAME response — for a
 * partial-field editor (e.g. the Layers dialog) that must base its edit on a
 * fresh read, never a list-derived snapshot, and must send exactly that same
 * `ETag` as `If-Match` on save (see {@link useSaveLayout}'s `ifMatch`).
 */
export async function fetchLayoutForEdit(
  client: MultiviewApiClient,
  id: string,
): Promise<LayoutWithEtag> {
  try {
    return await fetchLayoutWithEtag(client, id);
  } catch (error) {
    throw toApiError(error);
  }
}

/**
 * The shape passed to {@link useSaveLayout}.
 *
 * For **create**, supply both `id` (the caller-chosen resource id, per the
 * spec's `POST /api/v1/layouts/{id}`) and `input`.
 * For **update**, supply `id` and `input`; the stored ETag is read from the
 * cache and sent as `If-Match`.
 */
export interface SaveLayoutVars {
  /** The resource id — required for both create and update. */
  readonly id: string;
  /** The create/update payload. */
  readonly input: LayoutInput;
  /**
   * Pass `true` to force a create (POST), `false` to force an update (PUT).
   * When omitted, the presence of the id in the cached ETag map determines
   * whether to create or update: if the id has a known ETag it is an update,
   * otherwise a create.
   */
  readonly create?: boolean;
  /**
   * An explicit `If-Match` ETag that wins over the cached/fetched one. Use
   * this when the caller already holds a fresher read of the SAME resource
   * than the list/cache (e.g. a partial-field editor that GETs the layout
   * right before editing it) — sending the cache's possibly-stale ETag
   * instead would risk a silent overwrite of a concurrent save.
   */
  readonly ifMatch?: string;
}

interface SaveSnapshot {
  readonly previous: Layout[] | undefined;
}

/**
 * Create or update a layout with an optimistic cache update. On error the
 * previous layouts list is restored. ETags are read for `If-Match` (update) and
 * the response ETag is stored for the next write.
 *
 * The typed client calls `POST /api/v1/layouts/{id}` for create and
 * `PUT /api/v1/layouts/{id}` for update — both paths are in the generated spec.
 */
export function useSaveLayout(
  context: MutationContext,
): UseMutationResult<Layout, ApiError, SaveLayoutVars, SaveSnapshot> {
  const queryClient = useQueryClient();
  return useMutation<Layout, ApiError, SaveLayoutVars, SaveSnapshot>({
    mutationFn: async ({ input, id, create, ifMatch: explicitIfMatch }): Promise<Layout> => {
      const etags = readEtags(queryClient);
      let result;
      try {
        // An explicit `ifMatch` (the caller's own fresh same-resource read)
        // always wins — it is strictly fresher than anything the cache could
        // hold. Otherwise decide create vs update: explicit `create` flag
        // wins; a cached ETag means update. With neither (a reloaded session
        // editing an existing layout — the list carries no ETags), ask the
        // server: a current ETag means update with it as If-Match; a 404
        // means create.
        // A caller that asked for a conditional write must never get an
        // unconditional one: an empty explicit tag is refused, not dropped.
        if (explicitIfMatch !== undefined && explicitIfMatch.trim() === '') {
          throw new ApiError('Missing ETag for a conditional save', 428);
        }
        let ifMatch = explicitIfMatch ?? etags[id];
        let isCreate = explicitIfMatch !== undefined ? false : create;
        if (isCreate === undefined && ifMatch === undefined) {
          ifMatch = await fetchLayoutEtag(context.api, id);
          isCreate = ifMatch === undefined;
        }
        if (isCreate === true) {
          result = await createLayout(context.api, id, input);
        } else {
          result = await updateLayout(context.api, id, input, ifMatch);
        }
      } catch (error) {
        throw toApiError(error);
      }
      if (result.etag !== undefined) {
        queryClient.setQueryData<EtagMap>(queryKeys.etags, (current) => ({
          ...(current ?? {}),
          [result.layout.id]: result.etag ?? '',
        }));
      }
      return result.layout;
    },
    onMutate: async ({ input, id }): Promise<SaveSnapshot> => {
      await queryClient.cancelQueries({ queryKey: queryKeys.layouts });
      const previous = queryClient.getQueryData<Layout[]>(queryKeys.layouts);
      queryClient.setQueryData<Layout[]>(queryKeys.layouts, (current) =>
        applyOptimisticSave(current ?? [], input, id),
      );
      return { previous };
    },
    onError: (_error, _vars, snapshot): void => {
      if (snapshot?.previous !== undefined) {
        queryClient.setQueryData<Layout[]>(queryKeys.layouts, snapshot.previous);
      }
    },
    onSettled: (): void => {
      void queryClient.invalidateQueries({ queryKey: queryKeys.layouts });
    },
  });
}

function applyOptimisticSave(
  current: readonly Layout[],
  input: LayoutInput,
  id: string,
): Layout[] {
  const exists = current.some((layout) => layout.id === id);
  if (exists) {
    return current.map((layout) =>
      layout.id === id
        ? { ...layout, name: input.name, body: input.body }
        : layout,
    );
  }
  // Create: append a placeholder with the caller-supplied id until the server
  // confirms.
  const optimistic: Layout = { id, name: input.name, body: input.body };
  return [...current, optimistic];
}

interface DeleteSnapshot {
  readonly previous: Layout[] | undefined;
}

/**
 * Delete a layout with an optimistic removal; restores the list on error.
 * Calls `DELETE /api/v1/layouts/{id}` through the typed client, forwarding the
 * stored ETag as `If-Match`.
 */
export function useDeleteLayout(
  context: MutationContext,
): UseMutationResult<undefined, ApiError, string, DeleteSnapshot> {
  const queryClient = useQueryClient();
  return useMutation<undefined, ApiError, string, DeleteSnapshot>({
    mutationFn: async (id): Promise<undefined> => {
      const etag = readEtags(queryClient)[id];
      try {
        await deleteLayoutById(context.api, id, etag);
      } catch (error) {
        throw toApiError(error);
      }
      return undefined;
    },
    onMutate: async (id): Promise<DeleteSnapshot> => {
      await queryClient.cancelQueries({ queryKey: queryKeys.layouts });
      const previous = queryClient.getQueryData<Layout[]>(queryKeys.layouts);
      queryClient.setQueryData<Layout[]>(queryKeys.layouts, (current) =>
        (current ?? []).filter((layout) => layout.id !== id),
      );
      return { previous };
    },
    onError: (_error, _id, snapshot): void => {
      if (snapshot?.previous !== undefined) {
        queryClient.setQueryData<Layout[]>(queryKeys.layouts, snapshot.previous);
      }
    },
    onSettled: (): void => {
      void queryClient.invalidateQueries({ queryKey: queryKeys.layouts });
    },
  });
}
