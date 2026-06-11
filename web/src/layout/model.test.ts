// Unit tests for the pure layout view-model: geometry math, add/remove/reorder,
// validation, and the body <-> view-model round trip. No DOM, no React.
import { describe, expect, it } from 'vitest';

import {
  addCell,
  applyPreset,
  bindCellSource,
  clamp,
  clampRect,
  emptyLayout,
  fromLayoutBody,
  isLayoutValid,
  LAYOUT_PRESETS,
  MIN_CELL_EXTENT,
  moveCell,
  normalizeRotation,
  rectsOverlap,
  removeCell,
  reorderCell,
  resizeCell,
  rotateCell,
  setCanvas,
  setCellZ,
  snap,
  toLayoutBody,
  validateLayout,
} from './model';
import type { CellModel, LayoutModel } from './model';
import { setCellProps } from './model';
import { emptyCellProperties, onLossOf, parseCellProperties } from './cellProps';

function cell(id: string, over: Partial<CellModel> = {}): CellModel {
  return {
    id,
    label: id,
    rect: { x: 0.1, y: 0.1, w: 0.3, h: 0.3 },
    z: 0,
    rotation: 0,
    fit: 'contain',
    sourceId: undefined,
    sourceExtra: {},
    props: emptyCellProperties(),
    extra: {},
    ...over,
  };
}

function model(cells: CellModel[]): LayoutModel {
  return {
    id: 'lay-1',
    name: 'Studio',
    canvas: { width: 1920, height: 1080, fps: '30/1' },
    rootExtra: {},
    canvasExtra: {},
    layoutExtra: {},
    cells,
  };
}

describe('geometry helpers', () => {
  it('clamp bounds a value to the inclusive range', () => {
    expect(clamp(5, 0, 1)).toBe(1);
    expect(clamp(-2, 0, 1)).toBe(0);
    expect(clamp(0.5, 0, 1)).toBe(0.5);
  });

  it('snap rounds to the nearest step and is identity for step <= 0', () => {
    expect(snap(0.13, 0.05)).toBeCloseTo(0.15, 10);
    expect(snap(0.12, 0)).toBe(0.12);
    expect(snap(0.12, -1)).toBe(0.12);
  });

  it('clampRect keeps a rect on-canvas with a minimum extent', () => {
    const r = clampRect({ x: 0.9, y: -0.5, w: 2, h: 0 });
    expect(r.w).toBeLessThanOrEqual(1);
    expect(r.h).toBeGreaterThanOrEqual(MIN_CELL_EXTENT);
    expect(r.x + r.w).toBeLessThanOrEqual(1 + 1e-9);
    expect(r.y).toBeGreaterThanOrEqual(0);
  });

  it('normalizeRotation wraps into [0, 360)', () => {
    expect(normalizeRotation(370)).toBe(10);
    expect(normalizeRotation(-90)).toBe(270);
    expect(normalizeRotation(360)).toBe(0);
  });

  it('rectsOverlap detects overlap but not edge-touching', () => {
    const a = { x: 0, y: 0, w: 0.5, h: 0.5 };
    expect(rectsOverlap(a, { x: 0.25, y: 0.25, w: 0.5, h: 0.5 })).toBe(true);
    expect(rectsOverlap(a, { x: 0.5, y: 0, w: 0.5, h: 0.5 })).toBe(false);
  });
});

