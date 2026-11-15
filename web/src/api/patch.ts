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
/** A subtitle slot's source: subtitle essences come from sources only. */
export type SubtitlePatchSource = { readonly source: string } | 'none';
/** A mixer-input video crosspoint. */
export interface MixerPatchCrosspoint {
  readonly plane: 'video';
  readonly destination: { readonly mixer: string; readonly input: number };
  readonly source: PatchSource;
}
/** An output video/audio slot crosspoint (#308, slice E3). */
export interface OutputPatchCrosspoint {
  readonly plane: 'video' | 'audio';
  readonly destination: { readonly output: string; readonly slot: number };
  readonly source: PatchSource;
}
/**
 * An output subtitle slot crosspoint (ADR-0110 §12.8, #345 part F2):
 * `{"source": id}` re-points the slot, `"none"` unpatches it. A request
 * carries only subtitle crosspoints of ONE output, each slot once (mixing
 * with video/audio, a second output or a repeated slot is `unknown-slot`).
 */
export interface SubtitlePatchCrosspoint {
  readonly plane: 'subtitle';
  readonly destination: { readonly output: string; readonly slot: number };
  readonly source: SubtitlePatchSource;
}
/** One crosspoint: a mixer video patch, an output video/audio slot patch, or an output subtitle slot patch. */
export type PatchCrosspoint = MixerPatchCrosspoint | OutputPatchCrosspoint | SubtitlePatchCrosspoint;
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
  /** Absent is accepted as "no label" (the guard admits it); readers fall back with `??`. */
  readonly label?: string | null | undefined;
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

/**
 * One output's subtitle slot as `GET /patch` reports it (#345 F2): `live: true`
 * when the running engine samples this slot, else the accepted config
 * (`live: false`). `source` is `{source}`, `{redacted: true}` (unreadable to
 * the caller) or `null` (unpatched).
 */
export interface PatchSubtitleSlot {
  readonly slot: number;
  readonly format: string;
  readonly language: string | null;
  readonly name: string | null;
  readonly live: boolean;
  readonly source: { readonly source: string } | { readonly redacted: true } | null;
}

/**
 * One output's format + slots, as `GET /patch` `outputs[]` reports it: the
 * video/audio readback the matrix reads (E3) and the subtitle slots the
 * Subtitle patch panel patches (patch-f). `subtitle` is `[]` when the server
 * omits it (a pre-#345 server).
 */
export interface OutputPatchView {
  readonly output: string;
  readonly format?: { readonly width?: number; readonly height?: number; readonly fit?: string } | null | undefined;
  readonly video: readonly OutputVideoSlotView[];
  readonly audio: readonly OutputAudioSlotView[];
  readonly subtitle: readonly PatchSubtitleSlot[];
}

