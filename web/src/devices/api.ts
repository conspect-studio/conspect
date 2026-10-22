// Typed HTTP bindings + projections for the managed-devices domain.
//
// Devices and sync groups are ordinary `{ id, name, body }` resource records:
// their CRUD rides ../resources/api (ETag/If-Match, RFC 9457). This module
// adds what is devices-specific — the record → view projections (typed field
// guards, never `as`-casts), the honest sync-tier fold, the runtime-status
// REST fallback, the projection endpoints (ADR-M009 facets (a)/(b)), the
// management verbs (202 + declared impact), and discovery (untrusted
// inventory, ADR-0041). Request/response shapes reuse the generated
// `components['schemas']` types from ../api/schema — never hand-written
// duplicates — through the shared helpers in ../api/operations.
import type { components } from '../api/schema';
import {
  apiUrl,
  buildHeaders,
  readProblem,
  submitOperation,
} from '../api/operations';
import type { AcceptedBody, RequestOptions } from '../api/operations';
import { stringField } from '../resources/api';
import type { ResourceRecord } from '../resources/types';
import { parseDeviceStatus } from '../realtime/envelope';
import type { DeviceStatus } from '../realtime/generated-types';
import type { EngineClockRef } from '../realtime/useEngineEvents';
import { parseDeviceDriver } from './forms';
import type { DeviceView, SyncGroupView, SyncMemberView, SyncTier } from './types';

// --- projections ---------------------------------------------------------------

/**
 * Project a device record's opaque body into the {@link DeviceView}. An
 * unknown driver folds to `'unknown'` (never to a real driver) and refuses
 * Edit, while `rawDriver` keeps the authored tag for display.
 */
