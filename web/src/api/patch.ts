// The patch panel, mixer-input slice (ADR-0110 §12.8, #341). Wire types are
// generated from OpenAPI; a take returns 202 pending, never an on-air claim.
// GET /patch is the engine's live readback, filtered per object server-side.
import type { components } from './types';
import { apiUrl, buildHeaders, OperationApiError, readPendingProblem, newIdempotencyKey } from './operations';
import type { RequestOptions } from './operations';

export type OnAir = components['schemas']['OnAir'];
/** A crosspoint source: the generated schema types it as an opaque object. */
export type PatchSource = { readonly source: string } | 'none';
/** One crosspoint (this slice: video onto a mixer input). */
export interface PatchCrosspoint {
  readonly plane: 'video';
  readonly destination: { readonly mixer: string; readonly input: number };
  readonly source: PatchSource;
}
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

/** `GET /patch`: the mixer plane only in this release. */
export interface PatchView {
  readonly mixer: readonly MixerPatchView[];
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
function isView(value: unknown): value is PatchView {
  return isRecord(value) && Array.isArray(value.mixer) && value.mixer.every(isMixer);
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
export function mixerCrosspoint(me: string, input: number, source: string | undefined): PatchCrosspoint {
  return {
    plane: 'video',
    destination: { mixer: me, input },
    source: source === undefined ? 'none' : { source },
  };
}

/** Read the live patch. */
export async function getPatch(options: RequestOptions = {}): Promise<PatchView> {
  const response = await fetch(apiUrl(options, '/api/v1/patch'), { headers: buildHeaders(options, false) });
  if (!response.ok) throw await readPendingProblem(response);
  const body: unknown = await response.json();
  if (!isView(body)) throw new OperationApiError('Invalid patch readback');
  return body;
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