/** `GET /patch`: the mixer plane and the output plane (`[]` on a server without it). */
export interface PatchView {
  readonly mixer: readonly MixerPatchView[];
  readonly outputs: readonly OutputPatchView[];
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
    // `video` is required (null = unpatched): an omitted field is malformed,
    // never normalized, so every reader can rely on `null | object` (E3-07).
    && 'video' in value && isVideo(value.video)
    && typeof value.on_program === 'boolean' && typeof value.on_preview === 'boolean';
}
function isMixer(value: unknown): value is MixerPatchView {
  return isRecord(value) && typeof value.me === 'string' && Array.isArray(value.inputs)
    && value.inputs.every(isInput) && typeof value.inputs_version === 'number'
    && typeof value.tick === 'number';
}
function isOptionalString(value: unknown): boolean {
  return value === undefined || value === null || typeof value === 'string';
}
/** A readback source value: null, a known string sentinel, or an object the cell model inspects field by field. */
function isReadbackSource(value: unknown): boolean {
  return value === null || typeof value === 'string' || (isRecord(value) && !Array.isArray(value));
}
function isPending(value: unknown): value is OutputVideoSlotView['pending'] {
  return value === null || (isRecord(value) && 'source' in value && isReadbackSource(value.source)
    && typeof value.held_ticks === 'number' && Number.isFinite(value.held_ticks));
}
function isVideoSlot(value: unknown): value is OutputVideoSlotView {
  return isRecord(value) && typeof value.slot === 'number' && typeof value.live === 'boolean'
    && 'source' in value && isReadbackSource(value.source)
    && (value.path === null || typeof value.path === 'string')
    && 'pending' in value && isPending(value.pending)
    && typeof value.degraded === 'boolean';
}
function isAudioSlot(value: unknown): value is OutputAudioSlotView {
  return isRecord(value) && typeof value.slot === 'number'
    && isOptionalString(value.track) && isOptionalString(value.language)
    && (value.source === undefined || isReadbackSource(value.source));
}
function isSubtitleSource(value: unknown): value is PatchSubtitleSlot['source'] {
  return value === null || (isRecord(value)
    && (typeof value.source === 'string' || value.redacted === true));
}
/** A subtitle slot (#345 F2): `language`/`name`/`source` may be omitted (read as null). */
function isSubtitleSlot(value: unknown): value is PatchSubtitleSlotWire {
  return isRecord(value) && typeof value.slot === 'number' && typeof value.format === 'string'
    && isOptionalString(value.language) && isOptionalString(value.name)
    && typeof value.live === 'boolean' && isSubtitleSource(value.source ?? null);
}
function isOutput(value: unknown): value is OutputPatchWire {
  return isRecord(value) && typeof value.output === 'string'
    && (value.format === null || value.format === undefined || (isRecord(value.format) && !Array.isArray(value.format)))
    && Array.isArray(value.video) && value.video.every(isVideoSlot)
    && Array.isArray(value.audio) && value.audio.every(isAudioSlot)
    && (value.subtitle === undefined || (Array.isArray(value.subtitle) && value.subtitle.every(isSubtitleSlot)));
}
/** A subtitle slot as the wire may carry it: optional fields are normalized to null by {@link getPatch}. */
type PatchSubtitleSlotWire = Omit<PatchSubtitleSlot, 'language' | 'name' | 'source'> & {
  readonly language?: string | null;
  readonly name?: string | null;
  readonly source?: PatchSubtitleSlot['source'];
};
/** One output as the wire carries it: `subtitle` is absent on a pre-#345 server. */
type OutputPatchWire = Omit<OutputPatchView, 'subtitle'> & { readonly subtitle?: readonly PatchSubtitleSlotWire[] };
/** `GET /patch`'s wire shape: `outputs` is absent on a server without the output plane. */
export interface PatchViewWire {
  readonly mixer: readonly MixerPatchView[];
  readonly outputs?: readonly OutputPatchWire[];
}
/** Runtime guard for a `GET /patch` body: every field the matrix and the subtitle panel dereference is checked. */
export function isPatchView(value: unknown): value is PatchViewWire {
  return isRecord(value) && Array.isArray(value.mixer) && value.mixer.every(isMixer)
    && (value.outputs === undefined || (Array.isArray(value.outputs) && value.outputs.every(isOutput)));
}
/** Normalize a validated wire body: absent outputs/subtitle are `[]`, absent subtitle fields `null`. */
function normalizeView(body: PatchViewWire): PatchView {
  return {
    mixer: body.mixer,
    outputs: (body.outputs ?? []).map((output) => ({
      ...output,
      subtitle: (output.subtitle ?? []).map((slot) => ({
        slot: slot.slot,
        format: slot.format,
        language: slot.language ?? null,
        name: slot.name ?? null,
        live: slot.live,
        source: slot.source ?? null,
      })),
    })),
  };
}
function isOptionalInteger(value: unknown): boolean {
  return value === undefined || value === null || (typeof value === 'number' && Number.isInteger(value));
}
function isDestination(value: unknown): boolean {
  return isRecord(value) && isOptionalString(value.mixer) && isOptionalInteger(value.input)
    && isOptionalString(value.output) && isOptionalInteger(value.slot)
    && (typeof value.mixer === 'string' || typeof value.output === 'string');
}
function isRefusal(value: unknown): boolean {
  return value === undefined || value === null
    || (isRecord(value) && typeof value.type === 'string' && typeof value.detail === 'string');
}
function isPlannedCrosspoint(value: unknown): boolean {
  return isRecord(value) && isDestination(value.destination) && typeof value.path === 'string'
    && typeof value.on_air === 'boolean' && isRefusal(value.refusal)
    && (value.cost === undefined || value.cost === null || isRecord(value.cost));
}
/** Shared guard: a `PatchPlan` or `SetPlan` body, as `/patch/plan` and `/salvos/{id}/plan` both answer. */
export function isPatchPlanResponse(value: unknown): value is PatchPlanResponse {
  return isRecord(value) && typeof value.can_take === 'boolean' && typeof value.class === 'string'
    && Array.isArray(value.crosspoints) && value.crosspoints.every(isPlannedCrosspoint)
    && (value.set === undefined || (isRecord(value.set) && typeof value.set.new_encoders === 'number' && typeof value.set.fits === 'boolean'));
}
const isPlan = isPatchPlanResponse;
function isAccepted(value: unknown): value is PatchAccepted {
  return isRecord(value) && typeof value.operation_id === 'string' && value.status === 'accepted';
}

/**
 * Whether the caller holds the Write role (§8 `read-only`: the principal lacks
 * Write). The API exposes no principal/role read, so this asks the one
 * side-effect-free endpoint whose FIRST check is the Write role:
 * `POST /patch/plan` runs role → count → ..., so an empty crosspoint list
 * answers 403 for a role without Write and 422 (count) for one with it. It
 * plans nothing. `undefined` = could not tell (the server still enforces).
 */
export async function probePatchWrite(options: RequestOptions = {}): Promise<boolean | undefined> {
  try {
    const response = await fetch(apiUrl(options, '/api/v1/patch/plan'), {
      method: 'POST', headers: buildHeaders(options, true), body: JSON.stringify({ crosspoints: [] }),
    });
    if (response.status === 401 || response.status === 403) return false;
    if (response.status === 422 || response.status === 400 || response.ok) return true;
    return undefined;
  } catch {
    return undefined;
  }
}

