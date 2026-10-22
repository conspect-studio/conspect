// DeviceDetailPage — Overview / Streams / Sync / Maintenance / Events tabs
// (managed-devices.md §9), the failure UX (§11: UNREACHABLE guidance,
// AUTH_FAILED credentials prompt, program-continuity messaging), maintenance
// verbs (202 + declared DEV-class impact BEFORE set-mode), and stream binding
// (a bind creates an ORDINARY Source/Output carrying `device_ref`, ADR-M009).
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { MemoryRouter, Route, Routes } from 'react-router-dom';

import { DeviceDetailPage } from './DeviceDetailPage';
import { renderWithProviders } from '../test/render';

const DEVICE = {
  id: 'dev-a',
  name: 'Foyer box',
  body: {
    id: 'dev-a',
    driver: 'zowietek',
    address: 'http://[fd00::1]',
    desired_mode: 'decoder',
  },
};

let statusBody: Record<string, unknown> = {
  device_id: 'dev-a',
  state: 'ONLINE',
  mode: 'decoder',
  last_seen_ts: 90,
  temperature_c: 41.5,
};

const server = setupServer(
  http.get('*/api/v1/devices', () => HttpResponse.json([DEVICE])),
  http.get('*/api/v1/devices/dev-a', () =>
    HttpResponse.json(DEVICE, { headers: { ETag: '"1"' } }),
  ),
  http.get('*/api/v1/devices/dev-a/status', () => HttpResponse.json(statusBody)),
  http.get('*/api/v1/sync-groups', () => HttpResponse.json([])),
  http.get('*/api/v1/devices/dev-a/source-candidates', () =>
    HttpResponse.json([
      { id: 'main', kind: 'rtsp', url: 'rtsp://[fd00::1]:554/main', unverified: false },
      { id: 'aux', kind: 'rtsp', url: null, unverified: true },
    ]),
  ),
  http.get('*/api/v1/devices/dev-a/output-targets', () =>
    HttpResponse.json([{ id: 'slot-0', kind: 'rtsp', label: 'Decode slot 0' }]),
  ),
);

beforeAll(() => {
  server.listen();
});
afterEach(() => {
  server.resetHandlers();
  statusBody = {
    device_id: 'dev-a',
    state: 'ONLINE',
    mode: 'decoder',
    last_seen_ts: 90,
    temperature_c: 41.5,
  };
});
afterAll(() => {
  server.close();
});

function renderDetail(): void {
  renderWithProviders(
    <MemoryRouter initialEntries={['/devices/dev-a']}>
      <Routes>
        <Route path="/devices/:id" element={<DeviceDetailPage />} />
      </Routes>
    </MemoryRouter>,
  );
}

describe('DeviceDetailPage tabs', () => {
  it('renders the five §9 tabs', async () => {
    renderDetail();
    expect(await screen.findByText('Foyer box')).toBeInTheDocument();
    for (const tab of ['Overview', 'Streams', 'Sync', 'Maintenance', 'Events']) {
      expect(screen.getByRole('tab', { name: tab })).toBeInTheDocument();
    }
  });

  it('always states program-output continuity (invariants #1/#10)', async () => {
    renderDetail();
    expect(await screen.findByText('Foyer box')).toBeInTheDocument();
    expect(
      screen.getByText(/program output never depends on this device/i),
    ).toBeInTheDocument();
  });

  it('deep-links to the Streams tab via ?tab=streams (the list stream column target)', async () => {
    renderWithProviders(
      <MemoryRouter initialEntries={['/devices/dev-a?tab=streams']}>
        <Routes>
          <Route path="/devices/:id" element={<DeviceDetailPage />} />
        </Routes>
      </MemoryRouter>,
    );
    const tab = await screen.findByRole('tab', { name: 'Streams' });
    expect(tab).toHaveAttribute('aria-selected', 'true');
    expect(screen.getByRole('tab', { name: 'Overview' })).toHaveAttribute(
      'aria-selected',
      'false',
    );
  });

  it('falls back to Overview for an unknown ?tab= value', async () => {
    renderWithProviders(
      <MemoryRouter initialEntries={['/devices/dev-a?tab=nonsense']}>
        <Routes>
          <Route path="/devices/:id" element={<DeviceDetailPage />} />
        </Routes>
      </MemoryRouter>,
    );
    const tab = await screen.findByRole('tab', { name: 'Overview' });
    expect(tab).toHaveAttribute('aria-selected', 'true');
  });
});

