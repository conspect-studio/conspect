// The patch panel, mixer-input slice (ADR-0110 §12.8, #341). Wire types are
// generated from OpenAPI; a take returns 202 pending, never an on-air claim.
// GET /patch is the engine's live readback, filtered per object server-side.
import type { components } from './types';
import { apiUrl, buildHeaders, OperationApiError, readPendingProblem, newIdempotencyKey } from './operations';
import type { RequestOptions } from './operations';

export type OnAir = components['schemas']['OnAir'];
/** A crosspoint source: the generated schema types it as an opaque object. */
export type PatchSource = { readonly source: string } | 'none';
/** A mixer-input video crosspoint. */
export interface MixerPatchCrosspoint {
  readonly plane: 'video';
  readonly destination: { readonly mixer: string; readonly input: number };
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
  readonly source: PatchSource;
}
/** One crosspoint: a mixer video patch, or an output subtitle slot patch. */
export type PatchCrosspoint = MixerPatchCrosspoint | SubtitlePatchCrosspoint;
/** Plan/take body. */
export interface PatchRequest {
  readonly crosspoints: readonly PatchCrosspoint[];
  readonly on_air?: OnAir;
}
export type PatchPlan = components['schemas']['PatchPlan'];
export type PatchAccepted = components['schemas']['PatchAccepted'];

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

/**
 * One output's subtitle slot as `GET /patch` reports it: `live: true` when
 * the running engine samples this slot, else the accepted config
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

/** One output's subtitle slots. */
export interface PatchOutputView {
  readonly output: string;
  readonly subtitle: readonly PatchSubtitleSlot[];
}

/** `GET /patch`: the mixer plane and, for subtitle slots, the output plane. */
export interface PatchView {
  readonly mixer: readonly MixerPatchView[];
  readonly outputs: readonly PatchOutputView[];
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
function isSubtitleSource(value: unknown): value is PatchSubtitleSlot['source'] {
  return value === null || (isRecord(value)
    && (typeof value.source === 'string' || value.redacted === true));
}
function isSubtitleSlot(value: unknown): value is PatchSubtitleSlot {
  return isRecord(value) && typeof value.slot === 'number' && typeof value.format === 'string'
    && (value.language === null || value.language === undefined || typeof value.language === 'string')
    && (value.name === null || value.name === undefined || typeof value.name === 'string')
    && typeof value.live === 'boolean' && isSubtitleSource(value.source ?? null);
}
function isPatchOutput(value: unknown): value is PatchOutputView {
  return isRecord(value) && typeof value.output === 'string' && Array.isArray(value.subtitle)
    && value.subtitle.every(isSubtitleSlot);
}
/** `GET /patch`'s wire shape: `outputs` is absent on an older server. */
interface PatchWire {
  readonly mixer: readonly MixerPatchView[];
  readonly outputs?: readonly PatchOutputView[];
}
function isView(value: unknown): value is PatchWire {
  return isRecord(value) && Array.isArray(value.mixer) && value.mixer.every(isMixer)
    && (value.outputs === undefined || (Array.isArray(value.outputs) && value.outputs.every(isPatchOutput)));
}
function isPlan(value: unknown): value is PatchPlan {
  return isRecord(value) && typeof value.can_take === 'boolean' && Array.isArray(value.crosspoints);
}
function isAccepted(value: unknown): value is PatchAccepted {
  return isRecord(value) && typeof value.operation_id === 'string' && value.status === 'accepted';
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

/** Read the live patch. */
export async function getPatch(options: RequestOptions = {}): Promise<PatchView> {
  const response = await fetch(apiUrl(options, '/api/v1/patch'), { headers: buildHeaders(options, false) });
  if (!response.ok) throw await readPendingProblem(response);
  const body: unknown = await response.json();
  if (!isView(body)) throw new OperationApiError('Invalid patch readback');
  return { mixer: body.mixer, outputs: body.outputs ?? [] };
}

/** Plan crosspoints: path, on-air flag and refusals; changes nothing. */
export async function planPatch(request: PatchRequest, options: RequestOptions = {}): Promise<PatchPlan> {
  const response = await fetch(apiUrl(options, '/api/v1/patch/plan'), {
    method: 'POST', headers: buildHeaders(options, true), body: JSON.stringify(request),
  });
  if (!response.ok) throw await readPendingProblem(response);
  const body: unknown = await response.json();
  if (!isPlan(body)) throw new OperationApiError('Invalid patch plan');
  return body;
}

/** Take once; 202 is acceptance, the outcome is `switcher.operation`. */
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
