// The shared per-cell properties model — the full config `Cell` schema beyond
// placement (crates/multiview-config/src/schema.rs `Cell` +
// crates/multiview-config/src/failover.rs `FailoverSlate`).
//
// Both editors mount the same properties panel over this model: the absolute
// editor's CellsForm and the grid editor's per-area panel. The module is pure
// and framework-free so it unit-tests in isolation, and it follows the same
// lossless extra-preservation discipline as resources/forms.ts: anything the
// editor does not render (unknown keys, unknown sub-fields, even managed keys
// whose values do not parse) is re-emitted verbatim on save.

/** The failover slate tags — exactly the Rust `FailoverSlate` variants. */
export const FAILOVER_SLATES = ['bars', 'no_signal', 'black'] as const;

/** A known failover slate tag (`on_loss.slate`, snake_case). */
export type FailoverSlate = (typeof FAILOVER_SLATES)[number];

/** Scaler selections the schema documents (`Cell.scaler`). */
export const SCALER_MODES = ['auto', 'bilinear', 'lanczos'] as const;

/** A known scaler mode. */
export type ScalerMode = (typeof SCALER_MODES)[number];

/** Degradation strategies the schema documents (`CellQos.degradation`). */
export const DEGRADATION_MODES = [
  'maintain-fps',
  'maintain-resolution',
  'balanced',
] as const;

/** A known degradation strategy. */
export type DegradationMode = (typeof DEGRADATION_MODES)[number];

/**
 * The body keys this module manages on a cell record. Everything else on the
 * cell is some other concern (placement, source, id…) or an unknown extra.
 */
export const CELL_PROPERTY_KEYS: readonly string[] = [
  'align',
  'opacity',
  'corner_radius',
  'scaler',
  'visible',
  'static_friendly',
  'border',
  'qos',
  'on_loss',
];

/** The editable view of a `Border` record (`width_px`/`color`/`style`). */
export interface BorderModel {
  /** Border width in pixels, or `undefined` when the key is absent. */
  readonly widthPx: number | undefined;
  /** Border colour (hex string), or `undefined` when absent. */
  readonly color: string | undefined;
  /** Border style (e.g. `solid`), or `undefined` when absent. */
  readonly style: string | undefined;
  /** Unrendered `border` sub-fields, preserved verbatim. */
  readonly extra: Readonly<Record<string, unknown>>;
}

/** The editable view of a `CellQos` record (`priority`/`degradation`). */
export interface QosModel {
  /** Relative priority (higher is shed last), or `undefined` when absent. */
  readonly priority: number | undefined;
  /** Degradation strategy token, or `undefined` when absent. */
  readonly degradation: string | undefined;
  /** Unrendered `qos` sub-fields, preserved verbatim. */
  readonly extra: Readonly<Record<string, unknown>>;
}

/**
 * The `on_loss` failover policy. `slate` is the internally-tagged
 * discriminant; `raw` is the verbatim record re-emitted on save so a future
 * (`#[non_exhaustive]`) variant with parameters survives a round-trip.
 */
export interface OnLossModel {
  /** The `slate` tag — a {@link FailoverSlate}, or an unknown future tag. */
  readonly slate: string;
  /** The verbatim `on_loss` record (re-emitted on save). */
  readonly raw: Readonly<Record<string, unknown>>;
}

/** Every cell property beyond placement / id / source. */
export interface CellProperties {
  /** Failover slate policy, or `undefined` (omitted ⇒ engine default bars). */
  readonly onLoss: OnLossModel | undefined;
  /** Crop/letterbox anchor (e.g. `center`, `top_left`), or absent. */
  readonly align: string | undefined;
  /** Opacity `0..1` (premultiplied, linear), or absent (⇒ 1.0). */
  readonly opacity: number | undefined;
  /** Corner-radius clip in pixels, or absent. */
  readonly cornerRadius: number | undefined;
  /** Scaler selection, or absent (⇒ auto). */
  readonly scaler: string | undefined;
  /** Whether the cell renders (`false` ⇒ decode-skip), or absent (⇒ true). */
  readonly visible: boolean | undefined;
  /** Hint that the source is largely static, or absent. */
  readonly staticFriendly: boolean | undefined;
  /** Border specification, or absent. */
  readonly border: BorderModel | undefined;
  /** QoS / degradation policy, or absent. */
  readonly qos: QosModel | undefined;
  /**
   * Managed keys whose values did not parse as the schema shape (e.g. a
   * string `opacity`). Kept verbatim and re-emitted on save so the editor
   * never destroys data it cannot model.
   */
  readonly unparsed: Readonly<Record<string, unknown>>;
}

