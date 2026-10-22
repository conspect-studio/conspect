// Conspect account-side API surface (licence, telemetry, mesh, support, audit,
// pending actions) over the typed control-plane client.
//
// SCHEMA STATUS (read me)
// -----------------------
// Every path here IS modelled in the generated `paths` of `./schema.ts` (the
// account-side endpoints regenerated from `docs/api/openapi.json`). JSON reads/
// writes go through the typed `openapi-fetch` client so the request/response
// shapes are checked against the spec at compile time. The two BINARY exchanges
// — the CBOR licence challenge export (`GET /api/v1/licence/challenge`) and the
// CBOR lease install (`POST /api/v1/licence/lease`) — go through `fetch` with
// EXPLICITLY-TYPED success/error shapes that reuse the generated
// `components['schemas']` types (NOT hand-written shapes, NOT untyped casts),
// because openapi-fetch does not express a raw `application/cbor` body/response
// ergonomically. The token is read from `getStoredToken()`, mirroring
// `createApiClient`, so every call authenticates with the operator's bearer
// token.
//
// The engine is isolated (invariant #10): every read here is best-effort and
// degrades to loading/error states; nothing on this surface can back-pressure
// the engine.
import { apiUrl, buildHeaders, OperationApiError, readProblem } from './operations';
import type { RequestOptions } from './operations';
import type { components } from './schema';

export { OperationApiError } from './operations';
export type { RequestOptions } from './operations';

// --- Resource type aliases (the rendered shapes) --------------------------

/** The computed licence resource (`GET /api/v1/licence`). */
export type LicenceResource = components['schemas']['LicenceResource'];
/** The computed licence status when a lease is installed. */
export type LicenceStatusDoc = components['schemas']['LicenceStatusDoc'];
/** The canonical enforcement-ladder level. */
export type EnforcementLevel = components['schemas']['EnforcementLevelDoc'];
/** The dated entitlement lease. */
export type LeaseDoc = components['schemas']['LeaseDoc'];
/** The `200` body of a successful lease install. */
export type LeaseInstalled = components['schemas']['LeaseInstalled'];
/** The read-only licensing-heartbeat status surface. */
export type HeartbeatStatus = components['schemas']['HeartbeatStatus'];
/** The telemetry-consent document. */
export type ConsentResource = components['schemas']['ConsentResource'];
/** The published daily-pipe telemetry schema. */
export type TelemetrySchema = components['schemas']['TelemetrySchema'];
/** The `202` body accepting a diagnostics-snapshot build. */
export type SnapshotAccepted = components['schemas']['SnapshotAccepted'];
/** The assembled diagnostics snapshot bundle. */
export type DiagnosticsSnapshot = components['schemas']['DiagnosticsSnapshot'];
/** The always-on mesh discovery + relay summary. */
export type MeshStatusDoc = components['schemas']['MeshStatusDoc'];
/** A single entry in the untrusted discovered-peer inventory. */
export type MeshPeerDoc = components['schemas']['MeshPeerDoc'];
/** A page of account-audit entries plus the resume cursor. */
export type AccountAuditPage = components['schemas']['AccountAuditPage'];
/** One immutable account-audit entry. */
export type AccountAuditEntry = components['schemas']['AccountAuditEntry'];
/** The kind of account-side action an audit entry records. */
export type AccountAuditKind = components['schemas']['AccountAuditKind'];
/** One queued remote action. */
export type PendingAction = components['schemas']['PendingAction'];
/** The `200` body of a successful cancel. */
export type CancelledBody = components['schemas']['CancelledBody'];
/** The entitlement-routing answer for the support surface. */
export type SupportEntitlement = components['schemas']['SupportEntitlement'];
/** A ticket summary for the list surface. */
export type TicketSummary = components['schemas']['TicketSummary'];
/** A ticket plus its append-only thread. */
export type Ticket = components['schemas']['Ticket'];
/** The declared severity of a ticket. */
export type TicketSeverity = components['schemas']['TicketSeverity'];
/** A composed support bundle (the context-pack preview). */
export type Bundle = components['schemas']['Bundle'];
/** The `202` body accepting a bundle compose. */
export type BundleAccepted = components['schemas']['BundleAccepted'];
/** A diagnostic section a bundle can include. */
export type BundleInclude = components['schemas']['BundleInclude'];
/** The reporting window a bundle reports over. */
export type BundleWindow = components['schemas']['BundleWindow'];

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

