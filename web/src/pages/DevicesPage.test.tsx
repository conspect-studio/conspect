// DevicesPage — the managed-devices list + adopt flow + discovery UX
// (managed-devices.md §9, DEV-A6). The list shows each device's lifecycle
// state as icon+text (never colour alone), driver, mode, temperature,
// last-seen, and sync-group chip, fed by the conflated `device.status` lane
// with a REST fallback (these tests run with no WebSocket, so the REST
// fallback is what feeds the badges). Discovery is an UNTRUSTED inventory:
// the panel says so, never adopts by itself, and its per-row Adopt button
// only PREFILLS the explicit confirm-adopt dialog.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { MemoryRouter } from 'react-router-dom';

import { DevicesPage } from './DevicesPage';
import { renderWithProviders } from '../test/render';

const DEVICES = [
  {
    id: 'dev-a',
    name: 'Foyer box',
    body: {
      id: 'dev-a',
      driver: 'zowietek',
      address: 'http://[fd00::1]',
      desired_mode: 'decoder',
    },
  },
  {
    id: 'dev-b',
    name: 'Lobby node',
    body: { id: 'dev-b', driver: 'displaynode' },
  },
];

const SYNC_GROUPS = [
  {
    id: 'wall',
    name: 'Lobby wall',
    body: {
      id: 'wall',
      target_skew_ms: 80,
      members: [{ device: 'dev-b', offset_ms: 0 }],
    },
  },
];

const DISCOVERED = [
  {
    key: 'zowietek-control|box|_zowie._tcp',
    name: 'box',
    host: 'box.local.',
    driver_kind: 'zowietek-control',
    service_type: '_zowie._tcp',
    port: 80,
    primary_address: '[fd00::42]:80',
    endpoints: [{ address: '[fd00::42]:80', family: 'ipv6' }],
    last_seen_unix_ns: 7,
    txt: [],
  },
];

