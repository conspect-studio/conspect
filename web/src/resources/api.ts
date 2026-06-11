// CRUD bindings for the Sources / Outputs / Overlays control-plane resources.
//
// Each resource is a `{ id, name, body }` record (`body` is the opaque, validated
// config document). The collection lives at `/api/v1/{kind}` and an item at
// `/api/v1/{kind}/{id}`, with `ETag`/`If-Match` optimistic concurrency on update
// and delete (conventions §6, RFC 9457 on conflict) — the same shape as layouts.
//
// These operations are not (yet) modelled in the generated `paths` of
// `../api/schema.ts`, so — exactly as `../api/layouts.ts` does for the layout
// write ops — the calls go through `fetch` with EXPLICITLY-TYPED request/response
// shapes and typed field guards. The opaque `body` is read with `stringField`/
// `numberField`, never an `as`-cast of an untyped value.
import { getStoredToken } from '../api/token';
import {
  parseOutputFormKind,
  parseOverlayFormKind,
  parseProbeFormKind,
  parseSourceFormKind,
} from './forms';
import type {
  OutputKind,
  OutputView,
  OverlayKind,
  OverlayView,
  ProbeKind,
  ProbeView,
  ResourceInput,
  ResourceKind,
  ResourceRecord,
  SourceKind,
  SourceView,
} from './types';
import { OUTPUT_KINDS, OVERLAY_KINDS, PROBE_KINDS, SOURCE_KINDS } from './types';

/** A failed resource call, normalized to a message + status. */
export class ResourceApiError extends Error {
  /** The HTTP status code, when one was returned. */
  readonly status: number | undefined;

  constructor(message: string, status?: number) {
    super(message);
    this.name = 'ResourceApiError';
    this.status = status;
  }
}

