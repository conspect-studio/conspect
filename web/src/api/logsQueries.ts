// React Query binding for the read-only buffered log tail.
//
// `useLogs(query?)` reads `GET /api/v1/logs`, narrowed by the level / resource
// filters. The engine is isolated (invariant #10): the read degrades to loading
// / error states rather than assume a response. The tail is polled on a modest
// cadence so a left-open Logs page keeps trickling new records without a live
// socket (the cast/routing/logs surfaces use polling on this PR; a realtime
// upgrade is a later drop-in).
import { useQuery } from '@tanstack/react-query';
import type { UseQueryResult } from '@tanstack/react-query';

import { listLogs } from './logs';
import type { LogQuery, LogRecord, OperationApiError } from './logs';

export type { LogQuery, LogRecord, LogLevel, LogResourceKind } from './logs';
export { LOG_LEVELS, LOG_RESOURCE_KINDS } from './logs';
export { OperationApiError } from './operations';

/** Connection options threaded into the logs hook. */
export interface LogsContext {
  /** Base URL (defaults to same-origin). */
  readonly baseUrl?: string;
  /** Optional bearer token (defaults to the operator's stored token). */
  readonly token?: string;
  /**
   * Poll interval in milliseconds for the tail. Defaults to 5s; pass `false`
   * to disable polling (e.g. in a test).
   */
  readonly refetchInterval?: number | false;
}

/** Stable React Query key for the log tail (keyed by the narrowing filter). */
export const logKeys = {
  list: (query: LogQuery): readonly unknown[] => ['logs', query],
};

/** Default poll cadence for the log tail. */
const DEFAULT_LOG_POLL_MS = 5_000;

/** List buffered log records, narrowed by the given filter. */
export function useLogs(
  query: LogQuery = {},
  context: LogsContext = {},
): UseQueryResult<LogRecord[], OperationApiError> {
  return useQuery<LogRecord[], OperationApiError>({
    queryKey: logKeys.list(query),
    queryFn: (): Promise<LogRecord[]> =>
      listLogs(query, {
        ...(context.baseUrl !== undefined ? { baseUrl: context.baseUrl } : {}),
        ...(context.token !== undefined ? { token: context.token } : {}),
      }),
    refetchInterval: context.refetchInterval ?? DEFAULT_LOG_POLL_MS,
  });
}