export function toDeviceView(record: ResourceRecord): DeviceView {
  const raw = stringField(record.body, 'driver');
  const driver = parseDeviceDriver(raw);
  return {
    id: record.id,
    name: record.name,
    driver: driver ?? 'unknown',
    rawDriver: raw ?? 'unknown',
    address: stringField(record.body, 'address'),
    desiredMode: stringField(record.body, 'desired_mode'),
    editable: driver !== undefined,
  };
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

/**
 * Project a sync-group record's opaque body into the {@link SyncGroupView}.
 * Malformed member rows are dropped, never fabricated.
 */
export function toSyncGroupView(record: ResourceRecord): SyncGroupView {
  const skew = record.body.target_skew_ms;
  const rawMembers = record.body.members;
  const members: SyncMemberView[] = [];
  if (Array.isArray(rawMembers)) {
    for (const raw of rawMembers) {
      if (isRecord(raw) && typeof raw.device === 'string') {
        const offset = raw.offset_ms;
        members.push({
          device: raw.device,
          offsetMs:
            typeof offset === 'number' && Number.isFinite(offset) ? offset : 0,
        });
      }
    }
  }
  return {
    id: record.id,
    name: record.name,
    targetSkewMs:
      typeof skew === 'number' && Number.isFinite(skew) ? skew : 0,
    members,
    editable:
      typeof skew === 'number' &&
      Number.isFinite(skew) &&
      Array.isArray(rawMembers),
  };
}

// --- sync tiers (honest, weakest member — managed-devices.md §8) -----------------

/**
 * The tier a driver family can honestly contribute: display nodes are
 * frame-accurate (Tier A/B), vendor decoders hold a bounded skew (Tier C,
 * ±100–500 ms drift between re-aligns), cast is never synchronized (Tier D).
 * An unknown driver claims nothing — under-claiming is safe, over-claiming
 * never happens.
 */
export function driverSyncTier(driver: DeviceView['driver']): SyncTier {
  switch (driver) {
    case 'displaynode':
      return 'frame-accurate';
    case 'zowietek':
      return 'bounded-skew';
    case 'cast':
    case 'unknown':
      return 'none';
  }
}

const TIER_ORDER: readonly SyncTier[] = ['frame-accurate', 'bounded-skew', 'none'];

/**
 * The tier a group can claim: the WEAKEST member tier. An empty group claims
 * nothing.
 */
export function weakestMemberTier(
  drivers: readonly DeviceView['driver'][],
): SyncTier {
  let weakest = 0;
  if (drivers.length === 0) {
    return 'none';
  }
  for (const driver of drivers) {
    const index = TIER_ORDER.indexOf(driverSyncTier(driver));
    weakest = Math.max(weakest, index);
  }
  return TIER_ORDER.at(weakest) ?? 'none';
}

// --- last-seen age ---------------------------------------------------------------

const NS_PER_MS = 1_000_000;
const MS_PER_S = 1_000;

/**
 * The age in whole seconds of a device's `last_seen_ts` (engine-monotonic
 * nanoseconds), computed against the engine clock reference the realtime
 * stream maintains (`ENGINE_CLOCK_QUERY_KEY`): the engine "now" is the
 * reference timestamp advanced by the wall time elapsed since it was taken.
 * Returns `undefined` without a reference — an age is never fabricated.
 */
export function lastSeenAgeSeconds(
  lastSeenTs: number,
  clock: EngineClockRef | undefined,
  nowMs: number,
): number | undefined {
  if (clock === undefined) {
    return undefined;
  }
  const engineNowNs = clock.engineTs + (nowMs - clock.wallMs) * NS_PER_MS;
  const ageMs = (engineNowNs - lastSeenTs) / NS_PER_MS;
  if (!Number.isFinite(ageMs) || ageMs < 0) {
    return 0;
  }
  return Math.round(ageMs / MS_PER_S);
}

// --- runtime status (REST fallback for the conflated WS lane) --------------------

/**
 * `GET /api/v1/devices/{id}/status` — the latest-wins runtime snapshot. The
 * conflated `device.status` realtime lane is primary; this is the fallback a
 * page uses when the stream has not delivered that device yet. A `404` means
 * the device is not adopted into the runtime registry: `undefined`, never an
 * invented state.
 */
export async function fetchDeviceStatus(
  id: string,
  options: RequestOptions = {},
): Promise<DeviceStatus | undefined> {
  const response = await fetch(
    apiUrl(options, `/api/v1/devices/${encodeURIComponent(id)}/status`),
    { method: 'GET', headers: buildHeaders(options, false) },
  );
  if (response.status === 404) {
    return undefined;
  }
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  return parseDeviceStatus(body);
}

// --- management verbs -------------------------------------------------------------

/** The `202 Accepted` body of `set-mode` (declared DEV-class impact). */
export type SetModeAccepted = components['schemas']['SetModeAccepted'];

function isSetModeAccepted(value: unknown): value is SetModeAccepted {
  return (
    isRecord(value) &&
    typeof value.operation_id === 'string' &&
    typeof value.impact === 'string' &&
    typeof value.detail === 'string'
  );
}

/**
 * `POST /api/v1/devices/{id}/set-mode` — converge the device to a mode. The
 * impact is DECLARED in the 202 body before anything applies (ADR-M009): the
 * device restarts its own pipeline; Multiview program output is never
 * interrupted. The outcome arrives as `device.mode` on the realtime stream.
 */
export async function setDeviceMode(
  id: string,
  mode: string,
  options: RequestOptions = {},
): Promise<SetModeAccepted> {
  const response = await fetch(
    apiUrl(options, `/api/v1/devices/${encodeURIComponent(id)}/set-mode`),
    {
      method: 'POST',
      headers: buildHeaders(options, true),
      body: JSON.stringify({ mode }),
    },
  );
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isSetModeAccepted(body)) {
    throw new Error('The server returned an unexpected set-mode body.');
  }
  return body;
}

/** `POST /api/v1/devices/{id}/reboot` — 202; outcome on the realtime stream. */
export async function rebootDevice(
  id: string,
  options: RequestOptions = {},
): Promise<AcceptedBody> {
  return submitOperation(`/api/v1/devices/${encodeURIComponent(id)}/reboot`, options);
}

