// The GRID layout editor — the editing surface for `kind = "grid"` bodies
// (what every real config uses). Composes, around one `useGridEditor` state:
//   * track chip editors (columns/rows) + the three gap controls;
//   * the keyboard-operable AREAS MATRIX (role=grid) with rectangle
//     assignment, mirroring the Rust solver's contiguity rules;
//   * a per-area cell panel (source binding, fit, z, and the shared full
//     Cell property panel — failover slate, appearance, degradation);
//   * a READ-ONLY konva preview of the solved placement (lazy, like the
//     free-form canvas); and
//   * live validation (errors block save; warnings advise) + save controls
//     that PUT the body losslessly (canvas + unrendered fields preserved).
//
// The explicit, one-way "Convert to free-form" materializes the solved rects
// into an absolute layout for the free-form editor; nothing converts silently.
import { lazy, Suspense, useMemo, useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { Plus, Send, Shuffle, Trash2 } from 'lucide-react';

import { useGridEditor } from '../useGridEditor';
import {
  areaNames,
  editableCells,
  gridToLayoutModel,
  solveGridToRects,
  toGridLayoutBody,
} from '../gridModel';
import type { GridModel, TrackAxis } from '../gridModel';
import { FIT_MODES } from '../model';
import type { LayoutModel } from '../model';
import type { SourceView } from '../../resources/types';
import { AreaMatrixEditor } from './AreaMatrixEditor';
import { CellPropertiesPanel } from './CellPropertiesPanel';
import { GridValidationMessage } from './validationMessages';
import type { LayoutSavePayload } from './LayoutEditor';
import { HelpLink } from '../../components/HelpLink';
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
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '../../components/ui/select';
import {
  Tabs,
  TabsContent,
  TabsList,
  TabsTrigger,
} from '../../components/ui/tabs';

// The konva preview is heavy; code-split it exactly like LayoutCanvas.
const GridPreviewCanvas = lazy(() =>
  import('./GridPreviewCanvas').then((mod) => ({ default: mod.GridPreviewCanvas })),
);

const NONE_SOURCE = '__none__';
const DEFAULT_FIT = '__default__';

/** Props for {@link GridLayoutEditor}. */
export interface GridLayoutEditorProps {
  /** The loaded grid model. */
  readonly initial: GridModel;
  /** Sources available to bind. */
  readonly sources: readonly SourceView[];
  /** Called with the serialized payload on save. */
  readonly onSave: (payload: LayoutSavePayload) => void;
  /** Save, then apply to the running engine (omitted ⇒ button not offered). */
  readonly onSaveAndApply?: (payload: LayoutSavePayload) => void;
  /** Explicit one-way conversion to the free-form editor. */
  readonly onConvertToFreeForm: (model: LayoutModel) => void;
  /** Whether a save is in flight. */
  readonly isSaving?: boolean;
}

/** Parse a gap field: '' ⇒ absent, otherwise the number (validated live). */
function gapOf(raw: string): number | undefined {
  if (raw.trim() === '') {
    return undefined;
  }
  const parsed = Number(raw);
  return Number.isFinite(parsed) ? parsed : undefined;
}

/** One axis of editable track chips. */
function TrackChips({
  axis,
  tracks,
  legend,
  addLabel,
  removeLabel,
  onSet,
  onAdd,
  onRemove,
}: {
  readonly axis: TrackAxis;
  readonly tracks: readonly string[];
  readonly legend: string;
  readonly addLabel: string;
  readonly removeLabel: (index: number) => string;
  readonly onSet: (axis: TrackAxis, index: number, value: string) => void;
  readonly onAdd: (axis: TrackAxis) => void;
  readonly onRemove: (axis: TrackAxis, index: number) => void;
}): JSX.Element {
  return (
    <fieldset className="flex flex-wrap items-end gap-2">
      <legend className="mb-1 text-sm font-medium">{legend}</legend>
      {tracks.map((track, index) => (
        // Tracks are positional; the index is the stable identity.
        <span key={index} className="flex items-center gap-1 rounded-md border p-1">
          <Input
            value={track}
            className="h-8 w-20"
            aria-label={`${legend} ${String(index + 1)}`}
            data-testid={`track-${axis}-${String(index)}`}
            onChange={(event): void => {
              onSet(axis, index, event.target.value);
            }}
          />
          <Button
            type="button"
            variant="ghost"
            size="sm"
            disabled={tracks.length <= 1}
            aria-label={removeLabel(index)}
            onClick={(): void => {
              onRemove(axis, index);
            }}
          >
            <Trash2 aria-hidden="true" />
          </Button>
        </span>
      ))}
      <Button
        type="button"
        variant="outline"
        size="sm"
        data-testid={`add-track-${axis}`}
        onClick={(): void => {
          onAdd(axis);
        }}
      >
        <Plus aria-hidden="true" />
        {addLabel}
      </Button>
    </fieldset>
  );
}

/** The composed grid layout editor. */
export function GridLayoutEditor({
  initial,
  sources,
  onSave,
  onSaveAndApply,
  onConvertToFreeForm,
  isSaving = false,
}: GridLayoutEditorProps): JSX.Element {
  const { t } = useLingui();
  const editor = useGridEditor(initial);
  const [renameDraft, setRenameDraft] = useState('');
  const [renameError, setRenameError] = useState('');
  const [confirmConvert, setConfirmConvert] = useState(false);
  const [convertError, setConvertError] = useState(false);

  const model = editor.model;
  const names = useMemo(() => areaNames(model), [model]);
  const rects = useMemo(() => solveGridToRects(model), [model]);
  const sourceNames = useMemo(() => {
    const map = new Map<string, string>();
    for (const source of sources) {
      map.set(source.id, source.name);
    }
    return map;
  }, [sources]);
  const sourceLabels = useMemo(() => {
    const map = new Map<string, string>();
    for (const cell of editableCells(model)) {
      if (cell.sourceId !== undefined) {
        map.set(cell.area, sourceNames.get(cell.sourceId) ?? cell.sourceId);
      }
    }
    return map;
  }, [model, sourceNames]);

  const errors = editor.issues.filter((issue) => issue.severity === 'error');
  const warnings = editor.issues.filter((issue) => issue.severity === 'warning');
  const canSave = editor.isSavable && !isSaving;

  const savePayload = (): LayoutSavePayload => ({
    id: model.id,
    name: model.name,
    body: toGridLayoutBody(model),
  });

  const selectedArea = editor.selectedArea;
  const selectedCell = editor.selectedCell;
  const rawCellCount = model.cells.length - editableCells(model).length;
  // Cells whose area no longer exists in the matrix (e.g. after an
  // assignment overwrote it). The engine rejects such a document, so saving
  // is blocked until each one is re-pointed at a live area or removed.
  const orphanCells = editableCells(model).filter(
    (cell) => !names.includes(cell.area),
  );

  const convertNow = (): void => {
    const absolute = gridToLayoutModel(model);
    setConfirmConvert(false);
    if (absolute === undefined) {
      setConvertError(true);
      return;
    }
    setConvertError(false);
    onConvertToFreeForm(absolute);
  };

  return (
    <div className="flex flex-col gap-6">
      <div className="flex flex-wrap items-end justify-between gap-4">
        <div className="flex flex-col gap-1">
          <Label htmlFor="grid-layout-name">
            <Trans>Layout name</Trans>
          </Label>
          <Input
            id="grid-layout-name"
            value={model.name}
            lang=""
            dir="auto"
            className="w-72"
            aria-invalid={model.name.trim() === ''}
            placeholder={t`e.g. Main wall`}
            onChange={(event): void => {
              editor.setName(event.target.value);
            }}
          />
        </div>

        <div className="flex items-center gap-3">
          <Button
            type="button"
            variant="outline"
            data-testid="convert-freeform"
            onClick={(): void => {
              setConfirmConvert(true);
            }}
          >
            <Shuffle aria-hidden="true" />
            <Trans>Convert to free-form</Trans>
          </Button>
          <Button
            type="button"
            disabled={!canSave}
            data-testid="grid-save"
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
              data-testid="grid-save-apply"
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

      <p className="text-sm text-muted-foreground">
        <Trans>
          A grid layout places cells by named areas on column/row tracks —
          resize whole rows and columns at once, with even gaps.
        </Trans>{' '}
        <HelpLink to="/help/features#layouts" label={t`About layouts`} />
      </p>

      <div className="flex flex-wrap items-end gap-6">
        <fieldset className="flex flex-wrap items-end gap-3">
          <legend className="mb-1 text-sm font-medium">
            <Trans>Canvas</Trans>
          </legend>
          <div className="flex flex-col gap-1">
            <Label htmlFor="grid-canvas-width" className="text-xs">
              <Trans>Width (px)</Trans>
            </Label>
            <Input
              id="grid-canvas-width"
              type="number"
              inputMode="numeric"
              min={1}
              className="w-24"
              value={model.canvas.width}
              aria-invalid={errors.some((issue) => issue.path === 'canvas')}
              onChange={(event): void => {
                const parsed = Number.parseInt(event.target.value, 10);
                editor.setCanvas({
                  ...model.canvas,
                  width: Number.isFinite(parsed) ? parsed : 0,
                });
              }}
            />
          </div>
          <div className="flex flex-col gap-1">
            <Label htmlFor="grid-canvas-height" className="text-xs">
              <Trans>Height (px)</Trans>
            </Label>
            <Input
              id="grid-canvas-height"
              type="number"
              inputMode="numeric"
              min={1}
              className="w-24"
              value={model.canvas.height}
              aria-invalid={errors.some((issue) => issue.path === 'canvas')}
              onChange={(event): void => {
                const parsed = Number.parseInt(event.target.value, 10);
                editor.setCanvas({
                  ...model.canvas,
                  height: Number.isFinite(parsed) ? parsed : 0,
                });
              }}
            />
          </div>
          <div className="flex flex-col gap-1">
            <Label htmlFor="grid-canvas-fps" className="text-xs">
              <Trans>Frame rate (rational)</Trans>
            </Label>
            <Input
              id="grid-canvas-fps"
              className="w-32"
              value={model.canvas.fps}
              placeholder="30/1"
              aria-invalid={errors.some((issue) => issue.path === 'canvas.fps')}
              onChange={(event): void => {
                editor.setCanvas({ ...model.canvas, fps: event.target.value });
              }}
            />
          </div>
        </fieldset>

        <fieldset className="flex flex-wrap items-end gap-3">
          <legend className="mb-1 text-sm font-medium">
            <Trans>Gaps (px)</Trans>
          </legend>
          <div className="flex flex-col gap-1">
            <Label htmlFor="grid-gap" className="text-xs">
              <Trans>Gap</Trans>
            </Label>
            <Input
              id="grid-gap"
              type="number"
              inputMode="numeric"
              min={0}
              className="w-20"
              value={model.gap ?? ''}
              aria-invalid={errors.some((issue) => issue.path === 'layout.gap')}
              onChange={(event): void => {
                editor.setGap(gapOf(event.target.value));
              }}
            />
          </div>
          <div className="flex flex-col gap-1">
            <Label htmlFor="grid-row-gap" className="text-xs">
              <Trans>Row gap</Trans>
            </Label>
            <Input
              id="grid-row-gap"
              type="number"
              inputMode="numeric"
              min={0}
              className="w-20"
              value={model.rowGap ?? ''}
              aria-invalid={errors.some((issue) => issue.path === 'layout.row_gap')}
              onChange={(event): void => {
                editor.setRowGap(gapOf(event.target.value));
              }}
            />
          </div>
          <div className="flex flex-col gap-1">
            <Label htmlFor="grid-column-gap" className="text-xs">
              <Trans>Column gap</Trans>
            </Label>
            <Input
              id="grid-column-gap"
              type="number"
              inputMode="numeric"
              min={0}
              className="w-20"
              value={model.columnGap ?? ''}
              aria-invalid={errors.some((issue) => issue.path === 'layout.column_gap')}
              onChange={(event): void => {
                editor.setColumnGap(gapOf(event.target.value));
              }}
            />
          </div>
        </fieldset>
      </div>

      <div className="flex flex-wrap gap-6">
        <TrackChips
          axis="columns"
          tracks={model.columns}
          legend={t`Columns`}
          addLabel={t`Add column`}
          removeLabel={(index): string => `${t`Remove column`} ${String(index + 1)}`}
          onSet={editor.setTrack}
          onAdd={editor.addTrack}
          onRemove={editor.removeTrack}
        />
        <TrackChips
          axis="rows"
          tracks={model.rows}
          legend={t`Rows`}
          addLabel={t`Add row`}
          removeLabel={(index): string => `${t`Remove row`} ${String(index + 1)}`}
          onSet={editor.setTrack}
          onAdd={editor.addTrack}
          onRemove={editor.removeTrack}
        />
      </div>

      {errors.length > 0 ? (
        <div
          role="alert"
          className="rounded-md border border-destructive/50 bg-destructive/10 p-3"
        >
          <p className="mb-1 text-sm font-medium">
            <Trans>Fix these before saving:</Trans>
          </p>
          <ul className="list-disc ps-5 text-sm">
            {errors.map((issue) => (
              <li key={`${issue.path}:${issue.code}`}>
                <GridValidationMessage code={issue.code} />
              </li>
            ))}
          </ul>
        </div>
      ) : null}
      {warnings.length > 0 ? (
        <div role="note" className="rounded-md border bg-muted/40 p-3">
          <p className="mb-1 text-sm font-medium">
            <Trans>Advisories (saving still allowed):</Trans>
          </p>
          <ul className="list-disc ps-5 text-sm">
            {warnings.map((issue) => (
              <li key={`${issue.path}:${issue.code}`}>
                <span className="text-muted-foreground">{issue.path}</span>{' '}
                <GridValidationMessage code={issue.code} />
              </li>
            ))}
          </ul>
        </div>
      ) : null}
      {convertError ? (
        <p role="alert" className="text-sm text-destructive">
          <Trans>
            Cannot convert: fix the grid errors first so every area solves to a
            rectangle.
          </Trans>
        </p>
      ) : null}

      {orphanCells.length > 0 ? (
        <section
          aria-label={t`Orphaned cells`}
          className="rounded-md border border-destructive/50 p-3"
        >
          <p className="mb-2 text-sm">
            <Trans>
              These cells reference areas that are no longer in the grid. Move
              each to a live area (its source binding is kept) or remove it.
            </Trans>
          </p>
          <ul className="flex flex-col gap-2">
            {orphanCells.map((cell) => (
              <li key={cell.id} className="flex flex-wrap items-center gap-3">
                <code lang="" dir="auto" className="text-xs">
                  {cell.id}
                </code>
                <span className="text-xs text-muted-foreground">
                  <Trans>missing area: {cell.area}</Trans>
                </span>
                <Select
                  onValueChange={(value): void => {
                    editor.updateCell(cell.area, (current) => ({
                      ...current,
                      area: value,
                    }));
                  }}
                >
                  <SelectTrigger
                    aria-label={`${t`Move cell to area`}: ${cell.id}`}
                    className="w-36"
                    data-testid={`orphan-move-${cell.id}`}
                  >
                    <SelectValue placeholder={t`Move to area…`} />
                  </SelectTrigger>
                  <SelectContent>
                    {names.map((area) => (
                      <SelectItem key={area} value={area}>
                        <span lang="" dir="auto">
                          {area}
                        </span>
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
                <Button
                  type="button"
                  variant="destructive"
                  size="sm"
                  aria-label={`${t`Remove cell`}: ${cell.id}`}
                  data-testid={`orphan-remove-${cell.id}`}
                  onClick={(): void => {
                    editor.removeCellForArea(cell.area);
                  }}
                >
                  <Trash2 aria-hidden="true" />
                  <Trans>Remove</Trans>
                </Button>
              </li>
            ))}
          </ul>
        </section>
      ) : null}

      <Tabs defaultValue="areas">
        <TabsList>
          <TabsTrigger value="areas">
            <Trans>Areas &amp; cells</Trans>
          </TabsTrigger>
          <TabsTrigger value="preview" data-testid="grid-preview-tab">
            <Trans>Preview</Trans>
          </TabsTrigger>
        </TabsList>

        <TabsContent value="areas" className="mt-4">
          <div className="flex flex-wrap gap-8">
            <section aria-label={t`Grid areas`}>
              <AreaMatrixEditor
                matrix={model.areaMatrix}
                selectedArea={selectedArea}
                onSelectArea={editor.selectArea}
                onAssign={editor.assignArea}
              />
            </section>

            <section aria-label={t`Area cell`} className="min-w-72 flex-1">
              <div className="mb-3 flex flex-wrap gap-2" role="group" aria-label={t`Areas`}>
                {names.map((area) => (
                  <Button
                    key={area}
                    type="button"
                    size="sm"
                    variant={area === selectedArea ? 'default' : 'outline'}
                    data-testid={`area-chip-${area}`}
                    onClick={(): void => {
                      editor.selectArea(area);
                    }}
                  >
                    <span lang="" dir="auto">
                      {area}
                    </span>
                  </Button>
                ))}
              </div>

              {selectedArea === undefined ? (
                <p className="text-sm text-muted-foreground">
                  <Trans>Select an area to edit its cell.</Trans>
                </p>
              ) : (
                <div className="flex flex-col gap-4">
                  <div className="flex items-end gap-2">
                    <div className="flex flex-col gap-1">
                      <Label htmlFor="area-rename" className="text-xs">
                        <Trans>Rename area {selectedArea}</Trans>
                      </Label>
                      <Input
                        id="area-rename"
                        value={renameDraft}
                        className="w-40"
                        lang=""
                        dir="auto"
                        placeholder={selectedArea}
                        data-testid="rename-area-input"
                        onChange={(event): void => {
                          setRenameDraft(event.target.value);
                        }}
                      />
                    </div>
                    <Button
                      type="button"
                      variant="outline"
                      data-testid="rename-area"
                      onClick={(): void => {
                        const result = editor.renameArea(selectedArea, renameDraft);
                        if (result.ok) {
                          setRenameDraft('');
                          setRenameError('');
                        } else if (result.code === 'name-invalid') {
                          setRenameError(t`Enter a single-word area name (no spaces).`);
                        } else {
                          setRenameError(
                            t`Cannot rename: area(s) ${result.areas.join(', ')} would no longer be a rectangle.`,
                          );
                        }
                      }}
                    >
                      <Trans>Rename</Trans>
                    </Button>
                  </div>
                  {renameError !== '' ? (
                    <p role="alert" className="text-xs text-destructive">
                      {renameError}
                    </p>
                  ) : null}

                  {selectedCell === undefined ? (
                    <div className="flex items-center gap-3">
                      <p className="text-sm text-muted-foreground">
                        <Trans>This area has no cell yet.</Trans>
                      </p>
                      <Button
                        type="button"
                        variant="outline"
                        size="sm"
                        onClick={(): void => {
                          editor.ensureCell(selectedArea);
                        }}
                      >
                        <Plus aria-hidden="true" />
                        <Trans>Add cell</Trans>
                      </Button>
                    </div>
                  ) : (
                    <div className="flex flex-col gap-3">
                      <p className="text-xs text-muted-foreground">
                        <Trans>Cell id:</Trans>{' '}
                        <code lang="" dir="auto">
                          {selectedCell.id}
                        </code>
                      </p>

                      <div className="flex flex-wrap items-end gap-3">
                        <div className="flex flex-col gap-1">
                          <Label className="text-xs" id="area-cell-source-label">
                            <Trans>Source</Trans>
                          </Label>
                          <Select
                            value={selectedCell.sourceId ?? NONE_SOURCE}
                            onValueChange={(value): void => {
                              editor.updateCell(selectedArea, (cell) => ({
                                ...cell,
                                sourceId: value === NONE_SOURCE ? undefined : value,
                              }));
                            }}
                          >
                            <SelectTrigger
                              aria-labelledby="area-cell-source-label"
                              className="w-48"
                              data-testid="area-cell-source"
                            >
                              <SelectValue />
                            </SelectTrigger>
                            <SelectContent>
                              <SelectItem value={NONE_SOURCE}>
                                {t`No source`}
                              </SelectItem>
                              {sources.map((source) => (
                                <SelectItem key={source.id} value={source.id}>
                                  <span lang="" dir="auto">
                                    {source.name}
                                  </span>{' '}
                                  <span className="text-xs text-muted-foreground">
                                    ({source.kind} · {source.id})
                                  </span>
                                </SelectItem>
                              ))}
                            </SelectContent>
                          </Select>
                        </div>

                        <div className="flex flex-col gap-1">
                          <Label className="text-xs" id="area-cell-fit-label">
                            <Trans>Fit</Trans>
                          </Label>
                          <Select
                            value={selectedCell.fit ?? DEFAULT_FIT}
                            onValueChange={(value): void => {
                              editor.updateCell(selectedArea, (cell) => ({
                                ...cell,
                                fit:
                                  FIT_MODES.find((mode) => mode === value) ??
                                  (value === DEFAULT_FIT ? undefined : cell.fit),
                              }));
                            }}
                          >
                            <SelectTrigger
                              aria-labelledby="area-cell-fit-label"
                              className="w-40"
                            >
                              <SelectValue />
                            </SelectTrigger>
                            <SelectContent>
                              <SelectItem value={DEFAULT_FIT}>
                                {t`Default (contain)`}
                              </SelectItem>
                              {FIT_MODES.map((mode) => (
                                <SelectItem key={mode} value={mode}>
                                  {mode}
                                </SelectItem>
                              ))}
                            </SelectContent>
                          </Select>
                        </div>

                        <div className="flex flex-col gap-1">
                          <Label htmlFor="area-cell-z" className="text-xs">
                            <Trans>Z order</Trans>
                          </Label>
                          <Input
                            id="area-cell-z"
                            type="number"
                            inputMode="numeric"
                            className="w-20"
                            value={selectedCell.z ?? ''}
                            onChange={(event): void => {
                              const raw = event.target.value;
                              const parsed = Number.parseInt(raw, 10);
                              editor.updateCell(selectedArea, (cell) => ({
                                ...cell,
                                z:
                                  raw.trim() === '' || !Number.isFinite(parsed)
                                    ? undefined
                                    : parsed,
                              }));
                            }}
                          />
                        </div>
                      </div>

                      <CellPropertiesPanel
                        idPrefix={`grid-${selectedCell.id}`}
                        value={selectedCell.props}
                        onChange={(next): void => {
                          editor.updateCell(selectedArea, (cell) => ({
                            ...cell,
                            props: next,
                          }));
                        }}
                      />

                      <Button
                        type="button"
                        variant="destructive"
                        size="sm"
                        className="self-start"
                        onClick={(): void => {
                          editor.removeCellForArea(selectedArea);
                        }}
                      >
                        <Trash2 aria-hidden="true" />
                        <Trans>Remove cell</Trans>
                      </Button>
                    </div>
                  )}
                </div>
              )}
            </section>
          </div>
        </TabsContent>

        <TabsContent value="preview" className="mt-4">
          {rects === undefined ? (
            <p role="status" className="text-sm text-muted-foreground">
              <Trans>
                The grid cannot be solved yet — fix the validation errors to see
                the placement preview.
              </Trans>
            </p>
          ) : (
            <Suspense
              fallback={
                <p
                  role="status"
                  aria-live="polite"
                  className="rounded-md border bg-muted/30 p-8 text-center text-sm text-muted-foreground"
                >
                  <Trans>Loading preview…</Trans>
                </p>
              }
            >
              <GridPreviewCanvas
                canvas={model.canvas}
                rects={rects}
                sourceLabels={sourceLabels}
                selectedArea={selectedArea}
              />
            </Suspense>
          )}
        </TabsContent>
      </Tabs>

      <p className="text-xs text-muted-foreground">
        <Trans>
          {names.length} area(s) · {editableCells(model).length} cell(s) ·{' '}
          {sourceLabels.size} bound to a source
        </Trans>
        {rawCellCount > 0 ? (
          <>
            {' '}
            <Trans>
              ({rawCellCount} absolutely-placed cell(s) ride along unchanged —
              edit them via Convert to free-form.)
            </Trans>
          </>
        ) : null}
      </p>

      <Dialog
        open={confirmConvert}
        onOpenChange={(open): void => {
          if (!open) {
            setConfirmConvert(false);
          }
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>
              <Trans>Convert to a free-form layout?</Trans>
            </DialogTitle>
            <DialogDescription>
              <Trans>
                This is one-way: the grid's tracks and areas become fixed
                rectangles you move and resize individually. Nothing is saved
                until you save the layout.
              </Trans>
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button
              variant="outline"
              onClick={(): void => {
                setConfirmConvert(false);
              }}
            >
              <Trans>Cancel</Trans>
            </Button>
            <Button data-testid="confirm-convert" onClick={convertNow}>
              <Trans>Convert</Trans>
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
