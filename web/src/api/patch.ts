// The patch panel (ADR-0110 §12.8, #341/#308). Wire types are generated from
// OpenAPI; a take returns 202 pending, never an on-air claim. GET /patch is
// the engine's live readback, filtered per object server-side.
//
// Slice E3 (#308) extends the mixer-only slice with the output video/audio
// slots `GET /patch` now reports, and with atomic multi-destination sets: a
// plan/take whose crosspoints touch two or more destinations answers
// `SetPlan` (adds `set: {new_encoders, fits}`) instead of a plain `PatchPlan`.
// The matrix page (`RoutingMatrix`) builds on these.
import type { components } from './types';
import { apiUrl, buildHeaders, OperationApiError, readPendingProblem, newIdempotencyKey } from './operations';
import type { RequestOptions } from './operations';

export type OnAir = components['schemas']['OnAir'];
/** A crosspoint source: the generated schema types it as an opaque object. */
export type PatchSource =
  | { readonly source: string }
  | { readonly layout: string }
  | { readonly bus: 'program' | 'preview' | 'clean' }
  | 'follow'
  | 'none';
/** One crosspoint: a mixer input, or an output video/audio slot (#308). */
export interface PatchCrosspoint {
  readonly plane: 'video' | 'audio' | 'subtitle';
  readonly destination:
    | { readonly mixer: string; readonly input: number }
    | { readonly output: string; readonly slot: number };
  readonly source: PatchSource;
}
/** Plan/take body. */
export interface PatchRequest {
  readonly crosspoints: readonly PatchCrosspoint[];
  readonly on_air?: OnAir;
}
export type PatchPlan = components['schemas']['PatchPlan'];
export type PatchAccepted = components['schemas']['PatchAccepted'];
/** The set's aggregate cost (`SetPlan.set`): present only for a set of 2+ destinations. */
export interface SetCost {
  readonly new_encoders: number;
  readonly fits: boolean;
}
/** A plan response: a plain `PatchPlan`, or a `SetPlan` for an atomic set. */
export type PatchPlanResponse = PatchPlan & { readonly set?: SetCost };

/** One mixer input as the engine reports it. */
export interface MixerInputView {
  readonly number: number;
  readonly label: string | null;
  /** `{source}`, `{redacted: true}` for a source the caller cannot read, or null (unpatched). */
  readonly video: { readonly source: string } | { readonly redacted: true } | null;
  readonly on_program: boolean;
  readonly on_preview: boolean;
}

/** One M/E's live input bank. */
export interface MixerPatchView {
  readonly me: string;
  readonly inputs: readonly MixerInputView[];
  readonly inputs_version: number;
  readonly tick: number;
}

/** One output video slot's live readback (`GET /patch` `outputs[].video[]`). */
export interface OutputVideoSlotView {
  readonly slot: number;
  readonly live: boolean;
  /** `{source}` | `{bus}` | `{layout}` | `{redacted:true}` | null (no readback). */
  readonly source: PatchSource | { readonly redacted: true } | null;
  /** `P1` | `P2` | `P3` | null (no readback). */
  readonly path: string | null;
  readonly pending: { readonly source: unknown; readonly held_ticks: number } | null;
  readonly degraded: boolean;
}

/** One output audio slot's accepted feed (`GET /patch` `outputs[].audio[]`). */
export interface OutputAudioSlotView {
  readonly slot: number;
  readonly track?: string;
  readonly language?: string | null;
  /** `"follow"` (default), `"program"`, `"none"`, `{source}`, `{tracks}` or `{redacted:true}`. */
  readonly source: unknown;
}

/** One output subtitle slot (patch-f, #345): columns the Subtitle tab reads. */
export interface OutputSubtitleSlotView {
  readonly slot: number;
  readonly format: string;
  readonly language?: string | null;
  readonly name?: string | null;
  /** `{source}` | `"none"` | `{redacted:true}` | null (no readback). */
  readonly source: unknown;
}

/** One output's format + live slots, as `GET /patch` `outputs[]` reports it. */
export interface OutputPatchView {
  readonly output: string;
  readonly format: { readonly width?: number; readonly height?: number; readonly fit?: string } | null;
  readonly video: readonly OutputVideoSlotView[];
  readonly audio: readonly OutputAudioSlotView[];
  /** patch-f (#345) fills this later; absent/empty renders the Subtitle tab's empty state. */
  readonly subtitle?: readonly OutputSubtitleSlotView[];
}

