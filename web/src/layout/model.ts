// The layout editor's typed view-model and its pure geometry/validation logic.
//
// This module is the single source of truth for what the editor edits, and it
// is deliberately framework-free (no React, no konva, no dnd-kit) so it can be
// unit-tested in isolation and reused by both the canvas and the accessible
// non-canvas form. Every mutation is a pure function returning a NEW view-model;
// nothing is mutated in place.
//
// MAPPING TO THE CONFIG SCHEMA
// ----------------------------
// The control plane stores a layout as `{ id, name, body }` where `body` is an
// opaque, validated document (see the OpenAPI `Layout` schema — `body: unknown`).
// `body` mirrors the `multiview-config` document (crates/multiview-config/src/schema.rs):
// a `[canvas]` (width/height/fps), an absolute `[layout]` (`kind = "absolute"`),
// and `[[cells]]` carrying a normalized `rect` (`0..1` per axis), a stacking `z`,
// a `fit` mode, and a `source` binding (`input_id`).
//
// This editor authors the ABSOLUTE-placement subset (free-form rects); a
// loaded grid/preset document makes `fromLayoutBody` return `undefined`, and
// the page routes those bodies to the grid editor (`./gridModel.ts`) instead.
// `body` is `unknown` in the generated schema, so reading it is done with
// defensive type guards — never an unchecked `as` cast.
//
// LOSSLESSNESS: fields the editor does not render (unknown root/canvas/layout/
// cell/source keys, plus the full Cell property set parsed in `./cellProps`)
// are preserved verbatim and re-emitted on save, mirroring the resource forms'
// extra-preservation discipline.
import {
  CELL_PROPERTY_KEYS,
  emptyCellProperties,
  extraOf,
  isRecord,
  asFiniteNumber,
  asString,
  parseCellProperties,
  serializeCellProperties,
  validateCellProperties,
} from './cellProps';
import type { CellProperties, CellPropertyIssueCode } from './cellProps';

/** The five fit modes the compositor supports (config `fit`, snake_case). */
export type FitMode = 'fill' | 'contain' | 'cover' | 'none' | 'scale_down';

/** The fit modes in display order, for building a `<Select>`. */
export const FIT_MODES: readonly FitMode[] = [
  'fill',
  'contain',
  'cover',
  'none',
  'scale_down',
];

/** The output canvas geometry + cadence the cells are placed on. */
export interface CanvasModel {
  /** Canvas width in pixels (`>= 1`). */
  readonly width: number;
  /** Canvas height in pixels (`>= 1`). */
  readonly height: number;
  /** Output cadence as an exact `"num/den"` rational string (never a float). */
  readonly fps: string;
}

/**
 * A normalized rectangle, each axis a fraction of the canvas extent (`0..1`).
 * `w`/`h` are positive extents; `x`/`y` are the top-left corner.
 */
export interface NormalizedRect {
  readonly x: number;
  readonly y: number;
  readonly w: number;
  readonly h: number;
}

/** One editable cell: identity, placement, stacking, fit, and source binding. */
export interface CellModel {
  /** Stable cell id (unique within a layout). */
  readonly id: string;
  /** Operator-facing label (free text; rendered verbatim, never translated). */
  readonly label: string;
  /** Normalized placement rectangle. */
  readonly rect: NormalizedRect;
  /** Stacking order — higher draws on top. */
  readonly z: number;
  /** Rotation in degrees, clockwise, about the cell centre. */
  readonly rotation: number;
  /** Fit mode for the bound source within the cell. */
  readonly fit: FitMode;
  /** The bound source/input id, or `undefined` when the cell is unbound. */
  readonly sourceId: string | undefined;
  /** Unrendered `source` sub-fields (inline specs…), preserved verbatim. */
  readonly sourceExtra: Readonly<Record<string, unknown>>;
  /** The full Cell property set (on_loss / border / qos / appearance). */
  readonly props: CellProperties;
  /** Unrendered cell keys, preserved verbatim. */
  readonly extra: Readonly<Record<string, unknown>>;
}

