// Source lifecycle status (`GET /api/v1/sources/status`): whether each
// configured source is idle (not started, so it costs nothing), starting, ready
// or failed, and who uses it. Idle is normal, never a fault. Manual Start / Stop
// (`POST /api/v1/sources/{id}/start|stop`).
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import type { UseMutationResult, UseQueryResult } from '@tanstack/react-query';

import { apiUrl, buildHeaders, OperationApiError, readPendingProblem } from './operations';
import type { RequestOptions } from './operations';
import type { components } from './types';

/** One source's demand state. */
export type SourceDemand = components['schemas']['SourceDemandView'];
/** A source demand phase. */
export type SourceDemandPhase = components['schemas']['SourceDemandPhase'];

function isDemandList(value: unknown): value is SourceDemand[] {
  return (
    Array.isArray(value) &&
    value.every((v: unknown) => typeof v === 'object' && v !== null && 'id' in v && 'phase' in v)
  );
}

/** Fetch every source's demand state. */
export async function getSourceStatus(options: RequestOptions = {}): Promise<SourceDemand[]> {
  const response = await fetch(apiUrl(options, '/api/v1/sources/status'), {
    method: 'GET',
    headers: buildHeaders(options, false),
  });
  if (!response.ok) {
    throw await readPendingProblem(response);
  }
  const body: unknown = await response.json();
  if (!isDemandList(body)) {
    throw new OperationApiError('The server returned an unexpected source status list.');
  }
  return body;
}

/** Stable query key for the source demand list. */
export const sourceStatusKey = ['sources', 'status'] as const;

/** Every source's demand state, refreshed every few seconds. */
export function useSourceStatus(): UseQueryResult<ReadonlyMap<string, SourceDemand>, OperationApiError> {
  return useQuery<SourceDemand[], OperationApiError, ReadonlyMap<string, SourceDemand>>({
    queryKey: sourceStatusKey,
    queryFn: () => getSourceStatus(),
    select: (list) => new Map(list.map((s) => [s.id, s])),
    refetchInterval: 3000,
  });
}

/** Start or Stop. */
export type SourceAction = 'start' | 'stop';

/** Ask the server to start or stop source `id`. A refused Stop throws the
 * problem, whose detail names who still uses the source. */
export async function postSourceAction(
  id: string,
  action: SourceAction,
  options: RequestOptions = {},
): Promise<SourceDemand> {
  const response = await fetch(
    apiUrl(options, `/api/v1/sources/${encodeURIComponent(id)}/${action}`),
    { method: 'POST', headers: buildHeaders(options, true), body: '{}' },
  );
  if (!response.ok) {
    throw await readPendingProblem(response);
  }
  const body: unknown = await response.json();
  const list: unknown = [body];
  if (!isDemandList(list) || list[0] === undefined) {
    throw new OperationApiError('The server returned an unexpected source status.');
  }
  return list[0];
}

/** Start / Stop a source, refreshing the status list afterwards. */
export function useSourceAction(): UseMutationResult<
  SourceDemand,
  OperationApiError,
  { readonly id: string; readonly action: SourceAction }
> {
  const client = useQueryClient();
  return useMutation<SourceDemand, OperationApiError, { readonly id: string; readonly action: SourceAction }>({
    mutationFn: ({ id, action }) => postSourceAction(id, action),
    onSettled: () => client.invalidateQueries({ queryKey: sourceStatusKey }),
  });
}