/** Fresh, all-absent cell properties (for a newly-created cell). */
export function emptyCellProperties(): CellProperties {
  return {
    onLoss: undefined,
    align: undefined,
    opacity: undefined,
    cornerRadius: undefined,
    scaler: undefined,
    visible: undefined,
    staticFriendly: undefined,
    border: undefined,
    qos: undefined,
    unparsed: {},
  };
}

// --- Shared narrowing helpers (also used by the layout models) ---------------

/** Type guard: a non-null, non-array object (a plain record). */
export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

/** A finite number, else `undefined`. */
export function asFiniteNumber(value: unknown): number | undefined {
  return typeof value === 'number' && Number.isFinite(value) ? value : undefined;
}

/** A string, else `undefined`. */
export function asString(value: unknown): string | undefined {
  return typeof value === 'string' ? value : undefined;
}

/** A boolean, else `undefined`. */
export function asBoolean(value: unknown): boolean | undefined {
  return typeof value === 'boolean' ? value : undefined;
}

/**
 * The keys of `record` NOT in `managedKeys`, preserved verbatim.
 *
 * Keys land via `Object.defineProperty`, never plain assignment: a stored body
 * can carry an OWN `__proto__` key (JSON allows it), and `extra[key] = value`
 * for that key would swap the accumulator's prototype and silently drop the
 * key. `defineProperty` always creates an own data property, and the later
 * body spreads (`{ ...extra }`) copy it back as an own data property too.
 */
export function extraOf(
  record: Readonly<Record<string, unknown>>,
  managedKeys: readonly string[],
): Readonly<Record<string, unknown>> {
  const extra: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(record)) {
    if (!managedKeys.includes(key)) {
      Object.defineProperty(extra, key, {
        value,
        enumerable: true,
        configurable: true,
        writable: true,
      });
    }
  }
  return extra;
}

/** Define `key: value` on `target` as an own data property (proto-safe). */
function defineKey(
  target: Record<string, unknown>,
  key: string,
  value: unknown,
): void {
  Object.defineProperty(target, key, {
    value,
    enumerable: true,
    configurable: true,
    writable: true,
  });
}

// --- Parse -------------------------------------------------------------------

const BORDER_KEYS: readonly string[] = ['width_px', 'color', 'style'];
const QOS_KEYS: readonly string[] = ['priority', 'degradation'];

function parseBorder(value: unknown): BorderModel | undefined {
  if (!isRecord(value)) {
    return undefined;
  }
  return {
    widthPx: asFiniteNumber(value.width_px),
    color: asString(value.color),
    style: asString(value.style),
    extra: extraOf(value, BORDER_KEYS),
  };
}

function parseQos(value: unknown): QosModel | undefined {
  if (!isRecord(value)) {
    return undefined;
  }
  return {
    priority: asFiniteNumber(value.priority),
    degradation: asString(value.degradation),
    extra: extraOf(value, QOS_KEYS),
  };
}

function parseOnLoss(value: unknown): OnLossModel | undefined {
  if (!isRecord(value)) {
    return undefined;
  }
  return { slate: asString(value.slate) ?? '', raw: value };
}

/**
 * Parse the property fields off a cell record. Managed keys whose values do
 * not parse (or sub-fields the typed parse would lose — a non-number inside a
 * border `width_px` stays inside the border's own `extra`) land in
 * {@link CellProperties.unparsed} and are re-emitted verbatim on save.
 */
export function parseCellProperties(
  record: Readonly<Record<string, unknown>>,
): CellProperties {
  const unparsed: Record<string, unknown> = {};
  const keep = (key: string, parsedOk: boolean): void => {
    if (key in record && !parsedOk) {
      defineKey(unparsed, key, record[key]);
    }
  };

  const align = asString(record.align);
  keep('align', align !== undefined);
  const opacity = asFiniteNumber(record.opacity);
  keep('opacity', opacity !== undefined);
  const cornerRadius = asFiniteNumber(record.corner_radius);
  keep('corner_radius', cornerRadius !== undefined);
  const scaler = asString(record.scaler);
  keep('scaler', scaler !== undefined);
  const visible = asBoolean(record.visible);
  keep('visible', visible !== undefined);
  const staticFriendly = asBoolean(record.static_friendly);
  keep('static_friendly', staticFriendly !== undefined);
  const border = parseBorder(record.border);
  keep('border', border !== undefined);
  const qos = parseQos(record.qos);
  keep('qos', qos !== undefined);
  const onLoss = parseOnLoss(record.on_loss);
  keep('on_loss', onLoss !== undefined);

  return {
    onLoss,
    align,
    opacity,
    cornerRadius,
    scaler,
    visible,
    staticFriendly,
    border,
    qos,
    unparsed,
  };
}

// --- Serialize ----------------------------------------------------------------

