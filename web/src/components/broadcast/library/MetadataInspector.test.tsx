// MetadataInspector tests — the two-families contract (audit §4) plus
// honesty rules:
//   * MEASURED is read-only: no inputs/selects live in the measured
//     section, and every absent probed value renders "Not measured" —
//     never NaN, never `undefined`, never an invented precision;
//   * DECLARED is editable only through a supported gate: with
//     `supported: false` the section is read-only and shows the caller's
//     reason — never a fake editable state;
//   * trims are EXACT grid boundaries: the readout keeps the integer index
//     and the exact rational pair; steppers propose adjacent indices
//     clamped to the measured length and NEVER cross `in < out`
//     (media.rs:78); every proposal is an intent, nothing applies here;
//   * rights are declared, never auto-cleared: `unknown` renders the
//     visible warning (never "cleared") and only the operator's explicit
//     select can declare a status;
//   * provenance/usage are read-only records with honest fallbacks.
import { describe, expect, it, vi } from 'vitest';
import { fireEvent, screen, within } from '@testing-library/react';

import { renderWithProviders } from '../../../test/render';
import { MetadataInspector } from './MetadataInspector';
import type { LibraryAssetMetadata } from './MetadataInspector';
import type { ActionGate } from './types';

const SUPPORTED: ActionGate = { supported: true, unavailableReason: null };
const UNSUPPORTED: ActionGate = {
  supported: false,
  unavailableReason: 'Metadata API not implemented yet',
};

const NTSC_GRID = { kind: 'video' as const, numerator: 30000, denominator: 1001 };

const ASSET: LibraryAssetMetadata = {
  id: 'a-interview',
  name: 'Interview 12',
  kind: 'video',
  measured: {
    grid: NTSC_GRID,
    units: 3630,
    codec: 'h264',
    widthPx: 1920,
    heightPx: 1080,
    byteSize: 2_147_483_648,
    contentHash: 'sha256-abc123',
  },
  declared: {
    inBoundary: { kind: 'frames', index: 15, numerator: 30000, denominator: 1001 },
    outBoundary: { kind: 'frames', index: 3000, numerator: 30000, denominator: 1001 },
    tags: ['interview', 'press'],
    rights: 'unknown',
    rightsExpiryLabel: null,
  },
  provenance: {
    originKind: 'fetched',
    originDetail: 'Fetched from the source URL at boot',
    capturedAtLabel: '2026-11-08 18:04',
    sourceUrl: 'https://example.invalid/watch?v=12',
    parentAssetName: null,
  },
  usage: {
    slotReferences: [{ slotName: 'VT 2', onAir: false }],
  },
};

const ALL_UNMEASURED: LibraryAssetMetadata = {
  ...ASSET,
  measured: {
    grid: null,
    units: null,
    codec: null,
    widthPx: null,
    heightPx: null,
    byteSize: null,
    contentHash: null,
  },
  declared: {
    ...ASSET.declared,
    inBoundary: null,
    outBoundary: null,
  },
};

function renderInspector(overrides: {
  asset?: LibraryAssetMetadata;
  editGate?: ActionGate;
} = {}) {
  const onNameChange = vi.fn();
  const onTrimBoundaryChange = vi.fn();
  const onRightsChange = vi.fn();
  const { container } = renderWithProviders(
    <MetadataInspector
      asset={overrides.asset ?? ASSET}
      editGate={overrides.editGate ?? SUPPORTED}
      onNameChange={onNameChange}
      onTrimBoundaryChange={onTrimBoundaryChange}
      onRightsChange={onRightsChange}
    />,
  );
  return {
    container,
    onNameChange,
    onTrimBoundaryChange,
    onRightsChange,
  };
}

describe('MetadataInspector — measured is read-only and honest', () => {
  it('renders probed values with the exact rational cadence — and never an input', () => {
    const { container } = renderInspector();
    const measured = within(container).getByTestId('metadata-measured');
    expect(measured).toHaveTextContent(/29\.97 \(30000\/1001\) fps NDF/);
    expect(measured).toHaveTextContent(/h264/);
    expect(measured).toHaveTextContent(/1920 × 1080 px/);
    expect(measured).toHaveTextContent(/2,147,483,648 bytes/);
    expect(measured.querySelectorAll('input, select, textarea')).toHaveLength(0);
    expect(measured).toHaveTextContent(/Measured — probed machine truth/);
  });

  it('degrades absent probes to Not measured — never NaN or undefined', () => {
    const { container } = renderInspector({ asset: ALL_UNMEASURED });
    const measured = within(container).getByTestId('metadata-measured');
    const fallbacks = within(measured).getAllByText(/Not measured/);
    expect(fallbacks.length).toBeGreaterThanOrEqual(4);
    expect(measured.textContent).not.toMatch(/NaN|undefined|null/);
    expect(measured).toHaveTextContent(/Duration unknown/);
  });
});

