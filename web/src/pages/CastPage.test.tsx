// CastPage: a focused render test over MSW. Verifies the page lists discovered
// Cast receivers + live sessions, starts a session from a discovered receiver
// (pre-filling its address), and sets the volume (a 202 command).
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';

import { CastPage } from './CastPage';
import { renderWithProviders } from '../test/render';

let lastStartBody: unknown = null;
let lastVolumeBody: unknown = null;

const session = {
  address: '[2001:db8::5]:8009',
  id: 'cast-session-1',
  media_url: 'http://[2001:db8::1]:8080/hls/program.m3u8',
  name: 'Lobby TV',
  output: 'hls-main',
  state: 'ONLINE',
};

const server = setupServer(
  http.get('http://localhost:3000/api/v1/cast/sessions', () =>
    HttpResponse.json([session]),
  ),
  http.post('http://localhost:3000/api/v1/cast/sessions', async ({ request }) => {
    lastStartBody = await request.json();
    return HttpResponse.json(session, { status: 201 });
  }),
  http.post('http://localhost:3000/api/v1/cast/sessions/:id/volume', async ({ request }) => {
    lastVolumeBody = await request.json();
    return HttpResponse.json(
      { kind: 'cast-volume', operation_id: 'op-vol-1' },
      { status: 202 },
    );
  }),
  http.get('http://localhost:3000/api/v1/discovery/devices', () =>
    HttpResponse.json([
      {
        key: 'svc-1',
        name: 'Living Room',
        host: 'living-room.local.',
        driver_kind: 'cast',
        service_type: '_googlecast._tcp',
        port: 8009,
        primary_address: '[2001:db8::9]:8009',
        endpoints: [{ address: '[2001:db8::9]:8009', family: 'ipv6' }],
        last_seen_unix_ns: 1,
        txt: [],
      },
      {
        key: 'svc-2',
        name: 'A Camera',
        host: 'cam.local.',
        driver_kind: 'ndi-source',
        service_type: '_ndi._tcp',
        port: 5961,
        primary_address: '[2001:db8::2]:5961',
        endpoints: [],
        last_seen_unix_ns: 1,
        txt: [],
      },
    ]),
  ),
);

beforeAll(() => {
  server.listen({ onUnhandledRequest: 'error' });
});
afterEach(() => {
  server.resetHandlers();
  lastStartBody = null;
  lastVolumeBody = null;
});
afterAll(() => {
  server.close();
});

describe('CastPage', () => {
  it('lists discovered Cast receivers (only) and live sessions', async () => {
    renderWithProviders(<CastPage />);

    // The Cast receiver shows; the NDI source is filtered out.
    expect(await screen.findByText('Living Room')).toBeInTheDocument();
    expect(screen.queryByText('A Camera')).not.toBeInTheDocument();
    // The live session renders with its state.
    expect(await screen.findByText('Lobby TV')).toBeInTheDocument();
    expect(screen.getByText('ONLINE')).toBeInTheDocument();
  });

  it('starts a session from a discovered receiver, pre-filling its address', async () => {
    const user = userEvent.setup();
    renderWithProviders(<CastPage />);

    await user.click(await screen.findByRole('button', { name: /Cast to: Living Room/i }));
    const dialog = await screen.findByRole('dialog');
    // The address field is pre-filled from the discovered receiver.
    expect(within(dialog).getByLabelText('Receiver address')).toHaveValue('[2001:db8::9]:8009');
    await user.click(within(dialog).getByRole('button', { name: 'Start' }));

    await waitFor(() => {
      expect(lastStartBody).toEqual({
        address: '[2001:db8::9]:8009',
        name: 'Living Room',
      });
    });
  });

  it('sets the volume on a live session (202 command)', async () => {
    const user = userEvent.setup();
    renderWithProviders(<CastPage />);

    await user.click(await screen.findByRole('button', { name: /Set volume: cast-session-1/i }));
    const dialog = await screen.findByRole('dialog');
    const input = within(dialog).getByLabelText('Volume (0–100)');
    await user.clear(input);
    await user.type(input, '35');
    await user.click(within(dialog).getByRole('button', { name: 'Set volume' }));

    await waitFor(() => {
      expect(lastVolumeBody).toEqual({ level_percent: 35 });
    });
  });
});
