// MetadataInspector — the per-asset metadata surface of the library
// workspace. The repository's law is TWO metadata families
// (media-playout.md §212-257, carried in library-evidence.md §4):
//   * PROBED / measured = machine truth (cadence, length, codec, size,
//     hash) — read-only, never authored here, every absent value renders
//     an honest "Not measured" fallback — never NaN/undefined.
//   * DECLARED = operator intent (name, trims, rights) — editable through
//     controlled inputs that emit INTENT callbacks; the caller applies.
//
// Honesty rules pinned here:
//   * Trims are EXACT grid boundaries: the integer index plus the exact
//     rational pair (or integer sample rate) is always shown; steppers
//     propose adjacent indices clamped to the measured length and never
//     across each other (`in < out` — the config invariant media.rs:78).
//     A proposal is an intent, never an applied edit.
//   * Rights are a DECLARED family: `unknown` renders as the visible
//     warning chip and is never presented as cleared; only the operator's
//     explicit select can declare a status, via `onRightsChange`.
//   * Provenance and usage are read-only records (no lineage table,
//     no reference-count query exists — audit §4, ⚠A9).
//   * A `supported: false` edit gate renders the declared section
//     read-only with the caller's reason — never a fake editable state.
import type { JSX, ReactNode } from 'react';
import { useLingui } from '@lingui/react/macro';

import { Badge } from '../../ui/badge';
import { Input } from '../../ui/input';
import { TouchAction } from '../TouchAction';
import {
  describeGrid,
  formatAssetDuration,
  groupThousands,
  type AssetDurationSource,
  type DurationGrid,
  type RationalBoundary,
} from '../duration';
import {
  boundaryAtGridIndex,
  compareBoundaries,
  describeBoundaryUnit,
  formatBoundaryLabel,
  isSameGrid,
  stepBoundary,
} from './boundaries';
import { InUseChip, RightsChip } from './Chips';
import type {
  ActionGate,
  LibraryOriginKind,
  LibraryRightsStatus,
} from './types';

/** The measured (probed, machine-truth) family — read-only by law. */
export interface MeasuredMetadata {
  /** The asset's measured cadence; null = not probed. */
  readonly grid: DurationGrid | null;
  /** Length in the grid's own integer units; null = not measured. */
  readonly units: number | null;
  readonly codec: string | null;
  readonly widthPx: number | null;
  readonly heightPx: number | null;
  /** Exact integer byte size; null = unknown. */
  readonly byteSize: number | null;
  readonly contentHash: string | null;
}

/** The declared (operator-intent) family — edited through intents. */
export interface DeclaredMetadata {
  /** Trim-in as an exact grid boundary; null = not set. */
  readonly inBoundary: RationalBoundary | null;
  /** Trim-out as an exact grid boundary; null = not set. */
  readonly outBoundary: RationalBoundary | null;
  readonly tags: readonly string[];
  readonly rights: LibraryRightsStatus;
  /** Already-localized expiry note, shown for `expired` rights. */
  readonly rightsExpiryLabel: string | null;
}

/** The read-only provenance record (no lineage query exists — ⚠A9). */
export interface ProvenanceRecord {
  readonly originKind: LibraryOriginKind;
  readonly originDetail: string | null;
  /** Already-localized capture/creation time; null = unknown. */
  readonly capturedAtLabel: string | null;
  /** Source URL for fetched assets; null = not a fetch / unknown. */
  readonly sourceUrl: string | null;
  /** The parent asset's name for derivatives; null = not a derivative. */
  readonly parentAssetName: string | null;
}

/** The read-only usage record: slots currently referencing the asset. */
export interface UsageRecord {
  readonly slotReferences: readonly {
    readonly slotName: string;
    readonly onAir: boolean;
  }[];
}

/** Everything the inspector renders for one asset. */
export interface LibraryAssetMetadata {
  readonly id: string;
  readonly name: string;
  readonly kind: 'video' | 'audio' | 'clip';
  readonly measured: MeasuredMetadata;
  readonly declared: DeclaredMetadata;
  readonly provenance: ProvenanceRecord;
  readonly usage: UsageRecord;
}

