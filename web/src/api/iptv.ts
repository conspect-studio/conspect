// IPTV publication + M3U import bindings (epg-playlist).
//
// `GET`/`PUT /api/v1/iptv` is a singleton with a content ETag (`If-Match` on
// PUT). `POST /api/v1/sources/import/m3u` parses a catalogue into candidates;
// it creates nothing — the Sources page creates the selected candidates
// through the ordinary source create API.
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import type { UseMutationResult, UseQueryResult } from '@tanstack/react-query';

import { apiUrl, buildHeaders, OperationApiError, readProblem } from './operations';
import type { RequestOptions } from './operations';
import type { components } from './schema';

/** The `GET`/`PUT /api/v1/iptv` envelope. */
export type IptvState = components['schemas']['IptvState'];
/** The publication document. */
export type IptvConfig = components['schemas']['IptvConfigDoc'];
/** One publication entry. */
export type IptvEntry = components['schemas']['IptvEntryDoc'];
/** One resolved entry. */
export type ResolvedEntry = components['schemas']['ResolvedEntry'];
/** One feed's refresh status. */
export type FeedStatus = components['schemas']['FeedStatus'];
/** A parsed M3U catalogue. */
export type M3uCatalogue = components['schemas']['M3uCatalogue'];
/** One catalogue candidate. */
export type M3uEntry = components['schemas']['M3uEntry'];

export { OperationApiError } from './operations';

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

function isIptvState(value: unknown): value is IptvState {
  return (
    isRecord(value) &&
    isRecord(value.config) &&
    Array.isArray(value.entries) &&
    Array.isArray(value.feeds) &&
    typeof value.playlist_path === 'string'
  );
}

function isCatalogue(value: unknown): value is M3uCatalogue {
  return isRecord(value) && Array.isArray(value.url_tvg) && Array.isArray(value.entries);
}

/** The state plus its ETag. */
export interface IptvWithEtag {
  /** The envelope. */
  readonly state: IptvState;
  /** ETag for the next `If-Match`. */
  readonly etag: string | undefined;
}

/** `GET /api/v1/iptv`. */
export async function getIptv(options: RequestOptions = {}): Promise<IptvWithEtag> {
  const response = await fetch(apiUrl(options, '/api/v1/iptv'), {
    method: 'GET',
    headers: buildHeaders(options, false),
  });
  if (!response.ok) throw await readProblem(response);
  const body: unknown = await response.json();
  if (!isIptvState(body)) throw new OperationApiError('The server returned an unexpected IPTV document.');
  return { state: body, etag: response.headers.get('ETag') ?? undefined };
}

/** `PUT /api/v1/iptv` (requires `options.etag`). */
export async function putIptv(config: IptvConfig, options: RequestOptions = {}): Promise<IptvWithEtag> {
  const response = await fetch(apiUrl(options, '/api/v1/iptv'), {
    method: 'PUT',
    headers: buildHeaders(options, true),
    body: JSON.stringify(config),
  });
  if (!response.ok) throw await readProblem(response);
  const body: unknown = await response.json();
  if (!isIptvState(body)) throw new OperationApiError('The server returned an unexpected IPTV document.');
  return { state: body, etag: response.headers.get('ETag') ?? undefined };
}

/** The import request: exactly one of url/text. */
export interface M3uImportRequest {
  readonly url?: string;
  readonly text?: string;
  readonly allow_private_network?: boolean;
}

/** `POST /api/v1/sources/import/m3u`. */
export async function importM3u(request: M3uImportRequest, options: RequestOptions = {}): Promise<M3uCatalogue> {
  const response = await fetch(apiUrl(options, '/api/v1/sources/import/m3u'), {
    method: 'POST',
    headers: buildHeaders(options, true),
    body: JSON.stringify(request),
  });
  if (!response.ok) throw await readProblem(response);
  const body: unknown = await response.json();
  if (!isCatalogue(body)) throw new OperationApiError('The server returned an unexpected catalogue.');
  return body;
}

