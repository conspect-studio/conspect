// The ACCESSIBLE, NON-CANVAS editing path for a layout.
//
// This is a fully keyboard-operable table/form that edits the SAME cells as the
// react-konva canvas (both drive the one `useLayoutEditor` state). Everything
// the canvas does by dragging — position, size, rotation, z-order, fit, source
// binding, add/remove — is achievable here with labelled controls and buttons,
// satisfying the dual-model requirement (WCAG 2.2 AA, accessibility.md): the
// editor is fully usable without the canvas.
//
// Geometry is shown/edited as PERCENT (0–100) for usability and converted to the
// normalized 0..1 model on commit; the model clamps it, so an out-of-range entry
// can never push a cell off-canvas.
import { useId } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { ArrowDown, ArrowUp, Trash2 } from 'lucide-react';

import { FIT_MODES } from '../model';
import type { CellModel, FitMode, NormalizedRect } from '../model';
import type { CellProperties } from '../cellProps';
import { CellPropertiesPanel } from './CellPropertiesPanel';
import type { SourceView } from '../../resources/types';
import { Button } from '../../components/ui/button';
import { Input } from '../../components/ui/input';
import { Label } from '../../components/ui/label';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '../../components/ui/select';

/** Props for {@link CellsForm}. */
export interface CellsFormProps {
  /** The cells in authoring order. */
  readonly cells: readonly CellModel[];
  /** The selected cell id (kept in sync with the canvas). */
  readonly selectedId: string | undefined;
  /** Available sources to bind (from the resource list). */
  readonly sources: readonly SourceView[];
  /** Select a cell. */
  readonly onSelect: (id: string) => void;
  /** Rename a cell. */
  readonly onRename: (id: string, label: string) => void;
  /** Move a cell's top-left (normalized). */
  readonly onMove: (id: string, x: number, y: number) => void;
  /** Resize a cell (normalized rect). */
  readonly onResize: (id: string, rect: NormalizedRect) => void;
  /** Rotate a cell (degrees). */
  readonly onRotate: (id: string, degrees: number) => void;
  /** Set a cell's fit mode. */
  readonly onFit: (id: string, fit: FitMode) => void;
  /** Bind/clear a cell's source. */
  readonly onBindSource: (id: string, sourceId: string | undefined) => void;
  /** Replace a cell's full property set (on_loss / appearance / degradation). */
  readonly onProps: (id: string, props: CellProperties) => void;
  /** Remove a cell. */
  readonly onRemove: (id: string) => void;
  /** Move a cell one step toward the back (lower z). */
  readonly onMoveDown: (index: number) => void;
  /** Move a cell one step toward the front (higher z). */
  readonly onMoveUp: (index: number) => void;
}

const NONE_SOURCE = '__none__';
const PERCENT = 100;

function toPercent(value: number): number {
  return Math.round(value * PERCENT);
}

function fromPercent(value: number): number {
  return value / PERCENT;
}

function parseNumber(value: string, fallback: number): number {
  const parsed = Number.parseFloat(value);
  return Number.isFinite(parsed) ? parsed : fallback;
}

/** A labelled number field used for percent geometry + rotation. */
function NumberField({
  label,
  value,
  min,
  max,
  onCommit,
}: {
  readonly label: string;
  readonly value: number;
  readonly min: number;
  readonly max: number;
  readonly onCommit: (next: number) => void;
}): JSX.Element {
  const id = useId();
  return (
    <div className="flex flex-col gap-1">
      <Label htmlFor={id} className="text-xs">
        {label}
      </Label>
      <Input
        id={id}
        type="number"
        inputMode="numeric"
        min={min}
        max={max}
        value={value}
        className="w-20"
        onChange={(event): void => {
          onCommit(parseNumber(event.target.value, value));
        }}
      />
    </div>
  );
}

