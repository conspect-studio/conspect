// Unit tests for the pure grid layout model: body <-> model round-trip
// (lossless, extras preserved), the CSS-grid solver (mirroring
// crates/multiview-config/src/grid.rs exactly: px/% claim first, fr shares the
// remainder after gaps, areas must be contiguous rectangles), area-matrix
// editing, preset expansion, and validation. No DOM, no React.
import { describe, expect, it } from 'vitest';

import {
  addTrack,
  areaNames,
  assignArea,
  cellForArea,
  editableCells,
  ensureCell,
  expandPresetToGrid,
  fromGridLayoutBody,
  gridToLayoutModel,
  layoutBodyKind,
  presetBodyToGridModel,
  presetBodyToLayoutModel,
  presetNameOf,
  removeCellForArea,
  removeTrack,
  renameArea,
  setColumnGap,
  setGap,
  setRowGap,
  setTrack,
  solveGridToRects,
  toGridLayoutBody,
  updateCell,
  validateGrid,
  isGridSavable,
  parseTrack,
} from './gridModel';
import type { GridModel } from './gridModel';
import { onLossOf } from './cellProps';
import { presetCells, toLayoutBody } from './model';

/**
 * The frigate demo working-layout body: exactly what `seed_working_layout`
 * (crates/multiview-control/src/state.rs) produces for the 2x2 grid config —
 * serde-serialized Canvas/Layout/Cells, so cells carry z/fit/on_loss/source
 * and the canvas carries pixel_format/background/color.
 */
function frigateBody(): Record<string, unknown> {
  return {
    canvas: {
      width: 1920,
      height: 1080,
      fps: '25/1',
      pixel_format: 'nv12',
      background: '#101014',
      color: { profile: 'sdr-bt709-limited' },
    },
    layout: {
      kind: 'grid',
      columns: ['1fr', '1fr'],
      rows: ['1fr', '1fr'],
      gap: 4,
      areas: ['a b', 'c d'],
    },
    cells: ['a', 'b', 'c', 'd'].map((area) => ({
      id: `cell_${area}`,
      area,
      z: 0,
      fit: 'contain',
      on_loss: { slate: 'bars' },
      source: { input_id: `in_${area}` },
    })),
  };
}

function load(body: Record<string, unknown>): GridModel {
  const model = fromGridLayoutBody('lay-1', 'Working', body);
  if (model === undefined) {
    throw new Error('expected a grid model');
  }
  return model;
}

describe('layoutBodyKind / presetNameOf', () => {
  it('classifies grid, preset and absolute bodies', () => {
    expect(layoutBodyKind(frigateBody())).toBe('grid');
    expect(layoutBodyKind({ layout: { kind: 'preset', preset: '2x2' } })).toBe(
      'preset',
    );
    expect(layoutBodyKind({ layout: { kind: 'absolute' } })).toBe('absolute');
    // A body without a layout record is the legacy absolute shape.
    expect(layoutBodyKind({ cells: [] })).toBe('absolute');
    expect(layoutBodyKind(null)).toBeUndefined();
    expect(layoutBodyKind({ layout: { kind: 'mystery' } })).toBeUndefined();
  });

  it('reads the preset name', () => {
    expect(presetNameOf({ layout: { kind: 'preset', preset: 'pip' } })).toBe('pip');
    expect(presetNameOf(frigateBody())).toBeUndefined();
  });
});

