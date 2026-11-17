// K5 desk integration: the keyer strip/panel wired into SwitcherPage against
// the real plan→command flow (design note §10, K5).
import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from 'vitest';

vi.mock('../realtime/shared', () => ({ subscribeRealtime: () => () => undefined }));
import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { i18n } from '@lingui/core';
import { I18nProvider } from '@lingui/react';
import { MemoryRouter } from 'react-router-dom';
import { messages } from '../locales/en/messages';
import { SwitcherPage } from './SwitcherPage';

i18n.load('en', messages);
i18n.activate('en');

const DSK1 = {
  id: 'dsk1', family: 'dsk', fill: 'gfx-channel-1', key: null, type: 'linear',
  fly: null, mask: null, on_air: false, tie: false, ramp: null, rate: 15,
  loss: 'drop_to_off_air', stale: false, pending: false,
};
let state = { program_source: 'cam-1', preview_source: 'cam-2', tick: 7, in_flight: false, keyers: [DSK1], tally: { program: [], preview: [] } };
let keyerPlans: unknown[] = [];
let keyerCommands: { action: string; body: unknown }[] = [];
let planAnswer: unknown = { can_take: true, class: 'class1', keyer: 'main/dsk1', on_air: false };

const server = setupServer(
  http.get('*/api/v1/switcher/me1', () => HttpResponse.json(state)),
  http.get('*/api/v1/sources', () => HttpResponse.json([{ id: 'gfx-channel-1', name: 'GFX 1', body: { kind: 'bars' } }, { id: 'cam-1', name: 'Cam 1', body: { kind: 'bars' } }])),
  http.post('*/api/v1/switcher/plan', async ({ request }) => {
    const body: unknown = await request.json();
    keyerPlans.push(body);
    return HttpResponse.json(planAnswer as object);
  }),
  http.post('*/api/v1/switcher/commands/:kind', async ({ request, params }) => {
    const body: unknown = await request.json();
    const kind = String(params.kind);
    keyerCommands.push({ action: kind, body });
    return HttpResponse.json({ operation_id: `op-${kind}`, kind }, { status: 202 });
  }),
);
beforeAll(() => { server.listen(); });
afterEach(() => {
  server.resetHandlers();
  keyerPlans = [];
  keyerCommands = [];
  planAnswer = { can_take: true, class: 'class1', keyer: 'main/dsk1', on_air: false };
  state = { ...state, keyers: [DSK1] };
});
afterAll(() => { server.close(); });

function page() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  return render(<QueryClientProvider client={client}><I18nProvider i18n={i18n}><MemoryRouter><SwitcherPage /></MemoryRouter></I18nProvider></QueryClientProvider>);
}

describe('SwitcherPage keyer desk (K5)', () => {
  it('renders the DSK strip and panel from the snapshot keyers[]', async () => {
    page();
    expect(await screen.findByTestId('keyer-strip-dsk1')).toBeTruthy();
    expect(screen.getByTestId('keyer-panel-dsk1')).toBeTruthy();
    expect(screen.getByTestId('keyer-lamp-dsk1')).toHaveAttribute('data-on-air', 'false');
  });

  it('CUT plans then commands keyer_cut with the toggled target state', async () => {
    page();
    await screen.findByTestId('keyer-strip-dsk1');
    const strip = screen.getByTestId('keyer-strip-dsk1');
    await userEvent.click(within(strip).getByRole('button', { name: 'Cut' }));
    await waitFor(() => { expect(keyerCommands).toHaveLength(1); });
    expect(keyerCommands[0]).toEqual({ action: 'keyer_cut', body: expect.objectContaining({ keyer: 'dsk1', on: true, action: 'keyer_cut' }) });
  });

  it('TIE toggles via keyer_tie', async () => {
    page();
    await screen.findByTestId('keyer-strip-dsk1');
    const strip = screen.getByTestId('keyer-strip-dsk1');
    await userEvent.click(within(strip).getByRole('checkbox', { name: 'Tie' }));
    await waitFor(() => { expect(keyerCommands).toHaveLength(1); });
    expect(keyerCommands[0]).toEqual({ action: 'keyer_tie', body: expect.objectContaining({ keyer: 'dsk1', tie: true }) });
  });

  it('a capacity plan refusal is shown and nothing is applied', async () => {
    planAnswer = { can_take: false, class: 'class1', keyer: 'main/dsk1', on_air: false, cost: { delta: 1.5, fits: false }, refusal: { type: 'capacity', detail: 'DSK budget exceeded' } };
    page();
    await screen.findByTestId('keyer-panel-dsk1');
    const panel = screen.getByTestId('keyer-panel-dsk1');
    await userEvent.click(within(panel).getByRole('button', { name: 'Review & apply' }));
    expect(await within(panel).findByTestId('keyer-refusal-dsk1')).toHaveTextContent('DSK budget exceeded');
    expect(keyerCommands).toHaveLength(0);
  });

  it('an on-air fill change is refused on-air-protected, then applies after the explicit allow confirm', async () => {
    state = { ...state, keyers: [{ ...DSK1, on_air: true }] };
    planAnswer = { can_take: false, class: 'class1', keyer: 'main/dsk1', on_air: true, refusal: { type: 'on-air-protected', detail: 'needs allow' } };
    page();
    await screen.findByTestId('keyer-panel-dsk1');
    const panel = screen.getByTestId('keyer-panel-dsk1');
    await userEvent.selectOptions(within(panel).getByLabelText('Fill'), 'cam-1');
    await userEvent.click(within(panel).getByRole('button', { name: 'Review & apply' }));
    const confirm = await screen.findByRole('alertdialog', { name: 'Confirm on-air keyer change' });
    expect(keyerCommands).toHaveLength(0);
    planAnswer = { can_take: true, class: 'class1', keyer: 'main/dsk1', on_air: true };
    await userEvent.click(within(confirm).getByRole('button', { name: 'Allow on-air change' }));
    await waitFor(() => { expect(keyerCommands).toHaveLength(1); });
    expect(keyerCommands[0]?.action).toBe('keyer_set');
    expect(keyerCommands[0]?.body).toMatchObject({ on_air: 'allow', fill: 'cam-1' });
  });

  it('renders nothing when the snapshot carries no keyers (no backend keyers deployed)', async () => {
    state = { ...state, keyers: [] };
    page();
    await screen.findByRole('button', { name: 'Cut' }); // the background CUT still renders
    expect(screen.queryByText('Downstream keyers')).toBeNull();
  });
});
