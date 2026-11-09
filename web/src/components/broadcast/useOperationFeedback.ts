import { useQuery, useQueryClient } from '@tanstack/react-query';
import { t } from '@lingui/core/macro';
import { OperationOutcomeError } from './operationOutcomes';
import type { OperationOutcomeState } from './operationOutcomes';
import { getOperationTracker, OPERATION_TRACKER_QUERY_KEY } from './operationTracker';
import type { OperationExpectation, SubmissionResult } from './operationTracker';
import type { AcceptedBody } from '../../api/operations';

/** Read only the shared shell socket's QueryClient-scoped outcome store. */
export function useOperationFeedback(operationId?: string) {
  const client = useQueryClient();
  const query = useQuery<OperationOutcomeState>({
    queryKey: OPERATION_TRACKER_QUERY_KEY,
    queryFn: () => getOperationTracker(client).snapshot(), enabled: false,
    initialData: () => getOperationTracker(client).snapshot(),
  });
  return operationId === undefined ? undefined : query.data.records.get(operationId);
}

/** Existing API function is supplied by the page; never issues/retries its own POST. */
export function useSubmitTrackedOperation() {
  const client = useQueryClient();
  return async (send: () => Promise<AcceptedBody>, expectation?: OperationExpectation,
    options?: { readonly deadlineMs?: number }): Promise<SubmissionResult> => {
    try {
      return await getOperationTracker(client).submit(send, expectation, options);
    } catch (error) {
      if (error instanceof OperationOutcomeError && error.code === 'capacity') {
        throw new Error(t`Too many unresolved commands. Wait for an outcome or resolve an unknown command before retrying.`, { cause: error });
      }
      throw error;
    }
  };
}
