// React Query bindings for the cast-stream surface (`./castStreams`).
// `useCastConfig()` reads the receiver app id and the cast outputs; the
// controller (`../cast/castController`) owns stream creation and deletion
// so a cast survives page navigation.
import { useQuery } from '@tanstack/react-query';
import type { UseQueryResult } from '@tanstack/react-query';

import { getCastConfig } from './castStreams';
import type { CastConfig } from './castStreams';
import type { OperationApiError } from './operations';

export const castStreamKeys = {
  config: ['cast', 'config'] as const,
};

/** `GET /api/v1/cast/config`, refreshed (output readiness moves); `enabled` false skips it. */
export function useCastConfig(
  refetchInterval: number | false = 5_000,
  enabled = true,
): UseQueryResult<CastConfig, OperationApiError> {
  return useQuery<CastConfig, OperationApiError>({
    queryKey: castStreamKeys.config,
    queryFn: (): Promise<CastConfig> => getCastConfig(),
    refetchInterval,
    enabled,
  });
}