describe('body <-> model round-trip', () => {
  it('round-trips the frigate demo grid body identically (no-op edit)', () => {
    const body = frigateBody();
    const model = load(body);
    expect(model.columns).toEqual(['1fr', '1fr']);
    expect(model.rows).toEqual(['1fr', '1fr']);
    expect(model.gap).toBe(4);
    expect(model.rowGap).toBeUndefined();
    expect(model.columnGap).toBeUndefined();
    expect(model.areaMatrix).toEqual([
      ['a', 'b'],
      ['c', 'd'],
    ]);
    expect(editableCells(model)).toHaveLength(4);
    expect(toGridLayoutBody(model)).toEqual(frigateBody());
  });

  it('preserves root/canvas/layout/cell/source extras verbatim', () => {
    const body = {
      schema_version: 3,
      operator_note: 'do not touch',
      canvas: {
        width: 1280,
        height: 720,
        fps: '30/1',
        background: '#000000',
      },
      layout: {
        kind: 'grid',
        columns: ['1fr'],
        rows: ['1fr'],
        areas: ['solo'],
        future_grid_knob: true,
      },
      cells: [
        {
          id: 'c1',
          area: 'solo',
          fit: 'cover',
          opacity: 0.9,
          qos: { priority: 2, shed_hint: 'late' },
          on_loss: { slate: 'no_signal' },
          source: { input_id: 'in1', fallback: 'freeze' },
          annotation: { author: 'troy' },
        },
      ],
    };
    const model = load(body);
    // gap was absent (serde default) — it must stay absent on save.
    expect(model.gap).toBeUndefined();
    expect(toGridLayoutBody(model)).toEqual(body);
  });

  it('keeps non-area (absolute rect) cells under a grid layout verbatim and in order', () => {
    const body = frigateBody();
    const cells = body.cells as Record<string, unknown>[];
    const rectCell = {
      id: 'float',
      rect: { x: 0.4, y: 0.4, w: 0.2, h: 0.2 },
      z: 9,
      source: {},
    };
    cells.splice(2, 0, rectCell);
    const model = load(body);
    expect(editableCells(model)).toHaveLength(4);
    const saved = toGridLayoutBody(model);
    const savedCells = saved.cells as Record<string, unknown>[];
    expect(savedCells[2]).toEqual(rectCell);
    expect(saved).toEqual(body);
  });

  it('returns undefined for absolute and malformed bodies', () => {
    expect(fromGridLayoutBody('x', 'y', { layout: { kind: 'absolute' } })).toBeUndefined();
    expect(fromGridLayoutBody('x', 'y', null)).toBeUndefined();
    expect(
      fromGridLayoutBody('x', 'y', {
        layout: { kind: 'grid', columns: '1fr', rows: ['1fr'], areas: ['a'] },
      }),
    ).toBeUndefined();
  });
});