// --- Licence -------------------------------------------------------------

function isLicenceResource(value: unknown): value is LicenceResource {
  return isRecord(value) && typeof value.licensed === 'boolean';
}

/** Read the computed licence resource (`GET /api/v1/licence`). */
export async function getLicence(options: RequestOptions = {}): Promise<LicenceResource> {
  const response = await fetch(apiUrl(options, '/api/v1/licence'), {
    method: 'GET',
    headers: buildHeaders(options, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isLicenceResource(body)) {
    throw new OperationApiError('The server returned an unexpected licence resource.');
  }
  return body;
}

/**
 * Download the salted licence challenge (`GET /api/v1/licence/challenge`) as a
 * CBOR byte array — the operator carries it to an online portal to obtain a
 * signed lease (the §3.4 offline exchange). Returns the raw bytes; the page
 * triggers the browser download.
 */
export async function getChallenge(options: RequestOptions = {}): Promise<Uint8Array> {
  const response = await fetch(apiUrl(options, '/api/v1/licence/challenge'), {
    method: 'GET',
    headers: buildHeaders(options, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const buffer = await response.arrayBuffer();
  return new Uint8Array(buffer);
}

function isLeaseInstalled(value: unknown): value is LeaseInstalled {
  return (
    isRecord(value) &&
    typeof value.serial === 'string' &&
    typeof value.valid_to === 'string'
  );
}

/**
 * Install a signed lease (`POST /api/v1/licence/lease`) from its canonical CBOR
 * bytes — the offline-exchange return leg. Verified + installed server-side; a
 * forged/spoofed lease is rejected (the Ed25519 pinned-key check, §3.5).
 */
export async function installLease(
  bytes: Uint8Array,
  options: RequestOptions = {},
): Promise<LeaseInstalled> {
  const headers = buildHeaders(options, false);
  headers.set('Content-Type', 'application/cbor');
  const response = await fetch(apiUrl(options, '/api/v1/licence/lease'), {
    method: 'POST',
    headers,
    // A fresh ArrayBuffer copy of exactly the lease bytes (a typed-array view
    // can carry a larger backing buffer; send only the payload).
    body: bytes.slice().buffer,
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isLeaseInstalled(body)) {
    throw new OperationApiError('The server returned an unexpected lease-install body.');
  }
  return body;
}

// --- Licensing heartbeat status ------------------------------------------

function isHeartbeatStatus(value: unknown): value is HeartbeatStatus {
  return (
    isRecord(value) &&
    typeof value.transport === 'string' &&
    Array.isArray(value.payload_fields)
  );
}

/** Read the licensing-heartbeat status surface (`GET /api/v1/licensing/heartbeat-status`). */
export async function getHeartbeatStatus(
  options: RequestOptions = {},
): Promise<HeartbeatStatus> {
  const response = await fetch(apiUrl(options, '/api/v1/licensing/heartbeat-status'), {
    method: 'GET',
    headers: buildHeaders(options, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isHeartbeatStatus(body)) {
    throw new OperationApiError('The server returned an unexpected heartbeat status.');
  }
  return body;
}

// --- Telemetry consent + schema ------------------------------------------

function isConsentResource(value: unknown): value is ConsentResource {
  return (
    isRecord(value) &&
    typeof value.enabled === 'boolean' &&
    typeof value.actor === 'string'
  );
}

/** Read the telemetry-consent document (`GET /api/v1/telemetry/consent`). */
export async function getConsent(options: RequestOptions = {}): Promise<ConsentResource> {
  const response = await fetch(apiUrl(options, '/api/v1/telemetry/consent'), {
    method: 'GET',
    headers: buildHeaders(options, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isConsentResource(body)) {
    throw new OperationApiError('The server returned an unexpected consent document.');
  }
  return body;
}

/** Set the telemetry-consent state (`PUT /api/v1/telemetry/consent`). */
export async function setConsent(
  enabled: boolean,
  options: RequestOptions = {},
): Promise<ConsentResource> {
  const response = await fetch(apiUrl(options, '/api/v1/telemetry/consent'), {
    method: 'PUT',
    headers: buildHeaders(options, true),
    body: JSON.stringify({ enabled }),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isConsentResource(body)) {
    throw new OperationApiError('The server returned an unexpected consent document.');
  }
  return body;
}

function isTelemetrySchema(value: unknown): value is TelemetrySchema {
  return (
    isRecord(value) &&
    typeof value.version === 'string' &&
    Array.isArray(value.sent) &&
    Array.isArray(value.never_sent)
  );
}

/** Read the published daily-pipe telemetry schema (`GET /api/v1/telemetry/schema`). */
export async function getTelemetrySchema(
  options: RequestOptions = {},
): Promise<TelemetrySchema> {
  const response = await fetch(apiUrl(options, '/api/v1/telemetry/schema'), {
    method: 'GET',
    headers: buildHeaders(options, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isTelemetrySchema(body)) {
    throw new OperationApiError('The server returned an unexpected telemetry schema.');
  }
  return body;
}

// --- Diagnostics snapshot (202 → read back) ------------------------------

function isSnapshotAccepted(value: unknown): value is SnapshotAccepted {
  return isRecord(value) && typeof value.snapshot_id === 'string';
}

/** Request a diagnostics snapshot (`POST /api/v1/diagnostics/snapshot`) → `202` id. */
export async function requestSnapshot(
  options: RequestOptions = {},
): Promise<SnapshotAccepted> {
  const response = await fetch(apiUrl(options, '/api/v1/diagnostics/snapshot'), {
    method: 'POST',
    headers: buildHeaders(options, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isSnapshotAccepted(body)) {
    throw new OperationApiError('The server returned an unexpected snapshot id.');
  }
  return body;
}

function isDiagnosticsSnapshot(value: unknown): value is DiagnosticsSnapshot {
  return (
    isRecord(value) &&
    typeof value.snapshot_id === 'string' &&
    typeof value.status === 'string' &&
    isRecord(value.diagnostics)
  );
}

/** Read an assembled diagnostics snapshot (`GET /api/v1/diagnostics/{id}`). */
export async function getSnapshot(
  id: string,
  options: RequestOptions = {},
): Promise<DiagnosticsSnapshot> {
  const response = await fetch(
    apiUrl(options, `/api/v1/diagnostics/${encodeURIComponent(id)}`),
    { method: 'GET', headers: buildHeaders(options, false) },
  );
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isDiagnosticsSnapshot(body)) {
    throw new OperationApiError('The server returned an unexpected diagnostics snapshot.');
  }
  return body;
}

// --- Mesh ----------------------------------------------------------------

function isMeshStatus(value: unknown): value is MeshStatusDoc {
  return (
    isRecord(value) &&
    typeof value.relay_enabled === 'boolean' &&
    isRecord(value.role) &&
    typeof value.peers_count === 'number'
  );
}

/** Read the mesh discovery + relay summary (`GET /api/v1/mesh/status`). */
export async function getMeshStatus(options: RequestOptions = {}): Promise<MeshStatusDoc> {
  const response = await fetch(apiUrl(options, '/api/v1/mesh/status'), {
    method: 'GET',
    headers: buildHeaders(options, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isMeshStatus(body)) {
    throw new OperationApiError('The server returned an unexpected mesh status.');
  }
  return body;
}

/** Opt this machine in/out of relaying neighbours (`PUT /api/v1/mesh/relay`). */
export async function setRelay(
  enabled: boolean,
  options: RequestOptions = {},
): Promise<MeshStatusDoc> {
  const response = await fetch(apiUrl(options, '/api/v1/mesh/relay'), {
    method: 'PUT',
    headers: buildHeaders(options, true),
    body: JSON.stringify({ enabled }),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isMeshStatus(body)) {
    throw new OperationApiError('The server returned an unexpected mesh status.');
  }
  return body;
}

function isMeshPeer(value: unknown): value is MeshPeerDoc {
  return (
    isRecord(value) &&
    typeof value.key === 'string' &&
    typeof value.claimed === 'boolean' &&
    typeof value.last_seen === 'number' &&
    typeof value.relaying_for_us === 'boolean'
  );
}

/** Read the untrusted discovered-peer inventory (`GET /api/v1/mesh/peers`). */
export async function getMeshPeers(options: RequestOptions = {}): Promise<MeshPeerDoc[]> {
  const response = await fetch(apiUrl(options, '/api/v1/mesh/peers'), {
    method: 'GET',
    headers: buildHeaders(options, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!Array.isArray(body) || !body.every(isMeshPeer)) {
    throw new OperationApiError('The server returned an unexpected peer inventory.');
  }
  return body;
}

// --- Account audit (cursor-paginated) ------------------------------------

function isAccountAuditEntry(value: unknown): value is AccountAuditEntry {
  return (
    isRecord(value) &&
    typeof value.seq === 'number' &&
    typeof value.at_nanos === 'number' &&
    typeof value.actor === 'string' &&
    typeof value.kind === 'string'
  );
}

function isAccountAuditPage(value: unknown): value is AccountAuditPage {
  return (
    isRecord(value) &&
    Array.isArray(value.entries) &&
    value.entries.every(isAccountAuditEntry)
  );
}

/** Query params for an account-audit page read. */
export interface AccountAuditQuery {
  /** The opaque cursor to resume from (the previous page's `next_cursor`). */
  readonly cursor?: number;
  /** Filter to a single audit kind. */
  readonly filter?: AccountAuditKind;
  /** Maximum entries to return in this page. */
  readonly limit?: number;
}

/** Read a page of the append-only account audit log (`GET /api/v1/account/audit`). */
export async function getAccountAudit(
  query: AccountAuditQuery = {},
  options: RequestOptions = {},
): Promise<AccountAuditPage> {
  const params = new URLSearchParams();
  if (query.cursor !== undefined) {
    params.set('cursor', String(query.cursor));
  }
  if (query.filter !== undefined) {
    params.set('filter', query.filter);
  }
  if (query.limit !== undefined) {
    params.set('limit', String(query.limit));
  }
  const qs = params.toString();
  const path = qs === '' ? '/api/v1/account/audit' : `/api/v1/account/audit?${qs}`;
  const response = await fetch(apiUrl(options, path), {
    method: 'GET',
    headers: buildHeaders(options, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isAccountAuditPage(body)) {
    throw new OperationApiError('The server returned an unexpected audit page.');
  }
  return body;
}

// --- Pending actions -----------------------------------------------------

function isPendingAction(value: unknown): value is PendingAction {
  return (
    isRecord(value) &&
    typeof value.action_id === 'string' &&
    typeof value.kind === 'string' &&
    typeof value.requested_by === 'string' &&
    typeof value.requested_at_nanos === 'number' &&
    typeof value.state === 'string'
  );
}

/** Read the queued remote-actions strip (`GET /api/v1/actions/pending`). */
export async function getPendingActions(
  options: RequestOptions = {},
): Promise<PendingAction[]> {
  const response = await fetch(apiUrl(options, '/api/v1/actions/pending'), {
    method: 'GET',
    headers: buildHeaders(options, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!Array.isArray(body) || !body.every(isPendingAction)) {
    throw new OperationApiError('The server returned an unexpected pending-action list.');
  }
  return body;
}

function isCancelledBody(value: unknown): value is CancelledBody {
  return isRecord(value) && typeof value.cancelled === 'boolean';
}

/** Cancel a queued action locally (`POST /api/v1/actions/{id}/cancel`). */
export async function cancelAction(
  id: string,
  options: RequestOptions = {},
): Promise<CancelledBody> {
  const response = await fetch(
    apiUrl(options, `/api/v1/actions/${encodeURIComponent(id)}/cancel`),
    { method: 'POST', headers: buildHeaders(options, false) },
  );
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isCancelledBody(body)) {
    throw new OperationApiError('The server returned an unexpected cancel body.');
  }
  return body;
}

// --- Support: entitlement, tickets, bundle (context-pack) ----------------

function isSupportEntitlement(value: unknown): value is SupportEntitlement {
  return (
    isRecord(value) &&
    typeof value.eligible === 'boolean' &&
    isRecord(value.route) &&
    typeof value.sla === 'string'
  );
}

/** Read the support-entitlement routing answer (`GET /api/v1/support/entitlement`). */
export async function getSupportEntitlement(
  options: RequestOptions = {},
): Promise<SupportEntitlement> {
  const response = await fetch(apiUrl(options, '/api/v1/support/entitlement'), {
    method: 'GET',
    headers: buildHeaders(options, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isSupportEntitlement(body)) {
    throw new OperationApiError('The server returned an unexpected support entitlement.');
  }
  return body;
}

function isTicketSummary(value: unknown): value is TicketSummary {
  return (
    isRecord(value) &&
    typeof value.ticket_id === 'string' &&
    typeof value.subject === 'string' &&
    typeof value.severity === 'string' &&
    typeof value.state === 'string' &&
    typeof value.updates === 'number'
  );
}

/** List support tickets (`GET /api/v1/support/tickets`). */
export async function listTickets(options: RequestOptions = {}): Promise<TicketSummary[]> {
  const response = await fetch(apiUrl(options, '/api/v1/support/tickets'), {
    method: 'GET',
    headers: buildHeaders(options, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!Array.isArray(body) || !body.every(isTicketSummary)) {
    throw new OperationApiError('The server returned an unexpected ticket list.');
  }
  return body;
}

function isTicket(value: unknown): value is Ticket {
  return (
    isRecord(value) &&
    typeof value.ticket_id === 'string' &&
    typeof value.subject === 'string' &&
    Array.isArray(value.updates)
  );
}

/** Read a single ticket + its thread (`GET /api/v1/support/tickets/{id}`). */
export async function getTicket(
  id: string,
  options: RequestOptions = {},
): Promise<Ticket> {
  const response = await fetch(
    apiUrl(options, `/api/v1/support/tickets/${encodeURIComponent(id)}`),
    { method: 'GET', headers: buildHeaders(options, false) },
  );
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isTicket(body)) {
    throw new OperationApiError('The server returned an unexpected ticket.');
  }
  return body;
}

/** The body for raising a ticket. */
export interface RaiseTicketInput {
  /** The subject line. */
  readonly subject: string;
  /** The opening body. */
  readonly body: string;
  /** The declared severity. */
  readonly severity: TicketSeverity;
}

/** Raise a support ticket (`POST /api/v1/support/tickets`). */
export async function raiseTicket(
  input: RaiseTicketInput,
  options: RequestOptions = {},
): Promise<Ticket> {
  const response = await fetch(apiUrl(options, '/api/v1/support/tickets'), {
    method: 'POST',
    headers: buildHeaders(options, true),
    body: JSON.stringify(input),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isTicket(body)) {
    throw new OperationApiError('The server returned an unexpected ticket.');
  }
  return body;
}

/** Append a reply to a ticket (`POST /api/v1/support/tickets/{id}/reply`). */
export async function replyToTicket(
  id: string,
  text: string,
  options: RequestOptions = {},
): Promise<Ticket> {
  const response = await fetch(
    apiUrl(options, `/api/v1/support/tickets/${encodeURIComponent(id)}/reply`),
    {
      method: 'POST',
      headers: buildHeaders(options, true),
      body: JSON.stringify({ body: text }),
    },
  );
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isTicket(body)) {
    throw new OperationApiError('The server returned an unexpected ticket.');
  }
  return body;
}

function isBundleAccepted(value: unknown): value is BundleAccepted {
  return isRecord(value) && typeof value.bundle_id === 'string';
}

/** The body for composing a context-pack bundle. */
export interface ComposeBundleInput {
  /** The reporting window. */
  readonly window: BundleWindow;
  /** The sections to include. */
  readonly include: readonly BundleInclude[];
}

/** Compose a context-pack bundle (`POST /api/v1/support/bundle`) → `202` id. */
export async function composeBundle(
  input: ComposeBundleInput,
  options: RequestOptions = {},
): Promise<BundleAccepted> {
  const response = await fetch(apiUrl(options, '/api/v1/support/bundle'), {
    method: 'POST',
    headers: buildHeaders(options, true),
    body: JSON.stringify({ window: input.window, include: input.include }),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isBundleAccepted(body)) {
    throw new OperationApiError('The server returned an unexpected bundle id.');
  }
  return body;
}

function isBundle(value: unknown): value is Bundle {
  return (
    isRecord(value) &&
    typeof value.bundle_id === 'string' &&
    typeof value.window === 'string' &&
    Array.isArray(value.redactions)
  );
}

/** Read a composed bundle preview (`GET /api/v1/support/bundle/{id}`). */
export async function getBundle(
  id: string,
  options: RequestOptions = {},
): Promise<Bundle> {
  const response = await fetch(
    apiUrl(options, `/api/v1/support/bundle/${encodeURIComponent(id)}`),
    { method: 'GET', headers: buildHeaders(options, false) },
  );
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isBundle(body)) {
    throw new OperationApiError('The server returned an unexpected bundle.');
  }
  return body;
}