/** The full editor view-model: a canvas plus its absolutely-placed cells. */
export interface LayoutModel {
  /** Stable layout id (empty string for a not-yet-saved draft). */
  readonly id: string;
  /** Human-friendly layout name. */
  readonly name: string;
  /** The output canvas. */
  readonly canvas: CanvasModel;
  /** Unrendered root body keys (incl. an authored schema_version), verbatim. */
  readonly rootExtra: Readonly<Record<string, unknown>>;
  /** Unrendered canvas keys (pixel_format, background, color…), verbatim. */
  readonly canvasExtra: Readonly<Record<string, unknown>>;
  /** Unrendered layout-record keys (beyond `kind`), verbatim. */
  readonly layoutExtra: Readonly<Record<string, unknown>>;
  /** The cells, in authoring order (NOT necessarily z-order). */
  readonly cells: readonly CellModel[];
}

/** Snap granularity (fraction of a canvas axis) for snap-to-grid. */
export const DEFAULT_SNAP = 1 / 24;

/** The smallest allowed cell extent on either axis (1% of the canvas). */
export const MIN_CELL_EXTENT = 0.01;

// --- Geometry helpers (pure, total) ----------------------------------------

/** Clamp `value` into the inclusive `[min, max]` range. */
export function clamp(value: number, min: number, max: number): number {
  if (value < min) {
    return min;
  }
  if (value > max) {
    return max;
  }
  return value;
}

/** Round `value` to the nearest multiple of `step` (`step <= 0` is identity). */
export function snap(value: number, step: number): number {
  if (step <= 0) {
    return value;
  }
  return Math.round(value / step) * step;
}

/**
 * Constrain a rectangle to the unit canvas: positive extents of at least
 * {@link MIN_CELL_EXTENT}, fully inside `0..1` on both axes. Pure + total — it
 * always returns a valid rect, so the canvas can never push a cell off-screen.
 */
export function clampRect(rect: NormalizedRect): NormalizedRect {
  const w = clamp(rect.w, MIN_CELL_EXTENT, 1);
  const h = clamp(rect.h, MIN_CELL_EXTENT, 1);
  const x = clamp(rect.x, 0, 1 - w);
  const y = clamp(rect.y, 0, 1 - h);
  return { x, y, w, h };
}

/** Normalize a rotation into the half-open `[0, 360)` degree range. */
export function normalizeRotation(degrees: number): number {
  const wrapped = degrees % 360;
  return wrapped < 0 ? wrapped + 360 : wrapped;
}

/** Whether two normalized rects overlap (touching edges do not count). */
export function rectsOverlap(a: NormalizedRect, b: NormalizedRect): boolean {
  return (
    a.x < b.x + b.w &&
    b.x < a.x + a.w &&
    a.y < b.y + b.h &&
    b.y < a.y + a.h
  );
}

// --- Cell mutations (pure; each returns a new LayoutModel) ------------------

function replaceCell(
  model: LayoutModel,
  id: string,
  update: (cell: CellModel) => CellModel,
): LayoutModel {
  // No-op when the id is absent, so callers can safely pass a stale id.
  if (!model.cells.some((cell) => cell.id === id)) {
    return model;
  }
  const cells = model.cells.map((cell) =>
    cell.id === id ? update(cell) : cell,
  );
  return { ...model, cells };
}

/** Move a cell's top-left to `(x, y)`, snapped and clamped onto the canvas. */
export function moveCell(
  model: LayoutModel,
  id: string,
  x: number,
  y: number,
  step = 0,
): LayoutModel {
  return replaceCell(model, id, (cell) => {
    const rect = clampRect({
      x: snap(x, step),
      y: snap(y, step),
      w: cell.rect.w,
      h: cell.rect.h,
    });
    return { ...cell, rect };
  });
}

/** Resize a cell to a new normalized rect, snapped and clamped. */
export function resizeCell(
  model: LayoutModel,
  id: string,
  rect: NormalizedRect,
  step = 0,
): LayoutModel {
  return replaceCell(model, id, (cell) => {
    const snapped = clampRect({
      x: snap(rect.x, step),
      y: snap(rect.y, step),
      w: snap(rect.w, step),
      h: snap(rect.h, step),
    });
    return { ...cell, rect: snapped };
  });
}

/** Set a cell's rotation (degrees, normalized into `[0, 360)`). */
export function rotateCell(
  model: LayoutModel,
  id: string,
  degrees: number,
): LayoutModel {
  return replaceCell(model, id, (cell) => ({
    ...cell,
    rotation: normalizeRotation(degrees),
  }));
}

/** Set a cell's fit mode. */
export function setCellFit(
  model: LayoutModel,
  id: string,
  fit: FitMode,
): LayoutModel {
  return replaceCell(model, id, (cell) => ({ ...cell, fit }));
}

