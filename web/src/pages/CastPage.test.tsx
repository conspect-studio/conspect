// CastPage over MSW: the browser-cast section (feed picker, empty state,
// unsupported browser, default-receiver notice, "show something else" take)
// and the engine-side section (discovery scan, TXT names, sessions, volume).
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it } from 'vitest';
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import type { JsonBodyType } from 'msw';
import { setupServer } from 'msw/node';

import { CastPage } from './CastPage';
import { renderWithProviders } from '../test/render';
import { resetCastSdkLoaderForTests } from '../cast/sdkLoader';
import { installMemoryStorage } from '../cast/testApi';
import { installMockCastSdk } from '../cast/testMocks';

const BASE = 'http://localhost:3000';
let lastStartBody: unknown = null;
let lastVolumeBody: unknown = null;
let scans = 0;
const takes: { body: unknown; key: string | null }[] = [];

const session = {
  address: '[2001:db8::5]:8009',
  id: 'cast-session-1',
  media_url: 'https://engine.example/cast/<redacted>/master.m3u8',
  name: 'Lobby TV',
  output: 'cast-1',
  state: 'ONLINE',
  player_state: 'PLAYING',
  last_report: { received_unix_ms: 1, report: { state: 'playing', stats: { width: 1920, height: 1080, latency_s: 7.5 } } },
};

function config(
  appId: string | null,
  outputs: { id: string; active: boolean; ready: boolean; streams: number }[] = [{ id: 'cast-1', active: false, ready: true, streams: 0 }],
): JsonBodyType {
  return { receiver_app_id: appId, receiver_path: '/cast/receiver/', namespace: 'urn:x-cast:studio.conspect.cast', outputs };
}

