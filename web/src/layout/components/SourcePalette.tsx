// The dnd-kit source/overlay palette.
//
// A keyboard-accessible draggable list of sources and overlays. Dragging an item
// onto the layout drop zone adds a cell bound to that source (or, for an overlay,
// requests it be added as an overlay layer). dnd-kit's KeyboardSensor + screen-
// reader announcements make the drag operable without a mouse; an explicit
// "Add to layout" button on each item provides a non-drag fallback too (so the
// palette is never drag-only — accessibility.md).
import { useMemo } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import {
  DndContext,
  KeyboardSensor,
  PointerSensor,
  useDraggable,
  useDroppable,
  useSensor,
  useSensors,
} from '@dnd-kit/core';
import type { Announcements, DragEndEvent } from '@dnd-kit/core';
import { Plus } from 'lucide-react';

import type { OverlayView, SourceView } from '../../resources/types';
import { Badge } from '../../components/ui/badge';
import { Button } from '../../components/ui/button';

/** Identifies what kind of resource a palette item carries. */
export type PaletteKind = 'source' | 'overlay';

/** The id of the canvas drop target. */
export const LAYOUT_DROP_ID = 'layout-drop-zone';

/** Props for {@link SourcePalette}. */
export interface SourcePaletteProps {
  /** Sources available to drag/add. */
  readonly sources: readonly SourceView[];
  /** Overlays available to drag/add. */
  readonly overlays: readonly OverlayView[];
  /** Add a source-bound cell (drag-drop or button). */
  readonly onAddSource: (source: SourceView) => void;
  /** Add an overlay (drag-drop or button). */
  readonly onAddOverlay: (overlay: OverlayView) => void;
}

interface DragData {
  readonly kind: PaletteKind;
  readonly id: string;
}

function isDragData(value: unknown): value is DragData {
  return (
    typeof value === 'object' &&
    value !== null &&
    'kind' in value &&
    'id' in value
  );
}

/** A single draggable palette entry with a non-drag "Add" fallback. */
function PaletteItem({
  itemId,
  kind,
  name,
  badge,
  addLabel,
  onAdd,
}: {
  readonly itemId: string;
  readonly kind: PaletteKind;
  readonly name: string;
  readonly badge: string;
  readonly addLabel: string;
  readonly onAdd: () => void;
}): JSX.Element {
  const dragData: DragData = { kind, id: itemId };
  const { attributes, listeners, setNodeRef, isDragging } = useDraggable({
    id: `${kind}:${itemId}`,
    data: dragData,
  });
  return (
    <li className="flex items-center gap-2 rounded-md border p-2">
      <button
        type="button"
        ref={setNodeRef}
        className={`flex flex-1 items-center gap-2 rounded-sm text-start focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring ${isDragging ? 'opacity-50' : ''}`}
        {...listeners}
        {...attributes}
      >
        <Badge variant="outline">{badge}</Badge>
        <span lang="" dir="auto" className="truncate">
          {name}
        </span>
      </button>
      <Button
        type="button"
        size="sm"
        variant="ghost"
        aria-label={`${addLabel}: ${name}`}
        onClick={onAdd}
      >
        <Plus aria-hidden="true" />
        <Trans>Add</Trans>
      </Button>
    </li>
  );
}

/** The drop target that wraps the canvas. */
export function LayoutDropZone({
  children,
}: {
  readonly children: JSX.Element;
}): JSX.Element {
  const { setNodeRef, isOver } = useDroppable({ id: LAYOUT_DROP_ID });
  return (
    <div
      ref={setNodeRef}
      className={`rounded-md transition-shadow ${isOver ? 'ring-2 ring-primary' : ''}`}
    >
      {children}
    </div>
  );
}

/**
 * The palette + a DndContext that also wraps the dropzone passed as `children`.
 * Keeping the context here keeps draggables and the droppable under one provider.
 */
export function SourcePalette({
  sources,
  overlays,
  onAddSource,
  onAddOverlay,
  children,
}: SourcePaletteProps & { readonly children: JSX.Element }): JSX.Element {
  const { t } = useLingui();
  const sensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 4 } }),
    useSensor(KeyboardSensor),
  );

  const sourceById = useMemo(() => {
    const map = new Map<string, SourceView>();
    for (const source of sources) {
      map.set(source.id, source);
    }
    return map;
  }, [sources]);

  const overlayById = useMemo(() => {
    const map = new Map<string, OverlayView>();
    for (const overlay of overlays) {
      map.set(overlay.id, overlay);
    }
    return map;
  }, [overlays]);

  const addLabel = t`Add to layout`;

  function handleDragEnd(event: DragEndEvent): void {
    if (event.over?.id !== LAYOUT_DROP_ID) {
      return;
    }
    const data: unknown = event.active.data.current;
    if (!isDragData(data)) {
      return;
    }
    if (data.kind === 'source') {
      const source = sourceById.get(data.id);
      if (source !== undefined) {
        onAddSource(source);
      }
    } else {
      const overlay = overlayById.get(data.id);
      if (overlay !== undefined) {
        onAddOverlay(overlay);
      }
    }
  }

  const announcements: Announcements = {
    onDragStart: ({ active }) => t`Picked up ${String(active.id)}.`,
    onDragOver: ({ over }) =>
      over !== null ? t`Over the layout drop zone.` : t`No drop target.`,
    onDragEnd: ({ over }) =>
      over !== null
        ? t`Dropped onto the layout.`
        : t`Dropped outside any target; nothing changed.`,
    onDragCancel: () => t`Drag cancelled.`,
  };

  return (
    <DndContext
      sensors={sensors}
      onDragEnd={handleDragEnd}
      accessibility={{ announcements }}
    >
      <div className="grid gap-4 lg:grid-cols-[18rem_1fr]">
        <div className="flex flex-col gap-4">
          <section aria-labelledby="palette-sources">
            <h3 id="palette-sources" className="mb-2 text-sm font-semibold">
              <Trans>Sources</Trans>
            </h3>
            <ul className="flex flex-col gap-2">
              {sources.map((source) => (
                <PaletteItem
                  key={source.id}
                  itemId={source.id}
                  kind="source"
                  name={source.name}
                  badge={source.kind}
                  addLabel={addLabel}
                  onAdd={(): void => {
                    onAddSource(source);
                  }}
                />
              ))}
            </ul>
          </section>

          <section aria-labelledby="palette-overlays">
            <h3 id="palette-overlays" className="mb-2 text-sm font-semibold">
              <Trans>Overlays</Trans>
            </h3>
            <ul className="flex flex-col gap-2">
              {overlays.map((overlay) => (
                <PaletteItem
                  key={overlay.id}
                  itemId={overlay.id}
                  kind="overlay"
                  name={overlay.name}
                  badge={overlay.kind}
                  addLabel={addLabel}
                  onAdd={(): void => {
                    onAddOverlay(overlay);
                  }}
                />
              ))}
            </ul>
          </section>
        </div>

        <LayoutDropZone>{children}</LayoutDropZone>
      </div>
    </DndContext>
  );
}
