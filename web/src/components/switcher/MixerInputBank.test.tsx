import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from 'vitest';

vi.mock('../../realtime/shared', () => ({ subscribeRealtime: () => () => undefined }));
import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { i18n } from '@lingui/core';
import { I18nProvider } from '@lingui/react';
import { MemoryRouter } from 'react-router-dom';
import { messages } from '../../locales/en/messages';
import { SwitcherPage } from '../../pages/SwitcherPage';

i18n.load('en', messages);
i18n.activate('en');

const patch = {
  mixer: [{
    me: 'me1', inputs_version: 0, tick: 7,
    inputs: [
      { number: 1, label: 'Source 1', video: { source: 'abc-news' }, on_program: true, on_preview: false },
      { number: 2, label: null, video: { source: 'vt1' }, on_program: false, on_preview: true },
      { number: 3, label: null, video: { redacted: true }, on_program: false, on_preview: false },
      { number: 4, label: null, video: null, on_program: false, on_preview: false },
    ],
  }],
};
const sources = [{ id: 'bars', name: 'bars', body: { kind: 'bars' } }, { id: 'vt1', name: 'vt1', body: { kind: 'bars' } }];
let takes: unknown[] = [];
let selects: unknown[] = [];
const server = setupServer(
  http.get('*/api/v1/switcher/me1', () => HttpResponse.json({ program_source: 'abc-news', preview_source: 'vt1', tick: 7, in_flight: false })),
  http.get('*/api/v1/sources', () => HttpResponse.json(sources)),
  http.get('*/api/v1/patch', () => HttpResponse.json(patch)),
  http.post('*/api/v1/switcher/plan', () => HttpResponse.json({ can_take: true, class: 'class1', scene_version: 1 })),
  http.post('*/api/v1/switcher/commands/:kind', async ({ request }) => {
    selects.push(await request.json());
    return HttpResponse.json({ operation_id: 'sel-1', kind: 'select_preview' }, { status: 202 });
  }),
  http.post('*/api/v1/patch/plan', () => HttpResponse.json({ class: 'class1', can_take: true, crosspoints: [] })),
  http.post('*/api/v1/patch/take', async ({ request }) => {
    takes.push(await request.json());
    return HttpResponse.json({ operation_id: 'patch-1', status: 'accepted' }, { status: 202 });
  }),
);
beforeAll(() => { server.listen(); });
afterEach(() => { server.resetHandlers(); takes = []; selects = []; });
afterAll(() => { server.close(); });

function page() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  return render(<QueryClientProvider client={client}><I18nProvider i18n={i18n}><MemoryRouter><SwitcherPage /></MemoryRouter></I18nProvider></QueryClientProvider>);
}

describe('MixerInputBank on the Switcher page', () => {
  it('renders numbered inputs with label, source, tally and redaction', async () => {
    page();
    const one = await screen.findByRole('listitem', { name: 'Input 1' });
    expect(within(one).getByText(/Source 1/)).toBeTruthy();
    expect(within(one).getByText(/abc-news/)).toBeTruthy();
    expect(within(one).getByRole('status', { name: 'On Program' })).toBeTruthy();
    expect(within(screen.getByRole('listitem', { name: 'Input 2' })).getByRole('status', { name: 'On Preview' })).toBeTruthy();
    expect(within(screen.getByRole('listitem', { name: 'Input 3' })).getByText(/Restricted source/)).toBeTruthy();
    expect(within(screen.getByRole('listitem', { name: 'Input 4' })).getByText(/Unpatched/)).toBeTruthy();
    // The source-id select of today's page is replaced by the bank.
    expect(screen.queryByLabelText('Source')).toBeNull();
  });

  it('selects by input number, not by source', async () => {
    page();
    const three = await screen.findByRole('listitem', { name: 'Input 3' });
    await waitFor(() => { expect(within(three).getByRole('button', { name: 'PVW' })).toHaveProperty('disabled', false); });
    await userEvent.click(within(three).getByRole('button', { name: 'PVW' }));
    await waitFor(() => { expect(selects).toHaveLength(1); });
    expect(selects[0]).toMatchObject({ action: 'select_preview', input: 3 });
    expect(selects[0]).not.toHaveProperty('source');
  });

  it('re-patching the input on Program needs a confirm and only then sends on_air allow', async () => {
    page();
    const one = await screen.findByRole('listitem', { name: 'Input 1' });
    await within(one).findByRole('option', { name: 'bars' });
    await userEvent.selectOptions(within(one).getByLabelText('Patch source for input 1'), 'bars');
    await userEvent.click(within(one).getByRole('button', { name: 'Patch' }));
    const confirm = await screen.findByRole('alertdialog', { name: 'Confirm on-air re-patch' });
    expect(takes).toHaveLength(0);
    await userEvent.click(within(confirm).getByRole('button', { name: 'Cancel' }));
    expect(screen.queryByRole('alertdialog')).toBeNull();
    expect(takes).toHaveLength(0);
    await userEvent.click(within(one).getByRole('button', { name: 'Patch' }));
    await userEvent.click(within(await screen.findByRole('alertdialog')).getByRole('button', { name: 'Cut Program' }));
    await waitFor(() => { expect(takes).toHaveLength(1); });
    expect(takes[0]).toEqual({
      crosspoints: [{ plane: 'video', destination: { mixer: 'me1', input: 1 }, source: { source: 'bars' } }],
      on_air: 'allow',
    });
  });

  it('an off-air input re-patches without a confirm under protect', async () => {
    page();
    const four = await screen.findByRole('listitem', { name: 'Input 4' });
    await within(four).findByRole('option', { name: 'vt1' });
    await userEvent.selectOptions(within(four).getByLabelText('Patch source for input 4'), 'vt1');
    await userEvent.click(within(four).getByRole('button', { name: 'Patch' }));
    await waitFor(() => { expect(takes).toHaveLength(1); });
    expect(screen.queryByRole('alertdialog')).toBeNull();
    expect(takes[0]).toMatchObject({ on_air: 'protect' });
  });
});