/** Set (or clear, with `undefined`) a cell's bound source id. */
export function bindCellSource(
  model: LayoutModel,
  id: string,
  sourceId: string | undefined,
): LayoutModel {
  return replaceCell(model, id, (cell) => ({ ...cell, sourceId }));
}

/** Rename a cell's operator label. */
export function setCellLabel(
  model: LayoutModel,
  id: string,
  label: string,
): LayoutModel {
  return replaceCell(model, id, (cell) => ({ ...cell, label }));
}

/** Replace a cell's full property set (on_loss / border / qos / appearance). */
export function setCellProps(
  model: LayoutModel,
  id: string,
  props: CellProperties,
): LayoutModel {
  return replaceCell(model, id, (cell) => ({ ...cell, props }));
}

/** Set a cell's explicit `z` stacking order. */
export function setCellZ(
  model: LayoutModel,
  id: string,
  z: number,
): LayoutModel {
  return replaceCell(model, id, (cell) => ({
    ...cell,
    z: Math.round(z),
  }));
}

/** The largest `z` currently in use (or `-1` when there are no cells). */
function maxZ(cells: readonly CellModel[]): number {
  return cells.reduce((acc, cell) => Math.max(acc, cell.z), -1);
}

/**
 * Add a new cell. The id must be unique; a duplicate id is rejected by returning
 * the model unchanged. New cells stack above the current top.
 */
export function addCell(
  model: LayoutModel,
  cell: Pick<CellModel, 'id' | 'label'> & Partial<CellModel>,
): LayoutModel {
  if (model.cells.some((existing) => existing.id === cell.id)) {
    return model;
  }
  const next: CellModel = {
    id: cell.id,
    label: cell.label,
    rect: cell.rect ?? defaultRect(model.cells.length),
    z: cell.z ?? maxZ(model.cells) + 1,
    rotation: cell.rotation ?? 0,
    fit: cell.fit ?? 'contain',
    sourceId: cell.sourceId,
    sourceExtra: cell.sourceExtra ?? {},
    props: cell.props ?? emptyCellProperties(),
    extra: cell.extra ?? {},
  };
  return { ...model, cells: [...model.cells, next] };
}

/** Remove a cell by id (a no-op when it is absent). */
export function removeCell(model: LayoutModel, id: string): LayoutModel {
  const cells = model.cells.filter((cell) => cell.id !== id);
  return cells.length === model.cells.length ? model : { ...model, cells };
}

/**
 * Reorder a cell within the authoring list and renumber every cell's `z` to
 * match the new order (index 0 = bottom). This is what the accessible
 * "move up / move down" controls and the dnd-kit reorder both call, so z-order
 * stays consistent with the visible list order.
 */
export function reorderCell(
  model: LayoutModel,
  fromIndex: number,
  toIndex: number,
): LayoutModel {
  const count = model.cells.length;
  if (
    fromIndex < 0 ||
    fromIndex >= count ||
    toIndex < 0 ||
    toIndex >= count ||
    fromIndex === toIndex
  ) {
    return model;
  }
  const next = [...model.cells];
  const [moved] = next.splice(fromIndex, 1);
  if (moved === undefined) {
    return model;
  }
  next.splice(toIndex, 0, moved);
  return { ...model, cells: renumberZ(next) };
}

/** Renumber `z` to the array order (index 0 = bottom-most). */
function renumberZ(cells: readonly CellModel[]): CellModel[] {
  return cells.map((cell, index) =>
    cell.z === index ? cell : { ...cell, z: index },
  );
}

/** A reasonable default rect for the nth added cell (a cascading quarter). */
function defaultRect(index: number): NormalizedRect {
  const offset = (index % 6) * 0.05;
  return clampRect({ x: 0.1 + offset, y: 0.1 + offset, w: 0.35, h: 0.35 });
}

// --- Canvas ------------------------------------------------------------------

/** Replace the canvas geometry/cadence (pure; cells untouched). */
export function setCanvas(model: LayoutModel, canvas: CanvasModel): LayoutModel {
  return { ...model, canvas };
}

// --- Presets -----------------------------------------------------------------

/** The seedable layout presets (mirrors the config preset names). */
export type LayoutPreset = '2x2' | '3x3' | '1+5' | 'pip';

/** All presets in display order, for building the seed buttons. */
export const LAYOUT_PRESETS: readonly LayoutPreset[] = ['2x2', '3x3', '1+5', 'pip'];

