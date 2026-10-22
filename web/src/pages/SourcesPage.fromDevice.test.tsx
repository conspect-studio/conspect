// SourcesPage "From device" — the create dialog offers the managed devices'
// enumerated source candidates (ADR-M009 facet (a)); picking one prefills the
// transport form and the saved body carries `device_ref`, making an ORDINARY
// managed Source bound to the device.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { MemoryRouter } from 'react-router-dom';

import { SourcesPage } from './SourcesPage';
import { renderWithProviders } from '../test/render';

const DEVICES = [
  {
    id: 'dev-a',
    name: 'Foyer box',
    body: { id: 'dev-a', driver: 'zowietek', address: 'http://[fd00::1]' },
  },
];

const server = setupServer(
  http.get('*/api/v1/sources', () => HttpResponse.json([])),
  http.get('*/api/v1/devices', () => HttpResponse.json(DEVICES)),
  http.get('*/api/v1/devices/dev-a/source-candidates', () =>
    HttpResponse.json([
      { id: 'main', kind: 'rtsp', url: 'rtsp://[fd00::1]:554/main', unverified: false },
    ]),
  ),
);

beforeAll(() => {
  server.listen();
});
afterEach(() => {
  server.resetHandlers();
});
afterAll(() => {
  server.close();
});

function renderSources(): void {
  renderWithProviders(
    <MemoryRouter>
      <SourcesPage />
    </MemoryRouter>,
  );
}

describe('SourcesPage from-device section', () => {
  it('offers device streams in the create dialog and prefills the form', async () => {
    renderSources();
    await userEvent.click(await screen.findByRole('button', { name: 'New source' }));
    const dialog = await screen.findByRole('dialog');
    expect(
      await within(dialog).findByText(/from a managed device/i),
    ).toBeInTheDocument();
    await userEvent.click(
      await within(dialog).findByRole('button', { name: /use stream: main/i }),
    );
    // The candidate prefilled the transport form…
    expect(
      within(dialog).getByDisplayValue('rtsp://[fd00::1]:554/main'),
    ).toBeInTheDocument();
    // …and the dialog shows the active device binding.
    expect(within(dialog).getByText(/bound to device/i)).toBeInTheDocument();
    expect(within(dialog).getByText('dev-a')).toBeInTheDocument();
  });

  it('the saved body carries device_ref alongside the ordinary source fields', async () => {
    let posted: unknown;
    server.use(
      http.post('*/api/v1/sources/:id', async ({ request, params }) => {
        posted = { id: params.id, payload: await request.json() };
        return HttpResponse.json(
          { id: String(params.id), name: 'Foyer main', body: { id: String(params.id), kind: 'rtsp', url: 'rtsp://[fd00::1]:554/main' } },
          { status: 201, headers: { ETag: '"1"' } },
        );
      }),
    );
    renderSources();
    await userEvent.click(await screen.findByRole('button', { name: 'New source' }));
    const dialog = await screen.findByRole('dialog');
    await userEvent.click(
      await within(dialog).findByRole('button', { name: /use stream: main/i }),
    );
    await userEvent.type(within(dialog).getByLabelText('Identifier'), 'src-main');
    await userEvent.type(within(dialog).getByLabelText('Name'), 'Foyer main');
    await userEvent.click(within(dialog).getByRole('button', { name: 'Create' }));
    const captured = posted as {
      id: string;
      payload: { body: Record<string, unknown> };
    };
    expect(captured.id).toBe('src-main');
    expect(captured.payload.body.kind).toBe('rtsp');
    expect(captured.payload.body.url).toBe('rtsp://[fd00::1]:554/main');
    expect(captured.payload.body.device_ref).toBe('dev-a');
  });

  it('the binding is clearable before saving', async () => {
    renderSources();
    await userEvent.click(await screen.findByRole('button', { name: 'New source' }));
    const dialog = await screen.findByRole('dialog');
    await userEvent.click(
      await within(dialog).findByRole('button', { name: /use stream: main/i }),
    );
    await userEvent.click(
      within(dialog).getByRole('button', { name: /clear device binding/i }),
    );
    expect(within(dialog).queryByText(/bound to device/i)).not.toBeInTheDocument();
  });
});
