// Downstream keyer (DSK) control (design note docs/architecture/switcher-keyers.md
// §8/§10, K5). All four keyer verbs ride the existing switcher plan/command
// routes (ADR-0113 §8, K4): `keyer_cut`, `keyer_auto`, `keyer_tie`, `keyer_set`.
// Wire types are generated from OpenAPI (`KeyerRow`/`KeyerFields`/`KeyerPlan`/
// `KeyerCost`/`KeyerOnAir`) — never hand-written. The upstream `arm` verb and
// USK rows arrive with K3 (#388, not merged); nothing here assumes them.
import type { components } from './types';
import { apiUrl, buildHeaders, OperationApiError, readPendingProblem, newIdempotencyKey } from './operations';
import type { RequestOptions } from './operations';

export type KeyerRow = components['schemas']['KeyerRow'];
/**
 * `fly`/`mask`/`max_source` are free-form objects in the OpenAPI schema (the
 * server validates their shape, not the spec), so openapi-typescript emits
 * an opaque `Record<string, never>`. As `SwitcherState.transition` already
 * does for a sibling free-form field, this narrows them to the concrete
 * `KeyerRect`/`KeyerSize` shape the design note documents (§8) — never a
 * hand-written wire type for a field the generator DOES describe.
 */
export type KeyerFields = Omit<components['schemas']['KeyerFields'], 'fly' | 'mask' | 'max_source'> & {
  fly?: KeyerRect | null;
  mask?: KeyerRect | null;
  max_source?: KeyerSize | null;
};
export type KeyerPlan = components['schemas']['KeyerPlan'];
export type KeyerCost = components['schemas']['KeyerCost'];
export type KeyerOnAir = components['schemas']['KeyerOnAir'];
export type KeyerAction = 'keyer_cut' | 'keyer_auto' | 'keyer_tie' | 'keyer_set';
/**
 * A keyer verb request: the shared `SwitcherRequest` envelope plus the keyer
 * fields. `duration_frames` is the background-verb AUTO field, reused by
 * `keyer_auto` as an optional rate override (design note §8, K4).
 */
export type KeyerRequest = KeyerFields & {
  readonly me: string;
  readonly program: string;
  readonly action: KeyerAction;
  readonly duration_frames?: number;
};
export type KeyerAccepted = components['schemas']['SwitcherAccepted'];

/** A rectangle in pixels: `fly` in canvas pixels, `mask` in fill-source pixels. */
export interface KeyerRect {
  readonly x: number;
  readonly y: number;
  readonly w: number;
  readonly h: number;
}
/** A picture size in pixels (`max_source`). */
export interface KeyerSize {
  readonly w: number;
  readonly h: number;
}
export type KeyerKind = 'luma' | 'linear';
export type KeyerLossPolicy = 'drop_to_off_air' | 'hold_last_good';
export type KeyerLossTrigger = 'stale' | 'reconnecting' | 'no_signal';

/** The ramp descriptor of a keyer in flight; clients compute the weight. */
export interface KeyerRamp {
  readonly start_tick: number;
  readonly rate: number;
  readonly to_on: boolean;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}
function isRect(value: unknown): value is KeyerRect {
  return isRecord(value) && typeof value.x === 'number' && typeof value.y === 'number'
    && typeof value.w === 'number' && typeof value.h === 'number';
}
function isRamp(value: unknown): value is KeyerRamp {
  return isRecord(value) && typeof value.start_tick === 'number' && typeof value.rate === 'number'
    && typeof value.to_on === 'boolean';
}
function isKeyerRow(value: unknown): value is KeyerRow {
  return isRecord(value) && typeof value.id === 'string' && typeof value.family === 'string'
    && typeof value.fill === 'string' && (value.key === undefined || value.key === null || typeof value.key === 'string')
    && typeof value.type === 'string' && typeof value.on_air === 'boolean' && typeof value.tie === 'boolean'
    && typeof value.rate === 'number' && typeof value.loss === 'string' && typeof value.stale === 'boolean'
    && typeof value.pending === 'boolean'
    && (value.fly === undefined || value.fly === null || isRect(value.fly))
    && (value.mask === undefined || value.mask === null || isRect(value.mask))
    && (value.ramp === undefined || value.ramp === null || isRamp(value.ramp));
}
/** Runtime guard for the `keyers[]` array of `GET /api/v1/switcher/{me}`. */
export function isKeyerRows(value: unknown): value is readonly KeyerRow[] {
  return Array.isArray(value) && value.every(isKeyerRow);
}
function isCost(value: unknown): value is KeyerCost {
  return isRecord(value) && typeof value.delta === 'number' && typeof value.fits === 'boolean';
}
function isRefusal(value: unknown): value is { readonly type: string; readonly detail: string } {
  return isRecord(value) && typeof value.type === 'string' && typeof value.detail === 'string';
}
function isKeyerPlan(value: unknown): value is KeyerPlan {
  return isRecord(value) && typeof value.can_take === 'boolean' && typeof value.class === 'string'
    && typeof value.keyer === 'string' && typeof value.on_air === 'boolean'
    && (value.cost === undefined || value.cost === null || isCost(value.cost))
    && (value.refusal === undefined || value.refusal === null || isRefusal(value.refusal));
}
function isAccepted(value: unknown): value is KeyerAccepted {
  return isRecord(value) && typeof value.kind === 'string' && typeof value.operation_id === 'string';
}