/** Build a cell with the preset defaults (contain fit, no rotation/binding). */
function presetCell(id: string, rect: NormalizedRect, z: number): CellModel {
  return {
    id,
    label: id,
    rect,
    z,
    rotation: 0,
    fit: 'contain',
    sourceId: undefined,
    sourceExtra: {},
    props: emptyCellProperties(),
    extra: {},
  };
}

/** A uniform rows×cols grid of cells covering the whole canvas. */
function gridCells(rows: number, cols: number): CellModel[] {
  const cells: CellModel[] = [];
  for (let row = 0; row < rows; row += 1) {
    for (let col = 0; col < cols; col += 1) {
      cells.push(
        presetCell(
          `cell-r${String(row + 1)}c${String(col + 1)}`,
          { x: col / cols, y: row / rows, w: 1 / cols, h: 1 / rows },
          cells.length,
        ),
      );
    }
  }
  return cells;
}

/** The cells a preset seeds (pure; ids are unique within the preset). */
export function presetCells(preset: LayoutPreset): readonly CellModel[] {
  switch (preset) {
    case '2x2':
      return gridCells(2, 2);
    case '3x3':
      return gridCells(3, 3);
    case '1+5': {
      // One 2/3 hero top-left + five 1/3 satellites down the right column and
      // along the bottom row (the classic 1+5 monitor wall).
      const third = 1 / 3;
      return [
        presetCell('cell-hero', { x: 0, y: 0, w: 2 * third, h: 2 * third }, 0),
        presetCell('cell-s1', { x: 2 * third, y: 0, w: third, h: third }, 1),
        presetCell('cell-s2', { x: 2 * third, y: third, w: third, h: third }, 2),
        presetCell('cell-s3', { x: 0, y: 2 * third, w: third, h: third }, 3),
        presetCell('cell-s4', { x: third, y: 2 * third, w: third, h: third }, 4),
        presetCell('cell-s5', { x: 2 * third, y: 2 * third, w: third, h: third }, 5),
      ];
    }
    case 'pip':
      // A full-frame program with an inset picture-in-picture, bottom-right,
      // stacked above the program.
      return [
        presetCell('cell-program', { x: 0, y: 0, w: 1, h: 1 }, 0),
        presetCell('cell-pip', { x: 0.66, y: 0.66, w: 0.3, h: 0.3 }, 1),
      ];
  }
}

/** Replace the model's cells with a preset's seed (pure). */
export function applyPreset(model: LayoutModel, preset: LayoutPreset): LayoutModel {
  return { ...model, cells: [...presetCells(preset)] };
}

// --- Validation ------------------------------------------------------------

/** A single validation finding tied to a specific field (for inline display). */
export interface ValidationIssue {
  /** Dotted path of the offending field (e.g. `cells.0.rect.w`). */
  readonly path: string;
  /** A stable machine code (for tests + i18n message selection). */
  readonly code: ValidationCode;
}

/** The closed set of validation codes the editor can raise. */
export type ValidationCode =
  | 'name-empty'
  | 'canvas-dim'
  | 'fps-format'
  | 'cell-id-empty'
  | 'cell-id-duplicate'
  | 'rect-bounds'
  | 'rect-extent'
  | 'rotation-range'
  | 'no-cells'
  | CellPropertyIssueCode;

const FPS_PATTERN = /^\s*\d+\s*\/\s*[1-9]\d*\s*$/;

/** A canvas-only validation finding (the narrowed code set both editors share). */
export interface CanvasValidationIssue {
  /** Dotted path of the offending field. */
  readonly path: string;
  /** Either of the two canvas codes. */
  readonly code: 'canvas-dim' | 'fps-format';
}

/**
 * Validate just the canvas (geometry + exact-rational cadence). Shared with
 * the grid model so both editors enforce identical canvas rules.
 */
export function validateCanvas(
  canvas: CanvasModel,
): readonly CanvasValidationIssue[] {
  const issues: CanvasValidationIssue[] = [];
  if (
    !Number.isInteger(canvas.width) ||
    canvas.width < 1 ||
    !Number.isInteger(canvas.height) ||
    canvas.height < 1
  ) {
    issues.push({ path: 'canvas', code: 'canvas-dim' });
  }
  if (!FPS_PATTERN.test(canvas.fps)) {
    issues.push({ path: 'canvas.fps', code: 'fps-format' });
  }
  return issues;
}