/** Props for {@link MetadataInspector}. */
export interface MetadataInspectorProps {
  /** The asset whose metadata is rendered. */
  readonly asset: LibraryAssetMetadata;
  /**
   * Capability gate for DECLARED edits (the metadata mutation contract).
   * `supported: false` renders the declared section read-only with the
   * caller's reason — never a fake editable state.
   */
  readonly editGate: ActionGate;
  /** Intent: the operator changed the asset name. */
  readonly onNameChange?: ((nextName: string) => void) | undefined;
  /**
   * Intent: a trim stepper proposes the adjacent exact boundary for one
   * bound, clamped to the measured length; `in < out` is never crossed.
   */
  readonly onTrimBoundaryChange?: ((bound: 'in' | 'out', next: RationalBoundary) => void) | undefined;
  /** Intent: the operator explicitly declared a rights status. */
  readonly onRightsChange?: ((next: LibraryRightsStatus) => void) | undefined;
}

/** One read-only label + value row; `null` values render honest fallbacks. */
function Row({
  label,
  children,
}: {
  readonly label: ReactNode;
  readonly children: ReactNode;
}): JSX.Element {
  return (
    <div className="flex min-w-0 flex-col gap-x-3 gap-y-1 py-1 md:flex-row md:items-baseline">
      <dt className="min-w-24 shrink-0 text-xs font-medium text-muted-foreground">
        {label}
      </dt>
      <dd className="min-w-0 flex-1 break-words text-sm">{children}</dd>
    </div>
  );
}

/** The measured-metadata header note (localized once). */
function SectionHeading({
  children,
  note,
}: {
  readonly children: ReactNode;
  readonly note?: ReactNode | undefined;
}): JSX.Element {
  return (
    <div className="flex min-w-0 flex-col gap-1">
      <h3 className="min-w-0 break-words text-sm font-semibold">{children}</h3>
      {note !== undefined ? (
        <p className="min-w-0 break-words text-xs text-muted-foreground">{note}</p>
      ) : null}
    </div>
  );
}

/**
 * The per-asset metadata inspector: measured read-only truth, declared
 * operator intent behind capability gates, provenance/usage records.
 */
