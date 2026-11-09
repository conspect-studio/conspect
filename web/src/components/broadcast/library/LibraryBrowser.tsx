// LibraryBrowser — the asset list surface of the media-library workspace.
// The prototype's browser UX carries forward (search first, results before
// the facet wall on phones, exception rows visible, honest counts); its
// fake store/simulated jobs/success toasts do not (folder README).
//
// FULLY CONTROLLED, presentational:
//   * `assets` is the page of rows the caller chose to show (the caller
//     applies search/filter/sort — the component only renders and emits).
//   * The search field, facet chips, sort order, selection mode, selected
//     set, batch bar and every action are all controlled props; pressing
//     any of them emits an INTENT (change/toggle/action) — firing one
//     changes nothing in the DOM until the caller re-renders with new
//     props, so a blocked mutation can never look applied.
//   * Capability honesty: `listingGate.supported === false` renders the
//     unavailable panel with the caller's reason — search and sort render
//     disabled, never a fake working state (audit §9.10–11). Row and
//     batch actions render disabled with their visible reason when their
//     gate is `supported: false` (or the per-asset resolver blocks them).
//   * Touch-first: every interactive control keeps the 48px floor
//     (`TouchAction` / h-12 inputs); chips wrap with gaps (no collisions);
//     the facet wall collapses behind a controlled 48px Filters key on
//     narrow viewports and stays in flow from `md:` up (TaskWorkspace
//     pattern) — results never start below the first phone fold.
import type { JSX } from 'react';
import { useId } from 'react';
import { useLingui } from '@lingui/react/macro';

import { Badge } from '../../ui/badge';
import { Input } from '../../ui/input';
import { cn } from '../../../lib/utils';
import { TouchAction } from '../TouchAction';
import { AssetDurationLabel } from '../AssetDurationLabel';
import {
  AssetKindChip,
  InUseChip,
  RightsChip,
  TranscriptStateChip,
} from './Chips';
import type {
  ActionGate,
  LibraryAssetSummary,
  LibraryBatchAction,
  LibraryFacetGroup,
  LibraryRowAction,
  LibrarySortOption,
  RowActionGateResolver,
} from './types';

/** Props for {@link LibraryBrowser}. */
export interface LibraryBrowserProps {
  /** The rows to render — the caller's already-filtered, already-sorted page. */
  readonly assets: readonly LibraryAssetSummary[];
  /**
   * Capability gate for the asset listing itself. `supported: false`
   * renders the honest unavailable panel (with the reason) and disables
   * search/sort — the browser never fakes a working list.
   */
  readonly listingGate: ActionGate;
  /** Controlled search text (the caller owns matching). */
  readonly query: string;
  /** Intent: the search text changed. */
  readonly onSearchChange: (query: string) => void;
  /**
   * Overrides the search field's placeholder (already localized by the
   * caller — the honest scope depends on the caller's index, e.g. whether
   * transcript text is searchable at all).
   */
  readonly searchPlaceholder?: string | undefined;
  /** The facet chip groups the caller supports; omit for no filter UI. */
  readonly facetGroups?: readonly LibraryFacetGroup[] | undefined;
  /**
   * The ids of the currently active facet options (across all groups);
   * chips render `aria-pressed` from this list only.
   */
  readonly activeFilterOptionIds: readonly string[];
  /** Intent: a facet chip was toggled. */
  readonly onToggleFilter?: ((groupId: string, optionId: string) => void) | undefined;
  /**
   * Whether the facet panel is open on narrow viewports (controlled; from
   * `md:` the panel is always in flow). Required with `facetGroups`.
   */
  readonly filtersOpen?: boolean | undefined;
  /** Intent: the Filters disclosure toggled. Required with `facetGroups`. */
  readonly onFiltersToggle?: ((open: boolean) => void) | undefined;
  /** The caller's sort options (at least one). */
  readonly sortOptions: readonly LibrarySortOption[];
  /** The active sort id (must be one of `sortOptions`). */
  readonly activeSortId: string;
  /** Intent: the sort order changed. */
  readonly onSortChange: (sortId: string) => void;
  /** Whether selection mode is on (checkboxes render on every row). */
  readonly selectionMode: boolean;
  /** Intent: selection mode toggled. */
  readonly onSelectionModeToggle?: ((next: boolean) => void) | undefined;
  /** The caller's selected asset ids (the whole selection is caller-owned). */
  readonly selectedAssetIds: ReadonlySet<string>;
  /** Intent: one row's select checkbox toggled. */
  readonly onToggleAssetSelected?: ((assetId: string) => void) | undefined;
  /** Intent: clear the whole selection (the batch bar's ✕ Clear). */
  readonly onClearSelection?: (() => void) | undefined;
  /** Batch actions offered for the current selection; omit for no batch bar. */
  readonly batchActions?: readonly LibraryBatchAction[] | undefined;
  /** Intent: a batch action was pressed (with the caller's selection in hand). */
  readonly onBatchAction?: ((actionId: string) => void) | undefined;
  /** Per-row actions rendered on every row (e.g. load-into-slot, open). */
  readonly rowActions?: readonly LibraryRowAction[] | undefined;
  /** Intent: a row action was pressed on one asset. */
  readonly onRowAction?: ((actionId: string, assetId: string) => void) | undefined;
  /**
   * Optional per-asset gate resolver: return a gate to block the action
   * for this asset (e.g. destination slot on air), `null`/`undefined` for
   * the action's declared default gate. Pure per render.
   */
  readonly getRowActionGate?: RowActionGateResolver<LibraryAssetSummary> | undefined;
}