/** Connection options shared by every resource call. */
export interface ResourceRequestOptions {
  /** Base URL (defaults to same-origin, matching the typed client). */
  readonly baseUrl?: string;
  /** Optional bearer token for the `Authorization` header. */
  readonly token?: string;
  /**
   * The current ETag for `If-Match` on update/delete (optimistic concurrency).
   * Omit on create.
   */
  readonly etag?: string;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

/** Read a string field from an opaque body, or `undefined` when absent/typed wrong. */
export function stringField(body: Record<string, unknown>, key: string): string | undefined {
  const value = body[key];
  return typeof value === 'string' ? value : undefined;
}

/** Read a finite number field from an opaque body, or `undefined` otherwise. */
export function numberField(body: Record<string, unknown>, key: string): number | undefined {
  const value = body[key];
  return typeof value === 'number' && Number.isFinite(value) ? value : undefined;
}

function isProblem(value: unknown): value is { title: string; status: number } {
  return (
    isRecord(value) &&
    typeof value.status === 'number' &&
    typeof value.title === 'string'
  );
}

/** A resource record from the wire, validated structurally (no `as`-cast). */
function isResourceRecord(value: unknown): value is ResourceRecord {
  return (
    isRecord(value) &&
    typeof value.id === 'string' &&
    typeof value.name === 'string' &&
    isRecord(value.body)
  );
}

async function readProblem(response: Response): Promise<ResourceApiError> {
  try {
    const body: unknown = await response.json();
    if (isProblem(body)) {
      return new ResourceApiError(body.title, body.status);
    }
  } catch {
    // Fall through to a status-only error when the body is absent/unparseable.
  }
  return new ResourceApiError(`Request failed (${String(response.status)})`, response.status);
}

function headersFor(options: ResourceRequestOptions, withJsonBody: boolean): Headers {
  const headers = new Headers();
  if (withJsonBody) {
    headers.set('Content-Type', 'application/json');
  }
  // An explicit token wins; otherwise fall back to the operator's stored token
  // so every resource call authenticates, exactly like `createApiClient`.
  const token = options.token ?? getStoredToken();
  if (token !== undefined && token !== '') {
    headers.set('Authorization', `Bearer ${token}`);
  }
  if (options.etag !== undefined && options.etag !== '') {
    headers.set('If-Match', options.etag);
  }
  return headers;
}

function collectionUrl(kind: ResourceKind, options: ResourceRequestOptions): string {
  return `${options.baseUrl ?? ''}/api/v1/${kind}`;
}

function itemUrl(kind: ResourceKind, id: string, options: ResourceRequestOptions): string {
  return `${collectionUrl(kind, options)}/${encodeURIComponent(id)}`;
}

/** A single record plus the ETag the server stamped it with, when present. */
export interface ResourceWithEtag {
  /** The stored record. */
  readonly record: ResourceRecord;
  /** The ETag from the response, when one was carried. */
  readonly etag: string | undefined;
}

/** List a resource collection (`GET /api/v1/{kind}`), id-sorted by the server. */
export async function listResources(
  kind: ResourceKind,
  options: ResourceRequestOptions = {},
): Promise<ResourceRecord[]> {
  const response = await fetch(collectionUrl(kind, options), {
    method: 'GET',
    headers: headersFor(options, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!Array.isArray(body) || !body.every(isResourceRecord)) {
    throw new ResourceApiError('The server returned an unexpected resource list.');
  }
  return body;
}

/** Fetch one resource (`GET /api/v1/{kind}/{id}`) with its ETag for a later update. */
export async function getResource(
  kind: ResourceKind,
  id: string,
  options: ResourceRequestOptions = {},
): Promise<ResourceWithEtag> {
  const response = await fetch(itemUrl(kind, id, options), {
    method: 'GET',
    headers: headersFor(options, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isResourceRecord(body)) {
    throw new ResourceApiError('The server returned an unexpected resource body.');
  }
  const etag = response.headers.get('ETag');
  return { record: body, etag: etag ?? undefined };
}

/**
 * Create (`POST`) or update (`PUT` + `If-Match`) a resource. The path `id`
 * addresses the item in both cases; `create` selects the method.
 */
export async function writeResource(
  kind: ResourceKind,
  id: string,
  input: ResourceInput,
  create: boolean,
  options: ResourceRequestOptions = {},
): Promise<ResourceWithEtag> {
  const response = await fetch(itemUrl(kind, id, options), {
    method: create ? 'POST' : 'PUT',
    headers: headersFor(options, true),
    body: JSON.stringify({ name: input.name, body: input.body }),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isResourceRecord(body)) {
    throw new ResourceApiError('The server returned an unexpected resource body.');
  }
  const etag = response.headers.get('ETag');
  return { record: body, etag: etag ?? undefined };
}

/** Delete a resource (`DELETE /api/v1/{kind}/{id}`), sending `If-Match` when known. */
export async function deleteResource(
  kind: ResourceKind,
  id: string,
  options: ResourceRequestOptions = {},
): Promise<void> {
  const response = await fetch(itemUrl(kind, id, options), {
    method: 'DELETE',
    headers: headersFor(options, false),
  });
  // 404 is idempotent-success for a delete (the resource is already gone).
  if (!response.ok && response.status !== 404) {
    throw await readProblem(response);
  }
}

// --- body → view-model projections -----------------------------------------

// All recognized wire tags, including the legacy `test` alias that
// `SOURCE_KINDS` (the picker list) omits.
const PARSEABLE_SOURCE_KINDS: readonly SourceKind[] = [...SOURCE_KINDS, 'test'];

function asSourceKind(value: string | undefined): SourceKind {
  // The config wire tags (e.g. `bars`/`solid`/`clock`/`rtsp`/…/`test`) match
  // these literals; an unrecognized/absent kind folds to the parameter-free
  // `bars` built-in. The legacy `test` alias still parses to `test`.
  return PARSEABLE_SOURCE_KINDS.find((k) => k === value) ?? 'bars';
}

/** The body key that carries a source kind's locator (url/name/path), if any. */
export function sourceLocatorKey(kind: SourceKind): 'url' | 'name' | 'path' | undefined {
  switch (kind) {
    case 'ndi':
      return 'name';
    case 'file':
      return 'path';
    // Synthetic kinds (ADR-0027 / ADR-0047) carry no locator; `test` is the
    // legacy alias.
    case 'bars':
    case 'solid':
    case 'clock':
    case 'timer':
    case 'test':
      return undefined;
    default:
      return 'url';
  }
}

/** The body key that carries an output kind's target (mount/path/url/name/connector). */
export function outputTargetKey(
  kind: OutputKind,
): 'mount' | 'path' | 'url' | 'name' | 'connector' {
  switch (kind) {
    case 'rtsp':
      return 'mount';
    case 'hls':
    case 'll-hls':
      return 'path';
    case 'ndi':
      return 'name';
    case 'display':
      return 'connector';
    default:
      return 'url';
  }
}

/**
 * Whether an output kind carries a video codec — every kind except NDI and
 * display, which carry raw frames rather than an encoded rendition.
 */
export function outputHasCodec(kind: OutputKind): boolean {
  return kind !== 'ndi' && kind !== 'display';
}

function asOutputKind(value: string | undefined): OutputKind {
  // The config wire kinds use snake_case (`rtsp_server`, `ll_hls`); fold them to
  // the display kinds the OutputView exposes.
  switch (value) {
    case 'rtsp_server':
      return 'rtsp';
    case 'll_hls':
      return 'll-hls';
    default:
      return OUTPUT_KINDS.find((k) => k === value) ?? 'rtsp';
  }
}

function asOverlayKind(value: string | undefined): OverlayKind {
  return OVERLAY_KINDS.find((k) => k === value) ?? 'label';
}

/**
 * Project a source record's opaque body into the {@link SourceView}.
 *
 * `kind` folds an unknown tag for the typed consumers, but `rawKind` carries
 * the authored tag for display and `editable` flags whether the typed forms
 * can round-trip the record (`parseSourceFormKind` refuses unknown kinds —
 * editing through a fold would rewrite the authored document).
 */
export function toSourceView(record: ResourceRecord): SourceView {
  const raw = stringField(record.body, 'kind');
  const kind = asSourceKind(raw);
  const locatorKey = sourceLocatorKey(kind);
  return {
    id: record.id,
    name: record.name,
    kind,
    rawKind: raw ?? kind,
    editable: parseSourceFormKind(raw) !== undefined,
    locator: locatorKey !== undefined ? stringField(record.body, locatorKey) : undefined,
  };
}

/** Project an output record's opaque body into the {@link OutputView}. */
export function toOutputView(record: ResourceRecord): OutputView {
  const raw = stringField(record.body, 'kind');
  const kind = asOutputKind(raw);
  return {
    id: record.id,
    name: record.name,
    kind,
    rawKind: raw ?? kind,
    editable: parseOutputFormKind(raw) !== undefined,
    target: stringField(record.body, outputTargetKey(kind)),
    codec: outputHasCodec(kind) ? stringField(record.body, 'codec') : undefined,
  };
}

function asProbeKind(value: string | undefined): ProbeKind {
  return PROBE_KINDS.find((k) => k === value) ?? 'black';
}

/**
 * Project a probe record's opaque body into the {@link ProbeView}.
 *
 * `kind` folds an unknown tag for typed consumers, but `rawKind` carries the
 * authored tag for display and `editable` flags whether the typed form can
 * round-trip the record (`parseProbeFormKind` refuses unknown kinds).
 */
export function toProbeView(record: ResourceRecord): ProbeView {
  const raw = stringField(record.body, 'kind');
  return {
    id: record.id,
    name: record.name,
    kind: asProbeKind(raw),
    rawKind: raw ?? 'black',
    editable: parseProbeFormKind(raw) !== undefined,
    cell: stringField(record.body, 'cell') ?? '',
    // The schema default severity is Cleared; mirror it for an absent field.
    severity: stringField(record.body, 'severity') ?? 'Cleared',
    latched: record.body.latched === true,
  };
}

/** Project an overlay record's opaque body into the {@link OverlayView}. */
export function toOverlayView(record: ResourceRecord): OverlayView {
  const raw = stringField(record.body, 'kind');
  const kind = asOverlayKind(raw);
  return {
    id: record.id,
    name: record.name,
    kind,
    rawKind: raw ?? kind,
    editable: parseOverlayFormKind(raw) !== undefined,
    target: stringField(record.body, 'target') ?? 'canvas',
    z: numberField(record.body, 'z') ?? 0,
  };
}
