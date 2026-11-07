// Source demand status (`GET /api/v1/sources/status`): whether each configured
// source is idle (nothing consumes it, so it costs nothing), starting, live or
// failed, and who consumes it. Idle is normal, never a fault.
import { useQuery } from '@tanstack/react-query';
import type { UseQueryResult } from '@tanstack/react-query';

import { apiUrl, buildHeaders, OperationApiError, readProblem } from './operations';
import type { RequestOptions } from './operations';
import type { components } from './schema';

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
    throw await readProblem(response);
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