describe('solveGridToRects (mirrors the Rust solver)', () => {
  it('two equal columns with a gap: 1000px, gap 100 => 450px tracks', () => {
    // Mirrors grid_solver.rs::two_equal_columns_with_gap.
    const model = load({
      canvas: { width: 1000, height: 1000, fps: '30/1' },
      layout: {
        kind: 'grid',
        columns: ['1fr', '1fr'],
        rows: ['1fr'],
        gap: 100,
        areas: ['a b'],
      },
      cells: [],
    });
    const rects = solveGridToRects(model);
    expect(rects).toBeDefined();
    const a = rects?.get('a');
    const b = rects?.get('b');
    expect(a?.x).toBeCloseTo(0, 5);
    expect(a?.w).toBeCloseTo(0.45, 5);
    expect(b?.x).toBeCloseTo(0.55, 5);
    expect(b?.w).toBeCloseTo(0.45, 5);
    expect(a?.h).toBeCloseTo(1, 5);
  });

  it('px and % tracks claim space first; fr shares the remainder', () => {
    // 1000px wide: 200px + 25% (=250) fixed, three gaps of 10 => free 520,
    // each 1fr = 260. Offsets: 0, 210, 470, 740.
    const model = load({
      canvas: { width: 1000, height: 500, fps: '30/1' },
      layout: {
        kind: 'grid',
        columns: ['200px', '25%', '1fr', '1fr'],
        rows: ['1fr'],
        gap: 10,
        areas: ['p q r s'],
      },
      cells: [],
    });
    const rects = solveGridToRects(model);
    expect(rects?.get('p')).toEqual({ x: 0, y: 0, w: 0.2, h: 1 });
    expect(rects?.get('q')?.x).toBeCloseTo(0.21, 5);
    expect(rects?.get('q')?.w).toBeCloseTo(0.25, 5);
    expect(rects?.get('r')?.x).toBeCloseTo(0.47, 5);
    expect(rects?.get('r')?.w).toBeCloseTo(0.26, 5);
    expect(rects?.get('s')?.x).toBeCloseTo(0.74, 5);
    expect(rects?.get('s')?.w).toBeCloseTo(0.26, 5);
  });

  it('row_gap / column_gap override the uniform gap per axis', () => {
    const model = load({
      canvas: { width: 1000, height: 1000, fps: '30/1' },
      layout: {
        kind: 'grid',
        columns: ['1fr', '1fr'],
        rows: ['1fr', '1fr'],
        gap: 100,
        row_gap: 0,
        areas: ['a b', 'c d'],
      },
      cells: [],
    });
    const rects = solveGridToRects(model);
    // Columns keep the 100px gap; rows have none.
    expect(rects?.get('a')?.w).toBeCloseTo(0.45, 5);
    expect(rects?.get('a')?.h).toBeCloseTo(0.5, 5);
    expect(rects?.get('c')?.y).toBeCloseTo(0.5, 5);
  });

  it('a spanning area covers its tracks plus the interior gap', () => {
    // hero spans cols 0-1 and rows 0-1 of a 3x3 with gap 30 on 990x990:
    // track = (990 - 60) / 3 = 310; hero extent = 310 + 30 + 310 = 650.
    const model = load({
      canvas: { width: 990, height: 990, fps: '30/1' },
      layout: {
        kind: 'grid',
        columns: ['1fr', '1fr', '1fr'],
        rows: ['1fr', '1fr', '1fr'],
        gap: 30,
        areas: ['hero hero s1', 'hero hero s2', 's3 s4 s5'],
      },
      cells: [],
    });
    const rects = solveGridToRects(model);
    expect(rects?.get('hero')?.w).toBeCloseTo(650 / 990, 5);
    expect(rects?.get('hero')?.h).toBeCloseTo(650 / 990, 5);
    expect(rects?.get('s1')?.x).toBeCloseTo(680 / 990, 5);
    expect(rects?.get('s5')?.y).toBeCloseTo(680 / 990, 5);
  });

  it('rejects an L-shaped (non-rectangular) area, like the Rust solver', () => {
    const model = load({
      canvas: { width: 1000, height: 1000, fps: '30/1' },
      layout: {
        kind: 'grid',
        columns: ['1fr', '1fr'],
        rows: ['1fr', '1fr'],
        areas: ['a a', 'a b'],
      },
      cells: [],
    });
    expect(solveGridToRects(model)).toBeUndefined();
    expect(
      validateGrid(model).some(
        (issue) => issue.code === 'area-not-rectangle' && issue.path === 'areas.a',
      ),
    ).toBe(true);
  });

  it('treats "." as a regular area name needing a rectangle (Rust parity)', () => {
    // The Rust parse_area_map has no CSS null-cell: "." is just a name, so two
    // diagonal "." cells are a non-rectangular area and the grid is unsolvable.
    const model = load({
      canvas: { width: 1000, height: 1000, fps: '30/1' },
      layout: {
        kind: 'grid',
        columns: ['1fr', '1fr'],
        rows: ['1fr', '1fr'],
        areas: ['a .', '. b'],
      },
      cells: [],
    });
    expect(solveGridToRects(model)).toBeUndefined();
  });

  it('parseTrack accepts fr/px/% and rejects junk (Rust Track::from_str parity)', () => {
    expect(parseTrack('2fr')).toEqual({ unit: 'fr', value: 2 });
    expect(parseTrack(' 200px ')).toEqual({ unit: 'px', value: 200 });
    expect(parseTrack('25%')).toEqual({ unit: '%', value: 25 });
    expect(parseTrack('1')).toBeUndefined();
    expect(parseTrack('-1fr')).toBeUndefined();
    expect(parseTrack('aufr')).toBeUndefined();
  });

  it('parseTrack matches Rust f64::from_str on numeric forms (no hex/Infinity)', () => {
    // Number("0x10") happily parses hex; Rust's f64::from_str rejects it.
    expect(parseTrack('0x10px')).toBeUndefined();
    expect(parseTrack('Infinitypx')).toBeUndefined();
    // These forms parse identically in Rust and JS.
    expect(parseTrack('1e2px')).toEqual({ unit: 'px', value: 100 });
    expect(parseTrack('.5fr')).toEqual({ unit: 'fr', value: 0.5 });
    expect(parseTrack('5.fr')).toEqual({ unit: 'fr', value: 5 });
    expect(parseTrack('+2fr')).toEqual({ unit: 'fr', value: 2 });
  });

  it('flags grid overflow when fixed tracks exceed the canvas (core rejects on apply)', () => {
    // A 5000px column on a 1920 canvas solves to w > 1.0; multiview-core
    // rejects x+w > 1.0, so saving must be blocked here, not on apply.
    const model = load({
      canvas: { width: 1920, height: 1080, fps: '30/1' },
      layout: {
        kind: 'grid',
        columns: ['5000px', '1fr'],
        rows: ['1fr'],
        areas: ['a b'],
      },
      cells: [],
    });
    const overflow = validateGrid(model).filter((issue) => issue.code === 'grid-overflow');
    expect(overflow.length).toBeGreaterThan(0);
    expect(overflow[0]?.severity).toBe('error');
    expect(isGridSavable(model)).toBe(false);
    // A percentage sum over 100 overflows the same way.
    const pct = load({
      canvas: { width: 1000, height: 1000, fps: '30/1' },
      layout: { kind: 'grid', columns: ['80%', '80%'], rows: ['1fr'], areas: ['a b'] },
      cells: [],
    });
    expect(validateGrid(pct).map((issue) => issue.code)).toContain('grid-overflow');
    // The frigate grid does not overflow.
    expect(validateGrid(load(frigateBody())).map((issue) => issue.code)).not.toContain(
      'grid-overflow',
    );
  });
});

