// The pure grid-layout view-model: the editable form of a `kind = "grid"`
// config body (CSS-grid tracks + `grid-template-areas` + area-bound cells).
//
// Framework-free, like ./model.ts. Three responsibilities:
//   1. LOSSLESS body <-> model mapping (unknown root/canvas/layout/cell/source
//      keys preserved verbatim; non-area cells under a grid layout ride through
//      untouched, in their original order).
//   2. A solver mirroring crates/multiview-config/src/grid.rs EXACTLY: px/%
//      tracks claim space first, the remainder after fixed gaps is shared
//      between fr tracks, and every named area (including ".") must be a
//      contiguous rectangle of matrix cells.
//   3. Pure editing operations (tracks, gaps, area-matrix assignment/rename,
//      per-area cell bindings) and validation mirroring the Rust rules.
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
import { canvasFromBody, cellModelFromRecord, presetCells, validateCanvas } from './model';
import type {
  CanvasModel,
  CellModel,
  FitMode,
  LayoutModel,
  LayoutPreset,
  NormalizedRect,
} from './model';
import { FIT_MODES } from './model';

// --- Body classification -----------------------------------------------------

/** The placement strategies a stored body can declare. */
export type LayoutBodyKind = 'absolute' | 'grid' | 'preset';

/** Narrow an unknown value to a plain record without an unsafe assertion. */
function asRecord(value: unknown): Record<string, unknown> | undefined {
  return isRecord(value) ? value : undefined;
}

/**
 * Classify a stored layout body by its `layout.kind`. A body without a layout
 * record is the legacy absolute shape; an unknown kind returns `undefined`.
 */
export function layoutBodyKind(body: unknown): LayoutBodyKind | undefined {
  const root = asRecord(body);
  if (root === undefined) {
    return undefined;
  }
  const layout = asRecord(root.layout);
  if (layout === undefined) {
    return 'absolute';
  }
  const kind = asString(layout.kind);
  if (kind === 'absolute' || kind === 'grid' || kind === 'preset') {
    return kind;
  }
  return undefined;
}

/** The preset name of a `kind = "preset"` body, if that is what it is. */
export function presetNameOf(body: unknown): string | undefined {
  const root = asRecord(body);
  const layout = root === undefined ? undefined : asRecord(root.layout);
  if (layout === undefined || asString(layout.kind) !== 'preset') {
    return undefined;
  }
  return asString(layout.preset);
}

// --- The model -----------------------------------------------------------------

/** One editable area-bound cell (`Cell.area` placement). */
export interface GridCellModel {
  /** Stable cell id (unique within the layout). */
  readonly id: string;
  /** The grid area this cell renders into. */
  readonly area: string;
  /** Stacking order, or `undefined` when the key is absent from the body. */
  readonly z: number | undefined;
  /** Fit mode, or `undefined` when absent (engine default: contain). */
  readonly fit: FitMode | undefined;
  /** The bound source/input id, or `undefined` when unbound. */
  readonly sourceId: string | undefined;
  /** Unrendered `source` sub-fields, preserved verbatim. */
  readonly sourceExtra: Readonly<Record<string, unknown>>;
  /**
   * Whether the stored record carried a `source` key at all. Tracked so an
   * absent key stays absent on save (no invented `{}`) while a present empty
   * record is kept — both shapes round-trip exactly.
   */
  readonly sourcePresent: boolean;
  /**
   * Whether the record ALSO declares a `rect` (kept verbatim in `extra`).
   * Rust rejects a cell with both `area` and `rect`, so this raises an
   * error-severity issue until the operator resolves it.
   */
  readonly hasRect: boolean;
  /** The full Cell property set (on_loss / border / qos / appearance). */
  readonly props: CellProperties;
  /** Unrendered cell keys, preserved verbatim. */
  readonly extra: Readonly<Record<string, unknown>>;
}

/**
 * One entry of the body's `cells` array, in original order: an editable
 * area-bound cell, or a verbatim pass-through record (a rect-placed cell under
 * a grid layout — legal in the schema, edited in the free-form editor only).
 */
export type GridCellEntry =
  | { readonly kind: 'cell'; readonly cell: GridCellModel }
  | { readonly kind: 'raw'; readonly record: Readonly<Record<string, unknown>> };

/** The full grid editor view-model. */
export interface GridModel {
  /** Stable layout id (empty string for an unsaved draft). */
  readonly id: string;
  /** Human-friendly layout name. */
  readonly name: string;
  /** The output canvas (shared shape with the absolute editor). */
  readonly canvas: CanvasModel;
  /** Unrendered root body keys, verbatim. */
  readonly rootExtra: Readonly<Record<string, unknown>>;
  /** Unrendered canvas keys, verbatim. */
  readonly canvasExtra: Readonly<Record<string, unknown>>;
  /** Unrendered layout-record keys (beyond the grid fields), verbatim. */
  readonly layoutExtra: Readonly<Record<string, unknown>>;
  /** Column tracks, left to right (e.g. `"1fr"`, `"200px"`, `"25%"`). */
  readonly columns: readonly string[];
  /** Row tracks, top to bottom. */
  readonly rows: readonly string[];
  /** Uniform gap in pixels, or `undefined` when the key is absent (= 0). */
  readonly gap: number | undefined;
  /** Row-gap override in pixels, or `undefined` when absent. */
  readonly rowGap: number | undefined;
  /** Column-gap override in pixels, or `undefined` when absent. */
  readonly columnGap: number | undefined;
  /** The area map, rows × columns of area-name tokens. */
  readonly areaMatrix: readonly (readonly string[])[];
  /**
   * Whether the stored `areas` map was ragged (rows padded with `"."` or
   * excess tokens dropped) and had to be normalized on load — meaning even a
   * no-op save rewrites the stored body. Surfaced as an advisory issue.
   */
  readonly areasNormalized: boolean;
  /** The body's cells, in original order (editable + pass-through). */
  readonly cells: readonly GridCellEntry[];
}