const server = setupServer(
  http.get('*/api/v1/devices', () => HttpResponse.json(DEVICES)),
  http.get('*/api/v1/sync-groups', () => HttpResponse.json(SYNC_GROUPS)),
  http.get('*/api/v1/devices/dev-a/status', () =>
    HttpResponse.json({
      device_id: 'dev-a',
      state: 'UNREACHABLE',
      mode: 'decoder',
      last_seen_ts: 90,
      temperature_c: 47.5,
      streams: [
        { role: 'encode', healthy: true },
        { role: 'decode', healthy: false },
      ],
    }),
  ),
  http.get('*/api/v1/devices/dev-b/status', () =>
    HttpResponse.json({ device_id: 'dev-b', state: 'ONLINE' }),
  ),
  http.get('*/api/v1/discovery/devices', () => HttpResponse.json(DISCOVERED)),
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

function renderDevices(): void {
  renderWithProviders(
    <MemoryRouter>
      <DevicesPage />
    </MemoryRouter>,
  );
}

describe('DevicesPage list', () => {
  it('lists devices with driver, state badge text, and mode', async () => {
    renderDevices();
    expect(await screen.findByText('Foyer box')).toBeInTheDocument();
    expect(screen.getByText('Lobby node')).toBeInTheDocument();
    expect(screen.getByText('zowietek')).toBeInTheDocument();
    expect(screen.getByText('displaynode')).toBeInTheDocument();
    // State arrives via the REST fallback; the badge is text, never colour
    // alone.
    expect(await screen.findByText('Unreachable')).toBeInTheDocument();
    expect(await screen.findByText('Online')).toBeInTheDocument();
    expect(screen.getByText('decoder')).toBeInTheDocument();
  });

  it('shows temperature and the sync-group chip', async () => {
    renderDevices();
    expect(await screen.findByText(/47\.5/)).toBeInTheDocument();
    // dev-b is a member of the "Lobby wall" group: the chip names the group.
    expect(await screen.findByText('Lobby wall')).toBeInTheDocument();
  });

  it('links the first active stream to the device detail Streams tab', async () => {
    renderDevices();
    // dev-a reports streams: the cell shows the FIRST stream's role + health
    // and deep-links to the detail page's Streams tab (managed-devices.md §9).
    const link = await screen.findByRole('link', { name: /encode healthy/i });
    expect(link).toHaveAttribute('href', '/devices/dev-a?tab=streams');
  });

  it('shows an accessible placeholder when no stream is reported', async () => {
    renderDevices();
    // Wait for dev-a's status (the row WITH streams) so the remaining
    // placeholder is genuinely dev-b's no-streams cell.
    await screen.findByRole('link', { name: /encode healthy/i });
    expect(screen.getAllByText('No active stream.')).toHaveLength(1);
  });

  it('pairs every placeholder em-dash with sr-only text (the State-cell pattern)', async () => {
    renderDevices();
    expect(await screen.findByText('Unreachable')).toBeInTheDocument();
    expect(await screen.findByText('Online')).toBeInTheDocument();
    expect(await screen.findByText('Lobby wall')).toBeInTheDocument();
    // dev-b: no mode (and no desired mode), no temperature reported.
    expect(screen.getByText('No mode reported.')).toBeInTheDocument();
    expect(screen.getByText('No temperature reported.')).toBeInTheDocument();
    // dev-a: not a member of any sync group.
    expect(screen.getByText('Not in a sync group.')).toBeInTheDocument();
  });
});

describe('DevicesPage discovery (untrusted inventory)', () => {
  it('states the confirm-adopt doctrine: discovery NEVER adopts by itself', async () => {
    renderDevices();
    const panel = await screen.findByTestId('discovery-panel');
    expect(panel).toHaveTextContent(/untrusted/i);
    expect(panel).toHaveTextContent(/never adopt/i);
    expect(panel).toHaveTextContent(/explicit/i);
  });

  it('kicks a scan with POST /discovery/devices/scan (202 + operation id)', async () => {
    let scanned = 0;
    server.use(
      http.post('*/api/v1/discovery/devices/scan', () => {
        scanned += 1;
        return HttpResponse.json(
          {
            operation_id: 'op-scan',
            budget_ms: 4000,
            note: 'untrusted inventory; confirm-adopt only',
            service_types: ['_googlecast._tcp'],
          },
          { status: 202 },
        );
      }),
    );
    renderDevices();
    await userEvent.click(
      await screen.findByRole('button', { name: /scan for devices/i }),
    );
    expect(scanned).toBe(1);
  });

  it('lists inventory rows IPv6-first and prefills the adopt dialog from a row', async () => {
    renderDevices();
    const panel = await screen.findByTestId('discovery-panel');
    expect(await within(panel).findByText('box')).toBeInTheDocument();
    expect(within(panel).getByText(/IPv6/)).toBeInTheDocument();
    await userEvent.click(within(panel).getByRole('button', { name: /adopt: box/i }));
    const dialog = await screen.findByRole('dialog');
    // Prefilled from the discovery row: management address + inferred driver.
    expect(
      within(dialog).getByDisplayValue('http://[fd00::42]:80'),
    ).toBeInTheDocument();
    // The operator still has to confirm: nothing was created by opening this.
    expect(within(dialog).getByRole('button', { name: 'Create' })).toBeInTheDocument();
  });
});

describe('DevicesPage adopt flow', () => {
  it('adopting posts the exact config Device body', async () => {
    let posted: unknown;
    server.use(
      http.post('*/api/v1/devices/:id', async ({ request, params }) => {
        posted = { id: params.id, payload: await request.json() };
        return HttpResponse.json(
          { id: String(params.id), name: 'New box', body: { id: String(params.id), driver: 'zowietek' } },
          { status: 201, headers: { ETag: '"1"' } },
        );
      }),
    );
    renderDevices();
    expect(await screen.findByText('Foyer box')).toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: 'Adopt device' }));
    const dialog = await screen.findByRole('dialog');
    await userEvent.type(within(dialog).getByLabelText('Identifier'), 'dev-new');
    await userEvent.type(within(dialog).getByLabelText('Name'), 'New box');
    // userEvent keyboard syntax: `[[` escapes a literal `[`; `]` is literal.
    await userEvent.type(
      within(dialog).getByLabelText(/management address/i),
      'http://[[fd00::9]',
    );
    await userEvent.click(within(dialog).getByRole('button', { name: 'Create' }));
    expect(posted).toEqual({
      id: 'dev-new',
      payload: {
        name: 'New box',
        body: {
          id: 'dev-new',
          display_name: 'New box',
          driver: 'zowietek',
          address: 'http://[fd00::9]',
        },
      },
    });
  });
});
