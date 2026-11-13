// CastPage: covers both casting methods.
//   * "Cast from the engine" (server-side, existing): lists discovered Cast
//     receivers + live sessions, starts a session (pre-filling the address),
//     sets the volume, and — the improved part — shows the server's 409
//     `detail` plus a hint to use the browser method instead.
//   * "Cast from this browser" (new, PR 1 scope): Program + each HLS/LL-HLS
//     output gets a Cast button that loads that output's own playlist URL
//     directly into a (mocked) Cast Web Sender session.
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it } from 'vitest';
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';

import { CastPage } from './CastPage';
import { renderWithProviders } from '../test/render';
import { Toaster } from '../components/ui/toaster';
import { resetCastSdkLoaderForTests } from '../cast/sdkLoader';
import { installMockCastSdk } from '../cast/testMocks';
import type { MockCastSdk } from '../cast/testMocks';

let lastStartBody: unknown = null;
let lastVolumeBody: unknown = null;
let startStatus = 201;
let startProblemType = '';

const session = {
  address: '[2001:db8::5]:8009',
  id: 'cast-session-1',
  media_url: 'http://[2001:db8::1]:8080/hls/program.m3u8',
  name: 'Lobby TV',
  output: 'hls-main',
  state: 'ONLINE',
};

const OUTPUTS = [
  { id: 'program', name: 'Program HLS', body: { kind: 'hls', path: '/srv/index.m3u8', codec: 'h264' } },
];

const server = setupServer(
  http.get('http://localhost:3000/api/v1/cast/sessions', () =>
    HttpResponse.json([session]),
  ),
  http.post('http://localhost:3000/api/v1/cast/sessions', async ({ request }) => {
    lastStartBody = await request.json();
    if (startStatus !== 201) {
      return HttpResponse.json(
        {
          type: `https://example.com/problems/${startProblemType}`,
          title: 'Cannot start cast',
          status: startStatus,
          detail: 'The engine has no encoder capacity for another cast session.',
        },
        { status: startStatus },
      );
    }
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
  http.get('http://localhost:3000/api/v1/outputs', () => HttpResponse.json(OUTPUTS)),
);

beforeAll(() => {
  server.listen({ onUnhandledRequest: 'warn' });
});
beforeEach(() => {
  window.chrome = {};
});
afterEach(() => {
  server.resetHandlers();
  lastStartBody = null;
  lastVolumeBody = null;
  startStatus = 201;
  startProblemType = '';
  delete window.chrome;
});
afterAll(() => {
  server.close();
});

describe('CastPage — Cast from the engine (server-side)', () => {
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

  it('shows the server detail plus a hint to use the browser method on a 409', async () => {
    startStatus = 409;
    startProblemType = 'cast-no-encoder';
    const user = userEvent.setup();
    renderWithProviders(<><CastPage /><Toaster /></>);

    await user.click(await screen.findByRole('button', { name: /Cast to: Living Room/i }));
    const dialog = await screen.findByRole('dialog');
    await user.click(within(dialog).getByRole('button', { name: 'Start' }));

    expect(
      await screen.findByText(
        /The engine has no encoder capacity for another cast session\. Use Cast from this browser instead\./,
      ),
    ).toBeInTheDocument();
  });
});

describe('CastPage — Cast from this browser', () => {
  let sdk: MockCastSdk;

  beforeEach(() => {
    resetCastSdkLoaderForTests();
    sdk = installMockCastSdk({ deviceName: 'Living Room TV' });
  });

  afterEach(() => {
    delete window.cast;
    delete window.__onGCastApiAvailable;
  });

  it('casts Program (the first configured HLS output) to the device', async () => {
    const user = userEvent.setup();
    renderWithProviders(<CastPage />);

    await user.click(await screen.findByRole('button', { name: /^Cast: Program$/ }));

    await waitFor(() => {
      expect(sdk.loadMedia).toHaveBeenCalledTimes(1);
    });
    const request = sdk.loadMedia.mock.calls[0]?.[0] as { media: { contentId: string; contentType: string } };
    expect(request.media.contentId).toBe(`${window.location.origin}/hls/program/index.m3u8`);
    // Program casts the only configured HLS output, so both rows (Program and
    // the output's own row) show "Casting to" — they share the same target.
    expect((await screen.findAllByText(/Casting to Living Room TV/)).length).toBeGreaterThan(0);
  });

  it('shows the trademark note and the engine-method explanation', async () => {
    renderWithProviders(<CastPage />);
    expect(
      await screen.findByText(/Google Cast and Chromecast are trademarks of Google LLC/),
    ).toBeInTheDocument();
    expect(
      screen.getByText(/Cast from the engine \(same network only\)/),
    ).toBeInTheDocument();
  });
});