/** React Query key. */
export const iptvKeys = { state: (): readonly unknown[] => ['iptv'] };

/** Read the publication (re-polled every 30 s for feed status). */
export function useIptv(): UseQueryResult<IptvWithEtag, OperationApiError> {
  return useQuery<IptvWithEtag, OperationApiError>({
    queryKey: iptvKeys.state(),
    queryFn: () => getIptv(),
    refetchInterval: 30_000,
  });
}

/** Replace the publication with optimistic concurrency. */
export function useSaveIptv(): UseMutationResult<
  IptvWithEtag,
  OperationApiError,
  { config: IptvConfig; etag: string | undefined }
> {
  const client = useQueryClient();
  return useMutation<IptvWithEtag, OperationApiError, { config: IptvConfig; etag: string | undefined }>({
    mutationFn: ({ config, etag }) => putIptv(config, etag !== undefined ? { etag } : {}),
    onSuccess: (data) => {
      client.setQueryData(iptvKeys.state(), data);
    },
  });
}

/** Turn arbitrary text into a source-id slug (`[a-z0-9-]`, ≤ 48 chars). */
export function slugify(text: string): string {
  const slug = text
    .normalize('NFKD')
    .replace(/[\u0300-\u036f]/g, '')
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '')
    .slice(0, 48)
    .replace(/-+$/g, '');
  return slug === '' ? 'source' : slug;
}

/** A unique id for `text` given the ids already taken (mutated). */
export function uniqueId(text: string, taken: Set<string>): string {
  const base = slugify(text);
  let id = base;
  for (let n = 2; taken.has(id); n += 1) id = `${base}-${String(n)}`;
  taken.add(id);
  return id;
}

/** The HLS source body for an imported candidate. */
export function candidateToSourceBody(
  entry: M3uEntry,
  id: string,
  urlTvg: string | undefined,
  activation?: 'always_on' | 'on_demand',
): Record<string, unknown> {
  const body: Record<string, unknown> = { id, kind: 'hls', url: entry.url };
  if (activation !== undefined) body.activation = activation;
  if (urlTvg !== undefined && entry.tvg_id) {
    body.epg = { mode: 'manual', url: urlTvg, channel_id: entry.tvg_id };
  }
  const meta: Record<string, string> = {};
  if (entry.tvg_id) meta.tvg_id = entry.tvg_id;
  const name = entry.tvg_name ?? entry.name;
  if (name !== '') meta.name = name;
  if (entry.tvg_logo && /^https?:\/\//i.test(entry.tvg_logo)) meta.logo = entry.tvg_logo;
  if (entry.group) meta.group = entry.group;
  if (Object.keys(meta).length > 0) body.playlist_metadata = meta;
  return body;
}

/** A publication document with every optional collection present. */
export interface NormalizedConfig {
  readonly enabled: boolean;
  readonly public_base_url: string | null;
  readonly entries: readonly IptvEntry[];
  /** Minutes between background guide refreshes; null = the default (30). */
  readonly guide_refresh_minutes: number | null;
}

/** The default, minimum and maximum guide refresh interval in minutes. */
export const GUIDE_REFRESH_DEFAULT = 30;
export const GUIDE_REFRESH_MIN = 5;
export const GUIDE_REFRESH_MAX = 1440;

/** Fill defaults so the editor never handles `undefined` collections. */
export function normalizeIptv(config: IptvConfig): NormalizedConfig {
  return {
    enabled: config.enabled ?? false,
    public_base_url: config.public_base_url ?? null,
    entries: config.entries ?? [],
    guide_refresh_minutes: config.guide_refresh_minutes ?? null,
  };
}

/** Move `index` by `delta` within `list` (no-op at the ends). */
export function moveEntry<T>(list: readonly T[], index: number, delta: number): T[] {
  const next = [...list];
  const target = index + delta;
  if (target < 0 || target >= next.length) return next;
  const [item] = next.splice(index, 1);
  if (item !== undefined) next.splice(target, 0, item);
  return next;
}
