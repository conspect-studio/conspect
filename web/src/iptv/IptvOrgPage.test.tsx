// The IPTV-ORG browser: renders from mocked /index + /places, text search, facet
// counts, Wichita place ranking, flags as text, probe flow, Add as source seed,
// bulk add with partial failure. The map is stubbed (jsdom has no WebGL) —
// everything is exercised through the search + list path.
import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { MemoryRouter, Route, Routes } from 'react-router-dom';

import index from '../test/fixtures/iptv-org/index.json';
import places from '../test/fixtures/iptv-org/places.json';
import { renderWithProviders } from '../test/render';
import { SourcesPage } from '../pages/SourcesPage';
import { IptvOrgPage } from './IptvOrgPage';

vi.mock('./MapView', () => ({
  MapView: ({ onPick }: { onPick: (id: string) => void }) => (
    <button type="button" data-testid="map-stub" onClick={() => { onPick('s/US-KS'); }}>
      map stub
    </button>
  ),
}));

let created: { id: string; payload: unknown }[] = [];
let probeBodies: unknown[] = [];

const server = setupServer(
  http.get('*/api/v1/catalogues/iptv-org/status', () =>
    HttpResponse.json({
      state: 'idle', etag: null, synced_at: null, counts: { streams: 22, channels: 20 }, errors: [],
      server: { name: 'Dallas', lat: 32.78, lon: -96.8 },
    }),
  ),
  http.get('*/api/v1/catalogues/iptv-org/index', () => HttpResponse.json(index)),
  http.get('*/api/v1/catalogues/iptv-org/places', () => HttpResponse.json(places)),
  http.get('*/api/v1/sources', () =>
    HttpResponse.json([
      { id: 'ksn', name: 'KSN', body: { id: 'ksn', kind: 'hls', url: 'https://example.test/ksn/index.m3u8' } },
    ]),
  ),
  http.get('*/api/v1/catalogues/iptv-org/links', () => HttpResponse.json({ links: [] })),
  http.post('*/api/v1/catalogues/iptv-org/probes', async ({ request }) => {
    probeBodies.push(await request.json());
    return HttpResponse.json({
      id: 'p1', state: 'done', error: null,
      results: [
        {
          k: 's:kake000001', source_id: null, verdict: 'live', probed_from: 'dallas-1',
          probed_at: '2026-11-07T05:00:00Z', detail: null,
          facts: { width: 1280, height: 720, fps: 29.97, video_codec: 'h264', audio_codec: 'aac', bitrate_kbps: 2800, advertised_kbps: 3000, audio_languages: ['eng'], captions: [], thumbnail: null },
          events: [{ kind: 'cue_out', at: '12s', detail: 'duration 30' }],
        },
      ],
    });
  }),
  http.post('*/api/v1/sources/:id', async ({ request, params }) => {
    const id = String(params.id);
    if (id.startsWith('bbc')) {
      return HttpResponse.json({ title: 'Conflict', status: 409 }, { status: 409 });
    }
    created.push({ id, payload: await request.json() });
    return HttpResponse.json({ id, name: id, body: {} }, { status: 201, headers: { ETag: '"1"' } });
  }),
);

beforeAll(() => { server.listen(); });
afterEach(() => { server.resetHandlers(); created = []; probeBodies = []; });
afterAll(() => { server.close(); });

function renderPage(): void {
  renderWithProviders(
    <MemoryRouter initialEntries={['/sources/iptv-org']}>
      <Routes>
        <Route path="/sources/iptv-org" element={<IptvOrgPage />} />
        <Route path="/sources" element={<SourcesPage />} />
      </Routes>
    </MemoryRouter>,
  );
}

async function ready(): Promise<void> {
  await screen.findByRole('searchbox', { name: /search channels/i });
  await screen.findByTestId('iptv-results');
}

