// The layout editor's shared state hook.
//
// Holds the current view-model + the selected cell + the snap setting, and
// exposes typed actions that delegate to the pure functions in `./model`. BOTH
// the react-konva canvas and the accessible non-canvas form drive the SAME hook,
// so the two editing paths are guaranteed to edit identical state (the dual-model
// accessibility requirement). The hook holds no DOM and never blocks.
import { useCallback, useMemo, useState } from 'react';

import {
  addCell,
  applyPreset as applyPresetToModel,
  bindCellSource,
  DEFAULT_SNAP,
  emptyLayout,
  moveCell,
  removeCell,
  reorderCell,
  resizeCell,
  rotateCell,
  setCanvas as setCanvasOnModel,
  setCellFit,
  setCellLabel,
  setCellProps,
  validateLayout,
} from './model';
import type {
  CanvasModel,
  CellModel,
  FitMode,
  LayoutModel,
  LayoutPreset,
  NormalizedRect,
  ValidationIssue,
} from './model';
import type { CellProperties } from './cellProps';

let cellCounter = 0;

/** Generate a unique-ish cell id for a newly-added cell. */
function nextCellId(): string {
  cellCounter += 1;
  return `cell-${String(cellCounter)}`;
}

/** Everything the editor UI needs. */
export interface LayoutEditorState {
  /** The current view-model. */
  readonly model: LayoutModel;
  /** The selected cell id, or `undefined` when nothing is selected. */
  readonly selectedId: string | undefined;
  /** The currently selected cell, resolved for convenience. */
  readonly selectedCell: CellModel | undefined;
  /** Snap step (fraction of a canvas axis); `0` disables snapping. */
  readonly snap: number;
  /** Live validation issues (empty when valid). */
  readonly issues: readonly ValidationIssue[];
  /** Whether the layout currently validates. */
  readonly isValid: boolean;

  /** Replace the whole model (e.g. on load). Clears selection if it vanished. */
  readonly setModel: (next: LayoutModel) => void;
  /** Rename the layout. */
  readonly setName: (name: string) => void;
  /** Replace the canvas geometry/cadence (validated live). */
  readonly setCanvas: (canvas: CanvasModel) => void;
  /** Seed the cells from a preset (replaces the current cells). */
  readonly applyPreset: (preset: LayoutPreset) => void;
  /** Select a cell (or clear with `undefined`). */
  readonly select: (id: string | undefined) => void;
  /** Toggle snap-to-grid on/off. */
  readonly setSnapEnabled: (enabled: boolean) => void;

  /** Add a new cell and select it; returns its id. */
  readonly add: (label: string, partial?: Partial<CellModel>) => string;
  /** Remove a cell; clears selection if it was selected. */
  readonly remove: (id: string) => void;
  /** Move a cell's top-left to `(x, y)`. */
  readonly move: (id: string, x: number, y: number) => void;
  /** Resize a cell to a new rect. */
  readonly resize: (id: string, rect: NormalizedRect) => void;
  /** Rotate a cell to `degrees`. */
  readonly rotate: (id: string, degrees: number) => void;
  /** Set a cell's fit mode. */
  readonly setFit: (id: string, fit: FitMode) => void;
  /** Set/clear a cell's source binding. */
  readonly bindSource: (id: string, sourceId: string | undefined) => void;
  /** Replace a cell's full property set (on_loss / appearance / degradation). */
  readonly setProps: (id: string, props: CellProperties) => void;
  /** Rename a cell. */
  readonly rename: (id: string, label: string) => void;
  /** Reorder a cell within the list (renumbers z). */
  readonly reorder: (from: number, to: number) => void;
  /** Move a cell one step toward the back. */
  readonly moveDown: (index: number) => void;
  /** Move a cell one step toward the front. */
  readonly moveUp: (index: number) => void;
}

/** Construct the editor state from an initial model (defaults to an empty one). */
export function useLayoutEditor(initial?: LayoutModel): LayoutEditorState {
  const [model, setModelState] = useState<LayoutModel>(
    () => initial ?? emptyLayout(),
  );
  const [selectedId, setSelectedId] = useState<string | undefined>(undefined);
  const [snap, setSnap] = useState<number>(DEFAULT_SNAP);

  const select = useCallback((id: string | undefined): void => {
    setSelectedId(id);
  }, []);

  const setModel = useCallback((next: LayoutModel): void => {
    setModelState(next);
    setSelectedId((current) =>
      current !== undefined && next.cells.some((c) => c.id === current)
        ? current
        : undefined,
    );
  }, []);

  const setName = useCallback((name: string): void => {
    setModelState((current) => ({ ...current, name }));
  }, []);

  const setCanvas = useCallback((canvas: CanvasModel): void => {
    setModelState((current) => setCanvasOnModel(current, canvas));
  }, []);

  const applyPreset = useCallback((preset: LayoutPreset): void => {
    setModelState((current) => applyPresetToModel(current, preset));
    setSelectedId(undefined);
  }, []);

  const setSnapEnabled = useCallback((enabled: boolean): void => {
    setSnap(enabled ? DEFAULT_SNAP : 0);
  }, []);

  const add = useCallback(
    (label: string, partial?: Partial<CellModel>): string => {
      const id = partial?.id ?? nextCellId();
      setModelState((current) => addCell(current, { id, label, ...partial }));
      setSelectedId(id);
      return id;
    },
    [],
  );

  const remove = useCallback((id: string): void => {
    setModelState((current) => removeCell(current, id));
    setSelectedId((current) => (current === id ? undefined : current));
  }, []);

  const move = useCallback(
    (id: string, x: number, y: number): void => {
      setModelState((current) => moveCell(current, id, x, y, snap));
    },
    [snap],
  );

  const resize = useCallback(
    (id: string, rect: NormalizedRect): void => {
      setModelState((current) => resizeCell(current, id, rect, snap));
    },
    [snap],
  );

  const rotate = useCallback((id: string, degrees: number): void => {
    setModelState((current) => rotateCell(current, id, degrees));
  }, []);

  const setFit = useCallback((id: string, fit: FitMode): void => {
    setModelState((current) => setCellFit(current, id, fit));
  }, []);

  const bindSource = useCallback(
    (id: string, sourceId: string | undefined): void => {
      setModelState((current) => bindCellSource(current, id, sourceId));
    },
    [],
  );

  const setProps = useCallback((id: string, props: CellProperties): void => {
    setModelState((current) => setCellProps(current, id, props));
  }, []);

  const rename = useCallback((id: string, label: string): void => {
    setModelState((current) => setCellLabel(current, id, label));
  }, []);

  const reorder = useCallback((from: number, to: number): void => {
    setModelState((current) => reorderCell(current, from, to));
  }, []);

  const moveDown = useCallback((index: number): void => {
    setModelState((current) => reorderCell(current, index, index - 1));
  }, []);

  const moveUp = useCallback((index: number): void => {
    setModelState((current) => reorderCell(current, index, index + 1));
  }, []);

  const issues = useMemo(() => validateLayout(model), [model]);
  const selectedCell = useMemo(
    () =>
      selectedId === undefined
        ? undefined
        : model.cells.find((c) => c.id === selectedId),
    [model.cells, selectedId],
  );

  return {
    model,
    selectedId,
    selectedCell,
    snap,
    issues,
    isValid: issues.length === 0,
    setModel,
    setName,
    setCanvas,
    applyPreset,
    select,
    setSnapEnabled,
    add,
    remove,
    move,
    resize,
    rotate,
    setFit,
    bindSource,
    setProps,
    rename,
    reorder,
    moveDown,
    moveUp,
  };
}