/** The honest unavailable panel + disabled chrome for a missing listing contract. */
function ListingUnavailable({ reason }: { readonly reason: string | null }): JSX.Element {
  const { t } = useLingui();
  return (
    <div
      data-testid="library-listing-unavailable"
      className="rounded-lg border border-dashed p-4 text-sm"
    >
      <p className="min-w-0 break-words font-medium">{t`Asset listing is not available`}</p>
      {reason !== null ? (
        <p className="min-w-0 break-words text-xs text-muted-foreground">{reason}</p>
      ) : null}
    </div>
  );
}

/**
 * The media-library asset browser: controlled search/filter/sort/selection
 * surfaces plus honest capability-gated row and batch actions. Rendering
 * changes only when the caller re-renders with new props.
 */
export function LibraryBrowser({
  assets,
  listingGate,
  query,
  onSearchChange,
  searchPlaceholder,
  facetGroups,
  activeFilterOptionIds,
  onToggleFilter,
  filtersOpen,
  onFiltersToggle,
  sortOptions,
  activeSortId,
  onSortChange,
  selectionMode,
  onSelectionModeToggle,
  selectedAssetIds,
  onToggleAssetSelected,
  onClearSelection,
  batchActions,
  onBatchAction,
  rowActions,
  onRowAction,
  getRowActionGate,
}: LibraryBrowserProps): JSX.Element {
  const { t } = useLingui();
  const filtersPanelId = useId();
  const activeFilterIds = new Set(activeFilterOptionIds);
  const listingAvailable = listingGate.supported;
  const hasAnyActiveFilter = activeFilterIds.size > 0;
  const anyFiltering = query.trim() !== '' || hasAnyActiveFilter;
  const selectedCount = selectedAssetIds.size;

  return (
    <section
      data-testid="library-browser"
      data-listing-available={listingAvailable ? 'true' : 'false'}
      aria-label={t`Library browser`}
      className="flex min-w-0 flex-col gap-3"
    >
      <h2 className="min-w-0 break-words text-base font-semibold">
        {t`Library`}
        <span className="ml-2 align-middle text-xs font-normal text-muted-foreground">
          {t`${assets.length} shown`}
        </span>
      </h2>

      {/* Search + sort first — the phone fold starts with results, never a
          facet wall (round-4 blocker). */}
      <div className="flex min-w-0 flex-wrap items-center gap-2">
        <Input
          type="search"
          value={query}
          disabled={!listingAvailable}
          onChange={(event): void => {
            onSearchChange(event.currentTarget.value);
          }}
          aria-label={t`Search library assets`}
          placeholder={searchPlaceholder ?? t`Search library assets`}
          className="h-12 min-w-0 basis-56 flex-1"
        />
        <select
          value={activeSortId}
          disabled={!listingAvailable}
          onChange={(event): void => {
            onSortChange(event.currentTarget.value);
          }}
          aria-label={t`Sort order`}
          className="h-12 min-w-0 rounded-md border border-input bg-transparent px-3 text-sm shadow-sm focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring disabled:cursor-not-allowed disabled:opacity-50"
        >
          {sortOptions.map((option) => (
            <option key={option.id} value={option.id}>
              {option.label}
            </option>
          ))}
        </select>
        {selectionMode && onSelectionModeToggle !== undefined ? (
          <TouchAction
            variant="outline"
            aria-pressed={selectionMode}
            onPress={(): void => {
              onSelectionModeToggle(false);
            }}
          >
            {t`Exit select mode`}
          </TouchAction>
        ) : !selectionMode && onSelectionModeToggle !== undefined ? (
          <TouchAction
            variant="outline"
            onPress={(): void => {
              onSelectionModeToggle(true);
            }}
          >
            {t`Select…`}
          </TouchAction>
        ) : null}
        {facetGroups !== undefined && facetGroups.length > 0 && onFiltersToggle !== undefined ? (
          <TouchAction
            variant="outline"
            className="md:hidden"
            aria-expanded={filtersOpen ?? false}
            aria-controls={filtersPanelId}
            onPress={(): void => {
              onFiltersToggle(!(filtersOpen ?? false));
            }}
          >
            {t`Filters`}
          </TouchAction>
        ) : null}
      </div>

      {facetGroups !== undefined && facetGroups.length > 0 ? (
        <div
          id={filtersPanelId}
          className={cn(
            'flex min-w-0 flex-col gap-2',
            filtersOpen ? 'block' : 'hidden md:flex',
          )}
        >
          {facetGroups.map((group) => (
            <div
              key={group.id}
              data-facet-group={group.id}
              className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1"
            >
              <span className="text-xs font-medium text-muted-foreground">
                {group.label}
              </span>
              {group.options.map((option) => {
                const pressed = activeFilterIds.has(option.id);
                return (
                  <button
                    key={option.id}
                    type="button"
                    aria-pressed={pressed}
                    disabled={!listingAvailable}
                    onClick={(): void => {
                      onToggleFilter?.(group.id, option.id);
                    }}
                    data-facet={option.id}
                    className={cn(
                      'inline-flex min-h-12 min-w-12 items-center gap-1 rounded-md border px-3 py-1 text-xs font-medium transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring disabled:cursor-not-allowed disabled:opacity-50',
                      pressed
                        ? 'border-primary bg-primary text-primary-foreground'
                        : 'border-input bg-transparent hover:bg-accent',
                    )}
                  >
                    {option.label}
                    {option.count !== null ? (
                      <span className="text-xs opacity-70">{option.count}</span>
                    ) : null}
                  </button>
                );
              })}
            </div>
          ))}
        </div>
      ) : null}

      {!listingAvailable ? (
        <ListingUnavailable reason={listingGate.unavailableReason} />
      ) : (
        <>
          {batchActions !== undefined &&
          batchActions.length > 0 &&
          selectionMode &&
          selectedCount > 0 ? (
            <div
              data-testid="library-batch-bar"
              className="flex min-w-0 flex-wrap items-start gap-2 rounded-lg border p-2"
            >
              <p className="min-w-0 flex-1 basis-full break-words text-sm sm:basis-auto">
                {t`${selectedCount} selected`}
              </p>
              {batchActions.map((action) => (
                <div key={action.id} className="flex min-w-0 flex-col gap-1">
                  <TouchAction
                    variant="outline"
                    disabled={!action.gate.supported}
                    disabledReason={
                      action.gate.supported ? undefined : action.gate.unavailableReason ?? undefined
                    }
                    onPress={(): void => {
                      onBatchAction?.(action.id);
                    }}
                  >
                    {action.label}
                  </TouchAction>
                </div>
              ))}
              {onClearSelection !== undefined ? (
                <TouchAction
                  variant="ghost"
                  onPress={onClearSelection}
                >
                  {t`Clear selection`}
                </TouchAction>
              ) : null}
            </div>
          ) : null}

          {assets.length === 0 ? (
            <div
              data-testid="library-empty"
              className="rounded-lg border border-dashed p-4 text-sm"
            >
              <p className="min-w-0 break-words">
                {anyFiltering
                  ? t`No assets match the current search and filters.`
                  : t`No assets in the library yet.`}
              </p>
            </div>
          ) : (
            <ul className="flex min-w-0 flex-col gap-2">
              {assets.map((asset) => (
                <LibraryRow
                  key={asset.id}
                  asset={asset}
                  selectionMode={selectionMode}
                  selected={selectedAssetIds.has(asset.id)}
                  onToggleAssetSelected={onToggleAssetSelected}
                  rowActions={rowActions}
                  onRowAction={onRowAction}
                  getRowActionGate={getRowActionGate}
                />
              ))}
            </ul>
          )}
        </>
      )}
    </section>
  );
}

