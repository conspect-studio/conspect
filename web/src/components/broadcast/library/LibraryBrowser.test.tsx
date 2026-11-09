// LibraryBrowser tests — the controlled, capability-honest browser contract:
//   * every surface is controlled: firing any intent changes NOTHING until
//     the caller re-renders with new props (no silent mutation, no hidden
//     state — aria-pressed/value/selected all come from props only);
//   * a `supported: false` listing gate renders the honest unavailable
//     panel with the reason and disables search/sort — never a fake
//     working state;
//   * row/batch actions carry their capability gates: an unsupported
//     action is disabled with its visible reason and never fires; a
//     per-asset resolver override (e.g. destination slot ON AIR) blocks
//     with its own reason;
//   * unknown rights are visible as unknown (never cleared) and missing
//     measured data renders honest fallbacks — no NaN/undefined.
import { describe, expect, it, vi } from 'vitest';
import { fireEvent, screen, within } from '@testing-library/react';

import { renderWithProviders } from '../../../test/render';
import { LibraryBrowser } from './LibraryBrowser';
import type { LibraryAssetSummary } from './types';
import type { ActionGate } from './types';

const SUPPORTED: ActionGate = { supported: true, unavailableReason: null };

const ASSETS: readonly LibraryAssetSummary[] = [
  {
    id: 'a-match',
    name: 'Match 4 wide',
    kind: 'video',
    originLabel: 'Captured — studio A',
    duration: {
      grid: { kind: 'video', numerator: 25, denominator: 1 },
      units: 750,
    },
    sizeLabel: '1.2 GB',
    rights: 'cleared',
    rightsExpiryLabel: null,
    transcript: 'done',
    inUseSlots: [{ slotName: 'VT 1', onAir: true }],
    exceptionNote: null,
  },
  {
    id: 'a-fetch',
    name: 'Interview 12 — fetched',
    kind: 'video',
    originLabel: 'Fetched — youtube.com',
    duration: { grid: null, units: null },
    sizeLabel: null,
    rights: 'unknown',
    rightsExpiryLabel: null,
    transcript: 'failed',
    inUseSlots: [],
    exceptionNote: 'Transcript failed — unreadable audio',
  },
];

const SORT_OPTIONS = [
  { id: 'recent', label: 'Most recent' },
  { id: 'name', label: 'Name' },
];
const FACET_GROUPS = [
  {
    id: 'type',
    label: 'Type',
    options: [
      { id: 'video', label: 'Video', count: 1 },
      { id: 'audio', label: 'Audio', count: 0 },
    ],
  },
];

function renderBrowser(overrides: {
  assets?: readonly LibraryAssetSummary[];
  listingGate?: ActionGate;
  query?: string;
  activeFilterOptionIds?: readonly string[];
  selectionMode?: boolean;
  selectedAssetIds?: ReadonlySet<string>;
  rowActions?: readonly {
    id: string;
    label: string;
    gate: ActionGate;
  }[];
  batchActions?: readonly {
    id: string;
    label: string;
    gate: ActionGate;
  }[];
  getRowActionGate?: (
    asset: LibraryAssetSummary,
    actionId: string,
  ) => ActionGate | null | undefined;
} = {}) {
  const onSearchChange = vi.fn();
  const onToggleFilter = vi.fn();
  const onSortChange = vi.fn();
  const onSelectionModeToggle = vi.fn();
  const onToggleAssetSelected = vi.fn();
  const onClearSelection = vi.fn();
  const onBatchAction = vi.fn();
  const onRowAction = vi.fn();
  const onFiltersToggle = vi.fn();
  const { container } = renderWithProviders(
    <LibraryBrowser
      assets={overrides.assets ?? ASSETS}
      listingGate={overrides.listingGate ?? SUPPORTED}
      query={overrides.query ?? ''}
      onSearchChange={onSearchChange}
      facetGroups={FACET_GROUPS}
      activeFilterOptionIds={overrides.activeFilterOptionIds ?? []}
      onToggleFilter={onToggleFilter}
      filtersOpen={false}
      onFiltersToggle={onFiltersToggle}
      sortOptions={SORT_OPTIONS}
      activeSortId="recent"
      onSortChange={onSortChange}
      selectionMode={overrides.selectionMode ?? false}
      onSelectionModeToggle={onSelectionModeToggle}
      selectedAssetIds={overrides.selectedAssetIds ?? new Set<string>()}
      onToggleAssetSelected={onToggleAssetSelected}
      onClearSelection={onClearSelection}
      batchActions={overrides.batchActions ?? []}
      onBatchAction={onBatchAction}
      rowActions={overrides.rowActions ?? []}
      onRowAction={onRowAction}
      getRowActionGate={overrides.getRowActionGate}
    />,
  );
  return {
    container,
    onSearchChange,
    onToggleFilter,
    onSortChange,
    onSelectionModeToggle,
    onToggleAssetSelected,
    onClearSelection,
    onBatchAction,
    onRowAction,
    onFiltersToggle,
  };
}

