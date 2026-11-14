// #317: the Layouts page "Apply" with a switcher M/E declared. Where the page
// can know in advance (switcher on, layout has more than one cell) Apply is
// disabled with the reason shown in text; when the server refuses with the
// typed problem, the refusal and its reason are rendered, not a generic error.
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
let putBodies: Record<string, unknown>[] = [];
let putStatus = 200;
const server = setupServer(
  http.get('*/api/v1/layouts', () => HttpResponse.json(layouts)),
  http.get('*/api/v1/layouts/:id', ({ params }) => {
    const found = layouts.find((l) => l.id === params.id);
    if (found === undefined) {
      return new HttpResponse(null, { status: 404 });
    }
    return HttpResponse.json(found, { headers: { ETag: 'v1' } });
  }),
  http.put('*/api/v1/layouts/:id', async ({ request, params }) => {
    const body = (await request.json()) as Record<string, unknown>;
    putBodies.push(body);
    if (putStatus !== 200) {
      return HttpResponse.json(
        { type: '/problems/forbidden', title: 'Access denied', status: putStatus, detail: 'routed-layout: this layout is on air on an output; changing it needs Write on every output it feeds and Read on every source it shows' },
        { status: putStatus },
      );
    }
    const found = layouts.find((l) => l.id === params.id);
    return HttpResponse.json(
      { id: params.id, name: body.name, version: (found?.version ?? 1) + 1, body: body.body },
      { headers: { ETag: 'v2' } },
    );
  }),
  http.get('*/api/v1/switcher/me1', () => (switcherOn
    ? HttpResponse.json({ program_source: 'cam-1', preview_source: 'cam-2', tick: 7, in_flight: false })
    : HttpResponse.json({ type: '/problems/conflict', title: 'Conflict', status: 409 }, { status: 409 }))),
  http.post('*/api/v1/commands/apply-layout', async ({ request }) => {
    applied.push(await request.json());
    return HttpResponse.json({
      type: '/problems/layout-not-admissible-with-switcher',
      title: 'Not admissible while the switcher is on',
      status: 422,
      detail: 'The switcher is on: the Program cell must be bound to the M/E program source "cam-1" (it is bound to "cam-2").',
    }, { status: 422 });
  }),
);
beforeAll(() => { server.listen(); });
afterEach(() => { server.resetHandlers(); switcherOn = true; applied = []; putBodies = []; putStatus = 200; });
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

describe('LayoutsPage Apply with the switcher on (#317)', () => {
  it('disables Apply for a multi-cell layout and shows the reason in text', async () => {
    page();
    await screen.findByText('NewsOne');
    await waitFor(() => { expect(applyButton('NewsOne').disabled).toBe(true); });
    expect(applyButton('NewsOne').getAttribute('data-testid')).toBe('layout-apply-blocked-by-switcher');
    const reason = screen.getByTestId('layout-apply-blocked-reason');
    expect(reason.textContent).toContain('The switcher is on: a multi-cell layout cannot be the Program canvas.');
    expect(applyButton('NewsOne').getAttribute('aria-describedby')).toBe(reason.id);
    expect(applyButton('Single').disabled).toBe(false);
  });

  it('keeps Apply enabled for a multi-cell layout when no switcher is declared', async () => {
    switcherOn = false;
    page();
    await screen.findByText('NewsOne');
    await waitFor(() => { expect(applyButton('NewsOne').disabled).toBe(false); });
    expect(screen.queryByTestId('layout-apply-blocked-reason')).toBeNull();
  });

  it('renders the typed server refusal with its reason, not a generic failure', async () => {
    page();
    await screen.findByText('Single');
    await userEvent.click(applyButton('Single'));
    await waitFor(() => { expect(applied).toEqual([{ layout: 'single' }]); });
    const title = await screen.findByText('Layout not applied: the switcher is on');
    const toast = title.closest('li') ?? document.body;
    expect(within(toast).getByText(/must be bound to the M\/E program source/).textContent)
      .toContain('Nothing was changed; the current layout keeps running.');
    expect(screen.queryByText('Could not apply layout')).toBeNull();
  });
});

// PR-B: the "Layers" column + dialog editing the engine `overlays` sub-doc.
function layersButton(name: string): HTMLButtonElement {
  return screen.getByRole('button', { name: `Layers: ${name}` });
}

