// IPTV-ORG catalogue API (docs/io/iptv-org.md §2 / §2.2 / §3).
//
// TODO(iptv-org): switch to generated schema types. The backend endpoints are
// not in `schema.ts` yet, so the wire types are hand-written here and ONLY here
// (one file ⇒ the swap to generated types is a single-file change). Fields that
// the design doc does not pin down are marked `CONTRACT:` — the lead reconciles
// them with the backend at integration.
import { getStoredToken } from './token';

/** Computed-server-side flag vocabulary (§2.2). `already_added` is client-derived. */
export type StreamFlag =
  | 'geo_blocked'
  | 'not_24_7'
  | 'needs_headers'
  | 'http_only'
  | 'closed'
  | 'blocklisted'
  | 'unsupported'
  | 'no_metadata'
  | 'already_added';

/** Ingest protocol of a stream (§1). */
export type StreamProto = 'hls' | 'rtmp' | 'srt' | 'rtsp' | 'mmsh' | 'other';

/** Locality level of a stream's broadcast area (§2.2 `geo.lvl`). */
export type GeoLevel = 'city' | 'subdivision' | 'country' | 'region' | 'none';

/** One joined index row — one stream (§2.2). */
export interface IndexStream {
  /** Stable key: sha256(url)[..12] prefixed `s:`. */
  readonly k: string;
  readonly url: string;
  readonly title: string;
  /** `channel` id (`KAKE.us`) or null for a no-metadata stream. */
  readonly ch: string | null;
  readonly feed: string | null;
  readonly name: string | null;
  readonly alt: readonly string[];
  readonly net: string | null;
  readonly cats: readonly string[];
  /** ISO 639-3 language codes. */
  readonly langs: readonly string[];
  /** ISO 3166-1 alpha-2 country of the channel. */
  readonly country: string | null;
  /** Feed `broadcast_area` ids with prefix: `r/…`, `c/…`, `s/…`, `ct/…`. */
  readonly area: readonly string[];
  readonly geo: { readonly lvl: GeoLevel; readonly places: readonly string[] };
  /** Advertised quality, e.g. `720p`. */
  readonly q: string | null;
  readonly proto: StreamProto;
  readonly flags: readonly StreamFlag[];
  readonly ua: string | null;
  readonly ref: string | null;
  /** Logo cache key (`l:…`) served by `/logos/{k}`. */
  readonly logo: string | null;
  readonly closed: string | null;
  readonly replaced_by: string | null;
  readonly website: string | null;
}

/**
 * `GET /api/v1/catalogues/iptv-org/index`.
 * CONTRACT: the envelope (schema, generated_at, lookups) is not specified in the
 * design doc; the web needs category/language display names, so they ride here.
 */
export interface CatalogueIndex {
  readonly schema: number;
  readonly generated_at: string;
  readonly categories: readonly { readonly id: string; readonly name: string }[];
  readonly languages: readonly { readonly code: string; readonly name: string }[];
  readonly streams: readonly IndexStream[];
}

/** Place kind in the gazetteer. */
export type PlaceKind = 'city' | 'subdivision' | 'country' | 'region';

/**
 * One gazetteer entry. CONTRACT: shape not specified in the design doc. `id`
 * uses the SAME prefixed form as `IndexStream.area` (`ct/USICT`, `s/US-KS`,
 * `c/US`, `r/NAM`) so the web joins without translation.
 */
export interface Place {
  readonly id: string;
  readonly kind: PlaceKind;
  readonly name: string;
  readonly alt: readonly string[];
  /** ISO alpha-2 (city / subdivision / country); null for a region. */
  readonly country: string | null;
  /** Subdivision code (`US-KS`) for a city / subdivision; else null. */
  readonly subdivision: string | null;
  /** Wikidata P625 coordinates (cities; optional for subdivisions/countries). */
  readonly lat: number | null;
  readonly lon: number | null;
  /** Flag emoji (countries). */
  readonly flag: string | null;
  /** Member country codes (regions). */
  readonly members: readonly string[];
}

/** `GET /api/v1/catalogues/iptv-org/places`. */
export interface PlacesResponse {
  readonly schema: number;
  readonly places: readonly Place[];
}

/** `GET /api/v1/catalogues/iptv-org/status`. */
export interface CatalogueStatus {
  readonly state: 'idle' | 'syncing' | 'error' | 'disabled' | 'empty';
  readonly etag: string | null;
  readonly synced_at: string | null;
  readonly counts: { readonly streams: number; readonly channels: number };
  readonly errors: readonly string[];
  /** CONTRACT: optional server location for “near this server”; absent ⇒ hidden. */
  readonly server?: {
    readonly name: string;
    readonly lat: number;
    readonly lon: number;
  } | null;
}

/** Probe verdicts (§3). */
export type ProbeVerdict =
  | 'live'
  | 'degraded'
  | 'geo_blocked_here'
  | 'dead'
  | 'no_media'
  | 'blocked';

/** A timed event seen in the stream (§3 stage 6) — reported, never a failure. */
export interface ProbeEvent {
  /** e.g. `cue_out`, `cue_in`, `daterange`, `scte35`, `id3`, `discontinuity`. */
  readonly kind: string;
  /** Seconds into the sampled window, or an ISO instant (CONTRACT: free text). */
  readonly at: string | null;
  readonly detail: string | null;
}