export function MetadataInspector({
  asset,
  editGate,
  onNameChange,
  onTrimBoundaryChange,
  onRightsChange,
}: MetadataInspectorProps): JSX.Element {
  const { t } = useLingui();
  const editable = editGate.supported;
  const { measured, declared, provenance, usage } = asset;

  const measuredLengthBoundary =
    measured.grid !== null && measured.units !== null
      ? boundaryAtGridIndex(measured.grid, measured.units)
      : null;

  const durationFormat = formatAssetDuration({
    grid: measured.grid,
    units: measured.units,
  } satisfies AssetDurationSource);

  const rightsOptions: readonly LibraryRightsStatus[] = [
    'cleared',
    'unknown',
    'expired',
  ];

  const rightsLabel = (status: LibraryRightsStatus): string => {
    if (status === 'cleared') {
      return t`Cleared`;
    }
    return status === 'expired' ? t`Expired` : t`Unknown`;
  };

  const originLabel = (kind: LibraryOriginKind): string => {
    switch (kind) {
      case 'captured':
        return t`Captured`;
      case 'imported':
        return t`Imported`;
      case 'fetched':
        return t`Fetched`;
      case 'recovered':
        return t`Recovered`;
      case 'derived':
        return t`Derived clip`;
      default:
        return t`Origin unknown`;
    }
  };

  return (
    <section
      data-testid="metadata-inspector"
      data-asset-id={asset.id}
      data-edit-available={editable ? 'true' : 'false'}
      aria-label={t`Asset metadata`}
      className="flex min-w-0 flex-col gap-4"
    >
      <SectionHeading note={t`Metadata for one library asset.`}>
        {t`Metadata`}
      </SectionHeading>

      {/* DECLARED — operator intent, the only editable family. */}
      <div
        data-testid="metadata-declared"
        className="flex min-w-0 flex-col gap-2 rounded-lg border p-3"
      >
        <SectionHeading
          note={
            editable
              ? t`Declared — operator intent. Edits emit intents; the engine applies them.`
              : t`Declared — operator intent (read-only while editing is unavailable).`
          }
        >
          {t`Declared`}
        </SectionHeading>

        <Row label={t`Name`}>
          {editable && onNameChange !== undefined ? (
            <Input
              value={asset.name}
              onChange={(event): void => {
                onNameChange(event.currentTarget.value);
              }}
              aria-label={t`Asset name`}
              className="h-12 min-w-0 max-w-full"
            />
          ) : (
            <span className="min-w-0 break-words">{asset.name}</span>
          )}
        </Row>

        <Row label={t`Trims`}>
          <TrimControl
            inBoundary={declared.inBoundary}
            outBoundary={declared.outBoundary}
            measuredGrid={measured.grid}
            measuredLengthBoundary={measuredLengthBoundary}
            editable={editable && onTrimBoundaryChange !== undefined}
            onTrimBoundaryChange={onTrimBoundaryChange}
          />
        </Row>

        <Row label={t`Tags`}>
          {declared.tags.length > 0 ? (
            <span className="flex min-w-0 flex-wrap gap-1">
              {declared.tags.map((tag) => (
                <Badge key={tag} variant="outline">
                  {tag}
                </Badge>
              ))}
            </span>
          ) : (
            <span className="text-sm text-muted-foreground">{t`No tags`}</span>
          )}
        </Row>

        <Row label={t`Rights`}>
          <span className="flex min-w-0 flex-wrap items-center gap-2">
            <RightsChip
              rights={declared.rights}
              expiryLabel={declared.rightsExpiryLabel}
            />
            {editable && onRightsChange !== undefined ? (
              <select
                value={declared.rights}
                onChange={(event): void => {
                  const next = event.currentTarget.value;
                  if (
                    next === 'cleared' ||
                    next === 'unknown' ||
                    next === 'expired'
                  ) {
                    onRightsChange(next);
                  }
                }}
                aria-label={t`Declare rights status`}
                className="h-12 min-w-0 rounded-md border border-input bg-transparent px-3 text-sm shadow-sm focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
              >
                {rightsOptions.map((status) => (
                  <option key={status} value={status}>
                    {rightsLabel(status)}
                  </option>
                ))}
              </select>
            ) : null}
          </span>
        </Row>

        {!editable ? (
          <p
            data-testid="metadata-edit-unavailable"
            className="min-w-0 break-words text-xs text-muted-foreground"
          >
            {editGate.unavailableReason ?? t`Metadata editing is not available.`}
          </p>
        ) : null}
      </div>

      {/* MEASURED — probed machine truth, read-only by law. */}
      <div
        data-testid="metadata-measured"
        className="flex min-w-0 flex-col gap-2 rounded-lg border p-3"
      >
        <SectionHeading
          note={t`Measured — probed machine truth, read-only; never authored here.`}
        >
          {t`Measured`}
        </SectionHeading>
        <Row label={t`Cadence`}>
          <span className="font-mono text-sm">
            {measured.grid !== null
              ? describeGrid(measured.grid)
              : t`Cadence not probed`}
          </span>
        </Row>
        <Row label={t`Duration`}>
          <span className="font-mono text-sm">
            {durationFormat.state === 'formatted' && durationFormat.duration !== null
              ? durationFormat.duration
              : durationFormat.state === 'unmeasured'
                ? t`Duration not measured`
                : t`Duration unknown`}
          </span>
        </Row>
        <Row label={t`Codec`}>
          <MeasuredValue value={measured.codec} />
        </Row>
        <Row label={t`Resolution`}>
          <MeasuredValue
            value={
              measured.widthPx !== null && measured.heightPx !== null
                ? `${String(measured.widthPx)} × ${String(measured.heightPx)} px`
                : null
            }
          />
        </Row>
        <Row label={t`Size`}>
          <MeasuredValue
            value={
              measured.byteSize !== null
                ? `${groupThousands(measured.byteSize)} bytes`
                : null
            }
          />
        </Row>
        <Row label={t`Content hash`}>
          <MeasuredValue value={measured.contentHash} />
        </Row>
      </div>

      {/* PROVENANCE — a read-only record (no lineage query exists, ⚠A9). */}
      <div
        data-testid="metadata-provenance"
        className="flex min-w-0 flex-col gap-2 rounded-lg border p-3"
      >
        <SectionHeading note={t`Provenance — recorded history, read-only.`}>
          {t`Provenance`}
        </SectionHeading>
        <Row label={t`Origin`}>
          <MeasuredValue value={originLabel(provenance.originKind)} />
        </Row>
        {provenance.originDetail !== null ? (
          <Row label={t`Detail`}>
            <MeasuredValue value={provenance.originDetail} />
          </Row>
        ) : null}
        {provenance.parentAssetName !== null ? (
          <Row label={t`Derived from`}>
            <MeasuredValue value={provenance.parentAssetName} />
          </Row>
        ) : null}
        <Row label={t`Captured`}>
          <MeasuredValue value={provenance.capturedAtLabel} />
        </Row>
        <Row label={t`Source URL`}>
          <MeasuredValue value={provenance.sourceUrl} />
        </Row>
      </div>

      {/* USAGE — read-only slot references. */}
      <div
        data-testid="metadata-usage"
        className="flex min-w-0 flex-col gap-2 rounded-lg border p-3"
      >
        <SectionHeading note={t`Usage — slots currently referencing this asset.`}>
          {t`Usage`}
        </SectionHeading>
        <Row label={t`In slots`}>
          {usage.slotReferences.length > 0 ? (
            <span className="flex min-w-0 flex-wrap gap-1">
              {usage.slotReferences.map((slot) => (
                <InUseChip
                  key={slot.slotName}
                  slotName={slot.slotName}
                  onAir={slot.onAir}
                />
              ))}
            </span>
          ) : (
            <span className="text-sm text-muted-foreground">
              {t`Not referenced by any slot`}
            </span>
          )}
        </Row>
      </div>
    </section>
  );
}

