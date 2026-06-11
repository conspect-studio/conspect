// Alarms surface: list active/historical alarms and acknowledge one.
//
// `GET /api/v1/alarms` returns `AlarmRecordDoc[]` (filterable by severity / active
// / scope). `POST /api/v1/alarms/{id}/ack` acknowledges an alarm under `ETag` /
// `If-Match` optimistic concurrency so two operators cannot silently clobber each
// other's ack (ADR-W006). The list response carries NO per-item ETag and there is
// no single-alarm GET, so the ack cannot pre-read the exact version. The control
// plane version-stamps an alarm at 1 on first sight and bumps it each time the
// engine re-upserts a CHANGED record, so the ack sends `If-Match: W/"1"`
// optimistically and, on a `412` version conflict, parses the authoritative
// current version out of the RFC 9457 `detail` ("...current is N") and retries
// once. That makes the ack robust against an engine update racing the operator
// without inventing an unconditional ack the server does not allow.
import { getStoredToken } from './token';
import type { components } from './schema';

/** One alarm record, exactly as the control plane returns it. */
export type AlarmRecord = components['schemas']['AlarmRecordDoc'];

/** The X.733 perceived-severity vocabulary, lowest to highest. */
export type Severity = components['schemas']['PerceivedSeverityDoc'];

/** A failed alarm call, normalized to a message + status. */
export class AlarmApiError extends Error {
  /** The HTTP status code, when one was returned. */
  readonly status: number | undefined;

  constructor(message: string, status?: number) {
    super(message);
    this.name = 'AlarmApiError';
    this.status = status;
  }
}

/** Options shared by the alarm calls. */
export interface AlarmRequestOptions {
  /** Base URL (defaults to same-origin, matching the typed client). */
  readonly baseUrl?: string;
  /** Optional bearer token; falls back to the operator's stored token. */
  readonly token?: string;
}

/** Filters accepted by `GET /api/v1/alarms`. */
export interface AlarmFilter {
  /** Minimum X.733 severity to include (server-side filter). */
  readonly severity?: Severity;
  /** Keep only active (`true`) or only cleared/historical (`false`) alarms. */
  readonly active?: boolean;
  /** Keep only alarms whose scope kind matches (e.g. `tile`, `probe`). */
  readonly scope?: string;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

function isProblem(value: unknown): value is { title: string; status: number; detail?: string | null } {
  return (
    isRecord(value) &&
    typeof value.status === 'number' &&
    typeof value.title === 'string'
  );
}

function isAlarmRecord(value: unknown): value is AlarmRecord {
  return (
    isRecord(value) &&
    typeof value.id === 'string' &&
    typeof value.severity === 'string' &&
    isRecord(value.ack) &&
    isRecord(value.scope)
  );
}

async function readProblem(response: Response): Promise<AlarmApiError> {
  try {
    const body: unknown = await response.json();
    if (isProblem(body)) {
      return new AlarmApiError(body.title, body.status);
    }
  } catch {
    // Fall through to a status-only error when the body is absent/unparseable.
  }
  return new AlarmApiError(`Request failed (${String(response.status)})`, response.status);
}

function authHeaders(options: AlarmRequestOptions): Headers {
  const headers = new Headers();
  const token = options.token ?? getStoredToken();
  if (token !== undefined && token !== '') {
    headers.set('Authorization', `Bearer ${token}`);
  }
  return headers;
}

function listUrl(options: AlarmRequestOptions, filter: AlarmFilter): string {
  const params = new URLSearchParams();
  if (filter.severity !== undefined) {
    // The control plane matches the severity name case-insensitively.
    params.set('severity', filter.severity.toLowerCase());
  }
  if (filter.active !== undefined) {
    params.set('active', String(filter.active));
  }
  if (filter.scope !== undefined && filter.scope !== '') {
    params.set('scope', filter.scope);
  }
  const query = params.toString();
  const base = `${options.baseUrl ?? ''}/api/v1/alarms`;
  return query === '' ? base : `${base}?${query}`;
}

/** List alarms (`GET /api/v1/alarms`), applying the server-side filters. */
export async function listAlarms(
  filter: AlarmFilter = {},
  options: AlarmRequestOptions = {},
): Promise<AlarmRecord[]> {
  const response = await fetch(listUrl(options, filter), {
    method: 'GET',
    headers: authHeaders(options),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!Array.isArray(body) || !body.every(isAlarmRecord)) {
    throw new AlarmApiError('The server returned an unexpected alarm list.');
  }
  return body;
}

/** Parse the authoritative current version out of a `412` problem `detail`. */
function currentVersionFrom(detail: string | null | undefined): number | undefined {
  if (detail === null || detail === undefined) {
    return undefined;
  }
  const match = /current is (\d+)/.exec(detail);
  if (match === null) {
    return undefined;
  }
  const parsed = Number(match[1]);
  return Number.isInteger(parsed) ? parsed : undefined;
}

interface AckOutcome {
  /** The acknowledged record on success. */
  readonly record?: AlarmRecord;
  /** The parsed problem (status + optional detail) on a non-2xx response. */
  readonly problem?: { readonly status: number; readonly detail: string | null | undefined };
}

async function ackOnce(
  id: string,
  version: number,
  options: AlarmRequestOptions,
): Promise<AckOutcome> {
  const headers = authHeaders(options);
  headers.set('If-Match', `W/"${String(version)}"`);
  const response = await fetch(
    `${options.baseUrl ?? ''}/api/v1/alarms/${encodeURIComponent(id)}/ack`,
    { method: 'POST', headers },
  );
  if (response.ok) {
    const body: unknown = await response.json();
    if (!isAlarmRecord(body)) {
      throw new AlarmApiError('The server returned an unexpected alarm body.');
    }
    return { record: body };
  }
  let problemBody: unknown;
  try {
    problemBody = await response.json();
  } catch {
    problemBody = undefined;
  }
  if (isProblem(problemBody)) {
    return { problem: { status: problemBody.status, detail: problemBody.detail } };
  }
  throw new AlarmApiError(
    `Request failed (${String(response.status)})`,
    response.status,
  );
}

/**
 * Acknowledge an alarm (`POST /api/v1/alarms/{id}/ack`) under `If-Match`. Sends
 * the optimistic initial version, and on a `412` conflict re-reads the current
 * version from the problem `detail` and retries once.
 */
export async function ackAlarm(
  id: string,
  options: AlarmRequestOptions = {},
): Promise<AlarmRecord> {
  const first = await ackOnce(id, 1, options);
  if (first.record !== undefined) {
    return first.record;
  }
  const conflict = first.problem;
  // 412 (stale) carries the current version in `detail`; retry once with it.
  if (conflict?.status === 412) {
    const current = currentVersionFrom(conflict.detail);
    if (current !== undefined) {
      const retry = await ackOnce(id, current, options);
      if (retry.record !== undefined) {
        return retry.record;
      }
      throw new AlarmApiError(
        'The alarm changed again while acknowledging; reload and retry.',
        retry.problem?.status,
      );
    }
  }
  throw new AlarmApiError(
    conflict?.status === 404
      ? 'No alarm with that id (it may have cleared).'
      : 'Could not acknowledge the alarm.',
    conflict?.status,
  );
}
