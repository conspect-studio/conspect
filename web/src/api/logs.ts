// Logs surface: the read-only buffered structured log tail.
//
// `GET /api/v1/logs` returns the recent ring of `LogRecordDoc[]`, oldest first
// (ADR-0060 §2.3). Each record carries a level, a wall-clock millisecond stamp,
// the tracing target, the rendered message, and optional resource attribution
// (which configured source / output / layout / program / device it concerns).
// Read-only: there is no mutation surface here. The query parameters narrow the
// tail server-side — an unknown `level`/`kind` is a `422`, so the UI only ever
// sends the known enum values.
import {
  apiUrl,
  buildHeaders,
  OperationApiError,
  readProblem,
} from './operations';
import type { RequestOptions } from './operations';
import type { components } from './schema';

/** One buffered structured log record. */
export type LogRecord = components['schemas']['LogRecordDoc'];

/** A severity floor for the log tail (the spec's `LogLevelDoc`). */
export type LogLevel = components['schemas']['LogLevelDoc'];

/** A resource kind a log record can be attributed to (`LogResourceKindDoc`). */
export type LogResourceKind = components['schemas']['LogResourceKindDoc'];

export { OperationApiError } from './operations';
export type { RequestOptions } from './operations';

/** The level floor values, ordered least → most severe. */
export const LOG_LEVELS: readonly LogLevel[] = [
  'trace',
  'debug',
  'info',
  'warn',
  'error',
];

/** The resource-kind filter values. */
export const LOG_RESOURCE_KINDS: readonly LogResourceKind[] = [
  'source',
  'output',
  'layout',
  'program',
  'device',
];

/** The query that narrows the log tail (every field optional). */
export interface LogQuery {
  /** Only records attributed to this configured resource id. */
  readonly resourceId?: string;
  /** Only records attributed to this resource kind. */
  readonly kind?: LogResourceKind;
  /** Only records at or above this severity floor. */
  readonly level?: LogLevel;
  /** Keep only records whose capture `seq` is strictly greater than this. */
  readonly since?: number;
  /** The maximum number of most-recent records to return. */
  readonly limit?: number;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

function isLogRecord(value: unknown): value is LogRecord {
  return (
    isRecord(value) &&
    typeof value.level === 'string' &&
    typeof value.message === 'string' &&
    typeof value.seq === 'number' &&
    typeof value.target === 'string' &&
    typeof value.timestamp_ms === 'number'
  );
}

/** Build the `GET /api/v1/logs` query string from the narrowing filter. */
function logsPath(query: LogQuery): string {
  const params = new URLSearchParams();
  if (query.resourceId !== undefined && query.resourceId !== '') {
    params.set('resource_id', query.resourceId);
  }
  if (query.kind !== undefined) {
    params.set('kind', query.kind);
  }
  if (query.level !== undefined) {
    params.set('level', query.level);
  }
  if (query.since !== undefined) {
    params.set('since', String(query.since));
  }
  if (query.limit !== undefined) {
    params.set('limit', String(query.limit));
  }
  const qs = params.toString();
  return qs === '' ? '/api/v1/logs' : `/api/v1/logs?${qs}`;
}

/** List buffered log records (`GET /api/v1/logs`), oldest first. */
export async function listLogs(
  query: LogQuery = {},
  options: RequestOptions = {},
): Promise<LogRecord[]> {
  const response = await fetch(apiUrl(options, logsPath(query)), {
    method: 'GET',
    headers: buildHeaders(options, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!Array.isArray(body) || !body.every(isLogRecord)) {
    throw new OperationApiError('The server returned an unexpected log tail.');
  }
  return body;
}