/** Ask the server to classify a keyer verb (cost, on-air state, the refusal it would answer) before enqueuing it. */
export async function planKeyer(request: KeyerRequest, options: RequestOptions = {}): Promise<KeyerPlan> {
  const response = await fetch(apiUrl(options, '/api/v1/switcher/plan'), {
    method: 'POST', headers: buildHeaders(options, true), body: JSON.stringify(request),
  });
  if (!response.ok) throw await readPendingProblem(response);
  const body: unknown = await response.json();
  if (!isKeyerPlan(body)) throw new OperationApiError('Invalid keyer plan');
  return body;
}

/** Submit a keyer verb once; 202 is acceptance, never an on-air/applied claim. */
export async function commandKeyer(
  action: KeyerAction,
  request: KeyerRequest,
  options: RequestOptions = {},
): Promise<KeyerAccepted> {
  const requestOptions = { ...options, idempotencyKey: options.idempotencyKey ?? newIdempotencyKey() };
  const response = await fetch(apiUrl(requestOptions, `/api/v1/switcher/commands/${action}`), {
    method: 'POST', headers: buildHeaders(requestOptions, true), body: JSON.stringify(request),
  });
  if (!response.ok) throw await readPendingProblem(response);
  if (response.status !== 202) throw new OperationApiError('Keyer command was not accepted', response.status);
  const body: unknown = await response.json();
  if (!isAccepted(body)) throw new OperationApiError('Invalid keyer operation id');
  return body;
}

/** The fill source id, or `undefined` for a redacted row the caller cannot read. */
export function keyerFillSource(row: KeyerRow): string | undefined {
  return row.fill === 'redacted' ? undefined : row.fill;
}
/** The key source id, `undefined` for a self key (no key) or a redacted row. */
export function keyerKeySource(row: KeyerRow): string | undefined {
  return row.key === null || row.key === undefined || row.key === 'redacted' ? undefined : row.key;
}
/** Whether this row's fill is hidden from the caller. */
export function keyerFillRedacted(row: KeyerRow): boolean {
  return row.fill === 'redacted';
}
/** Whether this row's key is hidden from the caller. */
export function keyerKeyRedacted(row: KeyerRow): boolean {
  return row.key === 'redacted';
}
/** Whether this row is currently ramping (its `ramp` descriptor is present and well-shaped). */
export function keyerRamping(row: KeyerRow): boolean {
  return isRamp(row.ramp);
}

/**
 * Ramp progress in `[0, 1]` at `tick`, or `undefined` while not ramping. The
 * wire `ramp` is opaque in the generated schema (a free-form object), so this
 * re-validates its shape rather than trusting the generated type.
 */
export function keyerRampWeight(row: KeyerRow, tick: number): number | undefined {
  if (!isRamp(row.ramp)) return undefined;
  const ramp = row.ramp;
  const elapsed = tick - ramp.start_tick;
  const weight = ramp.rate > 0 ? Math.min(1, Math.max(0, elapsed / ramp.rate)) : (elapsed >= 0 ? 1 : 0);
  return ramp.to_on ? weight : 1 - weight;
}

/** Corner/centre/full presets for `fly`, given the canvas size and a destination size. */
export const FLY_PRESETS = ['top_left', 'top_right', 'bottom_left', 'bottom_right', 'centre', 'full'] as const;
export type FlyPreset = (typeof FLY_PRESETS)[number];

/**
 * Whether `sourceId` is a key source (a keyer's fill or key, readable to the
 * caller) that the engine tally currently reports on Program or Preview
 * (design note §9/§10: `● PGM KEY` / `▲ PVW KEY`). `undefined` when neither.
 */
export function keyTallyForSource(
  sourceId: string,
  keyers: readonly KeyerRow[] | undefined,
  tally: { readonly program: readonly string[]; readonly preview: readonly string[] } | undefined,
): 'program' | 'preview' | undefined {
  if (!keyers || !tally) return undefined;
  const isKeySource = keyers.some((row) => keyerFillSource(row) === sourceId || keyerKeySource(row) === sourceId);
  if (!isKeySource) return undefined;
  if (tally.program.includes(sourceId)) return 'program';
  if (tally.preview.includes(sourceId)) return 'preview';
  return undefined;
}

/** The `fly` rectangle a corner/centre/full preset implies for a canvas and a chosen box size. */
export function flyPresetRect(preset: FlyPreset, canvas: KeyerSize, box: KeyerSize): KeyerRect {
  if (preset === 'full') return { x: 0, y: 0, w: canvas.w, h: canvas.h };
  const w = Math.min(box.w, canvas.w);
  const h = Math.min(box.h, canvas.h);
  switch (preset) {
    case 'top_left': return { x: 0, y: 0, w, h };
    case 'top_right': return { x: canvas.w - w, y: 0, w, h };
    case 'bottom_left': return { x: 0, y: canvas.h - h, w, h };
    case 'bottom_right': return { x: canvas.w - w, y: canvas.h - h, w, h };
    case 'centre': return { x: Math.round((canvas.w - w) / 2), y: Math.round((canvas.h - h) / 2), w, h };
    default: return { x: 0, y: 0, w: canvas.w, h: canvas.h };
  }
}