describe('preset expansion', () => {
  it('expands 2x2 / 3x3 / 1+5 to the equivalent grids', () => {
    expect(expandPresetToGrid('2x2')).toEqual({
      columns: ['1fr', '1fr'],
      rows: ['1fr', '1fr'],
      areaMatrix: [
        ['a', 'b'],
        ['c', 'd'],
      ],
    });
    expect(expandPresetToGrid('3x3')?.areaMatrix).toEqual([
      ['a', 'b', 'c'],
      ['d', 'e', 'f'],
      ['g', 'h', 'i'],
    ]);
    expect(expandPresetToGrid('1+5')).toEqual({
      columns: ['1fr', '1fr', '1fr'],
      rows: ['1fr', '1fr', '1fr'],
      areaMatrix: [
        ['hero', 'hero', 's1'],
        ['hero', 'hero', 's2'],
        ['s3', 's4', 's5'],
      ],
    });
  });

  it('pip is not grid-expressible (tiles overlap) and returns undefined', () => {
    expect(expandPresetToGrid('pip')).toBeUndefined();
    expect(expandPresetToGrid('nonsense')).toBeUndefined();
  });

  it('the 1+5 grid solves to the documented preset geometry', () => {
    const body = {
      canvas: { width: 1920, height: 1080, fps: '30/1' },
      layout: { kind: 'preset', preset: '1+5' },
      cells: [],
    };
    const model = presetBodyToGridModel('p', 'OnePlusFive', body);
    expect(model).toBeDefined();
    if (model === undefined) {
      return;
    }
    const rects = solveGridToRects(model);
    const expected = presetCells('1+5');
    const hero = expected.find((cell) => cell.id === 'cell-hero');
    expect(rects?.get('hero')?.x).toBeCloseTo(hero?.rect.x ?? -1, 5);
    expect(rects?.get('hero')?.w).toBeCloseTo(hero?.rect.w ?? -1, 5);
    expect(rects?.get('s5')?.x).toBeCloseTo(2 / 3, 5);
    expect(rects?.get('s5')?.y).toBeCloseTo(2 / 3, 5);
    // A preset body with no cells gets one unbound cell per area.
    expect(editableCells(model)).toHaveLength(6);
    expect(cellForArea(model, 'hero')).toBeDefined();
  });

  it('a pip preset body converts to the documented free-form layout', () => {
    const body = {
      canvas: { width: 1920, height: 1080, fps: '30/1' },
      layout: { kind: 'preset', preset: 'pip' },
      cells: [],
    };
    const model = presetBodyToLayoutModel('p', 'Pip', body);
    expect(model?.cells.map((cell) => cell.id)).toEqual([
      'cell-program',
      'cell-pip',
    ]);
    expect(model?.cells[0]?.rect).toEqual({ x: 0, y: 0, w: 1, h: 1 });
  });
});