/**
 * Validate a whole layout, returning every issue found (an empty array means
 * the layout is valid). Geometry rules mirror {@link clampRect}; the fps rule
 * mirrors `multiview-config`'s rational-string requirement (invariant #3 — never a
 * float). This runs live in the editor so geometry is checked as it is edited.
 */
export function validateLayout(model: LayoutModel): readonly ValidationIssue[] {
  const issues: ValidationIssue[] = [];
  if (model.name.trim() === '') {
    issues.push({ path: 'name', code: 'name-empty' });
  }
  issues.push(...validateCanvas(model.canvas));
  if (model.cells.length === 0) {
    issues.push({ path: 'cells', code: 'no-cells' });
  }
  const seen = new Set<string>();
  model.cells.forEach((cell, index) => {
    const base = `cells.${String(index)}`;
    if (cell.id.trim() === '') {
      issues.push({ path: `${base}.id`, code: 'cell-id-empty' });
    } else if (seen.has(cell.id)) {
      issues.push({ path: `${base}.id`, code: 'cell-id-duplicate' });
    } else {
      seen.add(cell.id);
    }
    issues.push(...validateRect(cell.rect, base));
    if (
      !Number.isFinite(cell.rotation) ||
      cell.rotation < 0 ||
      cell.rotation >= 360
    ) {
      issues.push({ path: `${base}.rotation`, code: 'rotation-range' });
    }
    // Only error-severity property issues block here: warnings (e.g. the
    // border colour Rust never validates) are advisory and the absolute
    // editor gates saving on an empty issue list.
    issues.push(
      ...validateCellProperties(cell.props, base).filter(
        (issue) => issue.severity === 'error',
      ),
    );
  });
  return issues;
}

function validateRect(
  rect: NormalizedRect,
  base: string,
): readonly ValidationIssue[] {
  const issues: ValidationIssue[] = [];
  if (rect.w < MIN_CELL_EXTENT || rect.h < MIN_CELL_EXTENT) {
    issues.push({ path: `${base}.rect`, code: 'rect-extent' });
  }
  if (
    rect.x < 0 ||
    rect.y < 0 ||
    rect.x + rect.w > 1 + 1e-6 ||
    rect.y + rect.h > 1 + 1e-6
  ) {
    issues.push({ path: `${base}.rect`, code: 'rect-bounds' });
  }
  return issues;
}

/** Whether the layout has no validation issues. */
export function isLayoutValid(model: LayoutModel): boolean {
  return validateLayout(model).length === 0;
}

// --- Mapping to/from the opaque config `body` ------------------------------

/** Narrow an unknown value to a plain record without an unsafe assertion. */
function asRecord(value: unknown): Record<string, unknown> | undefined {
  return isRecord(value) ? value : undefined;
}

function asFit(value: unknown): FitMode {
  return FIT_MODES.find((mode) => mode === value) ?? 'contain';
}

function rectFrom(value: unknown): NormalizedRect | undefined {
  const record = asRecord(value);
  if (record === undefined) {
    return undefined;
  }
  const x = asFiniteNumber(record.x);
  const y = asFiniteNumber(record.y);
  const w = asFiniteNumber(record.w);
  const h = asFiniteNumber(record.h);
  if (x === undefined || y === undefined || w === undefined || h === undefined) {
    return undefined;
  }
  return clampRect({ x, y, w, h });
}

/** The cell keys the absolute editor manages; the rest is preserved extra. */
const ABSOLUTE_CELL_KEYS: readonly string[] = [
  'id',
  'label',
  'rect',
  'z',
  'rotation',
  'fit',
  'source',
  ...CELL_PROPERTY_KEYS,
];

/** The root body keys composed by the editors (the rest rides `rootExtra`). */
const ROOT_KEYS: readonly string[] = ['canvas', 'layout', 'cells'];

/** The canvas keys both editors render (the rest rides `canvasExtra`). */
const CANVAS_KEYS: readonly string[] = ['width', 'height', 'fps'];

/**
 * Parse one persisted cell record into a {@link CellModel} — used by the
 * absolute editor's body mapping and by grid→free-form conversion. Returns
 * `undefined` when the record lacks an id or a parsable absolute `rect`.
 */