describe('LayoutsPage Layers column + dialog', () => {
  it('shows "All" for a layout with no overlays key (every layer at its default)', async () => {
    page();
    await screen.findByText('Single');
    const row = screen.getByText('Single').closest('tr');
    expect(row).not.toBeNull();
    // The engine default is 5-of-6 (safe_area off), not "All" — labels/
    // meters/flags/tally/clock on, safe_area off.
    expect(within(row as HTMLElement).getByText('5 of 6')).toBeInTheDocument();
  });

  it('opens the Layers dialog with the accessible title/description and six controls', async () => {
    page();
    await screen.findByText('Single');
    await userEvent.click(layersButton('Single'));
    const dialog = await screen.findByRole('dialog');
    expect(within(dialog).getByText('Layers')).toBeInTheDocument();
    expect(within(dialog).getByLabelText('Source names')).toBeInTheDocument();
    expect(within(dialog).getByLabelText('Audio meters')).toBeInTheDocument();
    expect(within(dialog).getByLabelText('State & fault flags')).toBeInTheDocument();
    expect(within(dialog).getByLabelText('Tally borders')).toBeInTheDocument();
    expect(within(dialog).getByLabelText('Clock')).toBeInTheDocument();
    expect(within(dialog).getByLabelText('Safe-area markers')).toBeInTheDocument();
    const metersCheckbox = within(dialog).getByLabelText('Audio meters');
    expect(metersCheckbox).toHaveProperty('checked', true);
    const safeAreaCheckbox = within(dialog).getByLabelText('Safe-area markers');
    expect(safeAreaCheckbox).toHaveProperty('checked', false);
  });

  it('toggling meters off and saving sends overlays {..., meters:false} with every other key unchanged', async () => {
    page();
    await screen.findByText('Single');
    await userEvent.click(layersButton('Single'));
    const dialog = await screen.findByRole('dialog');
    await userEvent.click(within(dialog).getByLabelText('Audio meters'));
    await userEvent.click(within(dialog).getByRole('button', { name: 'Save' }));
    await waitFor(() => { expect(putBodies).toHaveLength(1); });
    const sent = putBodies[0];
    expect(sent?.name).toBe('Single');
    const body = sent?.body as Record<string, unknown>;
    expect(body.overlays).toEqual({
      labels: true,
      meters: false,
      flags: true,
      tally: true,
      clock: true,
      safe_area: false,
    });
    expect(body.canvas).toEqual(canvas);
    expect(body.layout).toEqual({ kind: 'absolute' });
    expect(body.cells).toEqual(layouts[0]?.body.cells);
  });

  it('"Clean (no overlays)" sets every layer off and saves all-false', async () => {
    page();
    await screen.findByText('Single');
    await userEvent.click(layersButton('Single'));
    const dialog = await screen.findByRole('dialog');
    await userEvent.click(within(dialog).getByRole('button', { name: 'Clean (no overlays)' }));
    await userEvent.click(within(dialog).getByRole('button', { name: 'Save' }));
    await waitFor(() => { expect(putBodies).toHaveLength(1); });
    const body = putBodies[0]?.body as Record<string, unknown>;
    expect(body.overlays).toEqual({
      labels: false,
      meters: false,
      flags: false,
      tally: false,
      clock: false,
      safe_area: false,
    });
  });

  it('"Reset to defaults" after toggling clears the overlays key entirely (byte-stable default)', async () => {
    page();
    await screen.findByText('Single');
    await userEvent.click(layersButton('Single'));
    const dialog = await screen.findByRole('dialog');
    await userEvent.click(within(dialog).getByLabelText('Audio meters'));
    await userEvent.click(within(dialog).getByRole('button', { name: 'Reset to defaults' }));
    await userEvent.click(within(dialog).getByRole('button', { name: 'Save' }));
    await waitFor(() => { expect(putBodies).toHaveLength(1); });
    const body = putBodies[0]?.body as Record<string, unknown>;
    expect('overlays' in body).toBe(false);
  });

  it('a 403 routed-layout failure on save shows a readable message, not a generic error', async () => {
    putStatus = 403;
    page();
    await screen.findByText('Single');
    await userEvent.click(layersButton('Single'));
    const dialog = await screen.findByRole('dialog');
    await userEvent.click(within(dialog).getByLabelText('Audio meters'));
    await userEvent.click(within(dialog).getByRole('button', { name: 'Save' }));
    const title = await screen.findByText('Could not save layers: this layout is routed');
    const toast = title.closest('li') ?? document.body;
    expect(within(toast).getByText(/routed-layout:/).textContent).toContain(
      'needs Write on every output it feeds',
    );
  });

  it('is disabled while saving', async () => {
    page();
    await screen.findByText('Single');
    await userEvent.click(layersButton('Single'));
    const dialog = await screen.findByRole('dialog');
    const saveBtn = within(dialog).getByRole('button', { name: 'Save' });
    await userEvent.click(saveBtn);
    // Immediately after the click the mutation is pending; the Save button
    // (and the checkboxes) must already be disabled.
    expect(saveBtn.hasAttribute('disabled')).toBe(true);
    await waitFor(() => { expect(putBodies).toHaveLength(1); });
  });
});