describe('DeviceDetailPage failure UX (§11)', () => {
  it('UNREACHABLE: shows last-seen + supervised-reconnect guidance', async () => {
    statusBody = { device_id: 'dev-a', state: 'UNREACHABLE', last_seen_ts: 90 };
    renderDetail();
    expect(await screen.findByText('Unreachable')).toBeInTheDocument();
    const guidance = await screen.findByTestId('failure-guidance');
    expect(guidance).toHaveTextContent(/reconnect/i);
    expect(guidance).toHaveTextContent(/last seen/i);
    // The operator can force a probe immediately.
    expect(screen.getByRole('button', { name: /probe now/i })).toBeInTheDocument();
  });

  it('AUTH_FAILED: distinct from unreachable, prompts a secret update, no blind retries', async () => {
    statusBody = { device_id: 'dev-a', state: 'AUTH_FAILED' };
    renderDetail();
    expect(await screen.findByText('Auth failed')).toBeInTheDocument();
    const guidance = await screen.findByTestId('failure-guidance');
    expect(guidance).toHaveTextContent(/credential/i);
    expect(guidance).toHaveTextContent(/secret/i);
    expect(guidance).toHaveTextContent(/no .*retries|retries are paused/i);
    expect(
      screen.getByRole('button', { name: /update credentials/i }),
    ).toBeInTheDocument();
  });

  it('DEGRADED: stream rows say device-reported stall leaves program output unaffected', async () => {
    statusBody = {
      device_id: 'dev-a',
      state: 'DEGRADED',
      streams: [{ role: 'decode', healthy: false }],
    };
    renderDetail();
    expect(await screen.findByText('Degraded')).toBeInTheDocument();
    await userEvent.click(screen.getByRole('tab', { name: 'Streams' }));
    expect(
      await screen.findByText(/decoding stalled \(device-reported\)/i),
    ).toBeInTheDocument();
    expect(screen.getByText(/program output is unaffected/i)).toBeInTheDocument();
  });
});

describe('DeviceDetailPage maintenance verbs', () => {
  it('reboot asks for confirmation, then POSTs and reports the 202', async () => {
    let rebooted = 0;
    server.use(
      http.post('*/api/v1/devices/dev-a/reboot', () => {
        rebooted += 1;
        return HttpResponse.json(
          { operation_id: 'op-1', kind: 'device-reboot' },
          { status: 202 },
        );
      }),
    );
    renderDetail();
    await userEvent.click(await screen.findByRole('tab', { name: 'Maintenance' }));
    await userEvent.click(screen.getByRole('button', { name: /reboot/i }));
    // Nothing fired yet: reboot is destructive and needs explicit confirmation.
    expect(rebooted).toBe(0);
    const dialog = await screen.findByRole('dialog');
    await userEvent.click(within(dialog).getByRole('button', { name: /^reboot$/i }));
    expect(rebooted).toBe(1);
  });

  it('identify and test-pattern fire-and-forget (204)', async () => {
    const calls: string[] = [];
    server.use(
      http.post('*/api/v1/devices/dev-a/identify', () => {
        calls.push('identify');
        return new HttpResponse(null, { status: 204 });
      }),
      http.post('*/api/v1/devices/dev-a/test-pattern', () => {
        calls.push('test-pattern');
        return new HttpResponse(null, { status: 204 });
      }),
    );
    renderDetail();
    await userEvent.click(await screen.findByRole('tab', { name: 'Maintenance' }));
    await userEvent.click(screen.getByRole('button', { name: /identify/i }));
    await userEvent.click(screen.getByRole('button', { name: /test pattern/i }));
    expect(calls).toEqual(['identify', 'test-pattern']);
  });

  it('set-mode declares the DEV-class impact BEFORE applying, then POSTs the mode', async () => {
    let posted: unknown;
    server.use(
      http.post('*/api/v1/devices/dev-a/set-mode', async ({ request }) => {
        posted = await request.json();
        return HttpResponse.json(
          { operation_id: 'op-3', impact: 'dev', detail: 'device restarts' },
          { status: 202 },
        );
      }),
    );
    renderDetail();
    await userEvent.click(await screen.findByRole('tab', { name: 'Maintenance' }));
    await userEvent.click(screen.getByRole('button', { name: /change mode/i }));
    const dialog = await screen.findByRole('dialog');
    // The impact statement is visible BEFORE the operator applies anything.
    expect(posted).toBeUndefined();
    expect(within(dialog).getByText(/restarts the device/i)).toBeInTheDocument();
    expect(
      within(dialog).getByText(/No Multiview program output is interrupted/i),
    ).toBeInTheDocument();
    await userEvent.click(
      within(dialog).getByRole('button', { name: /apply mode: encoder/i }),
    );
    expect(posted).toEqual({ mode: 'encoder' });
  });
});

