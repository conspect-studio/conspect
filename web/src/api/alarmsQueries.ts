// React Query bindings for the alarms surface.
//
// `useAlarms(filter)` reads `GET /api/v1/alarms`; `useAckAlarm()` posts an
// acknowledgement and, on success, invalidates the list so the table re-reads
// authoritative server state (the engine is isolated -- invariant #10 -- so the
// UI is a best-effort reader). All reads degrade to loading / error states.
import {
  useMutation,
  useQuery,
  useQueryClient,
} from '@tanstack/react-query';
import type {
  UseMutationResult,
  UseQueryResult,
} from '@tanstack/react-query';

import { ackAlarm, listAlarms } from './alarms';
import type { AlarmApiError, AlarmFilter, AlarmRecord } from './alarms';

export type { AlarmFilter, AlarmRecord, Severity } from './alarms';
export { AlarmApiError } from './alarms';

/** Connection options threaded into the alarm hooks. */
export interface AlarmContext {
  /** Base URL (defaults to same-origin). */
  readonly baseUrl?: string;
  /** Optional bearer token (defaults to the operator's stored token). */
  readonly token?: string;
}

function options(context: AlarmContext): { baseUrl?: string; token?: string } {
  return {
    ...(context.baseUrl !== undefined ? { baseUrl: context.baseUrl } : {}),
    ...(context.token !== undefined ? { token: context.token } : {}),
  };
}

/** Stable React Query keys for the alarm list (keyed by its filter). */
export const alarmKeys = {
  all: ['alarms'] as const,
  list: (filter: AlarmFilter): readonly unknown[] => ['alarms', 'list', filter],
};

/** List alarms for the given filter. */
export function useAlarms(
  filter: AlarmFilter = {},
  context: AlarmContext = {},
): UseQueryResult<AlarmRecord[], AlarmApiError> {
  return useQuery<AlarmRecord[], AlarmApiError>({
    queryKey: alarmKeys.list(filter),
    queryFn: (): Promise<AlarmRecord[]> => listAlarms(filter, options(context)),
  });
}

/** Acknowledge an alarm by id, then refetch every alarm list. */
export function useAckAlarm(
  context: AlarmContext = {},
): UseMutationResult<AlarmRecord, AlarmApiError, string> {
  const queryClient = useQueryClient();
  return useMutation<AlarmRecord, AlarmApiError, string>({
    mutationFn: (id): Promise<AlarmRecord> => ackAlarm(id, options(context)),
    onSettled: (): void => {
      void queryClient.invalidateQueries({ queryKey: alarmKeys.all });
    },
  });
}
