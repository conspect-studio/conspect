// VTReferenceSummary tests — the honest slot-state contract:
//   * explicit empty / temporary / library states, no invented ones;
//   * `pending` is the accepted-not-applied line — never "working",
//     never a success claim (the applied event is the only truth);
//   * ON AIR gates unload-style actions with a visible reason on the
//     control, while Save alone stays available (protecting bytes is
//     always safe);
//   * a `supported: false` capability gate disables the action with the
//     caller's reason — the UI never pretends a missing contract works;
//   * every callback fires as an INTENT exactly once; nothing in the
//     component applies, accepts or "saves" on its own.
import { describe, expect, it, vi } from 'vitest';
import { fireEvent, screen, within } from '@testing-library/react';

import { renderWithProviders } from '../../../test/render';
import type { ActionGate } from './types';
import { VTReferenceSummary } from './VTReferenceSummary';
import type { VTReferenceState } from './VTReferenceSummary';

const SUPPORTED: ActionGate = { supported: true, unavailableReason: null };
const UNSUPPORTED: ActionGate = {
  supported: false,
  unavailableReason: 'Save API not implemented yet',
};

const TEMP: VTReferenceState = {
  kind: 'temporary',
  label: 'Interview 12 — fetched clip',
  byteSizeLabel: '210.4 MB',
  originHostLabel: 'youtube.com',
  fetchedAtLabel: '18:04',
};

const LIBRARY: VTReferenceState = {
  kind: 'library',
  assetName: 'Match 4 wide',
  positionLabel: '00:00:12:04',
  duration: {
    grid: { kind: 'video', numerator: 25, denominator: 1 },
    units: 750,
  },
  rights: 'unknown',
  rightsExpiryLabel: null,
};

function renderSlot(overrides: {
  state?: VTReferenceState;
  onAir?: boolean;
  onAirReason?: string;
  saveGate?: ActionGate;
  unloadGate?: ActionGate;
  pendingOperation?: { label: string } | null;
} = {}) {
  const onSaveIntent = vi.fn();
  const onSaveAndUnloadIntent = vi.fn();
  const onUnloadIntent = vi.fn();
  const onOpenBrowserIntent = vi.fn();
  const { container } = renderWithProviders(
    <VTReferenceSummary
      slotId="vt1"
      slotName="VT 1"
      state={overrides.state ?? TEMP}
      onAir={overrides.onAir ?? false}
      onAirReason={overrides.onAirReason}
      pendingOperation={overrides.pendingOperation ?? null}
      saveGate={overrides.saveGate ?? SUPPORTED}
      unloadGate={overrides.unloadGate ?? SUPPORTED}
      onSaveIntent={onSaveIntent}
      onSaveAndUnloadIntent={onSaveAndUnloadIntent}
      onUnloadIntent={onUnloadIntent}
      onOpenBrowserIntent={onOpenBrowserIntent}
    />,
  );
  return {
    container,
    onSaveIntent,
    onSaveAndUnloadIntent,
    onUnloadIntent,
    onOpenBrowserIntent,
  };
}

function summary(container: HTMLElement): HTMLElement {
  return container.querySelector<HTMLElement>('[data-testid="vt-reference-summary"]')!;
}

describe('VTReferenceSummary — explicit states', () => {
  it('temporary renders the TEMP warning with the exact byte size and source label', () => {
    const { container } = renderSlot();
    const row = summary(container);
    expect(row.dataset.state).toBe('temporary');
    expect(row).toHaveTextContent('Interview 12 — fetched clip');
    expect(within(row).getByTestId('chip-temp')).toHaveTextContent(
      /TEMP — unsaved bytes \(210\.4 MB\)/,
    );
    expect(within(row).getByTestId('chip-origin')).toHaveTextContent('youtube.com');
    expect(row).toHaveTextContent(/Fetched 18:04/);
  });

  it('library renders the asset through the exact per-asset duration grid', () => {
    const { container } = renderSlot({ state: LIBRARY });
    const row = summary(container);
    expect(row.dataset.state).toBe('library');
    expect(row).toHaveTextContent('Match 4 wide');
    expect(row).toHaveTextContent(/Position 00:00:12:04/);
    // 750 frames @ 25 fps exact rational grid, honest precision line.
    expect(row).toHaveTextContent('00:00:30:00');
    expect(row).toHaveTextContent(/25 fps/);
    // Unknown rights are visible as unknown — never cleared, never hidden.
    expect(within(row).getByTestId('chip-rights')).toHaveTextContent(
      /Rights unknown/,
    );
  });

  it('empty is honest — no bytes at risk — and offers the browser intent', () => {
    const helpers = renderSlot({ state: { kind: 'empty' } });
    const row = summary(helpers.container);
    expect(row.dataset.state).toBe('empty');
    expect(row).toHaveTextContent(/Nothing loaded — no bytes at risk/);
    fireEvent.click(screen.getByRole('button', { name: /Load from library…/ }));
    expect(helpers.onOpenBrowserIntent).toHaveBeenCalledOnce();
    expect(helpers.onSaveIntent).not.toHaveBeenCalled();
  });

  it('pending is the accepted-not-applied line — never a success claim', () => {
    const { container } = renderSlot({
      pendingOperation: { label: 'Save VT 1' },
    });
    const pending = within(summary(container)).getByTestId('vt-reference-pending');
    expect(pending).toHaveTextContent(
      /Save VT 1 accepted — waiting for the applied event\./,
    );
    // The summary itself never claims applied/saved/working.
    expect(pending.textContent).not.toMatch(/applied\./);
    expect(pending.textContent).not.toMatch(/saved/i);
    expect(pending.textContent).not.toMatch(/working/i);
  });
});

