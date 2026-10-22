// React Query binding for the read-only system capability + licence report.
//
// `useSystemCapabilities()` reads `GET /api/v1/system/capabilities` once. The
// server value is a static startup snapshot (ADR-W030), so the query never goes
// stale and is not polled. The engine is isolated (invariant #10): the read
// degrades to loading / error states rather than assume a response.
import { useQuery } from '@tanstack/react-query';
import type { UseQueryResult } from '@tanstack/react-query';

import { getSystemCapabilities } from './system';
import type { SystemCapabilities, OperationApiError } from './system';

export type {
  SystemCapabilities,
  BackendCapability,
  CompositorCapability,
  BuildInfo,
  EffectiveLicense,
} from './system';
export { OperationApiError } from './operations';

/** Connection options threaded into the capabilities hook. */
export interface SystemCapabilitiesContext {
  /** Base URL (defaults to same-origin). */
  readonly baseUrl?: string;
  /** Optional bearer token (defaults to the operator's stored token). */
  readonly token?: string;
}

/** Stable React Query key for the capability report. */
export const systemCapabilitiesKeys = {
  report: (): readonly unknown[] => ['system', 'capabilities'],
};

/** Read the build capability + licence surface (static; not polled). */
export function useSystemCapabilities(
  context: SystemCapabilitiesContext = {},
): UseQueryResult<SystemCapabilities, OperationApiError> {
  return useQuery<SystemCapabilities, OperationApiError>({
    queryKey: systemCapabilitiesKeys.report(),
    queryFn: (): Promise<SystemCapabilities> =>
      getSystemCapabilities({
        ...(context.baseUrl !== undefined ? { baseUrl: context.baseUrl } : {}),
        ...(context.token !== undefined ? { token: context.token } : {}),
      }),
    // A static startup snapshot — never refetch on its own.
    staleTime: Number.POSITIVE_INFINITY,
  });
}
