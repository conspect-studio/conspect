// #317/#307: the Layouts page "Apply" with a switcher M/E declared. The page
// knows in advance that the Program canvas is the M/E bus, so Apply is
// disabled with the reason shown in text; when the server refuses with the
// typed problem anyway, the refusal and its reason are rendered, not a
// generic error.
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
import { LayoutsPage } from './LayoutsPage';
import { Toaster } from '../components/ui/toaster';

i18n.load('en', messages);
i18n.activate('en');

const cell = (id: string, x: number, w: number, source: string) => ({
  id, rect: { x, y: 0, w, h: 1 }, source: { input_id: source },
});
const canvas = { width: 64, height: 64, fps: '25/1' };
const layouts = [
  { id: 'single', name: 'Single', version: 1, body: { canvas, layout: { kind: 'absolute' }, cells: [cell('pgm', 0, 1, 'cam-1')] } },
  { id: 'news', name: 'NewsOne', version: 1, body: { canvas, layout: { kind: 'absolute' }, cells: [cell('a', 0, 0.5, 'cam-1'), cell('b', 0.5, 0.5, 'cam-2')] } },
];
let switcherOn = true;
let applied: unknown[] = [];
const server = setupServer(
  http.get('*/api/v1/layouts', () => HttpResponse.json(layouts)),
  http.get('*/api/v1/switcher/me1', () => (switcherOn
    ? HttpResponse.json({ program_source: 'cam-1', preview_source: 'cam-2', tick: 7, in_flight: false })
    : HttpResponse.json({ type: '/problems/conflict', title: 'Conflict', status: 409 }, { status: 409 }))),
  http.post('*/api/v1/commands/apply-layout', async ({ request }) => {
    applied.push(await request.json());
    return HttpResponse.json({
      type: '/problems/switcher-owns-program',
      title: 'The switcher owns the Program canvas',
      status: 422,
      detail: 'The switcher is on: the Program canvas is the M/E bus, so layout "single" cannot replace it.',
    }, { status: 422 });
  }),
);
beforeAll(() => { server.listen(); });
afterEach(() => { server.resetHandlers(); switcherOn = true; applied = []; });
afterAll(() => { server.close(); });

function page() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <I18nProvider i18n={i18n}>
        <MemoryRouter><LayoutsPage /><Toaster /></MemoryRouter>
      </I18nProvider>
    </QueryClientProvider>,
  );
}

function applyButton(name: string): HTMLButtonElement {
  return screen.getByRole('button', { name: `Apply layout to engine: ${name}` });
}

describe('LayoutsPage Apply with the switcher on (#307)', () => {
  it('disables Apply for every layout and shows the reason in text', async () => {
    page();
    await screen.findByText('NewsOne');
    await waitFor(() => { expect(applyButton('NewsOne').disabled).toBe(true); });
    expect(applyButton('NewsOne').getAttribute('data-testid')).toBe('layout-apply-blocked-by-switcher');
    const reasons = screen.getAllByTestId('layout-apply-blocked-reason');
    const reason = reasons.find((r) => r.id === applyButton('NewsOne').getAttribute('aria-describedby'));
    expect(reason?.textContent).toContain('The switcher is on: Program is the M/E bus.');
    expect(applyButton('Single').disabled).toBe(true);
  });

  it('keeps Apply enabled for a multi-cell layout when no switcher is declared', async () => {
    switcherOn = false;
    page();
    await screen.findByText('NewsOne');
    await waitFor(() => { expect(applyButton('NewsOne').disabled).toBe(false); });
    expect(screen.queryByTestId('layout-apply-blocked-reason')).toBeNull();
  });

  it('renders the typed server refusal with its reason, not a generic failure', async () => {
    // The switcher is turned on after the page loaded without it (stale page):
    // the server is authoritative and its typed refusal is rendered.
    switcherOn = false;
    page();
    await screen.findByText('Single');
    await waitFor(() => { expect(applyButton('Single').disabled).toBe(false); });
    await userEvent.click(applyButton('Single'));
    await waitFor(() => { expect(applied).toEqual([{ layout: 'single' }]); });
    const title = await screen.findByText('Layout not applied: the switcher is on');
    const toast = title.closest('li') ?? document.body;
    expect(within(toast).getByText(/Program canvas is the M\/E bus/).textContent)
      .toContain('Nothing was changed; the current layout keeps running.');
    expect(screen.queryByText('Could not apply layout')).toBeNull();
  });
});