async function postExpectEmpty(
  path: string,
  options: RequestOptions,
): Promise<void> {
  const response = await fetch(apiUrl(options, path), {
    method: 'POST',
    headers: buildHeaders(options, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
}

/** `POST /api/v1/devices/{id}/probe` — synchronous re-probe acknowledgement. */
export async function probeDevice(id: string, options: RequestOptions = {}): Promise<void> {
  await postExpectEmpty(`/api/v1/devices/${encodeURIComponent(id)}/probe`, options);
}

/** `POST /api/v1/devices/{id}/identify` — flash the identify indicator (204). */
export async function identifyDevice(
  id: string,
  options: RequestOptions = {},
): Promise<void> {
  await postExpectEmpty(`/api/v1/devices/${encodeURIComponent(id)}/identify`, options);
}

/** `POST /api/v1/devices/{id}/test-pattern` — show a test pattern (204). */
export async function testPatternDevice(
  id: string,
  options: RequestOptions = {},
): Promise<void> {
  await postExpectEmpty(`/api/v1/devices/${encodeURIComponent(id)}/test-pattern`, options);
}

/** `POST /api/v1/sync-groups/{id}/measure` — 202; result on the realtime stream. */
export async function measureSyncGroup(
  id: string,
  options: RequestOptions = {},
): Promise<AcceptedBody> {
  return submitOperation(`/api/v1/sync-groups/${encodeURIComponent(id)}/measure`, options);
}

// --- projection endpoints (ADR-M009 facets (a)/(b)) -------------------------------

/**
 * One stream a device serves, bindable as an ordinary Source with a
 * `device_ref`. `url` is absent when the vendor does not document the mount —
 * the operator supplies it and the candidate is flagged `unverified`, never
 * silently guessed.
 */
export interface SourceCandidateView {
  readonly id: string;
  readonly kind: string;
  readonly url: string | undefined;
  readonly unverified: boolean;
}

/** One decode slot a device offers, bindable as an ordinary Output. */
export interface OutputTargetView {
  readonly id: string;
  readonly kind: string;
  readonly label: string | undefined;
}

async function getJsonArray(
  path: string,
  options: RequestOptions,
): Promise<readonly Record<string, unknown>[]> {
  const response = await fetch(apiUrl(options, path), {
    method: 'GET',
    headers: buildHeaders(options, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  // The every-guard narrows the parsed array (the listResources pattern); a
  // response that is not an array of objects is treated as empty, never cast.
  if (!Array.isArray(body) || !body.every(isRecord)) {
    return [];
  }
  return body;
}

/** `GET /api/v1/devices/{id}/source-candidates` — honestly empty until enumerated. */
export async function listSourceCandidates(
  deviceId: string,
  options: RequestOptions = {},
): Promise<SourceCandidateView[]> {
  const rows = await getJsonArray(
    `/api/v1/devices/${encodeURIComponent(deviceId)}/source-candidates`,
    options,
  );
  const candidates: SourceCandidateView[] = [];
  for (const raw of rows) {
    if (typeof raw.id === 'string' && typeof raw.kind === 'string') {
      candidates.push({
        id: raw.id,
        kind: raw.kind,
        url: typeof raw.url === 'string' ? raw.url : undefined,
        unverified: raw.unverified === true,
      });
    }
  }
  return candidates;
}

/** `GET /api/v1/devices/{id}/output-targets` — honestly empty until enumerated. */
export async function listOutputTargets(
  deviceId: string,
  options: RequestOptions = {},
): Promise<OutputTargetView[]> {
  const rows = await getJsonArray(
    `/api/v1/devices/${encodeURIComponent(deviceId)}/output-targets`,
    options,
  );
  const targets: OutputTargetView[] = [];
  for (const raw of rows) {
    if (typeof raw.id === 'string' && typeof raw.kind === 'string') {
      targets.push({
        id: raw.id,
        kind: raw.kind,
        label: typeof raw.label === 'string' ? raw.label : undefined,
      });
    }
  }
  return targets;
}

// --- discovery (untrusted inventory, ADR-0041) -------------------------------------

/** One resolved endpoint of a discovered service (IPv6 lead, IPv4 legacy). */
export interface DiscoveredEndpointView {
  readonly address: string;
  readonly family: string;
}

/**
 * One untrusted discovery-inventory row: a hint requiring explicit
 * confirm-adopt. It carries no registry id — it is NOT a device.
 */
export interface DiscoveredServiceView {
  readonly key: string;
  readonly name: string;
  readonly host: string;
  readonly driverKind: string;
  readonly serviceType: string;
  readonly port: number;
  readonly primaryAddress: string;
  readonly endpoints: readonly DiscoveredEndpointView[];
  readonly lastSeenUnixNs: number;
}

function toDiscoveredView(raw: Record<string, unknown>): DiscoveredServiceView | undefined {
  if (
    typeof raw.key !== 'string' ||
    typeof raw.name !== 'string' ||
    typeof raw.driver_kind !== 'string' ||
    typeof raw.primary_address !== 'string'
  ) {
    return undefined;
  }
  const endpoints: DiscoveredEndpointView[] = [];
  if (Array.isArray(raw.endpoints)) {
    for (const endpoint of raw.endpoints) {
      if (
        isRecord(endpoint) &&
        typeof endpoint.address === 'string' &&
        typeof endpoint.family === 'string'
      ) {
        endpoints.push({ address: endpoint.address, family: endpoint.family });
      }
    }
  }
  return {
    key: raw.key,
    name: raw.name,
    host: typeof raw.host === 'string' ? raw.host : '',
    driverKind: raw.driver_kind,
    serviceType: typeof raw.service_type === 'string' ? raw.service_type : '',
    port: typeof raw.port === 'number' && Number.isFinite(raw.port) ? raw.port : 0,
    primaryAddress: raw.primary_address,
    endpoints,
    lastSeenUnixNs:
      typeof raw.last_seen_unix_ns === 'number' && Number.isFinite(raw.last_seen_unix_ns)
        ? raw.last_seen_unix_ns
        : 0,
  };
}

/** `GET /api/v1/discovery/devices` — the current untrusted inventory snapshot. */
export async function listDiscovered(
  options: RequestOptions = {},
): Promise<DiscoveredServiceView[]> {
  const rows = await getJsonArray('/api/v1/discovery/devices', options);
  const services: DiscoveredServiceView[] = [];
  for (const raw of rows) {
    const view = toDiscoveredView(raw);
    if (view !== undefined) {
      services.push(view);
    }
  }
  return services;
}

/** The `202 Accepted` body of a discovery scan. */
export type ScanAccepted = components['schemas']['ScanAccepted'];

function isScanAccepted(value: unknown): value is ScanAccepted {
  return (
    isRecord(value) &&
    typeof value.operation_id === 'string' &&
    typeof value.budget_ms === 'number' &&
    typeof value.note === 'string' &&
    Array.isArray(value.service_types)
  );
}

/**
 * `POST /api/v1/discovery/devices/scan` — kick a time-bounded, single-flight
 * mDNS browse. The 202 carries the operation id; `device.discovered` rows
 * stream on the `devices` topic correlated to it via the envelope `corr`. A
 * scan NEVER creates a device (ADR-0041).
 */
export async function scanDevices(options: RequestOptions = {}): Promise<ScanAccepted> {
  const response = await fetch(apiUrl(options, '/api/v1/discovery/devices/scan'), {
    method: 'POST',
    headers: buildHeaders(options, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isScanAccepted(body)) {
    throw new Error('The server returned an unexpected scan body.');
  }
  return body;
}
