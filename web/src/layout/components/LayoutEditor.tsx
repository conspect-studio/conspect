// The layout editor — the management app's centerpiece.
//
// Composes the four pieces around one `useLayoutEditor` state:
//   * a react-konva canvas for free-form drag/resize/rotate;
//   * a dnd-kit palette of sources/overlays that drops onto the canvas;
//   * an ACCESSIBLE, keyboard-operable non-canvas form editing the same cells; and
//   * a live validation summary + save controls (writes the opaque config body).
//
// The canvas and the form are two views of the SAME state, so the editor is
// fully usable without the canvas (WCAG 2.2 AA, accessibility.md).
import { lazy, Suspense, useMemo, useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { Grid2x2, Plus, Send } from 'lucide-react';

import { useLayoutEditor } from '../useLayoutEditor';
import { LAYOUT_PRESETS, toLayoutBody } from '../model';
import type { LayoutModel, LayoutPreset } from '../model';
import type { OverlayView, SourceView } from '../../resources/types';
import { CellsForm } from './CellsForm';
import { SourcePalette } from './SourcePalette';
import { ValidationMessage } from './validationMessages';
import { Button } from '../../components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '../../components/ui/dialog';
import { Input } from '../../components/ui/input';
import { Label } from '../../components/ui/label';
import {
  Tabs,
  TabsContent,
  TabsList,
  TabsTrigger,
} from '../../components/ui/tabs';

// The konva canvas is heavy (the konva renderer). Code-split it so the main
// bundle stays lean and the accessible form path loads without it.
const LayoutCanvas = lazy(() =>
  import('./LayoutCanvas').then((mod) => ({ default: mod.LayoutCanvas })),
);

/** What the editor emits on save: the input name + the serialized body. */
export interface LayoutSavePayload {
  /** The (possibly empty) existing layout id; empty means create. */
  readonly id: string;
  /** The layout name. */
  readonly name: string;
  /** The opaque config body produced from the view-model. */
  readonly body: Record<string, unknown>;
}

/** Props for {@link LayoutEditor}. */
export interface LayoutEditorProps {
  /** The layout to edit, or `undefined` to start a fresh draft. */
  readonly initial?: LayoutModel;
  /** Sources available to bind/drag. */
  readonly sources: readonly SourceView[];
  /** Overlays available to drag. */
  readonly overlays: readonly OverlayView[];
  /** Called with the serialized payload when the operator saves a valid layout. */
  readonly onSave: (payload: LayoutSavePayload) => void;
  /**
   * Called for "Save & Apply": persist the layout, then apply it to the
   * running engine (a LIVE action, unlike stored-resource edits). Omitted ⇒
   * the button is not offered (e.g. while the apply route is unavailable).
   */
  readonly onSaveAndApply?: (payload: LayoutSavePayload) => void;
  /** Whether a save is in flight (disables the save button). */
  readonly isSaving?: boolean;
}

/** The composed layout editor. */
export function LayoutEditor({
  initial,
  sources,
  overlays,
  onSave,
  onSaveAndApply,
  isSaving = false,
}: LayoutEditorProps): JSX.Element {
  const { t } = useLingui();
  const editor = useLayoutEditor(initial);
  const nameId = 'layout-name';
  const [pendingPreset, setPendingPreset] = useState<LayoutPreset | null>(null);

  const presetLabel = (preset: LayoutPreset): string => {
    switch (preset) {
      case '2x2':
        return t`2×2`;
      case '3x3':
        return t`3×3`;
      case '1+5':
        return t`1+5`;
      case 'pip':
        return t`PIP`;
    }
  };

  const seedPreset = (preset: LayoutPreset): void => {
    // Seeding replaces the composed cells; confirm before discarding work.
    if (editor.model.cells.length > 0) {
      setPendingPreset(preset);
      return;
    }
    editor.applyPreset(preset);
  };

  const savePayload = (): LayoutSavePayload => ({
    id: editor.model.id,
    name: editor.model.name,
    body: toLayoutBody(editor.model),
  });

  const addSourceCell = (source: SourceView): void => {
    editor.add(source.name, { sourceId: source.id });
  };
  const addOverlayCell = (overlay: OverlayView): void => {
    // Until overlays are first-class in the editor model, an overlay drop adds a
    // cell placeholder labelled for the overlay so the operator can position it.
    editor.add(overlay.name);
  };

  const canSave = editor.isValid && !isSaving;
  const issues = editor.issues;

  const sourceNames = useMemo(() => {
    const map = new Map<string, string>();
    for (const source of sources) {
      map.set(source.id, source.name);
    }
    return map;
  }, [sources]);

  return (
    <div className="flex flex-col gap-6">
      <div className="flex flex-wrap items-end justify-between gap-4">
        <div className="flex flex-col gap-1">
          <Label htmlFor={nameId}>
            <Trans>Layout name</Trans>
          </Label>
          <Input
            id={nameId}
            value={editor.model.name}
            lang=""
            dir="auto"
            className="w-72"
            aria-invalid={editor.model.name.trim() === ''}
            placeholder={t`e.g. Main wall`}
            onChange={(event): void => {
              editor.setName(event.target.value);
            }}
          />
        </div>

        <div className="flex items-center gap-3">
          <label className="flex items-center gap-2 text-sm">
            <input
              type="checkbox"
              className="size-4"
              checked={editor.snap > 0}
              onChange={(event): void => {
                editor.setSnapEnabled(event.target.checked);
              }}
            />
            <Trans>Snap to grid</Trans>
          </label>
          <Button
            type="button"
            variant="outline"
            onClick={(): void => {
              editor.add(t`New cell`);
            }}
          >
            <Plus aria-hidden="true" />
            <Trans>Add cell</Trans>
          </Button>
          <Button
            type="button"
            disabled={!canSave}
            onClick={(): void => {
              onSave(savePayload());
            }}
          >
            {isSaving ? <Trans>Saving…</Trans> : <Trans>Save layout</Trans>}
          </Button>
          {onSaveAndApply !== undefined ? (
            <Button
              type="button"
              variant="secondary"
              disabled={!canSave}
              onClick={(): void => {
                onSaveAndApply(savePayload());
              }}
            >
              <Send aria-hidden="true" />
              <Trans>Save &amp; apply to engine</Trans>
            </Button>
          ) : null}
        </div>
      </div>

      <div className="flex flex-wrap items-end gap-6">
        <fieldset className="flex flex-wrap items-end gap-3">
          <legend className="mb-1 text-sm font-medium">
            <Trans>Canvas</Trans>
          </legend>
          <div className="flex flex-col gap-1">
            <Label htmlFor="layout-canvas-width" className="text-xs">
              <Trans>Width (px)</Trans>
            </Label>
            <Input
              id="layout-canvas-width"
              type="number"
              inputMode="numeric"
              min={1}
              className="w-24"
              value={editor.model.canvas.width}
              aria-invalid={issues.some((issue) => issue.path === 'canvas')}
              onChange={(event): void => {
                const parsed = Number.parseInt(event.target.value, 10);
                editor.setCanvas({
                  ...editor.model.canvas,
                  width: Number.isFinite(parsed) ? parsed : 0,
                });
              }}
            />
          </div>
          <div className="flex flex-col gap-1">
            <Label htmlFor="layout-canvas-height" className="text-xs">
              <Trans>Height (px)</Trans>
            </Label>
            <Input
              id="layout-canvas-height"
              type="number"
              inputMode="numeric"
              min={1}
              className="w-24"
              value={editor.model.canvas.height}
              aria-invalid={issues.some((issue) => issue.path === 'canvas')}
              onChange={(event): void => {
                const parsed = Number.parseInt(event.target.value, 10);
                editor.setCanvas({
                  ...editor.model.canvas,
                  height: Number.isFinite(parsed) ? parsed : 0,
                });
              }}
            />
          </div>
          <div className="flex flex-col gap-1">
            <Label htmlFor="layout-canvas-fps" className="text-xs">
              <Trans>Frame rate (rational)</Trans>
            </Label>
            <Input
              id="layout-canvas-fps"
              className="w-32"
              value={editor.model.canvas.fps}
              placeholder="30/1"
              aria-invalid={issues.some((issue) => issue.path === 'canvas.fps')}
              aria-describedby="layout-canvas-fps-hint"
              onChange={(event): void => {
                editor.setCanvas({
                  ...editor.model.canvas,
                  fps: event.target.value,
                });
              }}
            />
            <p id="layout-canvas-fps-hint" className="text-xs text-muted-foreground">
              <Trans>Exact "num/den", e.g. 30/1 or 30000/1001 — never a float.</Trans>
            </p>
          </div>
        </fieldset>

        <fieldset className="flex flex-wrap items-end gap-2">
          <legend className="mb-1 text-sm font-medium">
            <Trans>Seed from a preset</Trans>
          </legend>
          {LAYOUT_PRESETS.map((preset) => (
            <Button
              key={preset}
              type="button"
              variant="outline"
              size="sm"
              aria-label={`${t`Seed cells from preset`}: ${presetLabel(preset)}`}
              onClick={(): void => {
                seedPreset(preset);
              }}
            >
              <Grid2x2 aria-hidden="true" />
              {presetLabel(preset)}
            </Button>
          ))}
        </fieldset>
      </div>

      {issues.length > 0 ? (
        <div
          role="alert"
          className="rounded-md border border-destructive/50 bg-destructive/10 p-3"
        >
          <p className="mb-1 text-sm font-medium">
            <Trans>Fix these before saving:</Trans>
          </p>
          <ul className="list-disc ps-5 text-sm">
            {issues.map((issue) => (
              <li key={`${issue.path}:${issue.code}`}>
                <ValidationMessage code={issue.code} />
              </li>
            ))}
          </ul>
        </div>
      ) : null}

      <Tabs defaultValue="form">
        <TabsList>
          <TabsTrigger value="canvas">
            <Trans>Canvas</Trans>
          </TabsTrigger>
          <TabsTrigger value="form">
            <Trans>Cells (accessible)</Trans>
          </TabsTrigger>
        </TabsList>

        <TabsContent value="canvas" className="mt-4">
          <p className="mb-2 text-sm text-muted-foreground">
            <Trans>
              Drag, resize and rotate cells on the canvas, or drop a source from
              the palette. The same cells are fully editable on the Cells tab
              without a pointer.
            </Trans>
          </p>
          <SourcePalette
            sources={sources}
            overlays={overlays}
            onAddSource={addSourceCell}
            onAddOverlay={addOverlayCell}
          >
            <Suspense
              fallback={
                <p
                  role="status"
                  aria-live="polite"
                  className="rounded-md border bg-muted/30 p-8 text-center text-sm text-muted-foreground"
                >
                  <Trans>Loading canvas…</Trans>
                </p>
              }
            >
              <LayoutCanvas
                model={editor.model}
                selectedId={editor.selectedId}
                onSelect={editor.select}
                onMove={editor.move}
                onResize={editor.resize}
                onRotate={editor.rotate}
              />
            </Suspense>
          </SourcePalette>
        </TabsContent>

        <TabsContent value="form" className="mt-4">
          <section aria-label={t`Cells`}>
            <CellsForm
              cells={editor.model.cells}
              selectedId={editor.selectedId}
              sources={sources}
              onSelect={editor.select}
              onRename={editor.rename}
              onMove={editor.move}
              onResize={editor.resize}
              onRotate={editor.rotate}
              onFit={editor.setFit}
              onBindSource={editor.bindSource}
              onProps={editor.setProps}
              onRemove={editor.remove}
              onMoveDown={editor.moveDown}
              onMoveUp={editor.moveUp}
            />
          </section>
        </TabsContent>
      </Tabs>

      <p className="text-xs text-muted-foreground">
        <Trans>
          {editor.model.cells.length} cell(s) ·{' '}
          {editor.model.cells.filter((c) => c.sourceId !== undefined).length}{' '}
          bound to a source
        </Trans>{' '}
        <span className="sr-only">
          {editor.model.cells
            .map((c) =>
              c.sourceId !== undefined
                ? (sourceNames.get(c.sourceId) ?? c.sourceId)
                : '',
            )
            .filter((name) => name !== '')
            .join(', ')}
        </span>
      </p>

      <Dialog
        open={pendingPreset !== null}
        onOpenChange={(open): void => {
          if (!open) {
            setPendingPreset(null);
          }
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>
              <Trans>Replace the current cells?</Trans>
            </DialogTitle>
            <DialogDescription>
              <Trans>
                Seeding from a preset replaces every cell in this draft. Nothing
                is saved until you save the layout.
              </Trans>
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button
              variant="outline"
              onClick={(): void => {
                setPendingPreset(null);
              }}
            >
              <Trans>Cancel</Trans>
            </Button>
            <Button
              onClick={(): void => {
                if (pendingPreset !== null) {
                  editor.applyPreset(pendingPreset);
                }
                setPendingPreset(null);
              }}
            >
              <Trans>Replace cells</Trans>
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
