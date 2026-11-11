import { useCallback, useSyncExternalStore } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { t } from '@lingui/core/macro';
import { OperationOutcomeError } from './operationOutcomes';
import { getStoredToken, subscribeStoredToken } from '../../api/token';
import { getOperationTracker, OPERATION_TRACKER_QUERY_KEY } from './operationTracker';
import type { OperationExpectation, SubmissionResult } from './operationTracker';
import type { AcceptedBody } from '../../api/operations';

/** Read only the shared shell socket's QueryClient-scoped outcome store. */
export function useOperationFeedback(operationId?: string) {
  const client = useQueryClient();
  const tracker = getOperationTracker(client);
  const subscribe = useCallback((notify: () => void) => {
    // Subscribe first, then reconcile the identity outside render. This also
    // closes a credential change between the render read and subscription.
    const unsubscribeCache = client.getQueryCache().subscribe((event) => {
      const key: unknown = event.query.queryKey;
      if (Array.isArray(key) && key.length === OPERATION_TRACKER_QUERY_KEY.length &&
          key.every((part: unknown, index: number) => part === OPERATION_TRACKER_QUERY_KEY[index])) notify();
    });
    const onIdentityChange = () => {
      tracker.resetIdentity(getStoredToken());
      notify();
    };
    const unsubscribeToken = subscribeStoredToken(onIdentityChange);
    window.addEventListener('storage', onIdentityChange);
    onIdentityChange();
    return () => {
      unsubscribeCache();
      unsubscribeToken();
      window.removeEventListener('storage', onIdentityChange);
    };
  }, [client, tracker]);
  const snapshot = useCallback(() => tracker.snapshot(), [tracker]);
  // Do not read query.data: the shared cache can still contain prior-identity
  // evidence on a remount. The canonical snapshot masks it before any effect.
  const state = useSyncExternalStore(subscribe, snapshot, snapshot);
  return operationId === undefined ? undefined : state.records.get(operationId);
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
