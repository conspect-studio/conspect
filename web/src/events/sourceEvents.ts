import { useQuery } from '@tanstack/react-query';
import { createApiClient } from '../api/client';
import { OperationApiError } from '../api/operations';
import type { components } from '../api/schema';

export type SourceEvents = components['schemas']['SourceEventsDto'];
export type SourceEvent = components['schemas']['SourceEventDto'];

/** Mounted only inside an open dialog. Consuming the signal cancels on close/switch. */
export function useSourceEvents(id: string) {
  return useQuery({
    queryKey: ['inputs', id, 'events'],
    queryFn: async ({ signal }): Promise<SourceEvents> => {
      const { data, response } = await createApiClient().GET('/api/v1/inputs/{id}/events', {
        params: { path: { id } }, signal, cache: 'no-store',
      });
      if (!response.ok) throw new OperationApiError('Source events request failed', response.status);
      if (data?.source_id !== id) {
        throw new OperationApiError('Invalid source events response');
      }
      return data;
    },
    refetchInterval: 1000,
    retry: false,
    gcTime: 0,
    staleTime: 0,
  });
}

/** Exact decimal seconds; never convert a nanosecond integer through Number. */
export function nanosecondsToSeconds(value: string): string | undefined {
  if (value.length > 20 || !/^(0|[1-9]\d*)$/.test(value)) return undefined;
  const ns = BigInt(value);
  if (ns > 18_446_744_073_709_551_615n) return undefined;
  const fraction = (ns % 1_000_000_000n).toString().padStart(9, '0').replace(/0+$/, '');
  return `${(ns / 1_000_000_000n).toString()}${fraction === '' ? '' : `.${fraction}`}`;
}