describe('area matrix editing', () => {
  it('assigns a rectangle to a new area name', () => {
    const model = load(frigateBody());
    const result = assignArea(model, { top: 0, left: 0, bottom: 0, right: 1 }, 'main');
    expect(result.ok).toBe(true);
    if (!result.ok) {
      return;
    }
    expect(result.model.areaMatrix).toEqual([
      ['main', 'main'],
      ['c', 'd'],
    ]);
    expect(areaNames(result.model)).toEqual(['main', 'c', 'd']);
  });

  it('rejects an assignment that would break another area into an L-shape', () => {
    const model = load({
      canvas: { width: 1920, height: 1080, fps: '30/1' },
      layout: {
        kind: 'grid',
        columns: ['1fr', '1fr', '1fr'],
        rows: ['1fr', '1fr', '1fr'],
        areas: ['a a a', 'a a a', 'a a a'],
      },
      cells: [],
    });
    const result = assignArea(model, { top: 1, left: 1, bottom: 1, right: 1 }, 'b');
    expect(result).toEqual({ ok: false, code: 'breaks-areas', areas: ['a'] });
  });

  it('rejects an empty or multi-token area name', () => {
    const model = load(frigateBody());
    expect(assignArea(model, { top: 0, left: 0, bottom: 0, right: 0 }, '').ok).toBe(false);
    expect(
      assignArea(model, { top: 0, left: 0, bottom: 0, right: 0 }, 'two words').ok,
    ).toBe(false);
  });

  it('renames an area across the matrix and its bound cells', () => {
    const model = load(frigateBody());
    const result = renameArea(model, 'a', 'main');
    expect(result.ok).toBe(true);
    if (!result.ok) {
      return;
    }
    expect(result.model.areaMatrix[0]).toEqual(['main', 'b']);
    expect(cellForArea(result.model, 'main')?.sourceId).toBe('in_a');
    expect(cellForArea(result.model, 'a')).toBeUndefined();
  });

  it('rename onto an adjacent area merges when the union is a rectangle, else rejects', () => {
    const model = load(frigateBody());
    // a + b are the full top row: merging is a rectangle.
    const merged = renameArea(model, 'a', 'b');
    expect(merged.ok).toBe(true);
    // a + d are diagonal: merging would not be a rectangle.
    const broken = renameArea(model, 'a', 'd');
    expect(broken.ok).toBe(false);
  });
});