/** The source id patched to an input, if the caller may read it. */
export function inputSource(input: MixerInputView): string | undefined {
  return input.video !== null && 'source' in input.video ? input.video.source : undefined;
}

/** One video crosspoint onto `me.in<input>`; `source` undefined unpatches. */
export function mixerCrosspoint(me: string, input: number, source: string | undefined): MixerPatchCrosspoint {
  return {
    plane: 'video',
    destination: { mixer: me, input },
    source: source === undefined ? 'none' : { source },
  };
}

/** One video crosspoint onto `output.video[slot]`. */
export function outputVideoCrosspoint(output: string, slot: number, source: PatchSource): OutputPatchCrosspoint {
  return { plane: 'video', destination: { output, slot }, source };
}

/** One audio crosspoint onto `output.audio[slot]` (slot 0 "follow" in this release). */
export function outputAudioCrosspoint(output: string, slot: number, source: PatchSource): OutputPatchCrosspoint {
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

/**
 * One subtitle crosspoint onto `output`'s subtitle `slot`; `source`
 * undefined unpatches it (ADR-0110 §12.8, #345 part F2).
 */
export function subtitleCrosspoint(
  output: string,
  slot: number,
  source: string | undefined,
): SubtitlePatchCrosspoint {
  return {
    plane: 'subtitle',
    destination: { output, slot },
    source: source === undefined ? 'none' : { source },
  };
}

/** The source id patched to a subtitle slot, if the caller may read it. */
export function subtitleSlotSource(slot: PatchSubtitleSlot): string | undefined {
  return slot.source !== null && 'source' in slot.source ? slot.source.source : undefined;
}

/** Whether a subtitle slot's source is hidden from the caller. */
export function subtitleSlotRedacted(slot: PatchSubtitleSlot): boolean {
  return slot.source !== null && 'redacted' in slot.source && slot.source.redacted;
}

/** Read the live patch (mixer + output planes). */
export async function getPatch(options: RequestOptions = {}): Promise<PatchView> {
  const response = await fetch(apiUrl(options, '/api/v1/patch'), { headers: buildHeaders(options, false) });
  if (!response.ok) throw await readPendingProblem(response);
  const body: unknown = await response.json();
  if (!isPatchView(body)) throw new OperationApiError('Invalid patch readback');
  return normalizeView(body);
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

/** A destination as a plan member or stored crosspoint names it (`PatchDestination`, all fields optional). */
export interface DestinationRef {
  readonly mixer?: string | null;
  readonly input?: number | null;
  readonly output?: string | null;
  readonly slot?: number | null;
}

function parseSource(value: unknown): PatchSource | undefined {
  if (value === 'follow' || value === 'none') return value;
  if (!isRecord(value)) return undefined;
  if (typeof value.source === 'string') return { source: value.source };
  if (typeof value.layout === 'string') return { layout: value.layout };
  if (value.bus === 'program' || value.bus === 'preview' || value.bus === 'clean') return { bus: value.bus };
  return undefined;
}

/**
 * Normalize stored crosspoints (a salvo's `patch`, typed opaquely by the
 * generated schema) into {@link PatchCrosspoint}s; malformed members are
 * dropped and reported through `invalid` so the caller can refuse to act on a
 * salvo it cannot fully resolve.
 */
export function parseCrosspoints(value: unknown): { readonly crosspoints: readonly PatchCrosspoint[]; readonly invalid: number } {
  if (!Array.isArray(value)) return { crosspoints: [], invalid: value === undefined || value === null ? 0 : 1 };
  const crosspoints: PatchCrosspoint[] = [];
  let invalid = 0;
  for (const item of value) {
    if (!isRecord(item) || (item.plane !== 'video' && item.plane !== 'audio' && item.plane !== 'subtitle')) {
      invalid += 1;
      continue;
    }
    const source = parseSource(item.source);
    const d = item.destination;
    if (source === undefined || !isRecord(d)) {
      invalid += 1;
      continue;
    }
    // Shapes no request type can carry (a non-video mixer crosspoint, a
    // subtitle slot fed by a layout/bus/follow) are counted invalid: the
    // server refuses them anyway (`slot-medium-mismatch`).
    if (typeof d.mixer === 'string' && typeof d.input === 'number') {
      if (item.plane !== 'video') {
        invalid += 1;
        continue;
      }
      crosspoints.push({ plane: 'video', destination: { mixer: d.mixer, input: d.input }, source });
    } else if (typeof d.output === 'string') {
      const destination = { output: d.output, slot: typeof d.slot === 'number' ? d.slot : 0 };
      if (item.plane === 'subtitle') {
        if (source !== 'none' && !(typeof source === 'object' && 'source' in source)) {
          invalid += 1;
          continue;
        }
        crosspoints.push({ plane: 'subtitle', destination, source });
      } else {
        crosspoints.push({ plane: item.plane, destination, source });
      }
    } else {
      invalid += 1;
    }
  }
  return { crosspoints, invalid };
}