export function cellModelFromRecord(
  record: Readonly<Record<string, unknown>>,
  fallbackZ: number,
): CellModel | undefined {
  const cellId = asString(record.id);
  const rect = rectFrom(record.rect);
  if (cellId === undefined || rect === undefined) {
    return undefined;
  }
  const source = asRecord(record.source);
  return {
    id: cellId,
    label: asString(record.label) ?? cellId,
    rect,
    z: asFiniteNumber(record.z) ?? fallbackZ,
    rotation: normalizeRotation(asFiniteNumber(record.rotation) ?? 0),
    fit: asFit(record.fit),
    sourceId: source !== undefined ? asString(source.input_id) : undefined,
    sourceExtra: source !== undefined ? extraOf(source, ['input_id']) : {},
    props: parseCellProperties(record),
    extra: extraOf(record, ABSOLUTE_CELL_KEYS),
  };
}

/** Parse a body's canvas record into the shared canvas model + its extras. */
export function canvasFromBody(root: Readonly<Record<string, unknown>>): {
  readonly canvas: CanvasModel;
  readonly canvasExtra: Readonly<Record<string, unknown>>;
} {
  const record = asRecord(root.canvas) ?? {};
  return {
    canvas: {
      width: asFiniteNumber(record.width) ?? 1920,
      height: asFiniteNumber(record.height) ?? 1080,
      fps: asString(record.fps) ?? '30/1',
    },
    canvasExtra: extraOf(record, CANVAS_KEYS),
  };
}

/**
 * Build a view-model from a persisted layout `{ id, name, body }`. Returns
 * `undefined` when `body` is not an absolute-placement document this editor
 * can round-trip — the page routes grid/preset bodies to the grid editor.
 */
export function fromLayoutBody(
  id: string,
  name: string,
  body: unknown,
): LayoutModel | undefined {
  const root = asRecord(body);
  if (root === undefined) {
    return undefined;
  }
  const layout = asRecord(root.layout);
  // Only the absolute placement kind is editable here.
  if (layout !== undefined && asString(layout.kind) !== 'absolute') {
    return undefined;
  }
  const { canvas, canvasExtra } = canvasFromBody(root);
  const rawCells = Array.isArray(root.cells) ? root.cells : [];
  const cells: CellModel[] = [];
  for (const raw of rawCells) {
    const record = asRecord(raw);
    if (record === undefined) {
      continue;
    }
    // A cell without an id or an absolute rect is outside the editable subset.
    const cell = cellModelFromRecord(record, cells.length);
    if (cell === undefined) {
      return undefined;
    }
    cells.push(cell);
  }
  return {
    id,
    name,
    canvas,
    rootExtra: extraOf(root, ROOT_KEYS),
    canvasExtra,
    layoutExtra: layout !== undefined ? extraOf(layout, ['kind']) : {},
    cells,
  };
}

/**
 * Serialize a view-model back to the opaque config `body` (canonical JSON). The
 * shape matches `multiview-config`'s document so the engine validates it on apply.
 * `z` is renumbered from the authoring order so list order and stacking agree.
 * Preserved extras (root/canvas/layout/cell/source + unrendered cell
 * properties) are re-emitted verbatim; an authored `schema_version` riding
 * `rootExtra` overrides the default `1` via spread order.
 */
export function toLayoutBody(model: LayoutModel): Record<string, unknown> {
  const cells = renumberZ(model.cells).map((cell) => {
    const source: Record<string, unknown> = {
      ...cell.sourceExtra,
      ...(cell.sourceId !== undefined ? { input_id: cell.sourceId } : {}),
    };
    return {
      id: cell.id,
      label: cell.label,
      rect: {
        x: cell.rect.x,
        y: cell.rect.y,
        w: cell.rect.w,
        h: cell.rect.h,
      },
      z: cell.z,
      rotation: cell.rotation,
      fit: cell.fit,
      ...serializeCellProperties(cell.props),
      source,
      ...cell.extra,
    };
  });
  return {
    schema_version: 1,
    ...model.rootExtra,
    canvas: {
      width: model.canvas.width,
      height: model.canvas.height,
      fps: model.canvas.fps,
      ...model.canvasExtra,
    },
    layout: { kind: 'absolute', ...model.layoutExtra },
    cells,
  };
}

/** An empty draft layout (one canvas, no cells) for the "new layout" flow. */
export function emptyLayout(name = ''): LayoutModel {
  return {
    id: '',
    name,
    canvas: { width: 1920, height: 1080, fps: '30/1' },
    rootExtra: {},
    canvasExtra: {},
    layoutExtra: {},
    cells: [],
  };
}
