// React Query bindings for the conspect account-side surfaces.
//
// Each read hook reads its endpoint through the typed helpers in `./conspect.ts`.
// The engine is isolated (invariant #10): every read degrades to loading/error
// states rather than assume a response, and no hook can back-pressure the engine.
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import type {
  UseMutationResult,
  UseQueryResult,
} from '@tanstack/react-query';

import {
  cancelAction,
  composeBundle,
  getAccountAudit,
  getBundle,
  getConsent,
  getHeartbeatStatus,
  getLicence,
  getMeshPeers,
  getMeshStatus,
  getPendingActions,
  getSupportEntitlement,
  getTelemetrySchema,
  getTicket,
  listTickets,
  raiseTicket,
  replyToTicket,
  setConsent,
  setRelay,
} from './conspect';
import type {
  AccountAuditPage,
  AccountAuditQuery,
  Bundle,
  BundleAccepted,
  CancelledBody,
  ComposeBundleInput,
  ConsentResource,
  HeartbeatStatus,
  LicenceResource,
  MeshPeerDoc,
  MeshStatusDoc,
  OperationApiError,
  PendingAction,
  RaiseTicketInput,
  SupportEntitlement,
  TelemetrySchema,
  Ticket,
  TicketSummary,
} from './conspect';

export type {
  AccountAuditEntry,
  AccountAuditKind,
  AccountAuditPage,
  Bundle,
  BundleInclude,
  BundleWindow,
  ConsentResource,
  DiagnosticsSnapshot,
  EnforcementLevel,
  HeartbeatStatus,
  LeaseDoc,
  LicenceResource,
  LicenceStatusDoc,
  MeshPeerDoc,
  MeshStatusDoc,
  PendingAction,
  SupportEntitlement,
  TelemetrySchema,
  Ticket,
  TicketSeverity,
  TicketSummary,
} from './conspect';
export { OperationApiError } from './conspect';

/** Stable React Query keys for the account-side resources. */
export const conspectKeys = {
  licence: ['conspect', 'licence'] as const,
  heartbeat: ['conspect', 'heartbeat'] as const,
  consent: ['conspect', 'consent'] as const,
  telemetrySchema: ['conspect', 'telemetry-schema'] as const,
  meshStatus: ['conspect', 'mesh', 'status'] as const,
  meshPeers: ['conspect', 'mesh', 'peers'] as const,
  pendingActions: ['conspect', 'actions', 'pending'] as const,
  supportEntitlement: ['conspect', 'support', 'entitlement'] as const,
  tickets: ['conspect', 'support', 'tickets'] as const,
  ticket: (id: string): readonly unknown[] => ['conspect', 'support', 'ticket', id],
  accountAudit: (query: AccountAuditQuery): readonly unknown[] => [
    'conspect',
    'account-audit',
    query.cursor ?? null,
    query.filter ?? null,
  ],
} as const;

/** Read the computed licence resource. */
export function useLicence(): UseQueryResult<LicenceResource, OperationApiError> {
  return useQuery<LicenceResource, OperationApiError>({
    queryKey: conspectKeys.licence,
    queryFn: (): Promise<LicenceResource> => getLicence(),
  });
}

/** Read the licensing-heartbeat status surface. */
export function useHeartbeatStatus(): UseQueryResult<HeartbeatStatus, OperationApiError> {
  return useQuery<HeartbeatStatus, OperationApiError>({
    queryKey: conspectKeys.heartbeat,
    queryFn: (): Promise<HeartbeatStatus> => getHeartbeatStatus(),
  });
}

/** Read the telemetry-consent document. */
export function useConsent(): UseQueryResult<ConsentResource, OperationApiError> {
  return useQuery<ConsentResource, OperationApiError>({
    queryKey: conspectKeys.consent,
    queryFn: (): Promise<ConsentResource> => getConsent(),
  });
}

/** Set the telemetry-consent state (LWW), refreshing the consent query. */
export function useSetConsent(): UseMutationResult<ConsentResource, OperationApiError, boolean> {
  const queryClient = useQueryClient();
  return useMutation<ConsentResource, OperationApiError, boolean>({
    mutationFn: (enabled: boolean): Promise<ConsentResource> => setConsent(enabled),
    onSuccess: (data): void => {
      queryClient.setQueryData<ConsentResource>(conspectKeys.consent, data);
    },
  });
}

/** Read the published daily-pipe telemetry schema. */
export function useTelemetrySchema(): UseQueryResult<TelemetrySchema, OperationApiError> {
  return useQuery<TelemetrySchema, OperationApiError>({
    queryKey: conspectKeys.telemetrySchema,
    queryFn: (): Promise<TelemetrySchema> => getTelemetrySchema(),
  });
}

/** Read the mesh discovery + relay summary. */
export function useMeshStatus(): UseQueryResult<MeshStatusDoc, OperationApiError> {
  return useQuery<MeshStatusDoc, OperationApiError>({
    queryKey: conspectKeys.meshStatus,
    queryFn: (): Promise<MeshStatusDoc> => getMeshStatus(),
  });
}