function rowOf(container: HTMLElement, assetId: string): HTMLElement {
  return container.querySelector<HTMLElement>(
    `[data-testid="library-asset-row"][data-asset-id="${assetId}"]`,
  )!;
}

describe('LibraryBrowser — controlled surfaces emit intents, never mutate', () => {
  it('renders rows with honest chips: on-air usage red, unknown rights never cleared', () => {
    const { container } = renderBrowser();
    const first = rowOf(container, 'a-match');
    expect(within(first).getByText('Match 4 wide')).toBeTruthy();
    expect(within(first).getByTestId('chip-in-use')).toHaveTextContent(
      /In VT 1 — ON AIR/,
    );
    expect(within(first).getByTestId('chip-rights')).toHaveTextContent(
      /Rights cleared/,
    );
    const second = rowOf(container, 'a-fetch');
    expect(within(second).getByTestId('chip-rights')).toHaveTextContent(
      /Rights unknown/,
    );
    expect(within(second).getByTestId('chip-transcript')).toHaveTextContent(
      /Transcript failed/,
    );
    // Missing measured duration + size degrade honestly — no NaN/undefined.
    expect(second.textContent).not.toMatch(/NaN|undefined/);
    expect(second).toHaveTextContent(/Duration unknown/);
    expect(within(second).getByTestId('library-row-exception')).toHaveTextContent(
      /Transcript failed — unreadable audio/,
    );
  });

  it('search and sort are controlled — typing changes nothing, only emits', () => {
    const helpers = renderBrowser();
    const search = screen.getByRole('searchbox', { name: /Search library assets/ });
    fireEvent.change(search, { target: { value: 'match' } });
    expect(helpers.onSearchChange).toHaveBeenCalledWith('match');
    // No silent mutation: the value is still the prop's.
    expect(search).toHaveValue('');
    const sort = screen.getByRole('combobox', { name: /Sort order/ });
    fireEvent.change(sort, { target: { value: 'name' } });
    expect(helpers.onSortChange).toHaveBeenCalledWith('name');
    expect(sort).toHaveValue('recent');
  });

  it('facet chips render pressed state from props only and emit toggle intents', () => {
    const helpers = renderBrowser({ activeFilterOptionIds: ['video'] });
    const video = screen.getByRole('button', { name: /^Video/ });
    const audio = screen.getByRole('button', { name: /^Audio/ });
    expect(video).toHaveAttribute('aria-pressed', 'true');
    expect(audio).toHaveAttribute('aria-pressed', 'false');
    fireEvent.click(audio);
    expect(helpers.onToggleFilter).toHaveBeenCalledWith('type', 'audio');
    // No silent mutation — aria-pressed is still prop-driven.
    expect(audio).toHaveAttribute('aria-pressed', 'false');
    // Facet chips keep the 48px touch floor.
    expect(audio.className).toMatch(/\bmin-h-12\b/);
  });

  it('the Filters disclosure is controlled and wired for the narrow fold', () => {
    const helpers = renderBrowser();
    const filters = screen.getByRole('button', { name: /^Filters$/ });
    expect(filters.className).toMatch(/md:hidden/);
    expect(filters).toHaveAttribute('aria-expanded', 'false');
    fireEvent.click(filters);
    expect(helpers.onFiltersToggle).toHaveBeenCalledWith(true);
    // Controlled: expansion changes only when the caller re-renders.
    expect(filters).toHaveAttribute('aria-expanded', 'false');
  });

  it('shows an honest empty state — no-match vs empty library', () => {
    const { container } = renderBrowser({ assets: [] });
    expect(
      within(container).getByTestId('library-empty'),
    ).toHaveTextContent(/No assets in the library yet\./);
    const { container: filtered } = renderBrowser({
      assets: [],
      query: 'nothing',
    });
    expect(
      within(filtered).getByTestId('library-empty'),
    ).toHaveTextContent(/No assets match the current search and filters\./);
  });
});