/** A read-only measured value with the honest not-measured fallback. */
function MeasuredValue({ value }: { readonly value: string | null }): JSX.Element {
  const { t } = useLingui();
  return (
    <span className="min-w-0 break-words text-sm">
      {value ?? t`Not measured`}
    </span>
  );
}

/** The trim control: exact-boundary readouts + 48px steppers, gated. */
function TrimControl({
  inBoundary,
  outBoundary,
  measuredGrid,
  measuredLengthBoundary,
  editable,
  onTrimBoundaryChange,
}: {
  readonly inBoundary: RationalBoundary | null;
  readonly outBoundary: RationalBoundary | null;
  readonly measuredGrid: DurationGrid | null;
  readonly measuredLengthBoundary: RationalBoundary | null;
  readonly editable: boolean;
  readonly onTrimBoundaryChange?: ((bound: 'in' | 'out', next: RationalBoundary) => void) | undefined;
}): JSX.Element {
  const { t } = useLingui();

  const validMeasured = measuredLengthBoundary !== null &&
    Number.isSafeInteger(measuredLengthBoundary.index) && measuredLengthBoundary.index > 0 &&
    formatBoundaryLabel(measuredLengthBoundary) !== null;
  const validEndpoint = (boundary: RationalBoundary | null): boolean => boundary === null ||
    (measuredLengthBoundary !== null && isSameGrid(boundary, measuredLengthBoundary) &&
      formatBoundaryLabel(boundary) !== null && boundary.index <= measuredLengthBoundary.index);
  const validTrim = validMeasured && validEndpoint(inBoundary) && validEndpoint(outBoundary) &&
    (inBoundary === null || outBoundary === null || inBoundary.index < outBoundary.index);
  const trimEditable = editable && validTrim;
  const maxIndexFor = (): number | null => measuredLengthBoundary?.index ?? null;

  // `in < out` is engine law (media.rs:78) — never propose a crossing.
  // (compareBoundaries returns null on differing grids; only a non-null
  // comparison may gate the step.)
  const stepIn = (direction: -1 | 1): (() => void) | undefined => {
    if (!trimEditable || inBoundary === null) {
      return undefined;
    }
    const next = stepBoundary(inBoundary, direction, maxIndexFor());
    if (next === null) {
      return undefined;
    }
    if (outBoundary !== null) {
      const cmp = compareBoundaries(next, outBoundary);
      if (cmp !== null && cmp >= 0) {
        return undefined;
      }
    }
    return (): void => {
      onTrimBoundaryChange?.('in', next);
    };
  };

  const stepOut = (direction: -1 | 1): (() => void) | undefined => {
    if (!trimEditable || outBoundary === null) {
      return undefined;
    }
    const next = stepBoundary(outBoundary, direction, maxIndexFor());
    if (next === null) {
      return undefined;
    }
    if (inBoundary !== null) {
      const cmp = compareBoundaries(next, inBoundary);
      if (cmp !== null && cmp <= 0) {
        return undefined;
      }
    }
    return (): void => {
      onTrimBoundaryChange?.('out', next);
    };
  };

  const setInAtStart =
    inBoundary === null && trimEditable
      ? (): void => {
          const start =
            measuredGrid !== null ? boundaryAtGridIndex(measuredGrid, 0) : null;
          if (start !== null) {
            onTrimBoundaryChange?.('in', start);
          }
        }
      : undefined;

  const setOutAtEnd =
    outBoundary === null && trimEditable
      ? (): void => {
          onTrimBoundaryChange?.('out', measuredLengthBoundary);
        }
      : undefined;

  return (
    <span
      data-testid="metadata-trim-control"
      className="flex min-w-0 flex-col gap-2"
    >
      {!validTrim ? <span role="status" className="text-xs text-destructive">{t`Trim unavailable — invalid measured length or mismatched asset grid and endpoints. Revalidate metadata before saving.`}</span> : null}
      <TrimBoundRow
        label={t`IN`}
        boundary={inBoundary}
        editable={trimEditable}
        onStepBackward={stepIn(-1)}
        onStepForward={stepIn(1)}
        onSet={setInAtStart}
        setLabel={t`Set at start`}
      />
      <TrimBoundRow
        label={t`OUT`}
        boundary={outBoundary}
        editable={trimEditable}
        onStepBackward={stepOut(-1)}
        onStepForward={stepOut(1)}
        onSet={setOutAtEnd}
        setLabel={t`Set at end`}
      />
    </span>
  );
}

