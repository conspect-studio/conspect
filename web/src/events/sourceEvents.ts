import { useMutation, useQueryClient } from '@tanstack/react-query';
import type { UseMutationResult } from '@tanstack/react-query';
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

/** Operator test-injection request body (ADR-0183 item 5). */
export interface TestEventRequest {
  boundary: 'start' | 'end';
  duration_ns?: number;
}

/**
 * Inject a clearly-labelled synthetic event for this source (administer
 * role). Invalidates the live `useSourceEvents` query on success so an open
 * panel reflects the injected evidence on its next poll.
 */
export function useInjectTestEvent(id: string): UseMutationResult<undefined, OperationApiError, TestEventRequest> {
  const queryClient = useQueryClient();
  return useMutation<undefined, OperationApiError, TestEventRequest>({
    mutationFn: async (body): Promise<undefined> => {
      const { response } = await createApiClient().POST('/api/v1/inputs/{id}/events/test', {
        params: { path: { id } },
        body,
      });
      if (!response.ok) throw new OperationApiError('Test event injection failed', response.status);
      return undefined;
    },
    onSuccess: (): void => {
      void queryClient.invalidateQueries({ queryKey: ['inputs', id, 'events'] });
    },
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

export type AutomationView = components['schemas']['AutomationView'];
export type AutomationRule = components['schemas']['EventAutomationRuleDoc'];

const automationKey = (id: string): readonly unknown[] => ['inputs', id, 'events', 'automation'];

async function problemDetail(response: Response): Promise<string | undefined> {
  try {
    const body: unknown = await response.clone().json();
    if (typeof body === 'object' && body !== null && 'detail' in body && typeof body.detail === 'string') return body.detail;
    if (typeof body === 'object' && body !== null && 'title' in body && typeof body.title === 'string') return body.title;
  } catch { /* not a problem document */ }
  return undefined;
}

/** Automation rule, live status and action log (polled while the dialog is open). */
export function useAutomation(id: string) {
  return useQuery({
    queryKey: automationKey(id),
    queryFn: async ({ signal }): Promise<AutomationView> => {
      const { data, response } = await createApiClient().GET('/api/v1/inputs/{id}/events/automation', {
        params: { path: { id } }, signal, cache: 'no-store',
      });
      if (!response.ok || data === undefined) {
        throw new OperationApiError('Automation request failed', response.status, await problemDetail(response));
      }
      return data;
    },
    refetchInterval: 1000,
    retry: false,
    gcTime: 0,
    staleTime: 0,
  });
}

/** Arm/disarm/restore/save the rule; each returns the fresh automation view. */
export type AutomationCommand =
  | { readonly kind: 'arm' | 'disarm' | 'restore' }
  | { readonly kind: 'save'; readonly rule: AutomationRule | null };

export function useAutomationCommand(id: string): UseMutationResult<AutomationView, OperationApiError, AutomationCommand> {
  const queryClient = useQueryClient();
  return useMutation<AutomationView, OperationApiError, AutomationCommand>({
    mutationFn: async (command): Promise<AutomationView> => {
      const client = createApiClient();
      const params = { params: { path: { id } } };
      const result = command.kind === 'save'
        ? await client.PUT('/api/v1/inputs/{id}/events/automation', { ...params, body: { rule: command.rule } })
        : command.kind === 'arm'
          ? await client.POST('/api/v1/inputs/{id}/events/automation/arm', params)
          : command.kind === 'disarm'
            ? await client.POST('/api/v1/inputs/{id}/events/automation/disarm', params)
            : await client.POST('/api/v1/inputs/{id}/events/automation/restore', params);
      if (!result.response.ok || result.data === undefined) {
        throw new OperationApiError('Automation command failed', result.response.status, await problemDetail(result.response));
      }
      return result.data;
    },
    onSuccess: (view): void => {
      queryClient.setQueryData(automationKey(id), view);
    },
  });
}