describe('IptvOrgPage', () => {
  it('renders from the index, hides blocklisted, shows footer attributions', async () => {
    renderPage();
    await ready();
    expect(screen.getByText(/Natural Earth/)).toBeInTheDocument();
    expect(screen.getByText(/Unlicense/)).toBeInTheDocument();
    expect(screen.getByText(/CC0/)).toBeInTheDocument();
    expect(screen.queryByText('Blocked Channel')).not.toBeInTheDocument();
  });

  it('text search filters channels (typo tolerant)', async () => {
    renderPage();
    await ready();
    await userEvent.type(screen.getByRole('searchbox', { name: /search channels/i }), 'accuwether');
    await waitFor(() => { expect(screen.getByTestId('iptv-results')).toHaveTextContent('AccuWeather'); });
    expect(screen.getByTestId('iptv-results')).not.toHaveTextContent('KAKE');
  });

  it('facet filtering shows counts and narrows the list', async () => {
    renderPage();
    await ready();
    const geo = await screen.findByRole('checkbox', { name: 'Geo-blocked' });
    expect(geo.closest('li')).toHaveTextContent(/\d+/);
    await userEvent.click(geo);
    await waitFor(() => { expect(screen.getByTestId('iptv-results')).toHaveTextContent('KSN'); });
    expect(screen.getByTestId('iptv-results')).not.toHaveTextContent('BBC News');
    // The flag renders as text, not colour alone.
    expect(within(screen.getByTestId('iptv-results')).getAllByText('Geo-blocked').length).toBeGreaterThan(0);
  });

  it('place search ranks Wichita first and lists local streams in the city tier', async () => {
    renderPage();
    await ready();
    await userEvent.type(screen.getByRole('searchbox', { name: /where is it happening/i }), 'wichta');
    const hits = await screen.findByRole('list', { name: /matching places/i });
    const buttons = within(hits).getAllByRole('button');
    expect(buttons[0]).toHaveTextContent('Wichita, Kansas, United States');
    const top = buttons[0];
    if (top === undefined) {
      throw new Error('no place hit');
    }
    await userEvent.click(top);
    const results = await screen.findByTestId('iptv-results');
    await waitFor(() => { expect(results).toHaveTextContent('Local (this city)'); });
    expect(results).toHaveTextContent('KAKE');
    expect(results).not.toHaveTextContent('BBC News');
    expect(screen.getByText(/Near Wichita/)).toBeInTheDocument();
    // State-wide + national tiers follow (national content is in a tier row too).
    expect(results).toHaveTextContent('State / province');
  });

  it('map click selects a subdivision through the same path; near-server is offered', async () => {
    renderPage();
    await ready();
    expect(await screen.findByRole('button', { name: /near this server/i })).toBeInTheDocument();
    await userEvent.click(screen.getByTestId('map-stub'));
    await waitFor(() => { expect(screen.getByTestId('iptv-results')).toHaveTextContent('Kansas Statewide'); });
  });

  it('probe flow shows verdict, facts and events', async () => {
    renderPage();
    await ready();
    await userEvent.type(screen.getByRole('searchbox', { name: /search channels/i }), 'kake');
    await userEvent.click(await screen.findByRole('button', { name: /show streams of kake/i }));
    await userEvent.click(await screen.findByRole('button', { name: /^probe kake 10 news$/i }));
    await userEvent.click(await screen.findByRole('button', { name: /details for kake/i }));
    const detail = await screen.findByTestId('iptv-detail');
    expect(await within(detail).findByText('Live')).toBeInTheDocument();
    expect(within(detail).getByText(/1280×720/)).toBeInTheDocument();
    expect(within(detail).getByText('cue_out')).toBeInTheDocument();
    expect(probeBodies[0]).toEqual({ keys: ['s:kake000001'] });
  });

  it('marks already-added streams', async () => {
    renderPage();
    await ready();
    await userEvent.type(screen.getByRole('searchbox', { name: /search channels/i }), 'ksn');
    await userEvent.click(await screen.findByRole('button', { name: /show streams of ksn/i }));
    expect((await screen.findAllByText('Already added')).length).toBeGreaterThan(0);
    expect(screen.getByRole('button', { name: /add ksn 3 as source/i })).toBeDisabled();
  });

  it('Add as source opens the Sources create dialog prefilled with url/UA/referer/origin', async () => {
    renderPage();
    await ready();
    await userEvent.type(screen.getByRole('searchbox', { name: /search channels/i }), 'kake');
    await userEvent.click(await screen.findByRole('button', { name: /show streams of kake/i }));
    await userEvent.click(await screen.findByRole('button', { name: /add kake 10 news as source/i }));
    const dialog = await screen.findByRole('dialog');
    expect(within(dialog).getByDisplayValue('https://example.test/kake/index.m3u8')).toBeInTheDocument();
    expect(within(dialog).getByText(/linked to iptv-org/i)).toBeInTheDocument();
    await userEvent.click(within(dialog).getByRole('button', { name: /^create$/i }));
    await waitFor(() => { expect(created).toHaveLength(1); });
    expect(created[0]?.payload).toMatchObject({
      body: {
        kind: 'hls',
        url: 'https://example.test/kake/index.m3u8',
        user_agent: 'Mozilla/5.0 Test',
        referrer: 'https://example.test/',
        origin: { provider: 'iptv-org', channel: 'KAKE.us', follow: 'notify' },
      },
    });
  });

  it('bulk add reports per-item results including a partial failure', async () => {
    renderPage();
    await ready();
    await userEvent.type(screen.getByRole('searchbox', { name: /search channels/i }), 'news');
    await userEvent.click(await screen.findByRole('checkbox', { name: /select kake for bulk add/i }));
    await userEvent.click(await screen.findByRole('checkbox', { name: /select bbc news for bulk add/i }));
    await userEvent.click(screen.getByRole('button', { name: /add selected \(2\)/i }));
    const dialog = await screen.findByRole('dialog');
    await userEvent.click(within(dialog).getByRole('button', { name: /add 2 sources/i }));
    await waitFor(() => { expect(within(dialog).getAllByText(/created|failed/i).length).toBeGreaterThanOrEqual(2); });
    expect(created.map((c) => c.id)).toEqual(['kake-us-sd']);
    expect(within(dialog).getByText(/Failed: Conflict/)).toBeInTheDocument();
  });
});