/** One bound: readout + exact unit line + 48px steppers. */
function TrimBoundRow({
  label,
  boundary,
  editable,
  onStepBackward,
  onStepForward,
  onSet,
  setLabel,
}: {
  readonly label: string;
  readonly boundary: RationalBoundary | null;
  readonly editable: boolean;
  readonly onStepBackward: (() => void) | undefined;
  readonly onStepForward: (() => void) | undefined;
  readonly onSet: (() => void) | undefined;
  readonly setLabel: string;
}): JSX.Element {
  const { t } = useLingui();
  const timecode = formatBoundaryLabel(boundary);
  const unit = describeBoundaryUnit(boundary);
  return (
    <span className="flex min-w-0 flex-wrap items-center gap-2">
      <span className="w-10 shrink-0 text-xs font-medium text-muted-foreground">
        {label}
      </span>
      {boundary !== null ? (
        <>
          <span className="min-w-0 break-words font-mono text-sm">
            {timecode ?? t`Unusable boundary`}
          </span>
          {unit !== null ? (
            <span className="min-w-0 break-words text-xs text-muted-foreground">
              {unit}
            </span>
          ) : null}
          {editable ? (
            <span className="flex items-center gap-1">
              <TouchAction
                variant="outline"
                disabled={onStepBackward === undefined}
                onPress={(): void => {
                  onStepBackward?.();
                }}
                aria-label={t`Step ${label} back one grid unit`}
              >
                <span aria-hidden="true">◀</span>
              </TouchAction>
              <TouchAction
                variant="outline"
                disabled={onStepForward === undefined}
                onPress={(): void => {
                  onStepForward?.();
                }}
                aria-label={t`Step ${label} forward one grid unit`}
              >
                <span aria-hidden="true">▶</span>
              </TouchAction>
            </span>
          ) : null}
        </>
      ) : (
        <span className="min-w-0 break-words text-sm text-muted-foreground">
          {editable ? t`Trim not set` : t`Trim not set (read-only)`}
          {onSet !== undefined ? (
            <TouchAction variant="outline" onPress={onSet}>
              {setLabel}
            </TouchAction>
          ) : null}
        </span>
      )}
    </span>
  );
}