/** Media facts (§3 stage 5). CONTRACT: field names. */
export interface ProbeFacts {
  readonly width: number | null;
  readonly height: number | null;
  readonly fps: number | null;
  readonly video_codec: string | null;
  readonly audio_codec: string | null;
  readonly bitrate_kbps: number | null;
  readonly advertised_kbps: number | null;
  readonly audio_languages: readonly string[];
  readonly captions: readonly string[];
  /** A 320 px JPEG, as a same-origin URL or data URL. */
  readonly thumbnail: string | null;
}

/** One probe result. */
export interface ProbeResult {
  readonly k: string | null;
  readonly source_id: string | null;
  readonly verdict: ProbeVerdict;
  readonly probed_from: string;
  readonly probed_at: string;
  readonly detail: string | null;
  readonly facts: ProbeFacts | null;
  readonly events: readonly ProbeEvent[];
}

/**
 * `POST …/probes` body. CONTRACT: the browser sends index keys (never raw URLs —
 * keeps the dial list server-owned) or the ids of linked sources; ≤ 50 total.
 */
export interface ProbeRequest {
  readonly keys?: readonly string[];
  readonly source_ids?: readonly string[];
}

/** `POST …/probes` (202) and `GET …/probes/{id}`. */
export interface ProbeJob {
  readonly id: string;
  readonly state: 'queued' | 'running' | 'done' | 'failed';
  readonly results: readonly ProbeResult[];
  readonly error: string | null;
}

/** Link status (§2.4). */
export type LinkStatus =
  | 'in_sync'
  | 'updated'
  | 'stream_removed'
  | 'channel_closed'
  | 'blocklisted'
  | 'unlinked'
  | 'edited';

/** URL/headers/labels triple compared in “Review changes”. */
export interface LinkTarget {
  readonly url: string;
  readonly user_agent: string | null;
  readonly referrer: string | null;
  readonly labels: readonly string[];
}

/** A ranked alternative stream for “Switch to alternative”. */
export interface LinkAlternative extends LinkTarget {
  readonly k: string;
  readonly ch: string | null;
  readonly feed: string | null;
  readonly q: string | null;
  readonly flags: readonly StreamFlag[];
  readonly verdict: ProbeVerdict | null;
}

/**
 * `GET /api/v1/catalogues/iptv-org/links`. CONTRACT: not in the design doc —
 * the Sources page needs the tracker's per-source output over REST (the realtime
 * `catalogue.link_status` event only carries changes).
 */
export interface LinkInfo {
  readonly source_id: string;
  readonly status: LinkStatus;
  readonly checked_at: string | null;
  /** Present for `updated`: what upstream now says for the same channel@feed. */
  readonly upstream: LinkTarget | null;
  readonly replaced_by: string | null;
  readonly alternatives: readonly LinkAlternative[];
}

/** `{ links: [...] }`. */
export interface LinksResponse {
  readonly links: readonly LinkInfo[];
}

/** A failed catalogue call. */
export class CatalogueApiError extends Error {
  readonly status: number | undefined;
  constructor(message: string, status?: number) {
    super(message);
    this.name = 'CatalogueApiError';
    this.status = status;
  }
}

/** Connection options. */
export interface CatalogueRequestOptions {
  readonly baseUrl?: string;
  readonly token?: string;
}

const BASE = '/api/v1/catalogues/iptv-org';

function headers(options: CatalogueRequestOptions, json: boolean): Headers {
  const h = new Headers();
  if (json) {
    h.set('Content-Type', 'application/json');
  }
  const token = options.token ?? getStoredToken();
  if (token !== undefined && token !== '') {
    h.set('Authorization', `Bearer ${token}`);
  }
  return h;
}

async function request<T>(
  path: string,
  options: CatalogueRequestOptions,
  init?: { readonly method: 'POST'; readonly body: unknown },
): Promise<T> {
  const response = await fetch(`${options.baseUrl ?? ''}${BASE}${path}`, {
    method: init?.method ?? 'GET',
    headers: headers(options, init !== undefined),
    ...(init !== undefined ? { body: JSON.stringify(init.body) } : {}),
  });
  if (!response.ok) {
    let message = `Request failed (${String(response.status)})`;
    try {
      const body = (await response.json()) as { title?: unknown };
      if (typeof body.title === 'string') {
        message = body.title;
      }
    } catch {
      // keep the status-only message
    }
    throw new CatalogueApiError(message, response.status);
  }
  return (await response.json()) as T;
}

/** `GET /status`. */
export function fetchStatus(o: CatalogueRequestOptions = {}): Promise<CatalogueStatus> {
  return request<CatalogueStatus>('/status', o);
}
/** `GET /index`. */
export function fetchIndex(o: CatalogueRequestOptions = {}): Promise<CatalogueIndex> {
  return request<CatalogueIndex>('/index', o);
}
/** `GET /places`. */
export function fetchPlaces(o: CatalogueRequestOptions = {}): Promise<PlacesResponse> {
  return request<PlacesResponse>('/places', o);
}
/** `GET /links`. */
export function fetchLinks(o: CatalogueRequestOptions = {}): Promise<LinksResponse> {
  return request<LinksResponse>('/links', o);
}
/** `POST /probes`. */
export function startProbe(
  body: ProbeRequest,
  o: CatalogueRequestOptions = {},
): Promise<ProbeJob> {
  return request<ProbeJob>('/probes', o, { method: 'POST', body });
}
/** `GET /probes/{id}`. */
export function fetchProbe(id: string, o: CatalogueRequestOptions = {}): Promise<ProbeJob> {
  return request<ProbeJob>(`/probes/${encodeURIComponent(id)}`, o);
}
/** Logo URL for a cache key (§5: served same-origin by the control plane). */
export function logoUrl(key: string): string {
  return `${BASE}/logos/${encodeURIComponent(key)}`;
}