describe('cell mutations', () => {
  it('moveCell snaps and clamps the top-left, preserving size', () => {
    const m = moveCell(model([cell('a')]), 'a', 0.98, 0.31, 0.1);
    const moved = m.cells[0];
    expect(moved?.rect.w).toBeCloseTo(0.3, 10);
    expect(moved?.rect.x).toBeLessThanOrEqual(1 - 0.3 + 1e-9);
    expect(moved?.rect.y).toBeCloseTo(0.3, 10);
  });

  it('resizeCell clamps a too-large rect onto the canvas', () => {
    const m = resizeCell(model([cell('a')]), 'a', { x: 0.5, y: 0.5, w: 1, h: 1 });
    const r = m.cells[0]?.rect;
    expect((r?.x ?? 0) + (r?.w ?? 0)).toBeLessThanOrEqual(1 + 1e-9);
  });

  it('rotateCell normalizes the angle', () => {
    const m = rotateCell(model([cell('a')]), 'a', 450);
    expect(m.cells[0]?.rotation).toBe(90);
  });

  it('bindCellSource sets and clears the source binding', () => {
    const bound = bindCellSource(model([cell('a')]), 'a', 'cam-1');
    expect(bound.cells[0]?.sourceId).toBe('cam-1');
    const cleared = bindCellSource(bound, 'a', undefined);
    expect(cleared.cells[0]?.sourceId).toBeUndefined();
  });

  it('moving an unknown id returns the same model reference', () => {
    const base = model([cell('a')]);
    expect(moveCell(base, 'missing', 0, 0)).toBe(base);
  });
});

describe('add / remove / reorder', () => {
  it('addCell appends a cell stacked above the current top', () => {
    const m = addCell(model([cell('a', { z: 0 })]), { id: 'b', label: 'B' });
    expect(m.cells).toHaveLength(2);
    expect(m.cells[1]?.z).toBe(1);
  });

  it('addCell rejects a duplicate id', () => {
    const base = model([cell('a')]);
    expect(addCell(base, { id: 'a', label: 'dupe' })).toBe(base);
  });

  it('removeCell drops the cell (and is a no-op when absent)', () => {
    const base = model([cell('a'), cell('b')]);
    expect(removeCell(base, 'a').cells.map((c) => c.id)).toEqual(['b']);
    expect(removeCell(base, 'zzz')).toBe(base);
  });

  it('reorderCell moves a cell and renumbers z to list order', () => {
    const base = model([cell('a'), cell('b'), cell('c')]);
    const m = reorderCell(base, 0, 2);
    expect(m.cells.map((c) => c.id)).toEqual(['b', 'c', 'a']);
    expect(m.cells.map((c) => c.z)).toEqual([0, 1, 2]);
  });

  it('reorderCell rejects out-of-range indices', () => {
    const base = model([cell('a'), cell('b')]);
    expect(reorderCell(base, 0, 9)).toBe(base);
    expect(reorderCell(base, -1, 0)).toBe(base);
    expect(reorderCell(base, 1, 1)).toBe(base);
  });

  it('setCellZ rounds and assigns an explicit stacking order', () => {
    const m = setCellZ(model([cell('a')]), 'a', 4.6);
    expect(m.cells[0]?.z).toBe(5);
  });
});

describe('validation', () => {
  it('a well-formed layout is valid', () => {
    expect(isLayoutValid(model([cell('a')]))).toBe(true);
  });

  it('flags an empty name, no cells, and a bad fps', () => {
    const bad: LayoutModel = {
      id: '',
      name: '   ',
      canvas: { width: 1920, height: 1080, fps: '29.97' },
      rootExtra: {},
      canvasExtra: {},
      layoutExtra: {},
      cells: [],
    };
    const codes = validateLayout(bad).map((i) => i.code);
    expect(codes).toContain('name-empty');
    expect(codes).toContain('no-cells');
    expect(codes).toContain('fps-format');
  });

  it('flags duplicate and empty cell ids', () => {
    const codes = validateLayout(
      model([cell('a'), cell('a'), cell('')]),
    ).map((i) => i.code);
    expect(codes).toContain('cell-id-duplicate');
    expect(codes).toContain('cell-id-empty');
  });

  it('flags an out-of-bounds rect and an out-of-range rotation', () => {
    const codes = validateLayout(
      model([
        cell('a', { rect: { x: 0.8, y: 0.8, w: 0.5, h: 0.5 }, rotation: 999 }),
      ]),
    ).map((i) => i.code);
    expect(codes).toContain('rect-bounds');
    expect(codes).toContain('rotation-range');
  });

  it('accepts the NTSC rational fps form', () => {
    const m = {
      ...model([cell('a')]),
      canvas: { width: 1920, height: 1080, fps: '30000/1001' },
    };
    expect(isLayoutValid(m)).toBe(true);
  });
});

