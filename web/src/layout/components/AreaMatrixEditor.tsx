// The grid-template-areas matrix editor: a rows × columns grid of selectable
// squares. Click-drag (pointer) or arrow keys + Shift (keyboard) select a
// rectangle; typing a name and pressing Assign maps that rectangle to an area.
// Assignments that would leave ANY area non-rectangular are rejected with a
// clear message (mirroring the Rust solver's contiguity rule), so the matrix
// can never become unsolvable through this editor.
//
// ACCESSIBILITY: a real role="grid" with roving-tabindex gridcells carrying
// aria-selected; instructions are wired via aria-describedby; outcomes are
// announced through a live region. Areas are distinguished by NAME text in
// every cell (never colour alone); the per-area hue is decorative.
import { useEffect, useRef, useState } from 'react';
import type { JSX, KeyboardEvent } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';

import { areaHue } from '../gridModel';
import type { AssignResult, MatrixSelection } from '../gridModel';
import { Button } from '../../components/ui/button';
import { Input } from '../../components/ui/input';
import { Label } from '../../components/ui/label';

/** Props for {@link AreaMatrixEditor}. */
export interface AreaMatrixEditorProps {
  /** The area map (rows × columns of tokens). */
  readonly matrix: readonly (readonly string[])[];
  /** The area highlighted as selected in the side panel, if any. */
  readonly selectedArea: string | undefined;
  /** Select the area under a cell (drives the per-area panel). */
  readonly onSelectArea: (area: string) => void;
  /** Apply an assignment; the parent runs the pure `assignArea`. */
  readonly onAssign: (selection: MatrixSelection, name: string) => AssignResult;
}

interface CellPos {
  readonly row: number;
  readonly col: number;
}

function selectionOf(anchor: CellPos, focus: CellPos): MatrixSelection {
  return {
    top: Math.min(anchor.row, focus.row),
    left: Math.min(anchor.col, focus.col),
    bottom: Math.max(anchor.row, focus.row),
    right: Math.max(anchor.col, focus.col),
  };
}

function inSelection(selection: MatrixSelection, row: number, col: number): boolean {
  return (
    row >= selection.top &&
    row <= selection.bottom &&
    col >= selection.left &&
    col <= selection.right
  );
}