/** `GET /patch`: the mixer plane and, as of slice B/E3, the output plane. */
export interface PatchView {
  readonly mixer: readonly MixerPatchView[];
  /** Absent on a server that has not yet deployed the output plane (slice B). */
  readonly outputs?: readonly OutputPatchView[];
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}
function isVideo(value: unknown): value is MixerInputView['video'] {
  return value === null || (isRecord(value)
    && (typeof value.source === 'string' || value.redacted === true));
}
function isInput(value: unknown): value is MixerInputView {
  return isRecord(value) && typeof value.number === 'number'
    && (value.label === null || value.label === undefined || typeof value.label === 'string')
    && isVideo(value.video ?? null)
    && typeof value.on_program === 'boolean' && typeof value.on_preview === 'boolean';
}
function isMixer(value: unknown): value is MixerPatchView {
  return isRecord(value) && typeof value.me === 'string' && Array.isArray(value.inputs)
    && value.inputs.every(isInput) && typeof value.inputs_version === 'number'
    && typeof value.tick === 'number';
}
function isVideoSlot(value: unknown): value is OutputVideoSlotView {
  return isRecord(value) && typeof value.slot === 'number' && typeof value.live === 'boolean'
    && (value.path === null || typeof value.path === 'string')
    && typeof value.degraded === 'boolean';
}
function isAudioSlot(value: unknown): value is OutputAudioSlotView {
  return isRecord(value) && typeof value.slot === 'number';
}
function isSubtitleSlot(value: unknown): value is OutputSubtitleSlotView {
  return isRecord(value) && typeof value.slot === 'number' && typeof value.format === 'string';
}
function isOutput(value: unknown): value is OutputPatchView {
  return isRecord(value) && typeof value.output === 'string'
    && Array.isArray(value.video) && value.video.every(isVideoSlot)
    && Array.isArray(value.audio) && value.audio.every(isAudioSlot)
    && (value.subtitle === undefined || (Array.isArray(value.subtitle) && value.subtitle.every(isSubtitleSlot)));
}
function isView(value: unknown): value is PatchView {
  return isRecord(value) && Array.isArray(value.mixer) && value.mixer.every(isMixer)
    && (value.outputs === undefined || (Array.isArray(value.outputs) && value.outputs.every(isOutput)));
}
/** Shared guard: a `PatchPlan` or `SetPlan` body, as `/patch/plan` and `/salvos/{id}/plan` both answer. */
export function isPatchPlanResponse(value: unknown): value is PatchPlanResponse {
  return isRecord(value) && typeof value.can_take === 'boolean' && Array.isArray(value.crosspoints)
    && (value.set === undefined || (isRecord(value.set) && typeof value.set.new_encoders === 'number' && typeof value.set.fits === 'boolean'));
}
const isPlan = isPatchPlanResponse;
function isAccepted(value: unknown): value is PatchAccepted {
  return isRecord(value) && typeof value.operation_id === 'string' && value.status === 'accepted';
}

/** The source id patched to an input, if the caller may read it. */
export function inputSource(input: MixerInputView): string | undefined {
  return input.video !== null && 'source' in input.video ? input.video.source : undefined;
}

/** One video crosspoint onto `me.in<input>`; `source` undefined unpatches. */
export function mixerCrosspoint(me: string, input: number, source: string | undefined): PatchCrosspoint {
  return {
    plane: 'video',
    destination: { mixer: me, input },
    source: source === undefined ? 'none' : { source },
  };
}

/** One video crosspoint onto `output.video[slot]`. */
export function outputVideoCrosspoint(output: string, slot: number, source: PatchSource): PatchCrosspoint {
  return { plane: 'video', destination: { output, slot }, source };
}

/** One audio crosspoint onto `output.audio[slot]` (slot 0 "follow" in this release). */
export function outputAudioCrosspoint(output: string, slot: number, source: PatchSource): PatchCrosspoint {
  return { plane: 'audio', destination: { output, slot }, source };
}

/** True once a request's crosspoints touch two or more distinct destinations (an atomic set). */
export function isSetRequest(request: PatchRequest): boolean {
  const keys = new Set(
    request.crosspoints.map((xp) =>
      'mixer' in xp.destination ? `mixer:${xp.destination.mixer}:${String(xp.destination.input)}` : `output:${xp.destination.output}`,
    ),
  );
  return keys.size >= 2;
}

/** Read the live patch (mixer + output planes). */
export async function getPatch(options: RequestOptions = {}): Promise<PatchView> {
  const response = await fetch(apiUrl(options, '/api/v1/patch'), { headers: buildHeaders(options, false) });
  if (!response.ok) throw await readPendingProblem(response);
  const body: unknown = await response.json();
  if (!isView(body)) throw new OperationApiError('Invalid patch readback');
  return body;
}

/** Plan crosspoints: path, on-air flag and refusals (+ `set` cost for 2+ destinations); changes nothing. */
export async function planPatch(request: PatchRequest, options: RequestOptions = {}): Promise<PatchPlanResponse> {
  const response = await fetch(apiUrl(options, '/api/v1/patch/plan'), {
    method: 'POST', headers: buildHeaders(options, true), body: JSON.stringify(request),
  });
  if (!response.ok) throw await readPendingProblem(response);
  const body: unknown = await response.json();
  if (!isPlan(body)) throw new OperationApiError('Invalid patch plan');
  return body;
}

/** Take once; 202 is acceptance. One destination settles on `switcher.operation`/`route.output`; a set of 2+ on `patch.take`. */
export async function takePatch(request: PatchRequest, options: RequestOptions = {}): Promise<PatchAccepted> {
  const requestOptions = { ...options, idempotencyKey: options.idempotencyKey ?? newIdempotencyKey() };
  const response = await fetch(apiUrl(requestOptions, '/api/v1/patch/take'), {
    method: 'POST', headers: buildHeaders(requestOptions, true), body: JSON.stringify(request),
  });
  if (!response.ok) throw await readPendingProblem(response);
  if (response.status !== 202) throw new OperationApiError('Patch take was not accepted', response.status);
  const body: unknown = await response.json();
  if (!isAccepted(body)) throw new OperationApiError('Invalid patch operation id');
  return body;
}