function serializeBorder(border: BorderModel): Record<string, unknown> {
  return {
    ...(border.widthPx !== undefined ? { width_px: border.widthPx } : {}),
    ...(border.color !== undefined ? { color: border.color } : {}),
    ...(border.style !== undefined ? { style: border.style } : {}),
    ...border.extra,
  };
}

function serializeQos(qos: QosModel): Record<string, unknown> {
  return {
    ...(qos.priority !== undefined ? { priority: qos.priority } : {}),
    ...(qos.degradation !== undefined ? { degradation: qos.degradation } : {}),
    ...qos.extra,
  };
}

/**
 * Serialize the properties back to their snake_case body keys, in schema
 * declaration order, skipping absent fields. Unparsed values ride back
 * verbatim under their original keys.
 */
export function serializeCellProperties(
  props: CellProperties,
): Record<string, unknown> {
  return {
    ...(props.align !== undefined ? { align: props.align } : {}),
    ...(props.opacity !== undefined ? { opacity: props.opacity } : {}),
    ...(props.cornerRadius !== undefined
      ? { corner_radius: props.cornerRadius }
      : {}),
    ...(props.scaler !== undefined ? { scaler: props.scaler } : {}),
    ...(props.visible !== undefined ? { visible: props.visible } : {}),
    ...(props.staticFriendly !== undefined
      ? { static_friendly: props.staticFriendly }
      : {}),
    ...(props.border !== undefined ? { border: serializeBorder(props.border) } : {}),
    ...(props.qos !== undefined ? { qos: serializeQos(props.qos) } : {}),
    ...(props.onLoss !== undefined ? { on_loss: { ...props.onLoss.raw } } : {}),
    ...props.unparsed,
  };
}

/**
 * Build the `on_loss` model for a known slate (the editor's selector), or
 * `undefined` to clear it (an omitted `on_loss` defaults to bars engine-side).
 */
export function onLossOf(slate: FailoverSlate | undefined): OnLossModel | undefined {
  if (slate === undefined) {
    return undefined;
  }
  return { slate, raw: { slate } };
}

// --- Validation ----------------------------------------------------------------

/** The validation codes the property panel can raise. */
export type CellPropertyIssueCode =
  | 'opacity-range'
  | 'corner-radius-invalid'
  | 'border-width-invalid'
  | 'border-color-hex'
  | 'qos-priority-int';

/** One property validation finding, tied to a dotted field path. */
export interface CellPropertyIssue {
  /** Dotted path of the offending field (e.g. `cells.0.opacity`). */
  readonly path: string;
  /** The stable machine code. */
  readonly code: CellPropertyIssueCode;
  /**
   * Errors mirror constraints the Rust schema/engine enforces (saving such a
   * document would fail on apply); warnings are advisory only — e.g. the
   * border colour, which Rust never validates, must not block saving a
   * pre-existing document.
   */
  readonly severity: 'error' | 'warning';
}

/** `#RGB` or `#RRGGBB` (mirrors the resource forms' hex validator). */
const HEX_COLOR = /^#(?:[0-9a-fA-F]{3}|[0-9a-fA-F]{6})$/;

function isNonNegativeInt(value: number): boolean {
  return Number.isInteger(value) && value >= 0;
}

/**
 * Validate the typed property fields (mirroring the config schema's types:
 * opacity `f32` in `0..=1`, `u32` pixel counts, `i64` priority, hex colour).
 */
export function validateCellProperties(
  props: CellProperties,
  base: string,
): readonly CellPropertyIssue[] {
  const issues: CellPropertyIssue[] = [];
  if (props.opacity !== undefined && (props.opacity < 0 || props.opacity > 1)) {
    issues.push({ path: `${base}.opacity`, code: 'opacity-range', severity: 'error' });
  }
  if (props.cornerRadius !== undefined && !isNonNegativeInt(props.cornerRadius)) {
    issues.push({
      path: `${base}.corner_radius`,
      code: 'corner-radius-invalid',
      severity: 'error',
    });
  }
  if (props.border !== undefined) {
    if (props.border.widthPx !== undefined && !isNonNegativeInt(props.border.widthPx)) {
      issues.push({
        path: `${base}.border.width_px`,
        code: 'border-width-invalid',
        severity: 'error',
      });
    }
    if (props.border.color !== undefined && !HEX_COLOR.test(props.border.color)) {
      // Advisory: Rust never validates Border.color, so a pre-existing
      // out-of-form colour must not make the layout unsavable.
      issues.push({
        path: `${base}.border.color`,
        code: 'border-color-hex',
        severity: 'warning',
      });
    }
  }
  if (props.qos?.priority !== undefined && !Number.isInteger(props.qos.priority)) {
    issues.push({
      path: `${base}.qos.priority`,
      code: 'qos-priority-int',
      severity: 'error',
    });
  }
  return issues;
}