/** One asset row: chips + honest meta + capability-gated actions. */
function LibraryRow({
  asset,
  selectionMode,
  selected,
  onToggleAssetSelected,
  rowActions,
  onRowAction,
  getRowActionGate,
}: {
  readonly asset: LibraryAssetSummary;
  readonly selectionMode: boolean;
  readonly selected: boolean;
  readonly onToggleAssetSelected?: ((assetId: string) => void) | undefined;
  readonly rowActions?: readonly LibraryRowAction[] | undefined;
  readonly onRowAction?: ((actionId: string, assetId: string) => void) | undefined;
  readonly getRowActionGate?: RowActionGateResolver<LibraryAssetSummary> | undefined;
}): JSX.Element {
  const { t } = useLingui();
  return (
    <li
      data-testid="library-asset-row"
      data-asset-id={asset.id}
      data-selected={selected ? 'true' : 'false'}
      className={cn(
        'flex min-w-0 flex-col gap-2 rounded-lg border p-3',
        selected && 'border-primary',
      )}
    >
      <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1">
        <span className="min-w-0 flex-1 basis-full break-words text-sm font-medium sm:basis-auto">
          {asset.name}
        </span>
        {selectionMode && onToggleAssetSelected !== undefined ? (
          <TouchAction
            variant="outline"
            aria-pressed={selected}
            onPress={(): void => {
              onToggleAssetSelected(asset.id);
            }}
          >
            {selected ? t`Deselect` : t`Select`}
          </TouchAction>
        ) : null}
      </div>

      <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1">
        <AssetKindChip kind={asset.kind} />
        {asset.originLabel !== null ? (
          <Badge variant="outline" data-testid="chip-origin">
            {asset.originLabel}
          </Badge>
        ) : null}
        <TranscriptStateChip state={asset.transcript} />
        <RightsChip rights={asset.rights} expiryLabel={asset.rightsExpiryLabel} />
        {asset.inUseSlots.map((slot) => (
          <InUseChip key={slot.slotName} slotName={slot.slotName} onAir={slot.onAir} />
        ))}
      </div>

      <p className="min-w-0 break-words text-xs text-muted-foreground">
        <AssetDurationLabel asset={asset.duration} />
        {asset.sizeLabel !== null ? <>{' · '}{asset.sizeLabel}</> : null}
        {asset.originLabel !== null ? <>{' · '}{asset.originLabel}</> : null}
      </p>

      {asset.exceptionNote !== null ? (
        <p
          data-testid="library-row-exception"
          className="min-w-0 break-words text-xs text-destructive"
        >
          {asset.exceptionNote}
        </p>
      ) : null}

      {rowActions !== undefined && rowActions.length > 0 ? (
        <div className="flex min-w-0 flex-wrap items-start gap-2">
          {rowActions.map((action) => {
            const override = getRowActionGate?.(asset, action.id);
            const gate: ActionGate = override ?? action.gate;
            const blocked = !gate.supported;
            return (
              <div key={action.id} className="flex min-w-0 flex-col gap-1">
                <TouchAction
                  variant="outline"
                  disabled={blocked}
                  disabledReason={
                    blocked ? gate.unavailableReason ?? undefined : undefined
                  }
                  onPress={(): void => {
                    onRowAction?.(action.id, asset.id);
                  }}
                >
                  {action.label}
                </TouchAction>
              </div>
            );
          })}
        </div>
      ) : null}
    </li>
  );
}