describe('track editing', () => {
  it('setTrack replaces one track value', () => {
    const model = setTrack(load(frigateBody()), 'columns', 0, '2fr');
    expect(model.columns).toEqual(['2fr', '1fr']);
  });

  it('addTrack appends a 1fr track and extends the edge areas', () => {
    const model = addTrack(load(frigateBody()), 'columns');
    expect(model.columns).toEqual(['1fr', '1fr', '1fr']);
    expect(model.areaMatrix).toEqual([
      ['a', 'b', 'b'],
      ['c', 'd', 'd'],
    ]);
    const rows = addTrack(load(frigateBody()), 'rows');
    expect(rows.areaMatrix).toEqual([
      ['a', 'b'],
      ['c', 'd'],
      ['c', 'd'],
    ]);
  });

  it('removeTrack drops the track and its matrix column; orphan cells get flagged', () => {
    const model = removeTrack(load(frigateBody()), 'columns', 1);
    expect(model.columns).toEqual(['1fr']);
    expect(model.areaMatrix).toEqual([['a'], ['c']]);
    const codes = validateGrid(model)
      .filter((issue) => issue.code === 'cell-area-unknown')
      .map((issue) => issue.path);
    expect(codes).toHaveLength(2);
    expect(isGridSavable(model)).toBe(false);
  });

  it('removeTrack refuses to remove the last track', () => {
    const one = removeTrack(removeTrack(load(frigateBody()), 'columns', 1), 'columns', 0);
    expect(one.columns).toEqual(['1fr']);
  });
});

describe('cell editing', () => {
  it('ensureCell creates an unbound cell for an area exactly once', () => {
    const base = load(frigateBody());
    const withE = assignArea(base, { top: 0, left: 0, bottom: 0, right: 0 }, 'e');
    if (!withE.ok) {
      throw new Error('assign failed');
    }
    const model = ensureCell(withE.model, 'e');
    const cell = cellForArea(model, 'e');
    expect(cell?.id).toBe('cell_e');
    expect(cell?.sourceId).toBeUndefined();
    // Idempotent.
    expect(ensureCell(model, 'e')).toEqual(model);
  });

  it('updateCell sets source / fit / z / props for the area cell', () => {
    let model = load(frigateBody());
    model = updateCell(model, 'a', (cell) => ({
      ...cell,
      sourceId: 'in_x',
      fit: 'cover',
      z: 5,
      props: { ...cell.props, onLoss: onLossOf('black') },
    }));
    const cell = cellForArea(model, 'a');
    expect(cell?.sourceId).toBe('in_x');
    expect(cell?.fit).toBe('cover');
    const saved = toGridLayoutBody(model);
    const savedCells = saved.cells as Record<string, unknown>[];
    expect(savedCells[0]).toEqual({
      id: 'cell_a',
      area: 'a',
      z: 5,
      fit: 'cover',
      on_loss: { slate: 'black' },
      source: { input_id: 'in_x' },
    });
  });

  it('removeCellForArea drops the cell and the area-no-cell warning appears', () => {
    const model = removeCellForArea(load(frigateBody()), 'd');
    expect(cellForArea(model, 'd')).toBeUndefined();
    const warning = validateGrid(model).find((issue) => issue.code === 'area-no-cell');
    expect(warning?.severity).toBe('warning');
    // Warnings do not block saving.
    expect(isGridSavable(model)).toBe(true);
  });
});

