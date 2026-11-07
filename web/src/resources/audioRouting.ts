// API bindings + React Query hooks for the AUDIO ROUTING singleton document.
//
// Unlike sources/outputs/overlays this is ONE document at ONE address —
// `GET`/`PUT /api/v1/audio-routing` — so it does not ride the collection
// helpers in `./api`. The GET is 404-free (`configured: false` + a null
// document when unset) and always carries the document `ETag`; the PUT
// replaces the whole document and must echo that `ETag` as `If-Match`
// (optimistic concurrency, 412/428 otherwise). Responses are validated
// structurally with typed guards — never an `as`-cast of untyped JSON.
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import type { UseMutationResult, UseQueryResult } from '@tanstack/react-query';

import { getStoredToken } from '../api/token';
import { listResources } from './api';
import { ApiError } from './queries';
import { AUDIO_CHANNELS } from './audioForms';
import type {
  AudioChannelsKind,
  AudioRouteDocument,
  AudioRoutingDocument,
} from './audioForms';

/** One discrete-track rename a routing `PUT`'s propagation applied. */
export interface RenamedTrack {
  /** The track name the output's selection carried before the replace. */
  readonly from: string;
  /** The name it now carries. */
  readonly to: string;
}

/**
 * One output a routing `PUT`'s rename/removal propagation rewrote
 * (ADR-R005 §4.3): the server already rewrote the output's `audio.tracks`
 * selection in the SAME atomic commit as the routing replace — this entry is
 * purely informational (what changed), never something the UI must re-apply.
 */
export interface OutputsUpdatedEntry {
  /** The output's stable id (`/api/v1/outputs/{id}`). */
  readonly output: string;
  /** Track names dropped from this output's selection. */
  readonly removed: readonly string[];
  /** Discrete tracks this output's selection now follows under a new name. */
  readonly renamed: readonly RenamedTrack[];
}

/** The wire envelope of `GET`/`PUT /api/v1/audio-routing`. */
export interface AudioRoutingState {
  /** Whether a routing document is configured. */
  readonly configured: boolean;
  /** The routing document, or `null` when unconfigured. */
  readonly routing: AudioRoutingDocument | null;
  /** `"prog"` + every declared discrete track, in declaration order. */
  readonly selectable_tracks: readonly string[];
  /**
   * Outputs a successful `PUT`'s rename/removal propagation rewrote, in
   * output order. Absent/empty on `GET`, or a `PUT` that propagated nothing.
   */
  readonly outputs_updated?: readonly OutputsUpdatedEntry[];
}

/** The envelope plus the `ETag` the server stamped it with. */
export interface AudioRoutingWithEtag {
  /** The routing state. */
  readonly state: AudioRoutingState;
  /** The document `ETag` for the next conditional PUT, when carried. */
  readonly etag: string | undefined;
  /**
   * How a PUT took effect (`X-Multiview-Apply`): `live` when the running
   * program bus applied it, `restart` when it needs a restart. `undefined` on
   * GET or when the server sent no header.
   */
  readonly apply?: 'live' | 'restart';
}