/** The areas-matrix editor (see the module docs). */
export function AreaMatrixEditor({
  matrix,
  selectedArea,
  onSelectArea,
  onAssign,
}: AreaMatrixEditorProps): JSX.Element {
  const { t } = useLingui();
  const rowCount = matrix.length;
  const colCount = matrix[0]?.length ?? 0;
  const [anchor, setAnchor] = useState<CellPos>({ row: 0, col: 0 });
  const [focus, setFocus] = useState<CellPos>({ row: 0, col: 0 });
  const [dragging, setDragging] = useState(false);
  const [name, setName] = useState('');
  const [status, setStatus] = useState('');
  const [statusIsError, setStatusIsError] = useState(false);
  const cellRefs = useRef(new Map<string, HTMLDivElement>());
  const pendingFocus = useRef(false);

  // Keep the focus cell inside the matrix when tracks are removed.
  const clampedFocus: CellPos = {
    row: Math.min(focus.row, Math.max(0, rowCount - 1)),
    col: Math.min(focus.col, Math.max(0, colCount - 1)),
  };
  const clampedAnchor: CellPos = {
    row: Math.min(anchor.row, Math.max(0, rowCount - 1)),
    col: Math.min(anchor.col, Math.max(0, colCount - 1)),
  };
  const selection = selectionOf(clampedAnchor, clampedFocus);

  useEffect(() => {
    if (!pendingFocus.current) {
      return;
    }
    pendingFocus.current = false;
    cellRefs.current
      .get(`${String(clampedFocus.row)}:${String(clampedFocus.col)}`)
      ?.focus();
  }, [clampedFocus.row, clampedFocus.col]);

  useEffect(() => {
    const stopDrag = (): void => {
      setDragging(false);
    };
    window.addEventListener('pointerup', stopDrag);
    return (): void => {
      window.removeEventListener('pointerup', stopDrag);
    };
  }, []);

  const moveFocus = (row: number, col: number, extend: boolean): void => {
    const next: CellPos = {
      row: Math.max(0, Math.min(rowCount - 1, row)),
      col: Math.max(0, Math.min(colCount - 1, col)),
    };
    pendingFocus.current = true;
    setFocus(next);
    if (!extend) {
      setAnchor(next);
    }
    const token = matrix[next.row]?.[next.col];
    if (token !== undefined) {
      onSelectArea(token);
    }
  };

  const onCellKeyDown = (event: KeyboardEvent<HTMLDivElement>): void => {
    const extend = event.shiftKey;
    switch (event.key) {
      case 'ArrowUp':
        event.preventDefault();
        moveFocus(clampedFocus.row - 1, clampedFocus.col, extend);
        break;
      case 'ArrowDown':
        event.preventDefault();
        moveFocus(clampedFocus.row + 1, clampedFocus.col, extend);
        break;
      case 'ArrowLeft':
        event.preventDefault();
        moveFocus(clampedFocus.row, clampedFocus.col - 1, extend);
        break;
      case 'ArrowRight':
        event.preventDefault();
        moveFocus(clampedFocus.row, clampedFocus.col + 1, extend);
        break;
      case 'Home':
        event.preventDefault();
        moveFocus(clampedFocus.row, 0, extend);
        break;
      case 'End':
        event.preventDefault();
        moveFocus(clampedFocus.row, colCount - 1, extend);
        break;
      default:
        break;
    }
  };

  const assign = (): void => {
    const result = onAssign(selection, name);
    if (result.ok) {
      setStatusIsError(false);
      setStatus(t`Assigned the selection to area "${name.trim()}".`);
      onSelectArea(name.trim());
      return;
    }
    setStatusIsError(true);
    if (result.code === 'name-invalid') {
      setStatus(t`Enter a single-word area name (no spaces).`);
      return;
    }
    const broken = result.areas.join(', ');
    setStatus(
      t`Cannot assign: area(s) ${broken} would no longer be a rectangle. Areas must stay contiguous rectangles.`,
    );
  };

  const selectionLabel = t`Selected rows ${String(selection.top + 1)}–${String(selection.bottom + 1)}, columns ${String(selection.left + 1)}–${String(selection.right + 1)}`;

  return (
    <div className="flex flex-col gap-3">
      <p id="area-matrix-instructions" className="text-xs text-muted-foreground">
        <Trans>
          Pick a rectangle of squares with the arrow keys (hold Shift to extend)
          or by click-dragging, then type a name and assign it. Every named
          area must form a contiguous rectangle.
        </Trans>
      </p>
      <div
        role="grid"
        aria-label={t`Grid areas`}
        aria-describedby="area-matrix-instructions"
        data-testid="area-matrix"
        className="inline-grid w-fit touch-none select-none gap-1"
        style={{ gridTemplateColumns: `repeat(${String(Math.max(1, colCount))}, 3.5rem)` }}
      >
        {matrix.map((row, rowIndex) => (
          // The CSS display:grid flattens row wrappers; display:contents keeps
          // the row semantics AT needs. Rows/cells are positional, so the
          // index is the correct, stable key.
          <div role="row" key={rowIndex} className="contents">
            {row.map((token, colIndex) => {
              const isFocusCell =
                rowIndex === clampedFocus.row && colIndex === clampedFocus.col;
              const selected = inSelection(selection, rowIndex, colIndex);
              const isSelectedArea = token === selectedArea;
              return (
                <div
                  role="gridcell"
                  key={colIndex}
                  ref={(node): void => {
                    const key = `${String(rowIndex)}:${String(colIndex)}`;
                    if (node === null) {
                      cellRefs.current.delete(key);
                    } else {
                      cellRefs.current.set(key, node);
                    }
                  }}
                  tabIndex={isFocusCell ? 0 : -1}
                  aria-selected={selected}
                  aria-label={t`Row ${String(rowIndex + 1)}, column ${String(colIndex + 1)}: area ${token}`}
                  data-testid={`matrix-cell-${String(rowIndex)}-${String(colIndex)}`}
                  className={`flex h-14 cursor-pointer items-center justify-center overflow-hidden rounded-sm border text-xs font-medium ${
                    selected ? 'ring-2 ring-primary' : ''
                  } ${isSelectedArea ? 'border-primary' : 'border-border'}`}
                  style={{
                    backgroundColor: `hsl(${String(areaHue(token))} 55% 45% / 0.45)`,
                  }}
                  onKeyDown={onCellKeyDown}
                  onPointerDown={(event): void => {
                    if (event.shiftKey) {
                      pendingFocus.current = true;
                      setFocus({ row: rowIndex, col: colIndex });
                    } else {
                      moveFocus(rowIndex, colIndex, false);
                    }
                    setDragging(true);
                  }}
                  onPointerEnter={(): void => {
                    if (dragging) {
                      setFocus({ row: rowIndex, col: colIndex });
                    }
                  }}
                >
                  <span lang="" dir="auto">
                    {token}
                  </span>
                </div>
              );
            })}
          </div>
        ))}
      </div>
      <p className="text-xs text-muted-foreground" data-testid="matrix-selection">
        {selectionLabel}
      </p>
      <div className="flex items-end gap-2">
        <div className="flex flex-col gap-1">
          <Label htmlFor="area-assign-name" className="text-xs">
            <Trans>Area name</Trans>
          </Label>
          <Input
            id="area-assign-name"
            value={name}
            className="w-40"
            lang=""
            dir="auto"
            onChange={(event): void => {
              setName(event.target.value);
            }}
            onKeyDown={(event): void => {
              if (event.key === 'Enter') {
                event.preventDefault();
                assign();
              }
            }}
          />
        </div>
        <Button
          type="button"
          variant="outline"
          data-testid="assign-area"
          onClick={assign}
        >
          <Trans>Assign to selection</Trans>
        </Button>
      </div>
      <p
        role={statusIsError ? 'alert' : 'status'}
        aria-live="polite"
        data-testid="matrix-status"
        className={`text-xs ${statusIsError ? 'text-destructive' : 'text-muted-foreground'}`}
      >
        {status}
      </p>
    </div>
  );
}
