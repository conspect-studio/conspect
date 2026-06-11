// The grid editor's shared state hook (the grid counterpart of
// useLayoutEditor): holds the current GridModel + the selected area, and
// exposes typed actions delegating to the pure functions in ./gridModel. The
// matrix editor, the track/gap controls, the per-area panel and the konva
// preview all drive this ONE state, so every view stays consistent. The hook
// holds no DOM and never blocks.
import { useCallback, useMemo, useState } from 'react';

import {
  addTrack as addTrackToModel,
  assignArea as assignAreaOnModel,
  cellForArea,
  ensureCell as ensureCellOnModel,
  isGridSavable,
  removeCellForArea as removeCellOnModel,
  removeTrack as removeTrackFromModel,
  renameArea as renameAreaOnModel,
  setColumnGap as setColumnGapOnModel,
  setGap as setGapOnModel,
  setGridCanvas,
  setGridName,
  setRowGap as setRowGapOnModel,
  setTrack as setTrackOnModel,
  updateCell as updateCellOnModel,
  validateGrid,
} from './gridModel';
import type {
  AssignResult,
  GridCellModel,
  GridModel,
  GridValidationIssue,
  MatrixSelection,
  TrackAxis,
} from './gridModel';
import type { CanvasModel } from './model';

/** Everything the grid editor UI needs. */
export interface GridEditorState {
  /** The current view-model. */
  readonly model: GridModel;
  /** The selected area name, or `undefined`. */
  readonly selectedArea: string | undefined;
  /** The selected area's cell, resolved for convenience. */
  readonly selectedCell: GridCellModel | undefined;
  /** Live validation issues (errors + warnings). */
  readonly issues: readonly GridValidationIssue[];
  /** Whether the grid can be saved (no error-severity issues). */
  readonly isSavable: boolean;

  /** Select an area (or clear with `undefined`). */
  readonly selectArea: (area: string | undefined) => void;
  /** Rename the layout. */
  readonly setName: (name: string) => void;
  /** Replace the canvas geometry/cadence. */
  readonly setCanvas: (canvas: CanvasModel) => void;
  /** Set/clear the uniform gap. */
  readonly setGap: (gap: number | undefined) => void;
  /** Set/clear the row-gap override. */
  readonly setRowGap: (gap: number | undefined) => void;
  /** Set/clear the column-gap override. */
  readonly setColumnGap: (gap: number | undefined) => void;
  /** Replace one track value. */
  readonly setTrack: (axis: TrackAxis, index: number, value: string) => void;
  /** Append a 1fr track (the matrix extends along that edge). */
  readonly addTrack: (axis: TrackAxis) => void;
  /** Remove a track and its matrix column/row. */
  readonly removeTrack: (axis: TrackAxis, index: number) => void;
  /**
   * Assign the selected matrix rectangle to an area name; on success a cell is
   * ensured for the new area. Returns the outcome for the matrix status line.
   */
  readonly assignArea: (selection: MatrixSelection, name: string) => AssignResult;
  /** Rename an area across the matrix + cells. */
  readonly renameArea: (from: string, to: string) => AssignResult;
  /** Add an unbound cell for an area that lacks one. */
  readonly ensureCell: (area: string) => void;
  /** Remove the cell(s) bound to an area. */
  readonly removeCellForArea: (area: string) => void;
  /** Update the area's cell through a pure transform. */
  readonly updateCell: (
    area: string,
    update: (cell: GridCellModel) => GridCellModel,
  ) => void;
}

/** Construct the grid editor state from a loaded model. */
export function useGridEditor(initial: GridModel): GridEditorState {
  const [model, setModel] = useState<GridModel>(initial);
  const [selectedArea, setSelectedArea] = useState<string | undefined>(undefined);

  const selectArea = useCallback((area: string | undefined): void => {
    setSelectedArea(area);
  }, []);

  const setName = useCallback((name: string): void => {
    setModel((current) => setGridName(current, name));
  }, []);

  const setCanvas = useCallback((canvas: CanvasModel): void => {
    setModel((current) => setGridCanvas(current, canvas));
  }, []);

  const setGap = useCallback((gap: number | undefined): void => {
    setModel((current) => setGapOnModel(current, gap));
  }, []);

  const setRowGap = useCallback((gap: number | undefined): void => {
    setModel((current) => setRowGapOnModel(current, gap));
  }, []);

  const setColumnGap = useCallback((gap: number | undefined): void => {
    setModel((current) => setColumnGapOnModel(current, gap));
  }, []);

  const setTrack = useCallback(
    (axis: TrackAxis, index: number, value: string): void => {
      setModel((current) => setTrackOnModel(current, axis, index, value));
    },
    [],
  );

  const addTrack = useCallback((axis: TrackAxis): void => {
    setModel((current) => addTrackToModel(current, axis));
  }, []);

  const removeTrack = useCallback((axis: TrackAxis, index: number): void => {
    setModel((current) => removeTrackFromModel(current, axis, index));
  }, []);

  const assignArea = useCallback(
    (selection: MatrixSelection, name: string): AssignResult => {
      const result = assignAreaOnModel(model, selection, name);
      if (result.ok) {
        // A fresh area immediately gets an (unbound) cell to bind/edit.
        setModel(ensureCellOnModel(result.model, name.trim()));
      }
      return result;
    },
    [model],
  );

  const renameArea = useCallback(
    (from: string, to: string): AssignResult => {
      const result = renameAreaOnModel(model, from, to);
      if (result.ok) {
        setModel(result.model);
        setSelectedArea((current) => (current === from ? to.trim() : current));
      }
      return result;
    },
    [model],
  );

  const ensureCell = useCallback((area: string): void => {
    setModel((current) => ensureCellOnModel(current, area));
  }, []);

  const removeCellForArea = useCallback((area: string): void => {
    setModel((current) => removeCellOnModel(current, area));
  }, []);

  const updateCell = useCallback(
    (area: string, update: (cell: GridCellModel) => GridCellModel): void => {
      setModel((current) => updateCellOnModel(current, area, update));
    },
    [],
  );

  const issues = useMemo(() => validateGrid(model), [model]);
  const isSavable = useMemo(() => isGridSavable(model), [model]);
  const selectedCell = useMemo(
    () => (selectedArea === undefined ? undefined : cellForArea(model, selectedArea)),
    [model, selectedArea],
  );

  return {
    model,
    selectedArea,
    selectedCell,
    issues,
    isSavable,
    selectArea,
    setName,
    setCanvas,
    setGap,
    setRowGap,
    setColumnGap,
    setTrack,
    addTrack,
    removeTrack,
    assignArea,
    renameArea,
    ensureCell,
    removeCellForArea,
    updateCell,
  };
}