/** Connection options shared by both calls. */
export interface AudioRoutingRequestOptions {
  /** Base URL (defaults to same-origin). */
  readonly baseUrl?: string;
  /** Optional bearer token (falls back to the stored operator token). */
  readonly token?: string;
  /** The current document ETag for `If-Match` on PUT. */
  readonly etag?: string;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

function isChannelsKind(value: unknown): value is AudioChannelsKind {
  return AUDIO_CHANNELS.some((kind) => kind === value);
}

function isOptionalString(value: unknown): value is string | undefined {
  return value === undefined || typeof value === 'string';
}

function isRouteDocument(value: unknown): value is AudioRouteDocument {
  return (
    isRecord(value) &&
    typeof value.input_id === 'string' &&
    isRecord(value.channels) &&
    isChannelsKind(value.channels.kind) &&
    isOptionalString(value.target_track) &&
    isOptionalString(value.language) &&
    isOptionalString(value.title) &&
    typeof value.include_in_program_bus === 'boolean' &&
    typeof value.gain_db === 'number' &&
    typeof value.mute === 'boolean'
  );
}

function isRoutingDocument(value: unknown): value is AudioRoutingDocument {
  return (
    isRecord(value) &&
    typeof value.sample_rate_hz === 'number' &&
    Array.isArray(value.routes) &&
    value.routes.every(isRouteDocument)
  );
}

/** Structural guard for the wire envelope. */
function isRenamedTrack(value: unknown): value is RenamedTrack {
  return isRecord(value) && typeof value.from === 'string' && typeof value.to === 'string';
}

function isOutputsUpdatedEntry(value: unknown): value is OutputsUpdatedEntry {
  return (
    isRecord(value) &&
    typeof value.output === 'string' &&
    Array.isArray(value.removed) &&
    value.removed.every((track) => typeof track === 'string') &&
    Array.isArray(value.renamed) &&
    value.renamed.every(isRenamedTrack)
  );
}

export function isAudioRoutingState(value: unknown): value is AudioRoutingState {
  return (
    isRecord(value) &&
    typeof value.configured === 'boolean' &&
    (value.routing === null || isRoutingDocument(value.routing)) &&
    Array.isArray(value.selectable_tracks) &&
    value.selectable_tracks.every((track) => typeof track === 'string') &&
    (value.outputs_updated === undefined ||
      (Array.isArray(value.outputs_updated) &&
        value.outputs_updated.every(isOutputsUpdatedEntry)))
  );
}

function isProblem(
  value: unknown,
): value is { title: string; status: number; detail?: unknown } {
  return (
    isRecord(value) &&
    typeof value.status === 'number' &&
    typeof value.title === 'string'
  );
}

async function readProblem(response: Response): Promise<ApiError> {
  try {
    const body: unknown = await response.json();
    if (isProblem(body)) {
      // The RFC 9457 `detail` carries the offending field path — surface it.
      const message =
        typeof body.detail === 'string' && body.detail !== ''
          ? `${body.title}: ${body.detail}`
          : body.title;
      return new ApiError(message, body.status);
    }
  } catch {
    // Fall through to a status-only error when the body is absent/unparseable.
  }
  return new ApiError(`Request failed (${String(response.status)})`, response.status);
}

function headersFor(options: AudioRoutingRequestOptions, withJsonBody: boolean): Headers {
  const headers = new Headers();
  if (withJsonBody) {
    headers.set('Content-Type', 'application/json');
  }
  const token = options.token ?? getStoredToken();
  if (token !== undefined && token !== '') {
    headers.set('Authorization', `Bearer ${token}`);
  }
  if (options.etag !== undefined && options.etag !== '') {
    headers.set('If-Match', options.etag);
  }
  return headers;
}

function documentUrl(options: AudioRoutingRequestOptions): string {
  return `${options.baseUrl ?? ''}/api/v1/audio-routing`;
}

async function readState(response: Response): Promise<AudioRoutingWithEtag> {
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isAudioRoutingState(body)) {
    throw new ApiError('The server returned an unexpected audio-routing document.');
  }
  const etag = response.headers.get('ETag');
  const applyHeader = response.headers.get('X-Multiview-Apply');
  const apply =
    applyHeader === 'live' || applyHeader === 'restart' ? applyHeader : undefined;
  return {
    state: body,
    etag: etag ?? undefined,
    ...(apply !== undefined ? { apply } : {}),
  };
}

/** Fetch the singleton document (404-free) with its `ETag`. */
export async function fetchAudioRouting(
  options: AudioRoutingRequestOptions = {},
): Promise<AudioRoutingWithEtag> {
  const response = await fetch(documentUrl(options), {
    method: 'GET',
    headers: headersFor(options, false),
  });
  return readState(response);
}

/** Replace the singleton document (`PUT` + `If-Match`). */
export async function putAudioRouting(
  document: AudioRoutingDocument,
  options: AudioRoutingRequestOptions = {},
): Promise<AudioRoutingWithEtag> {
  const response = await fetch(documentUrl(options), {
    method: 'PUT',
    headers: headersFor(options, true),
    body: JSON.stringify(document),
  });
  return readState(response);
}