describe('VTReferenceSummary — on-air unload guard', () => {
  it('disables Save & unload while on air, with the reason visible on the control', () => {
    const helpers = renderSlot({ onAir: true });
    expect(summary(helpers.container).dataset.onAir).toBe('true');
    const button = screen.getByRole('button', { name: /Save & unload/ });
    expect(button).toBeDisabled();
    expect(button.getAttribute('title')).toMatch(/on air/i);
    fireEvent.click(button);
    expect(helpers.onSaveAndUnloadIntent).not.toHaveBeenCalled();
  });

  it('keeps Save alone available while on air — protecting bytes is always safe', () => {
    const helpers = renderSlot({ onAir: true });
    const save = screen.getByRole('button', { name: /^Save$/ });
    expect(save).not.toBeDisabled();
    fireEvent.click(save);
    expect(helpers.onSaveIntent).toHaveBeenCalledOnce();
  });

  it('disables Unload for a library-backed slot while on air', () => {
    const helpers = renderSlot({ state: LIBRARY, onAir: true });
    const unload = screen.getByRole('button', { name: /Unload/ });
    expect(unload).toBeDisabled();
    fireEvent.click(unload);
    expect(helpers.onUnloadIntent).not.toHaveBeenCalled();
  });

  it('accepts a caller-supplied on-air reason', () => {
    renderSlot({
      onAir: true,
      state: LIBRARY,
      onAirReason: 'VT 1 is ON AIR — cut away first.',
    });
    const unload = screen.getByRole('button', { name: /Unload/ });
    expect(unload.getAttribute('title')).toBe('VT 1 is ON AIR — cut away first.');
  });
});

describe('VTReferenceSummary — capability gates and intents', () => {
  it('a supported:false save gate disables Save with the caller’s reason visible', () => {
    const helpers = renderSlot({ saveGate: UNSUPPORTED, unloadGate: UNSUPPORTED });
    const save = screen.getByRole('button', { name: /^Save$/ });
    expect(save).toBeDisabled();
    expect(save.getAttribute('title')).toBe('Save API not implemented yet');
    expect(screen.getAllByText('Save API not implemented yet').length).toBeGreaterThan(0);
    fireEvent.click(save);
    expect(helpers.onSaveIntent).not.toHaveBeenCalled();
    const saveUnload = screen.getByRole('button', { name: /Save & unload/ });
    expect(saveUnload).toBeDisabled();
    fireEvent.click(saveUnload);
    expect(helpers.onSaveAndUnloadIntent).not.toHaveBeenCalled();
  });

  it('fires each intent exactly once when enabled and gates the rest', () => {
    const helpers = renderSlot();
    fireEvent.click(screen.getByRole('button', { name: /^Save$/ }));
    fireEvent.click(screen.getByRole('button', { name: /Save & unload/ }));
    expect(helpers.onSaveIntent).toHaveBeenCalledTimes(1);
    expect(helpers.onSaveAndUnloadIntent).toHaveBeenCalledTimes(1);
    expect(helpers.onUnloadIntent).not.toHaveBeenCalled();
  });

  it('temporary rows never render an Unload action (unload is library-backed only)', () => {
    const { container } = renderSlot();
    expect(
      within(summary(container)).queryByRole('button', { name: /^Unload$/ }),
    ).toBeNull();
  });
});