describe('MetadataInspector — declared edits are gated, exact and intent-only', () => {
  it('shows the exact trim boundaries: integer index + rational pair, never float', () => {
    const { container } = renderInspector();
    const trim = within(container).getByTestId('metadata-trim-control');
    expect(trim.textContent).not.toMatch(/29\.9[67](?!\d)/);
    expect(trim).toHaveTextContent(/FR 15 · 30000\/1001/);
    expect(trim).toHaveTextContent(/FR 3,000 · 30000\/1001/);
    // 15 frames at ceil(30000/1001)=30 frame-fields/sec → 00:00:00:15.
    expect(trim).toHaveTextContent(/00:00:00:15/);
  });

  it('steppers propose the adjacent exact boundary as an intent — nothing applies', () => {
    const helpers = renderInspector();
    const inForward = screen.getByRole('button', {
      name: /Step IN forward one grid unit/,
    });
    expect(inForward.className).toMatch(/\bh-12\b/);
    fireEvent.click(inForward);
    expect(helpers.onTrimBoundaryChange).toHaveBeenCalledWith('in', {
      kind: 'frames',
      index: 16,
      numerator: 30000,
      denominator: 1001,
    });
    // No silent mutation: the readout still shows the prop's index.
    expect(within(helpers.container).getByTestId('metadata-trim-control')).toHaveTextContent(
      /FR 15 · 30000\/1001/,
    );
  });

  it('never proposes crossing in < out: IN forward at OUT and OUT backward at IN disable', () => {
    const helpers = renderInspector({
      asset: {
        ...ASSET,
        declared: {
          ...ASSET.declared,
          inBoundary: { kind: 'frames', index: 2999, numerator: 30000, denominator: 1001 },
          outBoundary: { kind: 'frames', index: 3000, numerator: 30000, denominator: 1001 },
        },
      },
    });
    const inForward = screen.getByRole('button', {
      name: /Step IN forward one grid unit/,
    });
    expect(inForward).toBeDisabled();
    fireEvent.click(inForward);
    const outBackward = screen.getByRole('button', {
      name: /Step OUT back one grid unit/,
    });
    expect(outBackward).toBeDisabled();
    fireEvent.click(outBackward);
    expect(helpers.onTrimBoundaryChange).not.toHaveBeenCalled();
  });

  it('clamps proposals to the measured length', () => {
    const helpers = renderInspector({
      asset: {
        ...ASSET,
        measured: { ...ASSET.measured, units: 3000 },
        declared: {
          ...ASSET.declared,
          outBoundary: { kind: 'frames', index: 3000, numerator: 30000, denominator: 1001 },
        },
      },
    });
    const outForward = screen.getByRole('button', {
      name: /Step OUT forward one grid unit/,
    });
    expect(outForward).toBeDisabled();
    fireEvent.click(outForward);
    expect(helpers.onTrimBoundaryChange).not.toHaveBeenCalled();
  });

  it('unset trims can be proposed from the measured grid, honestly', () => {
    renderInspector({ asset: ALL_UNMEASURED });
    // Nothing measured → no proposal is possible.
    expect(
      screen.queryByRole('button', { name: /Set at start/ }),
    ).toBeNull();

    const partial = renderInspector({
      asset: {
        ...ALL_UNMEASURED,
        measured: { ...ASSET.measured },
      },
    });
    fireEvent.click(screen.getByRole('button', { name: /Set at start/ }));
    expect(partial.onTrimBoundaryChange).toHaveBeenCalledWith('in', {
      kind: 'frames',
      index: 0,
      numerator: 30000,
      denominator: 1001,
    });
    fireEvent.click(screen.getByRole('button', { name: /Set at end/ }));
    expect(partial.onTrimBoundaryChange).toHaveBeenCalledWith('out', {
      kind: 'frames',
      index: 3630,
      numerator: 30000,
      denominator: 1001,
    });
  });

  it('name editing is a controlled input that emits intents only', () => {
    const helpers = renderInspector();
    const nameInput = screen.getByRole('textbox', { name: /Asset name/ });
    expect(nameInput).toHaveValue('Interview 12');
    fireEvent.change(nameInput, { target: { value: 'Interview 12 — edit' } });
    expect(helpers.onNameChange).toHaveBeenCalledWith('Interview 12 — edit');
    // Controlled: nothing applies from inside the component.
    expect(nameInput).toHaveValue('Interview 12');
  });

  it('an unsupported edit gate renders the declared family read-only with the reason', () => {
    const helpers = renderInspector({ editGate: UNSUPPORTED });
    const declared = within(helpers.container).getByTestId('metadata-declared');
    expect(declared.dataset.editAvailable ?? 'false').toBe('false');
    expect(
      within(helpers.container).getByTestId('metadata-edit-unavailable'),
    ).toHaveTextContent('Metadata API not implemented yet');
    expect(declared.querySelectorAll('input, select')).toHaveLength(0);
    expect(
      within(declared).queryByRole('button', { name: /Step IN/ }),
    ).toBeNull();
    expect(within(declared).getByText('Interview 12')).toBeTruthy();
  });
});

