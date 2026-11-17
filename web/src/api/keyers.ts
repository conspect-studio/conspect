// Downstream keyer (DSK) control (design note docs/architecture/switcher-keyers.md
// §8/§10, K5). All four keyer verbs ride the existing switcher plan/command
// routes (ADR-0113 §8, K4): `keyer_cut`, `keyer_auto`, `keyer_tie`, `keyer_set`.
// Requests go through the generated openapi-fetch client with the generated
// request/response types (`SwitcherRequest`, `KeyerRow`, `KeyerPlan`, …) —
// never a hand-written fetch or wire type. K3 (#388) adds upstream keyer rows
// (`family = "usk"`), the `arm` verb ({background, keys}) and the `armed`
// view; K7 (#392) adds `type = "chroma"` with its `color` (DSK only).
import { t } from '@lingui/core/macro';
import type { components } from './types';
import { createApiClient } from './client';
import { BackendUnavailableError, isUnavailableStatus, newIdempotencyKey, OperationApiError } from './operations';

type Schemas = components['schemas'];
export type KeyerRow = Schemas['KeyerRow'];
export type KeyerFields = Schemas['KeyerFields'];
export type KeyerPlan = Schemas['KeyerPlan'];
export type KeyerCost = Schemas['KeyerCost'];
export type KeyerOnAir = Schemas['KeyerOnAir'];
export type KeyerRect = Schemas['KeyerRectDoc'];
export type KeyerSize = Schemas['KeyerSizeDoc'];
export type KeyerRamp = Schemas['KeyerRampView'];
export type KeyerAccepted = Schemas['SwitcherAccepted'];
export type ArmedView = Schemas['ArmedView'];
export type KeyerAction = 'keyer_cut' | 'keyer_auto' | 'keyer_tie' | 'keyer_set' | 'arm';
/** A keyer verb request: the generated `SwitcherRequest` with the action narrowed to a keyer verb. */
export type KeyerRequest = Schemas['SwitcherRequest'] & { readonly action: KeyerAction };
export type KeyerKind = 'luma' | 'linear' | 'chroma';
export type KeyerLossPolicy = 'drop_to_off_air' | 'hold_last_good';

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}
function isKeyerPlan(value: unknown): value is KeyerPlan {
  return isRecord(value) && typeof value.can_take === 'boolean' && typeof value.class === 'string'
    && typeof value.keyer === 'string' && typeof value.on_air === 'boolean';
}

/** The RFC 9457 problem of a failed call; 404/405/501 without a problem body mean the route is not deployed. */
function failure(error: unknown, response: Response, fallback: string): OperationApiError {
  const problem = isRecord(error) && typeof error.title === 'string' && typeof error.status === 'number' ? error : undefined;
  if (problem === undefined && isUnavailableStatus(response.status)) return new BackendUnavailableError(response.status);
  if (problem === undefined) return new OperationApiError(fallback, response.status);
  return new OperationApiError(
    String(problem.title), response.status,
    typeof problem.detail === 'string' ? problem.detail : undefined,
    typeof problem.type === 'string' ? problem.type : undefined,
  );
}

/** Ask the server to classify a keyer verb (class, cost, on-air state, the refusal it would answer) before enqueuing it. */
export async function planKeyer(request: KeyerRequest): Promise<KeyerPlan> {
  const { data, error, response } = await createApiClient().POST('/api/v1/switcher/plan', { body: request });
  if (!response.ok) throw failure(error, response, t`The keyer plan failed`);
  if (!isKeyerPlan(data)) throw new OperationApiError(t`The server returned an invalid keyer plan`);
  return data;
}