/** The editable (area-bound) cells, in order. */
export function editableCells(model: GridModel): readonly GridCellModel[] {
  const cells: GridCellModel[] = [];
  for (const entry of model.cells) {
    if (entry.kind === 'cell') {
      cells.push(entry.cell);
    }
  }
  return cells;
}

/** The cell bound to `area`, if any (the first, when duplicates exist). */
export function cellForArea(
  model: GridModel,
  area: string,
): GridCellModel | undefined {
  return editableCells(model).find((cell) => cell.area === area);
}

/** The distinct area names in first-seen (reading) order — Rust solver order. */
export function areaNames(model: GridModel): readonly string[] {
  const names: string[] = [];
  for (const row of model.areaMatrix) {
    for (const name of row) {
      if (!names.includes(name)) {
        names.push(name);
      }
    }
  }
  return names;
}

/**
 * A deterministic decorative hue for an area name. Identity is always the
 * NAME TEXT shown in every cell; the colour is a secondary cue (WCAG: never
 * colour alone).
 */
export function areaHue(name: string): number {
  let hash = 0;
  for (let index = 0; index < name.length; index += 1) {
    hash = (hash * 31 + name.charCodeAt(index)) % 360;
  }
  return hash;
}

// --- Track parsing (Track::from_str parity) -----------------------------------

/** A parsed grid track: a flex factor, fixed pixels, or a canvas percentage. */
export interface ParsedTrack {
  /** The track unit. */
  readonly unit: 'fr' | 'px' | '%';
  /** The finite, non-negative numeric part. */
  readonly value: number;
}

/**
 * Parse a CSS-grid track string (`"<n>fr"` / `"<n>px"` / `"<n>%"`), mirroring
 * the Rust `Track::from_str`: trimmed, finite, non-negative; anything else is
 * `undefined`.
 */
/**
 * The decimal forms Rust's `f64::from_str` and JS agree on: `1`, `1.5`, `.5`,
 * `5.`, `1e2` (± signs allowed). Notably EXCLUDES the JS-only `Number()`
 * extras Rust rejects — hex (`0x10`), octal/binary prefixes, and `Infinity`.
 */
