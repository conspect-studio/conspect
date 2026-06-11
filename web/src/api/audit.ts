// Audit surface: the read-only change log.
//
// `GET /api/v1/audit` returns immutable `AuditEntry[]`, newest-first (optionally
// filtered to one object via `?object_id=`). Each entry records who did what to
// which object, and when (`at_nanos` is a media-timeline nanosecond stamp, NOT a
// wall-clock time). Read-only: there is no mutation surface here.
import {
  apiUrl,
  buildHeaders,
  OperationApiError,
  readProblem,
} from './operations';
import type { RequestOptions } from './operations';
import type { components } from './schema';

/** One immutable audit-log entry. */
export type AuditEntry = components['schemas']['AuditEntry'];

export { OperationApiError } from './operations';
export type { RequestOptions } from './operations';

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

function isAuditEntry(value: unknown): value is AuditEntry {
  return (
    isRecord(value) &&
    typeof value.action === 'string' &&
    typeof value.actor === 'string' &&
    typeof value.at_nanos === 'number' &&
    typeof value.object_id === 'string' &&
    typeof value.object_kind === 'string'
  );
}

/** List audit entries (`GET /api/v1/audit`), newest first. */
export async function listAudit(
  objectId: string | undefined,
  options: RequestOptions = {},
): Promise<AuditEntry[]> {
  const base = '/api/v1/audit';
  const path =
    objectId !== undefined && objectId !== ''
      ? `${base}?object_id=${encodeURIComponent(objectId)}`
      : base;
  const response = await fetch(apiUrl(options, path), {
    method: 'GET',
    headers: buildHeaders(options, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!Array.isArray(body) || !body.every(isAuditEntry)) {
    throw new OperationApiError('The server returned an unexpected audit list.');
  }
  return body;
}
