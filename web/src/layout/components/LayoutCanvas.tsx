// The react-konva free-form layout canvas.
//
// Renders the output canvas (aspect-correct) and one draggable/resizable/
// rotatable rectangle per cell. Geometry is normalized (0..1); the canvas only
// converts to/from pixels for display and delegates EVERY mutation back to the
// editor hook, so the model stays the single source of truth and the accessible
// form edits exactly the same state.
//
// ACCESSIBILITY: a <canvas> is inherently opaque to assistive tech. This view is
// the convenience/visual path; the keyboard-operable form/table beside it
// (CellsForm) is the equivalent, fully-operable editing path (WCAG 2.2 AA — see
// accessibility.md). The canvas is marked aria-hidden so it is not announced as
// interactive content the AT user cannot reach.
import { useEffect, useMemo, useRef, useState } from 'react';
import type { JSX } from 'react';
import { Group, Layer, Rect, Stage, Transformer } from 'react-konva';
import type Konva from 'konva';

import type { CellModel, LayoutModel, NormalizedRect } from '../model';

/** Props for {@link LayoutCanvas}. */
export interface LayoutCanvasProps {
  /** The layout being edited. */
  readonly model: LayoutModel;
  /** The selected cell id (drawn with handles), if any. */
  readonly selectedId: string | undefined;
  /** Select a cell (or clear with `undefined`). */
  readonly onSelect: (id: string | undefined) => void;
  /** Commit a move of `id` to a new normalized top-left. */
  readonly onMove: (id: string, x: number, y: number) => void;
  /** Commit a resize of `id` to a new normalized rect. */
  readonly onResize: (id: string, rect: NormalizedRect) => void;
  /** Commit a rotation of `id` (degrees). */
  readonly onRotate: (id: string, degrees: number) => void;
}

/** Cells sorted bottom-to-top so higher `z` draws last (on top). */
function byZ(cells: readonly CellModel[]): CellModel[] {
  return [...cells].sort((a, b) => a.z - b.z);
}

/** Measure the host element width so the stage stays aspect-correct + responsive. */
function useContainerWidth(): readonly [
  (node: HTMLDivElement | null) => void,
  number,
] {
  const [width, setWidth] = useState(640);
  const observerRef = useRef<ResizeObserver | null>(null);
  const setRef = useMemo(() => {
    return (node: HTMLDivElement | null): void => {
      observerRef.current?.disconnect();
      if (node === null) {
        return;
      }
      const measure = (): void => {
        setWidth(Math.max(160, node.clientWidth));
      };
      measure();
      const observer = new ResizeObserver(measure);
      observer.observe(node);
      observerRef.current = observer;
    };
  }, []);
  useEffect(() => {
    return (): void => {
      observerRef.current?.disconnect();
    };
  }, []);
  return [setRef, width];
}

/** A single cell rectangle on the stage. */
function CellRect({
  cell,
  selected,
  scaleW,
  scaleH,
  onSelect,
  onMove,
  onResize,
  onRotate,
}: {
  readonly cell: CellModel;
  readonly selected: boolean;
  readonly scaleW: number;
  readonly scaleH: number;
  readonly onSelect: (id: string) => void;
  readonly onMove: (id: string, x: number, y: number) => void;
  readonly onResize: (id: string, rect: NormalizedRect) => void;
  readonly onRotate: (id: string, degrees: number) => void;
}): JSX.Element {
  const shapeRef = useRef<Konva.Rect>(null);
  const trRef = useRef<Konva.Transformer>(null);

  useEffect(() => {
    if (selected && trRef.current !== null && shapeRef.current !== null) {
      trRef.current.nodes([shapeRef.current]);
      trRef.current.getLayer()?.batchDraw();
    }
  }, [selected]);

  const px = cell.rect.x * scaleW;
  const py = cell.rect.y * scaleH;
  const pw = cell.rect.w * scaleW;
  const ph = cell.rect.h * scaleH;

  return (
    <Group>
      <Rect
        ref={shapeRef}
        x={px}
        y={py}
        width={pw}
        height={ph}
        rotation={cell.rotation}
        draggable
        fill={selected ? 'rgba(59,130,246,0.25)' : 'rgba(148,163,184,0.18)'}
        stroke={selected ? '#3b82f6' : '#94a3b8'}
        strokeWidth={selected ? 2 : 1}
        cornerRadius={4}
        onMouseDown={(): void => {
          onSelect(cell.id);
        }}
        onTap={(): void => {
          onSelect(cell.id);
        }}
        onDragEnd={(event): void => {
          const node = event.target;
          onMove(cell.id, node.x() / scaleW, node.y() / scaleH);
        }}
        onTransformEnd={(): void => {
          const node = shapeRef.current;
          if (node === null) {
            return;
          }
          const nextW = (node.width() * node.scaleX()) / scaleW;
          const nextH = (node.height() * node.scaleY()) / scaleH;
          // Konva applies size via scale; bake it back into width/height.
          node.scaleX(1);
          node.scaleY(1);
          onResize(cell.id, {
            x: node.x() / scaleW,
            y: node.y() / scaleH,
            w: nextW,
            h: nextH,
          });
          onRotate(cell.id, node.rotation());
        }}
      />
      {selected ? (
        <Transformer
          ref={trRef}
          rotateEnabled
          keepRatio={false}
          ignoreStroke
          boundBoxFunc={(_oldBox, newBox) =>
            // Never allow a degenerate (negative/zero) box.
            newBox.width < 4 || newBox.height < 4 ? _oldBox : newBox
          }
        />
      ) : null}
    </Group>
  );
}

/** The interactive layout canvas. */
export function LayoutCanvas({
  model,
  selectedId,
  onSelect,
  onMove,
  onResize,
  onRotate,
}: LayoutCanvasProps): JSX.Element {
  const [setHostRef, width] = useContainerWidth();
  const aspect = model.canvas.height / model.canvas.width;
  const height = Math.round(width * aspect);
  const ordered = useMemo(() => byZ(model.cells), [model.cells]);

  return (
    <div
      ref={setHostRef}
      className="overflow-hidden rounded-md border bg-muted/30"
      // The canvas is a visual convenience; the equivalent operable path is the
      // CellsForm. Hide it from AT so it is not announced as unreachable UI.
      aria-hidden="true"
    >
      <Stage
        width={width}
        height={height}
        onMouseDown={(event): void => {
          // A click on empty stage clears the selection.
          if (event.target === event.target.getStage()) {
            onSelect(undefined);
          }
        }}
      >
        <Layer>
          <Rect x={0} y={0} width={width} height={height} fill="#0b0f19" />
          {ordered.map((cell) => (
            <CellRect
              key={cell.id}
              cell={cell}
              selected={cell.id === selectedId}
              scaleW={width}
              scaleH={height}
              onSelect={onSelect}
              onMove={onMove}
              onResize={onResize}
              onRotate={onRotate}
            />
          ))}
        </Layer>
      </Stage>
    </div>
  );
}