describe('validation', () => {
  it('the frigate demo grid is valid and savable', () => {
    const model = load(frigateBody());
    expect(validateGrid(model).filter((issue) => issue.severity === 'error')).toEqual([]);
    expect(isGridSavable(model)).toBe(true);
  });

  it('flags bad tracks, gaps, and duplicate cell ids', () => {
    const body = frigateBody();
    const layout = body.layout as Record<string, unknown>;
    layout.columns = ['1fr', 'banana'];
    layout.gap = -2;
    const cells = body.cells as Record<string, unknown>[];
    const first = cells[0];
    const second = cells[1];
    if (first === undefined || second === undefined) {
      throw new Error('expected seeded cells');
    }
    second.id = first.id;
    const model = load(body);
    const codes = validateGrid(model).map((issue) => issue.code);
    expect(codes).toContain('track-format');
    expect(codes).toContain('gap-invalid');
    expect(codes).toContain('cell-id-duplicate');
    expect(isGridSavable(model)).toBe(false);
  });

  it('flags cell property issues through the grid validator', () => {
    const body = frigateBody();
    const cells = body.cells as Record<string, unknown>[];
    const first = cells[0];
    if (first === undefined) {
      throw new Error('expected seeded cells');
    }
    first.opacity = 7;
    const model = load(body);
    expect(validateGrid(model).map((issue) => issue.code)).toContain('opacity-range');
  });

  it('flags empty track lists', () => {
    const model = load({
      canvas: { width: 100, height: 100, fps: '30/1' },
      layout: { kind: 'grid', columns: [], rows: [], areas: [] },
      cells: [],
    });
    const codes = validateGrid(model).map((issue) => issue.code);
    expect(codes).toContain('tracks-empty');
    expect(isGridSavable(model)).toBe(false);
  });

  it('surfaces a visible advisory when a ragged areas map was normalized on load', () => {
    // Row 0 has an extra token (dropped) — the body the editor would save is a
    // rewrite of the stored one, so the operator must be told.
    const model = load({
      canvas: { width: 1000, height: 1000, fps: '30/1' },
      layout: {
        kind: 'grid',
        columns: ['1fr', '1fr'],
        rows: ['1fr', '1fr'],
        areas: ['a b c', 'c d'],
      },
      cells: [],
    });
    const advisory = validateGrid(model).find((issue) => issue.code === 'areas-normalized');
    expect(advisory?.severity).toBe('warning');
    // A short row (padded with ".") is also a normalization.
    const padded = load({
      canvas: { width: 1000, height: 1000, fps: '30/1' },
      layout: { kind: 'grid', columns: ['1fr', '1fr'], rows: ['1fr'], areas: ['a'] },
      cells: [],
    });
    expect(validateGrid(padded).map((issue) => issue.code)).toContain('areas-normalized');
    // A well-formed map never raises it.
    expect(validateGrid(load(frigateBody())).map((issue) => issue.code)).not.toContain(
      'areas-normalized',
    );
  });

  it('flags a cell declaring BOTH area and rect (Rust: choose exactly one)', () => {
    const body = frigateBody();
    const cells = body.cells as Record<string, unknown>[];
    const first = cells[0];
    if (first === undefined) {
      throw new Error('expected seeded cells');
    }
    first.rect = { x: 0.9, y: 0.9, w: 0.1, h: 0.1 };
    const model = load(body);
    const issue = validateGrid(model).find((i) => i.code === 'cell-area-and-rect');
    expect(issue?.severity).toBe('error');
    expect(isGridSavable(model)).toBe(false);
    // The conflicting rect still round-trips verbatim (lossless).
    expect(toGridLayoutBody(model)).toEqual(body);
  });

  it('treats an out-of-range border colour as a warning, not a save blocker', () => {
    // Rust never validates Border.color: a pre-existing "red" must not make
    // the layout unsavable; the advisory still shows.
    const body = frigateBody();
    const cells = body.cells as Record<string, unknown>[];
    const first = cells[0];
    if (first === undefined) {
      throw new Error('expected seeded cells');
    }
    first.border = { width_px: 2, color: 'red' };
    const model = load(body);
    const issue = validateGrid(model).find((i) => i.code === 'border-color-hex');
    expect(issue?.severity).toBe('warning');
    expect(isGridSavable(model)).toBe(true);
  });

  it('a cell without a source key stays without one (no invented {})', () => {
    const body = {
      canvas: { width: 1000, height: 1000, fps: '30/1' },
      layout: { kind: 'grid', columns: ['1fr'], rows: ['1fr'], areas: ['solo'] },
      cells: [{ id: 'c1', area: 'solo' }],
    };
    const model = load(body);
    const saved = toGridLayoutBody(model);
    const savedCells = saved.cells as Record<string, unknown>[];
    expect(savedCells[0]).toEqual({ id: 'c1', area: 'solo' });
    expect(saved).toEqual(body);
    // An explicit empty source record is presence, and is kept.
    const withEmpty = {
      ...body,
      cells: [{ id: 'c1', area: 'solo', source: {} }],
    };
    expect(toGridLayoutBody(load(withEmpty))).toEqual(withEmpty);
  });
});