const server = setupServer(
  http.get(`${BASE}/api/v1/cast/config`, () => HttpResponse.json(config('8E05190C'))),
  http.get(`${BASE}/api/v1/cast/streams`, () => HttpResponse.json([])),
  http.get(`${BASE}/api/v1/switcher/me1`, () => HttpResponse.json({ error: 'none' }, { status: 404 })),
  http.get(`${BASE}/api/v1/layouts`, () => HttpResponse.json([])),
  http.get(`${BASE}/api/v1/sources`, () =>
    HttpResponse.json([
      { id: 'cam1', name: 'Studio camera', body: { kind: 'hls', url: 'https://example.test/a.m3u8' } },
      { id: 'cam2', name: 'Roof camera', body: { kind: 'hls', url: 'https://example.test/b.m3u8' } },
    ]),
  ),
  http.post(`${BASE}/api/v1/routing/output/take`, async ({ request }) => {
    takes.push({ body: await request.json(), key: request.headers.get('Idempotency-Key') });
    return HttpResponse.json({ operation_id: 'op-take-1', status: 'accepted' }, { status: 202 });
  }),
  http.get(`${BASE}/api/v1/cast/sessions`, () => HttpResponse.json([session])),
  http.post(`${BASE}/api/v1/cast/sessions`, async ({ request }) => {
    lastStartBody = await request.json();
    return HttpResponse.json(session, { status: 201 });
  }),
  http.post(`${BASE}/api/v1/cast/sessions/:id/volume`, async ({ request }) => {
    lastVolumeBody = await request.json();
    return HttpResponse.json({ kind: 'cast-volume', operation_id: 'op-vol-1' }, { status: 202 });
  }),
  http.post(`${BASE}/api/v1/discovery/devices/scan`, () => {
    scans += 1;
    return HttpResponse.json({ operation_id: 'op-scan', budget_ms: 500, note: '', service_types: [] }, { status: 202 });
  }),
  http.get(`${BASE}/api/v1/discovery/devices`, () =>
    HttpResponse.json([
      {
        key: 'svc-1',
        name: 'Chromecast-abc123',
        host: 'living-room.local.',
        driver_kind: 'cast',
        service_type: '_googlecast._tcp',
        port: 8009,
        primary_address: '[2001:db8::9]:8009',
        endpoints: [{ address: '[2001:db8::9]:8009', family: 'ipv6' }],
        last_seen_unix_ns: 1,
        txt: [
          { key: 'fn', value: 'Living Room' },
          { key: 'md', value: 'Chromecast Ultra' },
        ],
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
beforeEach(() => {
  installMemoryStorage();
  resetCastSdkLoaderForTests();
  installMockCastSdk();
});
afterEach(() => {
  server.resetHandlers();
  lastStartBody = null;
  lastVolumeBody = null;
  scans = 0;
  takes.length = 0;
  delete window.cast;
  delete window.chrome;
  delete window.__onGCastApiAvailable;
});
afterAll(() => {
  server.close();
});

describe('CastPage — from this browser', () => {
  it('offers Program and every readable source (no Preview/Clean without a switcher)', async () => {
    renderWithProviders(<CastPage />);
    const select = await screen.findByLabelText('What to cast');
    await within(select).findByRole('option', { name: 'Studio camera' });
    const names = within(select).getAllByRole('option').map((o) => o.textContent);
    expect(names).toEqual(['Program', 'Studio camera', 'Roof camera']);
    expect(await screen.findByRole('button', { name: 'Cast: Program' })).toBeInTheDocument();
    expect(screen.queryByText(/Using Google's default receiver/)).not.toBeInTheDocument();
  });

  it('shows the default-receiver notice when no receiver app is registered', async () => {
    server.use(http.get(`${BASE}/api/v1/cast/config`, () => HttpResponse.json(config(null))));
    renderWithProviders(<CastPage />);
    expect(await screen.findByText(/Using Google's default receiver/)).toBeInTheDocument();
  });

  it('explains how to add a cast output when none is configured', async () => {
    server.use(http.get(`${BASE}/api/v1/cast/config`, () => HttpResponse.json(config('8E05190C', []))));
    renderWithProviders(<CastPage />);
    expect(await screen.findByText(/No cast output is configured/)).toBeInTheDocument();
    expect(screen.getByLabelText('Configuration snippet')).toHaveTextContent('kind = "cast"');
    expect(screen.queryByRole('button', { name: /^Cast: / })).not.toBeInTheDocument();
  });

  it('says Cast needs Chrome or Edge on an unsupported browser', async () => {
    delete window.chrome;
    renderWithProviders(<CastPage />);
    expect(await screen.findByText('Cast needs Chrome or Edge on a computer or Android.')).toBeInTheDocument();
  });

  it('"Show something else" takes the cast output once, with an Idempotency-Key', async () => {
    const user = userEvent.setup();
    server.use(
      http.post(`${BASE}/api/v1/cast/streams`, () =>
        HttpResponse.json(
          {
            id: 'cs-1', output: 'cast-1', feed: { bus: 'program' }, path: '/cast/tok/master.m3u8',
            content_type: 'application/vnd.apple.mpegurl', hls_segment_format: 'fmp4', hls_video_segment_format: 'fmp4',
            ready: true, segments: 9, expires_unix_ms: 0, fetches: { playlists: 0, segments: 0, cast_device: false },
          },
          { status: 201 },
        ),
      ),
    );
    renderWithProviders(<CastPage />);
    const cast = await screen.findByRole('button', { name: 'Cast: Program' });
    await waitFor(() => {
      expect(cast).toBeEnabled();
    });
    await user.click(cast);
    const other = await screen.findByLabelText('Show something else');
    await user.selectOptions(other, 'src:cam2');
    await waitFor(() => {
      expect(takes).toHaveLength(1);
    });
    expect(takes[0]?.body).toMatchObject({ target: { kind: 'output', output: 'cast-1', stream: 'video' }, feed: { source: 'cam2' } });
    expect(takes[0]?.key).toMatch(/^[0-9a-f-]{36}$/);
  });
});

describe('CastPage — from the engine', () => {
  it('scans on open and lists Cast receivers by their TXT friendly name and model', async () => {
    renderWithProviders(<CastPage />);
    expect(await screen.findByText('Living Room')).toBeInTheDocument();
    expect(screen.getByText('Chromecast Ultra')).toBeInTheDocument();
    expect(screen.queryByText('A Camera')).not.toBeInTheDocument();
    await waitFor(() => {
      expect(scans).toBe(1);
    });
  });

  it('shows the live session with its player state and receiver report', async () => {
    renderWithProviders(<CastPage />);
    expect(await screen.findByText('Lobby TV')).toBeInTheDocument();
    expect(screen.getByText('ONLINE')).toBeInTheDocument();
    expect(screen.getByText('PLAYING · 1920×1080 · 7.5 s')).toBeInTheDocument();
  });

  it('starts a session from a discovered receiver with a chosen feed', async () => {
    const user = userEvent.setup();
    renderWithProviders(<CastPage />);
    await user.click(await screen.findByRole('button', { name: /Cast to: Living Room/i }));
    const dialog = await screen.findByRole('dialog');
    expect(within(dialog).getByLabelText('Receiver address')).toHaveValue('[2001:db8::9]:8009');
    const feed = within(dialog).getByLabelText('What to show');
    await within(feed).findByRole('option', { name: 'Studio camera' });
    await user.selectOptions(feed, 'src:cam1');
    await user.click(within(dialog).getByRole('button', { name: 'Start' }));
    await waitFor(() => {
      expect(lastStartBody).toEqual({ address: '[2001:db8::9]:8009', name: 'Living Room', feed: { source: 'cam1' } });
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