describe('LibraryBrowser — capability gates', () => {
  it('an unsupported listing gate shows the reason and disables search/sort — no fake working state', () => {
    renderBrowser({
      listingGate: {
        supported: false,
        unavailableReason: 'Asset listing API not implemented yet',
      },
    });
    expect(
      screen.getByTestId('library-listing-unavailable'),
    ).toHaveTextContent(/Asset listing is not available/);
    expect(screen.getByText('Asset listing API not implemented yet')).toBeTruthy();
    expect(screen.getByRole('searchbox')).toBeDisabled();
    expect(screen.getByRole('combobox', { name: /Sort order/ })).toBeDisabled();
    // No rows render for a missing listing contract — and never a fake
    // working state.
    expect(
      screen.queryByTestId('library-asset-row'),
    ).toBeNull();
  });

  it('hides stale selection, facet counts and result count without listing', () => {
    const helpers = renderBrowser({ listingGate: { supported: false, unavailableReason: 'No inventory' }, selectionMode: true });
    expect(screen.queryByText(/shown|selected|Exit select mode/i)).toBeNull();
    expect(screen.queryByRole('button', { name: /Filters/i })).toBeNull();
    expect(helpers.container.querySelector('[data-facet-group]')).toBeNull();
  });

  it('row actions fire intents only through supported gates; per-asset overrides block with the reason', () => {
    const helpers = renderBrowser({
      rowActions: [
        { id: 'load-vt1', label: 'Load into VT 1', gate: SUPPORTED },
        {
          id: 'delete',
          label: 'Delete',
          gate: { supported: false, unavailableReason: 'Delete API not implemented yet' },
        },
      ],
      getRowActionGate: (asset, actionId) =>
        actionId === 'load-vt1' && asset.id === 'a-match'
          ? {
              supported: false,
              unavailableReason: 'VT 1 is ON AIR — cut away first.',
            }
          : null,
    });
    const first = rowOf(helpers.container, 'a-match');
    const loadFirst = within(first).getByRole('button', {
      name: /Load into VT 1/,
    });
    expect(loadFirst).toBeDisabled();
    expect(loadFirst.getAttribute('title')).toBe('VT 1 is ON AIR — cut away first.');
    fireEvent.click(loadFirst);
    expect(helpers.onRowAction).not.toHaveBeenCalled();

    const second = rowOf(helpers.container, 'a-fetch');
    const loadSecond = within(second).getByRole('button', {
      name: /Load into VT 1/,
    });
    expect(loadSecond).not.toBeDisabled();
    fireEvent.click(loadSecond);
    expect(helpers.onRowAction).toHaveBeenCalledWith('load-vt1', 'a-fetch');

    const del = within(second).getByRole('button', { name: /^Delete$/ });
    expect(del).toBeDisabled();
    expect(del.getAttribute('title')).toBe('Delete API not implemented yet');
  });

  it('the batch bar renders for a non-empty selection and gates its actions', () => {
    const helpers = renderBrowser({
      selectionMode: true,
      selectedAssetIds: new Set(['a-match', 'a-fetch']),
      batchActions: [
        { id: 'batch-load', label: 'Load…', gate: SUPPORTED },
        {
          id: 'batch-delete',
          label: 'Delete…',
          gate: { supported: false, unavailableReason: 'Delete API not implemented yet' },
        },
      ],
    });
    const bar = screen.getByTestId('library-batch-bar');
    expect(bar).toHaveTextContent(/2 selected/);
    fireEvent.click(within(bar).getByRole('button', { name: /^Load…$/ }));
    expect(helpers.onBatchAction).toHaveBeenCalledWith('batch-load');
    const batchDelete = within(bar).getByRole('button', { name: /^Delete…$/ });
    expect(batchDelete).toBeDisabled();
    fireEvent.click(batchDelete);
    expect(helpers.onBatchAction).not.toHaveBeenCalledWith('batch-delete');
    fireEvent.click(within(bar).getByRole('button', { name: /Clear selection/ }));
    expect(helpers.onClearSelection).toHaveBeenCalledOnce();
  });

  it('selection is controlled: row checkboxes reflect props and emit toggle intents', () => {
    const helpers = renderBrowser({
      selectionMode: true,
      selectedAssetIds: new Set(['a-match']),
    });
    const first = rowOf(helpers.container, 'a-match');
    const second = rowOf(helpers.container, 'a-fetch');
    const checked = within(first).getByRole('button', { name: /Deselect/ });
    expect(checked).toHaveAttribute('aria-pressed', 'true');
    const unchecked = within(second).getByRole('button', { name: /^Select$/ });
    fireEvent.click(unchecked);
    expect(helpers.onToggleAssetSelected).toHaveBeenCalledWith('a-fetch');
    // No silent mutation: the row did not become selected on its own.
    expect(second.dataset.selected).toBe('false');
  });

  it('exposes the selection-mode intent from the header', () => {
    const helpers = renderBrowser();
    fireEvent.click(screen.getByRole('button', { name: /^Select…$/ }));
    expect(helpers.onSelectionModeToggle).toHaveBeenCalledWith(true);
  });
});