describe('gap setters', () => {
  it('set / clear the three gaps independently and round-trip', () => {
    let model = load(frigateBody());
    model = setRowGap(model, 0);
    model = setColumnGap(model, 12);
    const saved = toGridLayoutBody(model);
    const layout = saved.layout as Record<string, unknown>;
    expect(layout.gap).toBe(4);
    expect(layout.row_gap).toBe(0);
    expect(layout.column_gap).toBe(12);
    model = setGap(model, undefined);
    model = setRowGap(model, undefined);
    model = setColumnGap(model, undefined);
    const cleared = toGridLayoutBody(model).layout as Record<string, unknown>;
    expect('gap' in cleared).toBe(false);
    expect('row_gap' in cleared).toBe(false);
  });
});

describe('convert to free-form', () => {
  it('materializes the solved rects as an absolute layout (one-way)', () => {
    const model = gridToLayoutModel(load(frigateBody()));
    expect(model).toBeDefined();
    if (model === undefined) {
      return;
    }
    expect(model.cells).toHaveLength(4);
    const a = model.cells.find((cell) => cell.id === 'cell_a');
    // 1920x1080, two 1fr columns, gap 4: track w = (1920-4)/2 = 958.
    expect(a?.rect.x).toBeCloseTo(0, 5);
    expect(a?.rect.w).toBeCloseTo(958 / 1920, 5);
    expect(a?.sourceId).toBe('in_a');
    expect(a?.label).toBe('a');
    // Canvas extras ride along so the saved absolute body keeps them.
    expect(model.canvasExtra.pixel_format).toBe('nv12');
  });

  it('returns undefined when the grid cannot be solved', () => {
    const model = load({
      canvas: { width: 100, height: 100, fps: '30/1' },
      layout: { kind: 'grid', columns: ['1fr', '1fr'], rows: ['1fr', '1fr'], areas: ['a a', 'a b'] },
      cells: [],
    });
    expect(gridToLayoutModel(model)).toBeUndefined();
  });

  it('a conflicting cell rect never overrides the solved rect on conversion', () => {
    const body = {
      canvas: { width: 1000, height: 1000, fps: '30/1' },
      layout: {
        kind: 'grid',
        columns: ['1fr', '1fr'],
        rows: ['1fr'],
        areas: ['a b'],
      },
      cells: [
        {
          id: 'cell_a',
          area: 'a',
          rect: { x: 0.9, y: 0.9, w: 0.1, h: 0.1 },
          source: { input_id: 'in_a' },
        },
      ],
    };
    const converted = gridToLayoutModel(load(body));
    expect(converted).toBeDefined();
    if (converted === undefined) {
      return;
    }
    const cellA = converted.cells.find((cell) => cell.id === 'cell_a');
    // The SOLVED placement wins, in the model…
    expect(cellA?.rect).toEqual({ x: 0, y: 0, w: 0.5, h: 1 });
    // …and in the serialized body (the stale rect must not ride `extra` over
    // the managed key).
    const absBody = toLayoutBody(converted);
    const absCells = absBody.cells as Record<string, unknown>[];
    const savedA = absCells.find((cell) => cell.id === 'cell_a');
    expect(savedA?.rect).toEqual({ x: 0, y: 0, w: 0.5, h: 1 });
  });
});