describe('body <-> view-model mapping', () => {
  it('round-trips an absolute layout through body and back', () => {
    const original = model([
      cell('a', { rect: { x: 0.1, y: 0.2, w: 0.3, h: 0.4 }, sourceId: 'cam-1' }),
      cell('b', { z: 1, fit: 'cover' }),
    ]);
    const body = toLayoutBody(original);
    const restored = fromLayoutBody('lay-1', 'Studio', body);
    expect(restored).toBeDefined();
    expect(restored?.cells).toHaveLength(2);
    expect(restored?.cells[0]?.sourceId).toBe('cam-1');
    expect(restored?.cells[1]?.fit).toBe('cover');
    expect(restored?.canvas.fps).toBe('30/1');
  });

  it('returns undefined for a non-absolute (grid) layout body', () => {
    const grid = {
      canvas: { width: 1920, height: 1080, fps: '30/1' },
      layout: { kind: 'grid', columns: ['1fr'], rows: ['1fr'], areas: ['a'] },
      cells: [{ id: 'a', area: 'a', source: {} }],
    };
    expect(fromLayoutBody('x', 'Grid', grid)).toBeUndefined();
  });

  it('returns undefined for a non-object body', () => {
    expect(fromLayoutBody('x', 'y', null)).toBeUndefined();
    expect(fromLayoutBody('x', 'y', 42)).toBeUndefined();
  });

  it('emptyLayout is a valid canvas with no cells', () => {
    const m = emptyLayout('New');
    expect(m.cells).toHaveLength(0);
    expect(m.canvas.width).toBeGreaterThan(0);
    expect(validateLayout(m).map((i) => i.code)).toContain('no-cells');
  });
});

describe('layout presets', () => {
  it('2x2 fills the canvas with four quarter cells', () => {
    const m = applyPreset(model([]), '2x2');
    expect(m.cells).toHaveLength(4);
    expect(m.cells[0]?.rect).toEqual({ x: 0, y: 0, w: 0.5, h: 0.5 });
    expect(m.cells[3]?.rect).toEqual({ x: 0.5, y: 0.5, w: 0.5, h: 0.5 });
    expect(isLayoutValid({ ...m, name: 'n' })).toBe(true);
  });

  it('3x3 fills the canvas with nine cells', () => {
    const m = applyPreset(model([]), '3x3');
    expect(m.cells).toHaveLength(9);
    const last = m.cells[8];
    expect(last?.rect.x).toBeCloseTo(2 / 3, 9);
    expect(last?.rect.y).toBeCloseTo(2 / 3, 9);
    expect(last?.rect.w).toBeCloseTo(1 / 3, 9);
  });

  it('1+5 is one hero cell plus five satellites covering the canvas edges', () => {
    const m = applyPreset(model([]), '1+5');
    expect(m.cells).toHaveLength(6);
    const hero = m.cells[0];
    expect(hero?.rect).toEqual({ x: 0, y: 0, w: 2 / 3, h: 2 / 3 });
    // The satellites tile the right column and the bottom row.
    const xs = m.cells.slice(1).map((c) => c.rect);
    expect(xs).toHaveLength(5);
    for (const r of xs) {
      expect(r.w).toBeCloseTo(1 / 3, 9);
      expect(r.h).toBeCloseTo(1 / 3, 9);
    }
  });

  it('pip is a full-frame program with an inset picture-in-picture on top', () => {
    const m = applyPreset(model([]), 'pip');
    expect(m.cells).toHaveLength(2);
    expect(m.cells[0]?.rect).toEqual({ x: 0, y: 0, w: 1, h: 1 });
    const pip = m.cells[1];
    expect(pip).toBeDefined();
    expect(pip !== undefined && pip.z > (m.cells[0]?.z ?? 0)).toBe(true);
    expect(pip !== undefined && pip.rect.w < 0.5 && pip.rect.h < 0.5).toBe(true);
  });

  it('replaces any existing cells and renumbers z from the bottom', () => {
    const m = applyPreset(model([cell('old')]), '2x2');
    expect(m.cells.some((c) => c.id === 'old')).toBe(false);
    expect(m.cells.map((c) => c.z)).toEqual([0, 1, 2, 3]);
  });

  it('preset cell ids are unique', () => {
    for (const preset of LAYOUT_PRESETS) {
      const m = applyPreset(model([]), preset);
      expect(new Set(m.cells.map((c) => c.id)).size).toBe(m.cells.length);
    }
  });
});