const NUMERIC_FORM = /^[+-]?(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?$/;

export function parseTrack(value: string): ParsedTrack | undefined {
  const trimmed = value.trim();
  const tryUnit = (suffix: 'fr' | 'px' | '%'): ParsedTrack | undefined => {
    if (!trimmed.endsWith(suffix)) {
      return undefined;
    }
    const raw = trimmed.slice(0, trimmed.length - suffix.length).trim();
    if (!NUMERIC_FORM.test(raw)) {
      return undefined;
    }
    const num = Number(raw);
    if (!Number.isFinite(num) || num < 0) {
      return undefined;
    }
    return { unit: suffix, value: num };
  };
  return tryUnit('fr') ?? tryUnit('px') ?? tryUnit('%');
}

// --- Body <-> model mapping ------------------------------------------------------

/** The grid layout-record keys this editor manages. */
const GRID_LAYOUT_KEYS: readonly string[] = [
  'kind',
  'columns',
  'rows',
  'gap',
  'row_gap',
  'column_gap',
  'areas',
];

/** The cell keys the grid editor manages; the rest is preserved extra. */
const GRID_CELL_KEYS: readonly string[] = [
  'id',
  'area',
  'z',
  'fit',
  'source',
  ...CELL_PROPERTY_KEYS,
];

const ROOT_KEYS: readonly string[] = ['canvas', 'layout', 'cells'];

function asStringArray(value: unknown): readonly string[] | undefined {
  if (!Array.isArray(value)) {
    return undefined;
  }
  const out: string[] = [];
  for (const item of value) {
    const str = asString(item);
    if (str === undefined) {
      return undefined;
    }
    out.push(str);
  }
  return out;
}

function asFitOrUndefined(value: unknown): FitMode | undefined {
  return FIT_MODES.find((mode) => mode === value);
}

/**
 * Build the area matrix from the body's `areas` rows. The matrix is
 * normalized to `rows` × `columns`: missing tokens pad with `"."` and excess
 * tokens are dropped, so a ragged (Rust-invalid) map still loads for repair.
 */
function matrixFromAreas(
  areas: readonly string[],
  columnCount: number,
  rowCount: number,
): { readonly matrix: readonly (readonly string[])[]; readonly normalized: boolean } {
  const matrix: string[][] = [];
  let normalized = areas.length !== rowCount;
  for (let rowIndex = 0; rowIndex < rowCount; rowIndex += 1) {
    const tokens = (areas[rowIndex] ?? '').trim().split(/\s+/).filter((t) => t !== '');
    if (tokens.length !== columnCount) {
      normalized = true;
    }
    const row: string[] = [];
    for (let colIndex = 0; colIndex < columnCount; colIndex += 1) {
      row.push(tokens[colIndex] ?? '.');
    }
    matrix.push(row);
  }
  return { matrix, normalized };
}

function parseGridCellEntry(raw: unknown): GridCellEntry | undefined {
  const record = asRecord(raw);
  if (record === undefined) {
    return undefined;
  }
  const id = asString(record.id);
  const area = asString(record.area);
  if (id === undefined || area === undefined) {
    // Not an area-bound cell (rect placement or malformed): pass through
    // verbatim so the body round-trips losslessly.
    return { kind: 'raw', record };
  }
  const sourcePresent = 'source' in record;
  const source = asRecord(record.source);
  if (sourcePresent && source === undefined) {
    // A non-record `source` is out of schema; pass the whole cell through
    // verbatim rather than model (and rewrite) what we cannot represent.
    return { kind: 'raw', record };
  }
  return {
    kind: 'cell',
    cell: {
      id,
      area,
      z: asFiniteNumber(record.z),
      fit: asFitOrUndefined(record.fit),
      sourceId: source !== undefined ? asString(source.input_id) : undefined,
      sourceExtra: source !== undefined ? extraOf(source, ['input_id']) : {},
      sourcePresent,
      hasRect: 'rect' in record,
      props: parseCellProperties(record),
      extra: extraOf(record, GRID_CELL_KEYS),
    },
  };
}

/**
 * Build a grid view-model from a persisted layout `{ id, name, body }`.
 * Returns `undefined` when the body is not a `kind = "grid"` document with
 * string track/area lists (the page then shows an honest parse error — never
 * a read-only refusal).
 */
export function fromGridLayoutBody(
  id: string,
  name: string,
  body: unknown,
): GridModel | undefined {
  const root = asRecord(body);
  if (root === undefined) {
    return undefined;
  }
  const layout = asRecord(root.layout);
  if (layout === undefined || asString(layout.kind) !== 'grid') {
    return undefined;
  }
  const columns = asStringArray(layout.columns);
  const rows = asStringArray(layout.rows);
  const areas = asStringArray(layout.areas);
  if (columns === undefined || rows === undefined || areas === undefined) {
    return undefined;
  }
  const cellsRaw = Array.isArray(root.cells) ? root.cells : [];
  const cells: GridCellEntry[] = [];
  for (const raw of cellsRaw) {
    const entry = parseGridCellEntry(raw);
    if (entry !== undefined) {
      cells.push(entry);
    }
  }
  const { canvas, canvasExtra } = canvasFromBody(root);
  const { matrix, normalized } = matrixFromAreas(areas, columns.length, rows.length);
  return {
    id,
    name,
    canvas,
    rootExtra: extraOf(root, ROOT_KEYS),
    canvasExtra,
    layoutExtra: extraOf(layout, GRID_LAYOUT_KEYS),
    columns,
    rows,
    gap: asFiniteNumber(layout.gap),
    rowGap: asFiniteNumber(layout.row_gap),
    columnGap: asFiniteNumber(layout.column_gap),
    areaMatrix: matrix,
    areasNormalized: normalized,
    cells,
  };
}

function serializeGridCell(cell: GridCellModel): Record<string, unknown> {
  // `source` is emitted only when the stored record carried the key or the
  // editor bound something — an absent key stays absent (no invented `{}`).
  const emitSource =
    cell.sourcePresent ||
    cell.sourceId !== undefined ||
    Object.keys(cell.sourceExtra).length > 0;
  return {
    id: cell.id,
    area: cell.area,
    ...(cell.z !== undefined ? { z: cell.z } : {}),
    ...(cell.fit !== undefined ? { fit: cell.fit } : {}),
    ...serializeCellProperties(cell.props),
    ...(emitSource
      ? {
          source: {
            ...cell.sourceExtra,
            ...(cell.sourceId !== undefined ? { input_id: cell.sourceId } : {}),
          },
        }
      : {}),
    ...cell.extra,
  };
}

/**
 * Serialize the model back to the opaque config `body`. A no-op edit
 * round-trips deep-equal: absent optional keys stay absent, extras ride back
 * verbatim, and pass-through cells re-emit at their original positions.
 */
export function toGridLayoutBody(model: GridModel): Record<string, unknown> {
  return {
    canvas: {
      width: model.canvas.width,
      height: model.canvas.height,
      fps: model.canvas.fps,
      ...model.canvasExtra,
    },
    layout: {
      kind: 'grid',
      columns: [...model.columns],
      rows: [...model.rows],
      ...(model.gap !== undefined ? { gap: model.gap } : {}),
      ...(model.rowGap !== undefined ? { row_gap: model.rowGap } : {}),
      ...(model.columnGap !== undefined ? { column_gap: model.columnGap } : {}),
      areas: model.areaMatrix.map((row) => row.join(' ')),
      ...model.layoutExtra,
    },
    cells: model.cells.map((entry) =>
      entry.kind === 'cell' ? serializeGridCell(entry.cell) : { ...entry.record },
    ),
    ...model.rootExtra,
  };
}

// --- The solver (crates/multiview-config/src/grid.rs parity) ---------------------

/** Per-track pixel offsets/sizes on one axis (gaps folded into offsets). */
interface AxisLayout {
  readonly offsets: readonly number[];
  readonly sizes: readonly number[];
}

function layOutAxis(
  tracks: readonly ParsedTrack[],
  extentPx: number,
  gapPx: number,
): AxisLayout {
  const gapCount = Math.max(0, tracks.length - 1);
  const totalGap = gapPx * gapCount;
  let fixedTotal = 0;
  let frTotal = 0;
  for (const track of tracks) {
    if (track.unit === 'px') {
      fixedTotal += track.value;
    } else if (track.unit === '%') {
      fixedTotal += extentPx * (track.value / 100);
    } else {
      frTotal += track.value;
    }
  }
  const free = Math.max(0, extentPx - totalGap - fixedTotal);
  const sizes = tracks.map((track) => {
    if (track.unit === 'px') {
      return track.value;
    }
    if (track.unit === '%') {
      return extentPx * (track.value / 100);
    }
    return frTotal > 0 ? free * (track.value / frTotal) : 0;
  });
  const offsets: number[] = [];
  let cursor = 0;
  sizes.forEach((size, index) => {
    if (index > 0) {
      cursor += gapPx;
    }
    offsets.push(cursor);
    cursor += size;
  });
  return { offsets, sizes };
}

/** The inclusive track-index bounding box of one named area. */
interface AreaBox {
  colStart: number;
  colEnd: number;
  rowStart: number;
  rowEnd: number;
  cellCount: number;
}

/**
 * Bounding boxes per area name, in first-seen order — like the Rust
 * `parse_area_map`, every name (including `"."`) is a regular area.
 */
function areaBoxes(
  matrix: readonly (readonly string[])[],
): ReadonlyMap<string, AreaBox> {
  const boxes = new Map<string, AreaBox>();
  matrix.forEach((row, rowIndex) => {
    row.forEach((name, colIndex) => {
      const existing = boxes.get(name);
      if (existing === undefined) {
        boxes.set(name, {
          colStart: colIndex,
          colEnd: colIndex,
          rowStart: rowIndex,
          rowEnd: rowIndex,
          cellCount: 1,
        });
      } else {
        existing.colStart = Math.min(existing.colStart, colIndex);
        existing.colEnd = Math.max(existing.colEnd, colIndex);
        existing.rowStart = Math.min(existing.rowStart, rowIndex);
        existing.rowEnd = Math.max(existing.rowEnd, rowIndex);
        existing.cellCount += 1;
      }
    });
  });
  return boxes;
}

/** The names whose cells do NOT fill their bounding box (non-rectangles). */
function nonRectangularAreas(
  matrix: readonly (readonly string[])[],
): readonly string[] {
  const broken: string[] = [];
  for (const [name, box] of areaBoxes(matrix)) {
    const expected =
      (box.colEnd - box.colStart + 1) * (box.rowEnd - box.rowStart + 1);
    if (box.cellCount !== expected) {
      broken.push(name);
    }
  }
  return broken;
}

/**
 * Solve the grid into one normalized rect per area, mirroring the Rust solver
 * exactly: px/% claim space first, fr tracks share the post-gap remainder, a
 * spanning area covers its tracks plus interior gaps. Returns `undefined`
 * when the grid is unsolvable (empty/invalid tracks, negative or fractional
 * gaps, a zero canvas, or a non-rectangular area) — exactly the inputs the
 * Rust solver rejects.
 */
export function solveGridToRects(
  model: GridModel,
): ReadonlyMap<string, NormalizedRect> | undefined {
  const width = model.canvas.width;
  const height = model.canvas.height;
  if (!Number.isFinite(width) || width <= 0 || !Number.isFinite(height) || height <= 0) {
    return undefined;
  }
  if (model.columns.length === 0 || model.rows.length === 0) {
    return undefined;
  }
  const columns: ParsedTrack[] = [];
  for (const track of model.columns) {
    const parsed = parseTrack(track);
    if (parsed === undefined) {
      return undefined;
    }
    columns.push(parsed);
  }
  const rows: ParsedTrack[] = [];
  for (const track of model.rows) {
    const parsed = parseTrack(track);
    if (parsed === undefined) {
      return undefined;
    }
    rows.push(parsed);
  }
  const columnGap = model.columnGap ?? model.gap ?? 0;
  const rowGap = model.rowGap ?? model.gap ?? 0;
  // Gaps are u32 pixels in the schema: negative/fractional gaps are invalid.
  if (!Number.isInteger(columnGap) || columnGap < 0 || !Number.isInteger(rowGap) || rowGap < 0) {
    return undefined;
  }
  if (nonRectangularAreas(model.areaMatrix).length > 0) {
    return undefined;
  }
  const colLayout = layOutAxis(columns, width, columnGap);
  const rowLayout = layOutAxis(rows, height, rowGap);
  const rects = new Map<string, NormalizedRect>();
  for (const [name, box] of areaBoxes(model.areaMatrix)) {
    const x0 = colLayout.offsets[box.colStart];
    const xEndOffset = colLayout.offsets[box.colEnd];
    const xEndSize = colLayout.sizes[box.colEnd];
    const y0 = rowLayout.offsets[box.rowStart];
    const yEndOffset = rowLayout.offsets[box.rowEnd];
    const yEndSize = rowLayout.sizes[box.rowEnd];
    if (
      x0 === undefined ||
      xEndOffset === undefined ||
      xEndSize === undefined ||
      y0 === undefined ||
      yEndOffset === undefined ||
      yEndSize === undefined
    ) {
      return undefined;
    }
    rects.set(name, {
      x: x0 / width,
      y: y0 / height,
      w: (xEndOffset + xEndSize - x0) / width,
      h: (yEndOffset + yEndSize - y0) / height,
    });
  }
  return rects;
}

// --- Editing operations (pure) -----------------------------------------------------

/** Replace the layout name. */
export function setGridName(model: GridModel, name: string): GridModel {
  return { ...model, name };
}

/** Replace the canvas geometry/cadence. */
export function setGridCanvas(model: GridModel, canvas: CanvasModel): GridModel {
  return { ...model, canvas };
}

/** Set (or clear, with `undefined`) the uniform gap. */
export function setGap(model: GridModel, gap: number | undefined): GridModel {
  return { ...model, gap };
}

/** Set (or clear) the row-gap override. */
export function setRowGap(model: GridModel, rowGap: number | undefined): GridModel {
  return { ...model, rowGap };
}

/** Set (or clear) the column-gap override. */
export function setColumnGap(
  model: GridModel,
  columnGap: number | undefined,
): GridModel {
  return { ...model, columnGap };
}

/** The two track axes. */
export type TrackAxis = 'columns' | 'rows';

/** Replace one track value (no-op for an out-of-range index). */
export function setTrack(
  model: GridModel,
  axis: TrackAxis,
  index: number,
  value: string,
): GridModel {
  const tracks = model[axis];
  if (index < 0 || index >= tracks.length) {
    return model;
  }
  const next = tracks.map((track, i) => (i === index ? value : track));
  return axis === 'columns' ? { ...model, columns: next } : { ...model, rows: next };
}

/**
 * Append a `1fr` track. The matrix extends by duplicating the trailing edge
 * (the last column's / last row's area names), which keeps every area a
 * rectangle — the new track simply widens the areas touching that edge.
 */
export function addTrack(model: GridModel, axis: TrackAxis): GridModel {
  if (axis === 'columns') {
    const matrix = model.areaMatrix.map((row) => [
      ...row,
      row[row.length - 1] ?? '.',
    ]);
    return { ...model, columns: [...model.columns, '1fr'], areaMatrix: matrix };
  }
  const lastRow = model.areaMatrix[model.areaMatrix.length - 1];
  const newRow =
    lastRow !== undefined
      ? [...lastRow]
      : model.columns.map((): string => '.').concat();
  return {
    ...model,
    rows: [...model.rows, '1fr'],
    areaMatrix: [...model.areaMatrix, newRow],
  };
}

/**
 * Remove a track and its matrix column/row. Refuses to remove the last track
 * on an axis. Removing a full column/row keeps every remaining area
 * contiguous; an area that lived only there disappears (its cell is then
 * flagged `cell-area-unknown` by validation, never silently deleted).
 */
export function removeTrack(
  model: GridModel,
  axis: TrackAxis,
  index: number,
): GridModel {
  const tracks = model[axis];
  if (tracks.length <= 1 || index < 0 || index >= tracks.length) {
    return model;
  }
  const next = tracks.filter((_, i) => i !== index);
  if (axis === 'columns') {
    const matrix = model.areaMatrix.map((row) => row.filter((_, i) => i !== index));
    return { ...model, columns: next, areaMatrix: matrix };
  }
  const matrix = model.areaMatrix.filter((_, i) => i !== index);
  return { ...model, rows: next, areaMatrix: matrix };
}

/** An inclusive rectangle of matrix cells (track indices). */
export interface MatrixSelection {
  readonly top: number;
  readonly left: number;
  readonly bottom: number;
  readonly right: number;
}

/** The outcome of an area assignment/rename. */
export type AssignResult =
  | { readonly ok: true; readonly model: GridModel }
  | {
      readonly ok: false;
      readonly code: 'name-invalid' | 'breaks-areas';
      readonly areas: readonly string[];
    };

/** A single non-whitespace token (a valid `grid-template-areas` name). */
function isValidAreaName(name: string): boolean {
  return name !== '' && !/\s/.test(name);
}

/**
 * Assign `name` to the selected rectangle. Rejected (with the broken names)
 * when the result would leave ANY area non-rectangular — mirroring the Rust
 * solver's contiguity rule, so the editor can never author an unsolvable map.
 */
export function assignArea(
  model: GridModel,
  selection: MatrixSelection,
  name: string,
): AssignResult {
  const trimmed = name.trim();
  if (!isValidAreaName(trimmed)) {
    return { ok: false, code: 'name-invalid', areas: [] };
  }
  const matrix = model.areaMatrix.map((row, rowIndex) =>
    row.map((token, colIndex) =>
      rowIndex >= selection.top &&
      rowIndex <= selection.bottom &&
      colIndex >= selection.left &&
      colIndex <= selection.right
        ? trimmed
        : token,
    ),
  );
  const broken = nonRectangularAreas(matrix);
  if (broken.length > 0) {
    return { ok: false, code: 'breaks-areas', areas: broken };
  }
  return { ok: true, model: { ...model, areaMatrix: matrix } };
}

/**
 * Rename an area everywhere (matrix + bound cells). Renaming onto an existing
 * name merges the two; the merge is rejected unless the union stays a
 * rectangle.
 */
export function renameArea(
  model: GridModel,
  from: string,
  to: string,
): AssignResult {
  const trimmed = to.trim();
  if (!isValidAreaName(trimmed)) {
    return { ok: false, code: 'name-invalid', areas: [] };
  }
  if (trimmed === from) {
    return { ok: true, model };
  }
  const matrix = model.areaMatrix.map((row) =>
    row.map((token) => (token === from ? trimmed : token)),
  );
  const broken = nonRectangularAreas(matrix);
  if (broken.length > 0) {
    return { ok: false, code: 'breaks-areas', areas: broken };
  }
  const cells = model.cells.map((entry) =>
    entry.kind === 'cell' && entry.cell.area === from
      ? { kind: 'cell' as const, cell: { ...entry.cell, area: trimmed } }
      : entry,
  );
  return { ok: true, model: { ...model, areaMatrix: matrix, cells } };
}

/** Every cell id in use (editable + pass-through records). */
function usedCellIds(model: GridModel): ReadonlySet<string> {
  const ids = new Set<string>();
  for (const entry of model.cells) {
    if (entry.kind === 'cell') {
      ids.add(entry.cell.id);
    } else {
      const id = asString(entry.record.id);
      if (id !== undefined) {
        ids.add(id);
      }
    }
  }
  return ids;
}

/** A unique cell id derived from the area name (`cell_<area>`, `_2`, …). */
function freshCellId(model: GridModel, area: string): string {
  const base = `cell_${area.replace(/[^A-Za-z0-9_-]/g, '_')}`;
  const used = usedCellIds(model);
  if (!used.has(base)) {
    return base;
  }
  let suffix = 2;
  while (used.has(`${base}_${String(suffix)}`)) {
    suffix += 1;
  }
  return `${base}_${String(suffix)}`;
}

/** Add an unbound cell for `area` if none exists yet (idempotent). */
export function ensureCell(model: GridModel, area: string): GridModel {
  if (cellForArea(model, area) !== undefined) {
    return model;
  }
  const cell: GridCellModel = {
    id: freshCellId(model, area),
    area,
    z: undefined,
    fit: undefined,
    sourceId: undefined,
    sourceExtra: {},
    // The schema requires `source` on every cell, so a fresh cell emits `{}`.
    sourcePresent: true,
    hasRect: false,
    props: emptyCellProperties(),
    extra: {},
  };
  return { ...model, cells: [...model.cells, { kind: 'cell', cell }] };
}

/** Update the (first) cell bound to `area` (no-op when there is none). */
export function updateCell(
  model: GridModel,
  area: string,
  update: (cell: GridCellModel) => GridCellModel,
): GridModel {
  const index = model.cells.findIndex(
    (entry) => entry.kind === 'cell' && entry.cell.area === area,
  );
  if (index < 0) {
    return model;
  }
  const cells = model.cells.map((entry, i) =>
    i === index && entry.kind === 'cell'
      ? { kind: 'cell' as const, cell: update(entry.cell) }
      : entry,
  );
  return { ...model, cells };
}

/** Remove every cell bound to `area`. */
export function removeCellForArea(model: GridModel, area: string): GridModel {
  const cells = model.cells.filter(
    (entry) => !(entry.kind === 'cell' && entry.cell.area === area),
  );
  return cells.length === model.cells.length ? model : { ...model, cells };
}

// --- Validation -------------------------------------------------------------------

/** The grid validation codes (a superset of the property codes). */
export type GridValidationCode =
  | 'name-empty'
  | 'canvas-dim'
  | 'fps-format'
  | 'tracks-empty'
  | 'track-format'
  | 'gap-invalid'
  | 'area-not-rectangle'
  | 'grid-overflow'
  | 'areas-normalized'
  | 'cell-area-unknown'
  | 'cell-area-and-rect'
  | 'cell-id-empty'
  | 'cell-id-duplicate'
  | 'area-no-cell'
  | CellPropertyIssueCode;

/** One grid validation finding. Warnings never block saving. */
export interface GridValidationIssue {
  /** Dotted path of the offending field. */
  readonly path: string;
  /** The stable machine code. */
  readonly code: GridValidationCode;
  /** Errors block saving; warnings are advisory. */
  readonly severity: 'error' | 'warning';
}

function err(path: string, code: GridValidationCode): GridValidationIssue {
  return { path, code, severity: 'error' };
}

function gapIssue(
  value: number | undefined,
  path: string,
): GridValidationIssue | undefined {
  if (value !== undefined && (!Number.isInteger(value) || value < 0)) {
    return err(path, 'gap-invalid');
  }
  return undefined;
}

/**
 * Validate the whole grid, mirroring the Rust rules: parsable non-empty track
 * lists, non-negative integer pixel gaps, rectangular areas, every cell's
 * area present in the map (errors), plus the advisory warning that an area
 * has no cell to render into it.
 */
export function validateGrid(model: GridModel): readonly GridValidationIssue[] {
  const issues: GridValidationIssue[] = [];
  if (model.name.trim() === '') {
    issues.push(err('name', 'name-empty'));
  }
  for (const canvasIssue of validateCanvas(model.canvas)) {
    issues.push(err(canvasIssue.path, canvasIssue.code));
  }
  (['columns', 'rows'] as const).forEach((axis) => {
    const tracks = model[axis];
    if (tracks.length === 0) {
      issues.push(err(`layout.${axis}`, 'tracks-empty'));
    }
    tracks.forEach((track, index) => {
      if (parseTrack(track) === undefined) {
        issues.push(err(`layout.${axis}.${String(index)}`, 'track-format'));
      }
    });
  });
  for (const issue of [
    gapIssue(model.gap, 'layout.gap'),
    gapIssue(model.rowGap, 'layout.row_gap'),
    gapIssue(model.columnGap, 'layout.column_gap'),
  ]) {
    if (issue !== undefined) {
      issues.push(issue);
    }
  }
  for (const name of nonRectangularAreas(model.areaMatrix)) {
    issues.push(err(`areas.${name}`, 'area-not-rectangle'));
  }
  if (model.areasNormalized) {
    // The stored `areas` map was ragged and got repaired on load: even a
    // no-op save rewrites the stored body, so say so where the operator looks.
    issues.push({ path: 'layout.areas', code: 'areas-normalized', severity: 'warning' });
  }
  // The solver clamps nothing the core would accept: a solved rect past the
  // canvas (oversized px tracks, % sums over 100) fails multiview-core's
  // x+w <= 1.0 check on apply, so it must block saving here.
  const solved = solveGridToRects(model);
  if (solved !== undefined) {
    const epsilon = 1e-6;
    for (const [name, rect] of solved) {
      if (rect.x + rect.w > 1 + epsilon || rect.y + rect.h > 1 + epsilon) {
        issues.push(err(`areas.${name}`, 'grid-overflow'));
      }
    }
  }
  const names = areaNames(model);
  const seen = new Set<string>();
  const boundAreas = new Set<string>();
  editableCells(model).forEach((cell, index) => {
    const base = `cells.${String(index)}`;
    if (cell.id.trim() === '') {
      issues.push(err(`${base}.id`, 'cell-id-empty'));
    } else if (seen.has(cell.id)) {
      issues.push(err(`${base}.id`, 'cell-id-duplicate'));
    } else {
      seen.add(cell.id);
    }
    if (!names.includes(cell.area)) {
      issues.push(err(`${base}.area`, 'cell-area-unknown'));
    }
    if (cell.hasRect) {
      // Rust rejects a cell declaring both `area` and `rect` — the operator
      // must drop one before the document can apply.
      issues.push(err(`${base}.rect`, 'cell-area-and-rect'));
    }
    boundAreas.add(cell.area);
    for (const propIssue of validateCellProperties(cell.props, base)) {
      issues.push({
        path: propIssue.path,
        code: propIssue.code,
        severity: propIssue.severity,
      });
    }
  });
  for (const name of names) {
    if (!boundAreas.has(name)) {
      issues.push({ path: `areas.${name}`, code: 'area-no-cell', severity: 'warning' });
    }
  }
  return issues;
}

/** Whether the grid can be saved (no error-severity issues; warnings allowed). */
export function isGridSavable(model: GridModel): boolean {
  return !validateGrid(model).some((issue) => issue.severity === 'error');
}

// --- Preset expansion ----------------------------------------------------------------

/** The grid-expressible presets (`pip` overlaps, so it converts to free-form). */
export const GRID_PRESETS: readonly string[] = ['2x2', '3x3', '1+5'];

/** The grid tracks + area map equivalent to a named preset. */
export interface PresetGrid {
  /** Column tracks. */
  readonly columns: readonly string[];
  /** Row tracks. */
  readonly rows: readonly string[];
  /** The area map, rows × columns. */
  readonly areaMatrix: readonly (readonly string[])[];
}

/**
 * Expand a preset name into the equivalent grid. The geometry matches the
 * documented preset cells in ./model.ts exactly (gapless uniform tracks);
 * `pip` (an overlapping inset) is not expressible as a CSS grid and returns
 * `undefined` — the UI offers free-form conversion for it instead.
 */
export function expandPresetToGrid(preset: string): PresetGrid | undefined {
  switch (preset) {
    case '2x2':
      return {
        columns: ['1fr', '1fr'],
        rows: ['1fr', '1fr'],
        areaMatrix: [
          ['a', 'b'],
          ['c', 'd'],
        ],
      };
    case '3x3':
      return {
        columns: ['1fr', '1fr', '1fr'],
        rows: ['1fr', '1fr', '1fr'],
        areaMatrix: [
          ['a', 'b', 'c'],
          ['d', 'e', 'f'],
          ['g', 'h', 'i'],
        ],
      };
    case '1+5':
      return {
        columns: ['1fr', '1fr', '1fr'],
        rows: ['1fr', '1fr', '1fr'],
        areaMatrix: [
          ['hero', 'hero', 's1'],
          ['hero', 'hero', 's2'],
          ['s3', 's4', 's5'],
        ],
      };
    default:
      return undefined;
  }
}

/**
 * Convert a `kind = "preset"` body into a grid model (the explicit
 * "Convert to grid" action). Existing cells ride along (area cells editable,
 * rect cells pass through); a body with no cells seeds one unbound cell per
 * area. Returns `undefined` for a non-preset body or a non-grid preset (pip).
 */
export function presetBodyToGridModel(
  id: string,
  name: string,
  body: unknown,
): GridModel | undefined {
  const root = asRecord(body);
  const preset = presetNameOf(body);
  if (root === undefined || preset === undefined) {
    return undefined;
  }
  const expansion = expandPresetToGrid(preset);
  if (expansion === undefined) {
    return undefined;
  }
  const layout = asRecord(root.layout) ?? {};
  const cellsRaw = Array.isArray(root.cells) ? root.cells : [];
  const cells: GridCellEntry[] = [];
  for (const raw of cellsRaw) {
    const entry = parseGridCellEntry(raw);
    if (entry !== undefined) {
      cells.push(entry);
    }
  }
  const { canvas, canvasExtra } = canvasFromBody(root);
  let model: GridModel = {
    id,
    name,
    canvas,
    rootExtra: extraOf(root, ROOT_KEYS),
    canvasExtra,
    layoutExtra: extraOf(layout, ['kind', 'preset']),
    columns: expansion.columns,
    rows: expansion.rows,
    gap: undefined,
    rowGap: undefined,
    columnGap: undefined,
    areaMatrix: expansion.areaMatrix,
    areasNormalized: false,
    cells,
  };
  if (cells.length === 0) {
    for (const area of areaNames(model)) {
      model = ensureCell(model, area);
    }
  }
  return model;
}

/**
 * Convert a `kind = "preset"` body into a free-form (absolute) model — the
 * path for `pip`, whose overlapping tiles a CSS grid cannot express. A body
 * with no cells seeds the documented preset cells; existing cells must carry
 * parsable absolute rects (otherwise the conversion is refused).
 */
export function presetBodyToLayoutModel(
  id: string,
  name: string,
  body: unknown,
): LayoutModel | undefined {
  const root = asRecord(body);
  const preset = presetNameOf(body);
  if (root === undefined || preset === undefined) {
    return undefined;
  }
  const layout = asRecord(root.layout) ?? {};
  const { canvas, canvasExtra } = canvasFromBody(root);
  const cellsRaw = Array.isArray(root.cells) ? root.cells : [];
  const cells: CellModel[] = [];
  for (const raw of cellsRaw) {
    const record = asRecord(raw);
    if (record === undefined) {
      continue;
    }
    const cell = cellModelFromRecord(record, cells.length);
    if (cell === undefined) {
      return undefined;
    }
    cells.push(cell);
  }
  if (cells.length === 0) {
    const isPreset = (value: string): value is LayoutPreset =>
      value === '2x2' || value === '3x3' || value === '1+5' || value === 'pip';
    if (!isPreset(preset)) {
      return undefined;
    }
    cells.push(...presetCells(preset));
  }
  return {
    id,
    name,
    canvas,
    rootExtra: extraOf(root, ROOT_KEYS),
    canvasExtra,
    layoutExtra: extraOf(layout, ['kind', 'preset']),
    cells,
  };
}

// --- Convert to free-form ---------------------------------------------------------

/**
 * Materialize the solved grid as an absolute (free-form) layout — the
 * explicit one-way "Convert to free-form" action. Every area becomes a cell
 * with its solved rect (bound cells keep their bindings/properties; unbound
 * areas get fresh cells so the visual layout is preserved); rect-placed
 * pass-through cells convert via the absolute parser. Returns `undefined`
 * when the grid is unsolvable or a pass-through cell has no parsable rect.
 */
export function gridToLayoutModel(model: GridModel): LayoutModel | undefined {
  const rects = solveGridToRects(model);
  if (rects === undefined) {
    return undefined;
  }
  const cells: CellModel[] = [];
  const usedIds = new Set<string>();
  for (const entry of model.cells) {
    if (entry.kind === 'raw') {
      const cell = cellModelFromRecord(entry.record, cells.length);
      if (cell === undefined) {
        return undefined;
      }
      cells.push(cell);
      usedIds.add(cell.id);
      continue;
    }
    const rect = rects.get(entry.cell.area);
    if (rect === undefined) {
      // A cell bound to an area absent from the matrix cannot be placed.
      return undefined;
    }
    cells.push({
      id: entry.cell.id,
      label: entry.cell.area,
      rect,
      z: entry.cell.z ?? cells.length,
      rotation: 0,
      fit: entry.cell.fit ?? 'contain',
      sourceId: entry.cell.sourceId,
      sourceExtra: entry.cell.sourceExtra,
      props: entry.cell.props,
      // A conflicting verbatim `rect` (the area+rect case) must not ride
      // `extra` over the SOLVED managed rect in the serialized body.
      extra: extraOf(entry.cell.extra, ['rect']),
    });
    usedIds.add(entry.cell.id);
  }
  // Unbound areas still occupy canvas space: keep them visible as empty cells.
  const bound = new Set(editableCells(model).map((cell) => cell.area));
  for (const [area, rect] of rects) {
    if (bound.has(area)) {
      continue;
    }
    let id = `cell_${area.replace(/[^A-Za-z0-9_-]/g, '_')}`;
    let suffix = 2;
    while (usedIds.has(id)) {
      id = `cell_${area}_${String(suffix)}`;
      suffix += 1;
    }
    usedIds.add(id);
    cells.push({
      id,
      label: area,
      rect,
      z: cells.length,
      rotation: 0,
      fit: 'contain',
      sourceId: undefined,
      sourceExtra: {},
      props: emptyCellProperties(),
      extra: {},
    });
  }
  return {
    id: model.id,
    name: model.name,
    canvas: model.canvas,
    rootExtra: model.rootExtra,
    canvasExtra: model.canvasExtra,
    layoutExtra: model.layoutExtra,
    cells,
  };
}
