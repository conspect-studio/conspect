// A READ-ONLY konva preview of the solved grid placement: one rectangle per
// area (via the pure solveGridToRects), labelled with the area name and the
// bound source. Pure visualization — every edit happens in the matrix/track/
// cell controls, so this stage is aria-hidden like the free-form canvas (the
// matrix editor is the accessible equivalent surface).
//
// Code-split (lazy) by the grid editor, exactly like LayoutCanvas, so the
// konva renderer stays out of the main bundle.
import { useEffect, useMemo, useRef, useState } from 'react';
import type { JSX } from 'react';
import { Layer, Rect, Stage, Text } from 'react-konva';

import type { CanvasModel, NormalizedRect } from '../model';
import { areaHue } from '../gridModel';

/** Props for {@link GridPreviewCanvas}. */
export interface GridPreviewCanvasProps {
  /** The output canvas geometry (drives the preview aspect). */
  readonly canvas: CanvasModel;
  /** Solved normalized rect per area name. */
  readonly rects: ReadonlyMap<string, NormalizedRect>;
  /** Secondary label per area (the bound source), if any. */
  readonly sourceLabels: ReadonlyMap<string, string>;
  /** The area highlighted in the editor, if any. */
  readonly selectedArea: string | undefined;
}

/** Measure the host element width so the stage stays aspect-correct. */
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

/** The read-only grid placement preview stage. */
export function GridPreviewCanvas({
  canvas,
  rects,
  sourceLabels,
  selectedArea,
}: GridPreviewCanvasProps): JSX.Element {
  const [setRef, width] = useContainerWidth();
  const aspect = canvas.width > 0 && canvas.height > 0 ? canvas.height / canvas.width : 9 / 16;
  const stageW = width;
  const stageH = Math.round(width * aspect);

  return (
    <div ref={setRef} aria-hidden="true" className="rounded-md border bg-muted/30">
      <Stage width={stageW} height={stageH}>
        <Layer>
          <Rect x={0} y={0} width={stageW} height={stageH} fill="#101014" />
          {[...rects.entries()].map(([area, rect]) => {
            const selected = area === selectedArea;
            const x = rect.x * stageW;
            const y = rect.y * stageH;
            const w = rect.w * stageW;
            const h = rect.h * stageH;
            return (
              <Rect
                key={`rect-${area}`}
                x={x}
                y={y}
                width={w}
                height={h}
                fill={`hsl(${String(areaHue(area))} 55% 45% / ${selected ? '0.6' : '0.35'})`}
                stroke={selected ? '#3b82f6' : '#94a3b8'}
                strokeWidth={selected ? 2 : 1}
              />
            );
          }).concat(
            [...rects.entries()].map(([area, rect]) => {
              const source = sourceLabels.get(area);
              return (
                <Text
                  key={`label-${area}`}
                  x={rect.x * stageW + 6}
                  y={rect.y * stageH + 6}
                  width={Math.max(10, rect.w * stageW - 12)}
                  text={source !== undefined ? `${area}\n${source}` : area}
                  fontSize={13}
                  fill="#e2e8f0"
                  listening={false}
                />
              );
            }),
          )}
        </Layer>
      </Stage>
    </div>
  );
}
