// OutputsPage "From device" — the create dialog offers the managed devices'
// decode targets (ADR-M009 facet (b)); picking one binds the new (ordinary)
// Output to the device via `device_ref`, and the driver later points the
// device's decode slot at the rendition.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { MemoryRouter } from 'react-router-dom';

import { OutputsPage } from './OutputsPage';
import { renderWithProviders } from '../test/render';

const DEVICES = [
  {
    id: 'dev-a',
    name: 'Foyer box',
    body: { id: 'dev-a', driver: 'zowietek', address: 'http://[fd00::1]' },
  },
];

const server = setupServer(
  http.get('*/api/v1/outputs', () => HttpResponse.json([])),
  http.get('*/api/v1/devices', () => HttpResponse.json(DEVICES)),
  http.get('*/api/v1/devices/dev-a/output-targets', () =>
    HttpResponse.json([{ id: 'slot-0', kind: 'rtsp', label: 'Decode slot 0' }]),
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

function renderOutputs(): void {
  renderWithProviders(
    <MemoryRouter>
      <OutputsPage />
    </MemoryRouter>,
  );
}

describe('OutputsPage from-device section', () => {
  it('offers the device decode targets and binds the new output via device_ref', async () => {
    let posted: unknown;
    server.use(
      http.post('*/api/v1/outputs/:id', async ({ request, params }) => {
        posted = { id: params.id, payload: await request.json() };
        return HttpResponse.json(
          { id: String(params.id), name: 'Foyer feed', body: { kind: 'rtsp_server', mount: '/foyer' } },
          { status: 201, headers: { ETag: '"1"' } },
        );
      }),
    );
    renderOutputs();
    await userEvent.click(await screen.findByRole('button', { name: 'New output' }));
    const dialog = await screen.findByRole('dialog');
    expect(
      await within(dialog).findByText(/from a managed device/i),
    ).toBeInTheDocument();
    await userEvent.click(
      await within(dialog).findByRole('button', {
        name: /use decode target: decode slot 0/i,
      }),
    );
    // The binding chip is visible before saving.
    expect(within(dialog).getByText(/bound to device/i)).toBeInTheDocument();
    await userEvent.type(within(dialog).getByLabelText('Identifier'), 'out-foyer');
    await userEvent.type(within(dialog).getByLabelText('Name'), 'Foyer feed');
    await userEvent.type(within(dialog).getByLabelText(/mount/i), '/foyer');
    await userEvent.click(within(dialog).getByRole('button', { name: 'Create' }));
    const captured = posted as {
      id: string;
      payload: { body: Record<string, unknown> };
    };
    expect(captured.id).toBe('out-foyer');
    expect(captured.payload.body.kind).toBe('rtsp_server');
    expect(captured.payload.body.device_ref).toBe('dev-a');
  });
});