/** One editable cell row (fieldset) in the accessible form. */
function CellRow({
  cell,
  index,
  total,
  selected,
  sources,
  labels,
  onSelect,
  onRename,
  onMove,
  onResize,
  onRotate,
  onFit,
  onBindSource,
  onProps,
  onRemove,
  onMoveDown,
  onMoveUp,
}: {
  readonly cell: CellModel;
  readonly index: number;
  readonly total: number;
  readonly selected: boolean;
  readonly sources: readonly SourceView[];
  readonly labels: {
    readonly name: string;
    readonly x: string;
    readonly y: string;
    readonly w: string;
    readonly h: string;
    readonly rotation: string;
    readonly fit: string;
    readonly source: string;
    readonly none: string;
    readonly select: string;
    readonly remove: string;
    readonly forward: string;
    readonly backward: string;
  };
  readonly onSelect: (id: string) => void;
  readonly onRename: (id: string, label: string) => void;
  readonly onMove: (id: string, x: number, y: number) => void;
  readonly onResize: (id: string, rect: NormalizedRect) => void;
  readonly onRotate: (id: string, degrees: number) => void;
  readonly onFit: (id: string, fit: FitMode) => void;
  readonly onBindSource: (id: string, sourceId: string | undefined) => void;
  readonly onProps: (id: string, props: CellProperties) => void;
  readonly onRemove: (id: string) => void;
  readonly onMoveDown: (index: number) => void;
  readonly onMoveUp: (index: number) => void;
}): JSX.Element {
  const nameId = useId();
  return (
    <fieldset
      className={`rounded-md border p-3 ${selected ? 'border-primary ring-1 ring-primary' : ''}`}
      onFocusCapture={(): void => {
        onSelect(cell.id);
      }}
    >
      <legend className="px-1 text-sm font-medium">
        <button
          type="button"
          className="rounded-sm underline-offset-2 hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
          aria-pressed={selected}
          aria-label={`${labels.select}: ${cell.label}`}
          onClick={(): void => {
            onSelect(cell.id);
          }}
        >
          <span lang="" dir="auto">
            {cell.label}
          </span>
        </button>
      </legend>

      <div className="flex flex-col gap-3">
        <div className="flex flex-col gap-1">
          <Label htmlFor={nameId} className="text-xs">
            {labels.name}
          </Label>
          <Input
            id={nameId}
            value={cell.label}
            lang=""
            dir="auto"
            onChange={(event): void => {
              onRename(cell.id, event.target.value);
            }}
          />
        </div>

        <div className="flex flex-wrap gap-3">
          <NumberField
            label={labels.x}
            value={toPercent(cell.rect.x)}
            min={0}
            max={100}
            onCommit={(next): void => {
              onMove(cell.id, fromPercent(next), cell.rect.y);
            }}
          />
          <NumberField
            label={labels.y}
            value={toPercent(cell.rect.y)}
            min={0}
            max={100}
            onCommit={(next): void => {
              onMove(cell.id, cell.rect.x, fromPercent(next));
            }}
          />
          <NumberField
            label={labels.w}
            value={toPercent(cell.rect.w)}
            min={1}
            max={100}
            onCommit={(next): void => {
              onResize(cell.id, { ...cell.rect, w: fromPercent(next) });
            }}
          />
          <NumberField
            label={labels.h}
            value={toPercent(cell.rect.h)}
            min={1}
            max={100}
            onCommit={(next): void => {
              onResize(cell.id, { ...cell.rect, h: fromPercent(next) });
            }}
          />
          <NumberField
            label={labels.rotation}
            value={Math.round(cell.rotation)}
            min={0}
            max={359}
            onCommit={(next): void => {
              onRotate(cell.id, next);
            }}
          />
        </div>

        <div className="flex flex-wrap items-end gap-3">
          <div className="flex flex-col gap-1">
            <Label className="text-xs" id={`${nameId}-fit`}>
              {labels.fit}
            </Label>
            <Select
              value={cell.fit}
              onValueChange={(value): void => {
                const fit = FIT_MODES.find((mode) => mode === value);
                if (fit !== undefined) {
                  onFit(cell.id, fit);
                }
              }}
            >
              <SelectTrigger aria-labelledby={`${nameId}-fit`} className="w-40">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {FIT_MODES.map((mode) => (
                  <SelectItem key={mode} value={mode}>
                    {mode}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>

          <div className="flex flex-col gap-1">
            <Label className="text-xs" id={`${nameId}-source`}>
              {labels.source}
            </Label>
            <Select
              value={cell.sourceId ?? NONE_SOURCE}
              onValueChange={(value): void => {
                onBindSource(cell.id, value === NONE_SOURCE ? undefined : value);
              }}
            >
              <SelectTrigger aria-labelledby={`${nameId}-source`} className="w-48">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value={NONE_SOURCE}>{labels.none}</SelectItem>
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
        </div>

        <CellPropertiesPanel
          idPrefix={cell.id}
          value={cell.props}
          onChange={(next): void => {
            onProps(cell.id, next);
          }}
        />

        <div className="flex items-center gap-2">
          <Button
            type="button"
            variant="outline"
            size="sm"
            disabled={index >= total - 1}
            aria-label={`${labels.forward}: ${cell.label}`}
            onClick={(): void => {
              onMoveUp(index);
            }}
          >
            <ArrowUp aria-hidden="true" />
            <Trans>Forward</Trans>
          </Button>
          <Button
            type="button"
            variant="outline"
            size="sm"
            disabled={index <= 0}
            aria-label={`${labels.backward}: ${cell.label}`}
            onClick={(): void => {
              onMoveDown(index);
            }}
          >
            <ArrowDown aria-hidden="true" />
            <Trans>Backward</Trans>
          </Button>
          <span className="text-xs text-muted-foreground tabular-nums">
            <Trans>z = {cell.z}</Trans>
          </span>
          <Button
            type="button"
            variant="destructive"
            size="sm"
            className="ms-auto"
            aria-label={`${labels.remove}: ${cell.label}`}
            onClick={(): void => {
              onRemove(cell.id);
            }}
          >
            <Trash2 aria-hidden="true" />
            <Trans>Remove</Trans>
          </Button>
        </div>
      </div>
    </fieldset>
  );
}

/** The accessible cells editor (the non-canvas path). */
export function CellsForm(props: CellsFormProps): JSX.Element {
  const { t } = useLingui();
  const { cells } = props;

  const labels = {
    name: t`Cell name`,
    x: t`Left (%)`,
    y: t`Top (%)`,
    w: t`Width (%)`,
    h: t`Height (%)`,
    rotation: t`Rotation (°)`,
    fit: t`Fit`,
    source: t`Source`,
    none: t`No source`,
    select: t`Select cell`,
    remove: t`Remove cell`,
    forward: t`Bring forward`,
    backward: t`Send backward`,
  };

  if (cells.length === 0) {
    return (
      <p className="text-sm text-muted-foreground">
        <Trans>No cells yet. Add one to start composing the multiview.</Trans>
      </p>
    );
  }

  return (
    <div className="flex flex-col gap-3">
      {cells.map((cell, index) => (
        <CellRow
          key={cell.id}
          cell={cell}
          index={index}
          total={cells.length}
          selected={cell.id === props.selectedId}
          sources={props.sources}
          labels={labels}
          onSelect={props.onSelect}
          onRename={props.onRename}
          onMove={props.onMove}
          onResize={props.onResize}
          onRotate={props.onRotate}
          onFit={props.onFit}
          onBindSource={props.onBindSource}
          onProps={props.onProps}
          onRemove={props.onRemove}
          onMoveDown={props.onMoveDown}
          onMoveUp={props.onMoveUp}
        />
      ))}
    </div>
  );
}
