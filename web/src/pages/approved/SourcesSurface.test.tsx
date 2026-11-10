import { afterEach, describe, expect, it, vi } from 'vitest';
import { screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter } from 'react-router-dom';

import { SourcesSurface } from './SourcesSurface';
import type { SourceDemand } from '../../api/sourceStatus';
import type { SourceView } from '../../resources/types';
import { renderWithProviders } from '../../test/render';

const src = (id: string, name: string, rawKind = 'hls'): SourceView => ({
  id, name, kind: 'hls', rawKind, editable: true, locator: `https://x.test/${id}`, eventObservation: 'not_supported',
});
const demand = (id: string, phase: SourceDemand['phase'], extra: Partial<SourceDemand> = {}): SourceDemand => ({
  id, phase, consumers: [], video_running: false, audio_running: false, video_wanted: false, audio_wanted: false,
  activation: 'on_demand', ...extra,
});

const rows = [src('ok', 'Fine'), src('bad', 'Broken', 'youtube'), src('wait', 'Waiting'), src('unk', 'Mystery')];
const status = new Map<string, SourceDemand>([
  ['ok', demand('ok', 'ready', { activation: 'always_on' })],
  ['bad', demand('bad', 'failed', { detail: 'ingest ended remotely' })],
  ['wait', demand('wait', 'starting')],
]);

function setTable(matches: boolean): void {
  window.matchMedia = (query: string) => ({
    matches, media: query, onchange: null,
    addListener: () => undefined, removeListener: () => undefined,
    addEventListener: () => undefined, removeEventListener: () => undefined, dispatchEvent: () => false,
  });
}
afterEach(() => { vi.restoreAllMocks(); });

function mount(onEdit = vi.fn(), onDelete = vi.fn(), tally?: { program: string; preview: string }): void {
  renderWithProviders(
    <MemoryRouter>
      <SourcesSurface rows={rows} demand={status} tileFor={() => undefined} linkFor={() => undefined} onEdit={onEdit} onDelete={onDelete} tally={tally} />
    </MemoryRouter>,
  );
}

describe('SourcesSurface', () => {
  it('table: six columns, exceptions first, failure reason inline, unknowns stay unknown', () => {
    setTable(true);
    mount();
    const headers = screen.getAllByRole('columnheader').map((h) => h.textContent);
    expect(headers).toEqual(['Source', 'Signal', 'Lifecycle', 'Activation', 'Tally', 'Actions']);
    const order = screen.getAllByTestId(/^source-row-/).map((r) => r.getAttribute('data-testid'));
    expect(order).toEqual(['source-row-bad', 'source-row-wait', 'source-row-ok', 'source-row-unk']);
    expect(screen.getByTestId('source-failure-bad')).toHaveTextContent('ingest ended remotely');
    const unk = screen.getByTestId('source-row-unk');
    expect(within(unk).getByText('Status unknown')).toBeVisible();
    expect(within(unk).getByText('Signal unknown')).toBeVisible();
    expect(within(unk).getByText('Activation unknown')).toBeVisible();
    expect(within(unk).getByText('Tally unknown')).toBeVisible();
    expect(within(unk).queryByRole('button', { name: /^(Start|Stop)/ })).toBeNull();
    // A ready source is not reported On air.
    expect(screen.queryByText(/on air/i, { selector: '.src-pill-host *' })).toBeNull();
  });

  it('filters by search and Needs me; On air is unavailable with a reason', async () => {
    setTable(true);
    mount();
    await userEvent.click(screen.getByRole('button', { name: 'Needs me' }));
    expect(screen.getAllByTestId(/^source-row-/)).toHaveLength(2);
    await userEvent.click(screen.getByRole('button', { name: 'All' }));
    await userEvent.type(screen.getByRole('searchbox'), 'myst');
    expect(screen.getAllByTestId(/^source-row-/)).toHaveLength(1);
    const onAir = screen.getByRole('button', { name: 'On air' });
    expect(onAir).toHaveAttribute('aria-disabled', 'true');
    expect(onAir).toHaveAccessibleDescription(/program bus is not confirmed/);
  });

  it('table: Details reveals the retained Edit and Delete controls', async () => {
    setTable(true);
    const onEdit = vi.fn();
    const onDelete = vi.fn();
    mount(onEdit, onDelete);
    expect(screen.queryByRole('button', { name: 'Edit source: Fine' })).toBeNull();
    await userEvent.click(screen.getByRole('button', { name: 'Details: Fine' }));
    await userEvent.click(screen.getByRole('button', { name: 'Edit source: Fine' }));
    await userEvent.click(screen.getByRole('button', { name: 'Delete source: Fine' }));
    expect(onEdit).toHaveBeenCalledWith(rows[0]);
    expect(onDelete).toHaveBeenCalledWith(rows[0]);
  });

  it('phone: cards with exceptions first and controls inline', () => {
    setTable(false);
    mount();
    expect(screen.queryByRole('table')).toBeNull();
    const cards = screen.getAllByTestId(/^source-card-/).map((c) => c.getAttribute('data-testid'));
    expect(cards[0]).toBe('source-card-bad');
    expect(within(screen.getByTestId('source-card-ok')).getByRole('button', { name: 'Edit source: Fine' })).toBeVisible();
  });

  it('confirmed program bus: On air filter works and tally shows red/amber/dark', async () => {
    setTable(true);
    mount(vi.fn(), vi.fn(), { program: 'ok', preview: 'wait' });
    expect(within(screen.getByTestId('source-row-ok')).getByText('● RED — on air')).toBeVisible();
    expect(within(screen.getByTestId('source-row-wait')).getByText('● AMBER — preview')).toBeVisible();
    expect(within(screen.getByTestId('source-row-bad')).getByText('— dark')).toBeVisible();
    const onAir = screen.getByRole('button', { name: 'On air' });
    expect(onAir).not.toHaveAttribute('aria-disabled');
    await userEvent.click(onAir);
    expect(screen.getAllByTestId(/^source-row-/)).toHaveLength(1);
    expect(screen.getByTestId('source-row-ok')).toBeVisible();
  });
});