describe('setCanvas', () => {
  it('replaces the canvas geometry/cadence without touching the cells', () => {
    const before = model([cell('a')]);
    const after = setCanvas(before, { width: 3840, height: 2160, fps: '30000/1001' });
    expect(after.canvas).toEqual({ width: 3840, height: 2160, fps: '30000/1001' });
    expect(after.cells).toBe(before.cells);
  });

  it('a float fps string still fails validation (invariant #3: exact rationals)', () => {
    const m = setCanvas(model([cell('a')]), { width: 1920, height: 1080, fps: '29.97' });
    expect(validateLayout(m).map((i) => i.code)).toContain('fps-format');
  });
});

describe('lossless extras (absolute editor)', () => {
  it('preserves root / canvas / layout / cell / source extras through a no-op edit', () => {
    const body = {
      schema_version: 2,
      operator_note: 'keep me',
      canvas: {
        width: 1920,
        height: 1080,
        fps: '25/1',
        pixel_format: 'nv12',
        background: '#101014',
        color: { profile: 'sdr-bt709-limited' },
      },
      layout: { kind: 'absolute', future_layout_knob: 1 },
      cells: [
        {
          id: 'a',
          label: 'a',
          rect: { x: 0, y: 0, w: 0.5, h: 0.5 },
          z: 0,
          rotation: 0,
          fit: 'contain',
          opacity: 0.8,
          border: { width_px: 2, color: '#0f0f0f' },
          qos: { priority: 3, degradation: 'balanced' },
          on_loss: { slate: 'no_signal' },
          annotation: 'hand-authored',
          source: { input_id: 'cam-1', fallback: 'freeze' },
        },
      ],
    };
    const restored = fromLayoutBody('lay', 'Studio', body);
    expect(restored).toBeDefined();
    if (restored === undefined) {
      return;
    }
    expect(restored.cells[0]?.props.opacity).toBe(0.8);
    expect(restored.cells[0]?.props.onLoss?.slate).toBe('no_signal');
    expect(toLayoutBody(restored)).toEqual(body);
  });

  it('setCellProps writes the snake_case property fields into the body', () => {
    const base = model([cell('a')]);
    const next = setCellProps(base, 'a', {
      ...parseCellProperties({ opacity: 0.5, qos: { priority: 9 } }),
      onLoss: onLossOf('black'),
    });
    const body = toLayoutBody(next);
    const cells = body.cells as Record<string, unknown>[];
    expect(cells[0]?.opacity).toBe(0.5);
    expect(cells[0]?.qos).toEqual({ priority: 9 });
    expect(cells[0]?.on_loss).toEqual({ slate: 'black' });
  });

  it('cell property issues surface through validateLayout', () => {
    const bad = setCellProps(model([cell('a')]), 'a', parseCellProperties({ opacity: 2 }));
    expect(validateLayout(bad).map((i) => i.code)).toContain('opacity-range');
  });

  it('a non-hex border colour is advisory only and never blocks an absolute save', () => {
    // Rust never validates Border.color; a pre-existing "red" must not make
    // the layout uneditable/unsavable in the free-form editor either.
    const m = setCellProps(
      model([cell('a')]),
      'a',
      parseCellProperties({ border: { width_px: 1, color: 'red' } }),
    );
    expect(validateLayout(m).map((i) => i.code)).not.toContain('border-color-hex');
    expect(isLayoutValid(m)).toBe(true);
  });

  it('a new draft body still writes schema_version 1 and an empty source', () => {
    const body = toLayoutBody(model([cell('a')]));
    expect(body.schema_version).toBe(1);
    const cells = body.cells as Record<string, unknown>[];
    expect(cells[0]?.source).toEqual({});
  });
});