/** Submit a keyer verb once with an `Idempotency-Key`; 202 is acceptance, never an on-air/applied claim. */
export async function commandKeyer(
  action: KeyerAction,
  request: KeyerRequest,
  options: { readonly idempotencyKey?: string } = {},
): Promise<KeyerAccepted> {
  const { data, error, response } = await createApiClient().POST('/api/v1/switcher/commands/{kind}', {
    params: { path: { kind: action }, header: { 'Idempotency-Key': options.idempotencyKey ?? newIdempotencyKey() } },
    body: request,
  });
  if (!response.ok) throw failure(error, response, t`The keyer command failed`);
  if (response.status !== 202) throw new OperationApiError(t`The keyer command was not accepted`, response.status);
  if (!isRecord(data) || typeof data.operation_id !== 'string' || typeof data.kind !== 'string') {
    throw new OperationApiError(t`The server returned an invalid keyer operation id`);
  }
  return data;
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
/** Whether this row is an upstream keyer (K3): CUT outside a transition only, armed through `arm`, no AUTO/TIE/chroma. */
export function keyerIsUsk(row: KeyerRow): boolean {
  return row.family === 'usk';
}
/**
 * A row's identity on the desk: DSK and USK ids live in separate namespaces
 * (a DSK `key1` and a USK `key1` may coexist), so React keys and DOM ids
 * (label targets, `aria-describedby`) are namespaced by family + id.
 */
export function keyerDomKey(row: KeyerRow): string {
  return `${keyerIsUsk(row) ? 'usk' : 'dsk'}-${row.id}`;
}
/** The committed key type of a row (an unknown wire value reads as linear). */
export function keyerKind(row: KeyerRow): KeyerKind {
  return row.type === 'luma' || row.type === 'chroma' ? row.type : 'linear';
}
/** `#RGB` / `#RRGGBB` to lowercase `#rrggbb`, or `undefined` when not a hex colour. */
export function normalizeHexColor(value: string | null | undefined): string | undefined {
  if (value === null || value === undefined) return undefined;
  if (/^#[0-9a-f]{3}$/i.test(value)) return `#${value.slice(1).split('').map((c) => c + c).join('')}`.toLowerCase();
  return /^#[0-9a-f]{6}$/i.test(value) ? value.toLowerCase() : undefined;
}
/** One arm-strip toggle: BKGD or an upstream keyer id. */
export type ArmTarget = { readonly kind: 'background' } | { readonly kind: 'key'; readonly id: string };
/** An `arm` request's set: `{background, keys}` (never the view's `complete`). */
export interface ArmSet { readonly background: boolean; readonly keys: readonly string[] }
/** The `arm` set after toggling BKGD or one key. */
export function toggleArmed(armed: ArmSet, target: ArmTarget): ArmSet {
  if (target.kind === 'background') return { background: !armed.background, keys: [...armed.keys] };
  return armed.keys.includes(target.id)
    ? { background: armed.background, keys: armed.keys.filter((k) => k !== target.id) }
    : { background: armed.background, keys: [...armed.keys, target.id] };
}

/** Whether this row is currently ramping. */
export function keyerRamping(row: KeyerRow): boolean {
  return row.ramp !== null && row.ramp !== undefined;
}

/**
 * Ramp progress in `[0, 1]` at output tick `tick` (fractional when
 * extrapolated between snapshots), or `undefined` while not ramping.
 */
export function keyerRampWeight(row: KeyerRow, tick: number): number | undefined {
  const ramp = row.ramp;
  if (ramp === null || ramp === undefined) return undefined;
  const elapsed = tick - ramp.start_tick;
  const weight = ramp.rate > 0 ? Math.min(1, Math.max(0, elapsed / ramp.rate)) : (elapsed >= 0 ? 1 : 0);
  return ramp.to_on ? weight : 1 - weight;
}

/**
 * The output tick now, extrapolated from the snapshot's authoritative `tick`
 * at the exact cadence `fps_num / fps_den` for the wall time since the
 * snapshot was received (never float fps for state: only for this display).
 */
export function extrapolatedTick(
  tick: number, receivedAtMs: number, nowMs: number, fpsNum: number, fpsDen: number,
): number {
  if (fpsNum <= 0 || fpsDen <= 0) return tick;
  return tick + (Math.max(0, nowMs - receivedAtMs) / 1000) * (fpsNum / fpsDen);
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

/**
 * The `fly` rectangle a corner/centre preset implies for a canvas and a box
 * size; `full` is `null` (the backend contract: a cleared fly is the full
 * canvas, whatever its size).
 */
export function flyPresetRect(preset: FlyPreset, canvas: KeyerSize, box: KeyerSize): KeyerRect | null {
  if (preset === 'full') return null;
  const w = Math.min(box.w, canvas.w);
  const h = Math.min(box.h, canvas.h);
  switch (preset) {
    case 'top_left': return { x: 0, y: 0, w, h };
    case 'top_right': return { x: canvas.w - w, y: 0, w, h };
    case 'bottom_left': return { x: 0, y: canvas.h - h, w, h };
    case 'bottom_right': return { x: canvas.w - w, y: canvas.h - h, w, h };
    case 'centre': return { x: Math.round((canvas.w - w) / 2), y: Math.round((canvas.h - h) / 2), w, h };
    default: return null;
  }
}
