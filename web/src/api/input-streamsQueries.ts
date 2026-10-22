// React Query binding for the read-only input elementary-stream inventory.
//
// `useInputStreams(inputId?)` reads `GET /api/v1/inputs/{id}/streams`. It is
// disabled until an input id is given. The engine is isolated (invariant #10):
// the read degrades to loading / error / empty states rather than assume a
// response, and the inventory is honestly empty until the input is probed.
import { useQuery } from '@tanstack/react-query';
import type { UseQueryResult } from '@tanstack/react-query';

import { getInputStreams } from './input-streams';
import type { OperationApiError, StreamInventory } from './input-streams';

export type { StreamInventory, StreamDescriptor } from './input-streams';
export { OperationApiError } from './operations';

/** Connection options threaded into the input-streams hook. */
export interface InputStreamsContext {
  /** Base URL (defaults to same-origin). */
  readonly baseUrl?: string;
  /** Optional bearer token (defaults to the operator's stored token). */
  readonly token?: string;
}

/** Stable React Query key for an input's stream inventory. */
export const inputStreamKeys = {
  inventory: (inputId: string | undefined): readonly unknown[] => [
    'inputs',
    inputId ?? null,
    'streams',
  ],
};

/** Read an input's elementary-stream inventory (disabled until an id is given). */
export function useInputStreams(
  inputId?: string,
  context: InputStreamsContext = {},
): UseQueryResult<StreamInventory, OperationApiError> {
  const enabled = inputId !== undefined && inputId !== '';
  return useQuery<StreamInventory, OperationApiError>({
    queryKey: inputStreamKeys.inventory(inputId),
    enabled,
    queryFn: (): Promise<StreamInventory> =>
      getInputStreams(inputId ?? '', {
        ...(context.baseUrl !== undefined ? { baseUrl: context.baseUrl } : {}),
        ...(context.token !== undefined ? { token: context.token } : {}),
      }),
  });
}