/** Opt in/out of relaying neighbours, refreshing the mesh-status query. */
export function useSetRelay(): UseMutationResult<MeshStatusDoc, OperationApiError, boolean> {
  const queryClient = useQueryClient();
  return useMutation<MeshStatusDoc, OperationApiError, boolean>({
    mutationFn: (enabled: boolean): Promise<MeshStatusDoc> => setRelay(enabled),
    onSuccess: (data): void => {
      queryClient.setQueryData<MeshStatusDoc>(conspectKeys.meshStatus, data);
    },
  });
}

/** Read the untrusted discovered-peer inventory. */
export function useMeshPeers(): UseQueryResult<MeshPeerDoc[], OperationApiError> {
  return useQuery<MeshPeerDoc[], OperationApiError>({
    queryKey: conspectKeys.meshPeers,
    queryFn: (): Promise<MeshPeerDoc[]> => getMeshPeers(),
  });
}

/** Read the queued remote-actions strip. */
export function usePendingActions(): UseQueryResult<PendingAction[], OperationApiError> {
  return useQuery<PendingAction[], OperationApiError>({
    queryKey: conspectKeys.pendingActions,
    queryFn: (): Promise<PendingAction[]> => getPendingActions(),
  });
}

/** Cancel a queued action locally, refreshing the pending-actions strip. */
export function useCancelAction(): UseMutationResult<CancelledBody, OperationApiError, string> {
  const queryClient = useQueryClient();
  return useMutation<CancelledBody, OperationApiError, string>({
    mutationFn: (id: string): Promise<CancelledBody> => cancelAction(id),
    onSuccess: (): void => {
      void queryClient.invalidateQueries({ queryKey: conspectKeys.pendingActions });
    },
  });
}

/** Read the support-entitlement routing answer. */
export function useSupportEntitlement(): UseQueryResult<SupportEntitlement, OperationApiError> {
  return useQuery<SupportEntitlement, OperationApiError>({
    queryKey: conspectKeys.supportEntitlement,
    queryFn: (): Promise<SupportEntitlement> => getSupportEntitlement(),
  });
}

/** List support tickets. */
export function useTickets(): UseQueryResult<TicketSummary[], OperationApiError> {
  return useQuery<TicketSummary[], OperationApiError>({
    queryKey: conspectKeys.tickets,
    queryFn: (): Promise<TicketSummary[]> => listTickets(),
  });
}

/** Read a single ticket + thread (only fetched when `id` is set). */
export function useTicket(id: string | undefined): UseQueryResult<Ticket, OperationApiError> {
  return useQuery<Ticket, OperationApiError>({
    queryKey: conspectKeys.ticket(id ?? ''),
    queryFn: (): Promise<Ticket> => getTicket(id ?? ''),
    enabled: id !== undefined && id !== '',
  });
}

/** Raise a support ticket, refreshing the ticket list. */
export function useRaiseTicket(): UseMutationResult<Ticket, OperationApiError, RaiseTicketInput> {
  const queryClient = useQueryClient();
  return useMutation<Ticket, OperationApiError, RaiseTicketInput>({
    mutationFn: (input: RaiseTicketInput): Promise<Ticket> => raiseTicket(input),
    onSuccess: (ticket): void => {
      queryClient.setQueryData<Ticket>(conspectKeys.ticket(ticket.ticket_id), ticket);
      void queryClient.invalidateQueries({ queryKey: conspectKeys.tickets });
    },
  });
}

/** Variables for a ticket reply. */
export interface ReplyVars {
  /** The ticket id. */
  readonly id: string;
  /** The reply text. */
  readonly body: string;
}

/** Append a reply to a ticket, refreshing that ticket + the list. */
export function useReplyToTicket(): UseMutationResult<Ticket, OperationApiError, ReplyVars> {
  const queryClient = useQueryClient();
  return useMutation<Ticket, OperationApiError, ReplyVars>({
    mutationFn: ({ id, body }: ReplyVars): Promise<Ticket> => replyToTicket(id, body),
    onSuccess: (ticket): void => {
      queryClient.setQueryData<Ticket>(conspectKeys.ticket(ticket.ticket_id), ticket);
      void queryClient.invalidateQueries({ queryKey: conspectKeys.tickets });
    },
  });
}

/** Compose a context-pack bundle → `202` id (the page then reads the preview). */
export function useComposeBundle(): UseMutationResult<
  BundleAccepted,
  OperationApiError,
  ComposeBundleInput
> {
  return useMutation<BundleAccepted, OperationApiError, ComposeBundleInput>({
    mutationFn: (input: ComposeBundleInput): Promise<BundleAccepted> => composeBundle(input),
  });
}

/** Read a composed bundle preview (only fetched when `id` is set). */
export function useBundle(id: string | undefined): UseQueryResult<Bundle, OperationApiError> {
  return useQuery<Bundle, OperationApiError>({
    queryKey: ['conspect', 'support', 'bundle', id ?? ''],
    queryFn: (): Promise<Bundle> => getBundle(id ?? ''),
    enabled: id !== undefined && id !== '',
  });
}

/** Read a page of the append-only account audit log. */
export function useAccountAudit(
  query: AccountAuditQuery,
): UseQueryResult<AccountAuditPage, OperationApiError> {
  return useQuery<AccountAuditPage, OperationApiError>({
    queryKey: conspectKeys.accountAudit(query),
    queryFn: (): Promise<AccountAuditPage> => getAccountAudit(query),
  });
}