describe('MetadataInspector — rights are declared, never auto-cleared', () => {
  it('unknown rights render the visible warning — never cleared', () => {
    const { container } = renderInspector();
    const declared = within(container).getByTestId('metadata-declared');
    expect(within(declared).getByTestId('chip-rights')).toHaveTextContent(
      /Rights unknown/,
    );
    expect(declared.textContent).not.toMatch(/Rights cleared/);
  });

  it('expired rights render the caller’s expiry note', () => {
    const { container } = renderInspector({
      asset: {
        ...ASSET,
        declared: {
          ...ASSET.declared,
          rights: 'expired',
          rightsExpiryLabel: '2026-12-01',
        },
      },
    });
    const declared = within(container).getByTestId('metadata-declared');
    expect(within(declared).getByTestId('chip-rights')).toHaveTextContent(
      /Rights expired — 2026-12-01/,
    );
  });

  it('only the operator’s explicit select declares a status, as an intent', () => {
    const helpers = renderInspector();
    const select = screen.getByRole('combobox', {
      name: /Declare rights status/,
    });
    expect(select).toHaveValue('unknown');
    fireEvent.change(select, { target: { value: 'cleared' } });
    expect(helpers.onRightsChange).toHaveBeenCalledWith('cleared');
    // No silent mutation: the declared status is still the prop's.
    expect(select).toHaveValue('unknown');
  });
});

describe('MetadataInspector — provenance and usage records', () => {
  it('renders the read-only provenance record with honest unknowns', () => {
    const { container } = renderInspector();
    const provenance = within(container).getByTestId('metadata-provenance');
    expect(provenance).toHaveTextContent(/Fetched/);
    expect(provenance).toHaveTextContent(/Fetched from the source URL at boot/);
    expect(provenance).toHaveTextContent(/2026-11-08 18:04/);
    expect(provenance).toHaveTextContent(/https:\/\/example\.invalid\/watch\?v=12/);
    expect(provenance.querySelectorAll('input, select, textarea')).toHaveLength(0);
  });

  it('renders usage slot references read-only, with the not-in-use fallback', () => {
    const { container } = renderInspector();
    const usage = within(container).getByTestId('metadata-usage');
    expect(within(usage).getByTestId('chip-in-use')).toHaveTextContent(/In VT 2/);
    expect(usage.querySelectorAll('input, select, textarea')).toHaveLength(0);

    const { container: unused } = renderInspector({
      asset: { ...ASSET, usage: { slotReferences: [] } },
    });
    const usageUnused = within(unused).getByTestId('metadata-usage');
    expect(usageUnused).toHaveTextContent(/Not referenced by any slot/);
  });

  it('renders derivative lineage read-only when present', () => {
    const { container } = renderInspector({
      asset: {
        ...ASSET,
        kind: 'clip',
        provenance: {
          ...ASSET.provenance,
          originKind: 'derived',
          parentAssetName: 'Interview 12',
        },
      },
    });
    const provenance = within(container).getByTestId('metadata-provenance');
    expect(provenance).toHaveTextContent(/Derived clip/);
    expect(provenance).toHaveTextContent(/Derived from/);
  });
});
