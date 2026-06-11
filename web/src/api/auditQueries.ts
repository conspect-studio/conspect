// React Query binding for the read-only audit log.
//
// `useAudit(objectId?)` reads `GET /api/v1/audit` (optionally scoped to one
// object). The engine is isolated (invariant #10): the read degrades to loading /
// error states rather than assume a response.
import { useQuery } from '@tanstack/react-query';
import type { UseQueryResult } from '@tanstack/react-query';

import { listAudit } from './audit';
import type { AuditEntry, OperationApiError } from './audit';

export type { AuditEntry } from './audit';
export { OperationApiError } from './operations';

/** Connection options threaded into the audit hook. */
export interface AuditContext {
  /** Base URL (defaults to same-origin). */
  readonly baseUrl?: string;
  /** Optional bearer token (defaults to the operator's stored token). */
  readonly token?: string;
}

/** Stable React Query key for the audit list (keyed by the object filter). */
export const auditKeys = {
  list: (objectId: string | undefined): readonly unknown[] => ['audit', objectId ?? null],
};

/** List audit entries, optionally scoped to a single object id. */
export function useAudit(
  objectId?: string,
  context: AuditContext = {},
): UseQueryResult<AuditEntry[], OperationApiError> {
  return useQuery<AuditEntry[], OperationApiError>({
    queryKey: auditKeys.list(objectId),
    queryFn: (): Promise<AuditEntry[]> =>
      listAudit(objectId, {
        ...(context.baseUrl !== undefined ? { baseUrl: context.baseUrl } : {}),
        ...(context.token !== undefined ? { token: context.token } : {}),
      }),
  });
}