/** The stable React Query key of the singleton. */
export const audioRoutingKey: readonly string[] = ['audio-routing'];

function toApiError(error: unknown): ApiError {
  if (error instanceof ApiError) {
    return error;
  }
  if (error instanceof Error) {
    return new ApiError(error.message);
  }
  return new ApiError('Unknown error');
}

/** Read the audio-routing document (best-effort; the engine is isolated). */
export function useAudioRouting(
  options: AudioRoutingRequestOptions = {},
): UseQueryResult<AudioRoutingWithEtag, ApiError> {
  return useQuery<AudioRoutingWithEtag, ApiError>({
    queryKey: audioRoutingKey,
    queryFn: async (): Promise<AudioRoutingWithEtag> => {
      try {
        return await fetchAudioRouting(options);
      } catch (error) {
        throw toApiError(error);
      }
    },
  });
}

/** The variables of a save: the whole document + the `ETag` it replaces. */
export interface SaveAudioRoutingVars {
  /** The complete replacement document. */
  readonly document: AudioRoutingDocument;
  /** The `ETag` of the version being replaced (`If-Match`). */
  readonly etag: string | undefined;
}

/**
 * Replace the document. The fresh `ETag` rides the result; the query is
 * invalidated on settle so readers re-read authoritative server state.
 */
export function useSaveAudioRouting(
  options: AudioRoutingRequestOptions = {},
): UseMutationResult<AudioRoutingWithEtag, ApiError, SaveAudioRoutingVars> {
  const queryClient = useQueryClient();
  return useMutation<AudioRoutingWithEtag, ApiError, SaveAudioRoutingVars>({
    mutationFn: async ({ document, etag }): Promise<AudioRoutingWithEtag> => {
      try {
        return await putAudioRouting(document, {
          ...options,
          ...(etag !== undefined ? { etag } : {}),
        });
      } catch (error) {
        throw toApiError(error);
      }
    },
    onSettled: (): void => {
      void queryClient.invalidateQueries({ queryKey: audioRoutingKey });
    },
  });
}

/**
 * One output's audio selection, read directly from its raw resource body
 * (`GET /api/v1/outputs`) rather than the typed {@link OutputView} (which
 * does not carry `audio`) — used ONLY to compute the Audio page's pre-save
 * warning (which outputs currently carry a track the operator is about to
 * rename or remove).
 */
export interface OutputAudioSelection {
  /** The output's stable id. */
  readonly id: string;
  /** The output's operator label. */
  readonly name: string;
  /** The selected tracks when `mode = "tracks"`; empty otherwise. */
  readonly tracks: readonly string[];
}

function isTrackList(value: unknown): value is readonly string[] {
  return Array.isArray(value) && value.every((item) => typeof item === 'string');
}

/** Project a raw output body's `audio.tracks` list, or `[]` when absent. */
function tracksFromBody(body: Record<string, unknown>): readonly string[] {
  const audio = body.audio;
  if (!isRecord(audio) || audio.mode !== 'tracks') {
    return [];
  }
  return isTrackList(audio.tracks) ? audio.tracks : [];
}

/**
 * List every output's current discrete-track selection (best-effort; a
 * fetch failure degrades to an empty list rather than blocking the Audio
 * page, which has its own primary read).
 */
export function useOutputAudioSelections(): UseQueryResult<
  readonly OutputAudioSelection[],
  ApiError
> {
  return useQuery<readonly OutputAudioSelection[], ApiError>({
    queryKey: ['outputs', 'audio-selections'],
    queryFn: async (): Promise<readonly OutputAudioSelection[]> => {
      try {
        const records = await listResources('outputs');
        return records.map((record) => ({
          id: record.id,
          name: record.name,
          tracks: tracksFromBody(record.body),
        }));
      } catch (error) {
        throw toApiError(error);
      }
    },
  });
}