describe('DeviceDetailPage streams binding (ADR-M009)', () => {
  it('binding a candidate creates an ordinary Source carrying device_ref', async () => {
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
    renderDetail();
    await userEvent.click(await screen.findByRole('tab', { name: 'Streams' }));
    await userEvent.click(
      await screen.findByRole('button', { name: /bind as source: main/i }),
    );
    const dialog = await screen.findByRole('dialog');
    // Prefilled from the candidate (verified URL).
    expect(
      within(dialog).getByDisplayValue('rtsp://[fd00::1]:554/main'),
    ).toBeInTheDocument();
    await userEvent.type(within(dialog).getByLabelText('Identifier'), 'src-main');
    await userEvent.type(within(dialog).getByLabelText('Name'), 'Foyer main');
    await userEvent.click(within(dialog).getByRole('button', { name: 'Create' }));
    expect(posted).toEqual({
      id: 'src-main',
      payload: {
        name: 'Foyer main',
        body: {
          id: 'src-main',
          display_name: 'Foyer main',
          kind: 'rtsp',
          url: 'rtsp://[fd00::1]:554/main',
          device_ref: 'dev-a',
        },
      },
    });
  });

  it('an unverified candidate is labelled and requires the operator-supplied URL', async () => {
    renderDetail();
    await userEvent.click(await screen.findByRole('tab', { name: 'Streams' }));
    expect(await screen.findByText(/unverified/i)).toBeInTheDocument();
    await userEvent.click(
      screen.getByRole('button', { name: /bind as source: aux/i }),
    );
    const dialog = await screen.findByRole('dialog');
    const url = within(dialog).getByLabelText(/source url/i);
    // Never silently guessed: the URL starts empty.
    expect(url).toHaveValue('');
  });

  it('binding a decode target creates an ordinary Output carrying device_ref', async () => {
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
    renderDetail();
    await userEvent.click(await screen.findByRole('tab', { name: 'Streams' }));
    await userEvent.click(
      await screen.findByRole('button', { name: /bind as output: decode slot 0/i }),
    );
    const dialog = await screen.findByRole('dialog');
    await userEvent.type(within(dialog).getByLabelText('Identifier'), 'out-foyer');
    await userEvent.type(within(dialog).getByLabelText('Name'), 'Foyer feed');
    await userEvent.type(within(dialog).getByLabelText(/mount/i), '/foyer');
    await userEvent.click(within(dialog).getByRole('button', { name: 'Create' }));
    const captured = posted as { id: string; payload: { body: Record<string, unknown> } };
    expect(captured.id).toBe('out-foyer');
    expect(captured.payload.body.device_ref).toBe('dev-a');
    expect(captured.payload.body.kind).toBe('rtsp_server');
    expect(captured.payload.body.mount).toBe('/foyer');
  });
});

describe('DeviceDetailPage events tab', () => {
  it('is honest when no device events have streamed this session', async () => {
    renderDetail();
    await userEvent.click(await screen.findByRole('tab', { name: 'Events' }));
    expect(
      await screen.findByText(/no device events .*this session/i),
    ).toBeInTheDocument();
  });
});
