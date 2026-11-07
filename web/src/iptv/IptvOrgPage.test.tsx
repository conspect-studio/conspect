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
import type { IndexStream } from '../api/catalogue';
import { IptvOrgPage } from './IptvOrgPage';

vi.mock('./MapView', () => ({
  MapView: ({ onPick }: { onPick: (id: string) => void }) => (
    <button type="button" data-testid="map-stub" onClick={() => { onPick('s/US-KS'); }}>
      map stub
    </button>
  ),
}));

const AXN = index.streams.find((x) => x.ch === 'AXNBlack.us');
if (AXN === undefined) {
  throw new Error('fixture');
}

let created: { id: string; payload: unknown }[] = [];
let probeBodies: unknown[] = [];

const server = setupServer(
  http.get('*/api/v1/catalogues/iptv-org/status', () =>
    HttpResponse.json({
      state: 'idle', etag: null, synced_at: null, counts: { streams: 28, channels: 24 }, errors: [],
      server: { name: 'Dallas', lat: 32.78, lon: -96.8 },
    }),
  ),
  http.get('*/api/v1/catalogues/iptv-org/index', () => HttpResponse.json(index)),
  http.get('*/api/v1/catalogues/iptv-org/places', () => HttpResponse.json(places)),
  http.get('*/api/v1/sources', () =>
    HttpResponse.json([
      { id: 'wps', name: 'WPS TV', body: { id: 'wps', kind: 'hls', url: 'https://2-fss-2.streamhoster.com/pl_120/amlst:203530-1426406/playlist.m3u8' } },
    ]),
  ),
  http.get('*/api/v1/catalogues/iptv-org/links', () => HttpResponse.json({ links: [] })),
  http.post('*/api/v1/catalogues/iptv-org/probes', async ({ request }) => {
    probeBodies.push(await request.json());
    return HttpResponse.json({
      id: 'p1', state: 'done', error: null,
      results: [
        {
          k: 's:d4e9ec8986d2', source_id: null, verdict: 'live', probed_from: 'dallas-1',
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
    expect(screen.getByTestId('iptv-results')).not.toHaveTextContent('KSHB');
  });

  it('facet filtering shows counts and narrows the list', async () => {
    renderPage();
    await ready();
    const geo = await screen.findByRole('checkbox', { name: 'Geo-blocked' });
    expect(geo.closest('li')).toHaveTextContent(/\d+/);
    await userEvent.click(geo);
    await waitFor(() => { expect(screen.getByTestId('iptv-results')).toHaveTextContent('PBS Sedalia'); });
    expect(screen.getByTestId('iptv-results')).not.toHaveTextContent('BBC News');
    // The flag renders as text, not colour alone.
    expect(within(screen.getByTestId('iptv-results')).getAllByText('Geo-blocked').length).toBeGreaterThan(0);
  });

  it('place search ranks Kansas City, MO first; the row leads with the station, not the network', async () => {
    renderPage();
    await ready();
    await userEvent.type(screen.getByRole('searchbox', { name: /where is it happening/i }), 'kansas citty, MO');
    const hits = await screen.findByRole('list', { name: /matching places/i });
    const buttons = within(hits).getAllByRole('button');
    expect(buttons[0]).toHaveTextContent('Kansas City, Missouri, United States');
    const top = buttons[0];
    if (top === undefined) {
      throw new Error('no place hit');
    }
    await userEvent.click(top);
    const results = await screen.findByTestId('iptv-results');
    await waitFor(() => { expect(results).toHaveTextContent('Local (this city)'); });
    expect(results).toHaveTextContent('KSHB 41 (Kansas City MO)');
    expect(results).toHaveTextContent('KSHB-TV');
    expect(results).toHaveTextContent('on NBC');
    expect(results).toHaveTextContent('KCKS-LD 25.3');
    expect(results).not.toHaveTextContent('BBC News');
    expect(screen.getByText(/Near Kansas City/)).toBeInTheDocument();
    // Later tiers follow: Missouri has no other state-level row here, national does.
    expect(results).toHaveTextContent('Nearby (within 150 km)');
    expect(results).toHaveTextContent('National');
  });

  it('says plainly that iptv-org lists few local streams, and still shows state and national tiers', async () => {
    renderPage();
    await ready();
    await userEvent.type(screen.getByRole('searchbox', { name: /where is it happening/i }), 'tampa');
    const hits = await screen.findByRole('list', { name: /matching places/i });
    const top = within(hits).getAllByRole('button')[0];
    if (top === undefined) {
      throw new Error('no place hit');
    }
    await userEvent.click(top);
    const sparse = await screen.findByTestId('iptv-sparse');
    expect(sparse).toHaveTextContent(/iptv-org lists few local streams here/);
    expect(sparse).toHaveTextContent(/none in this city/);
    const results = screen.getByTestId('iptv-results');
    await waitFor(() => { expect(results).toHaveTextContent('State / province'); });
    expect(results).toHaveTextContent('Tampa Bay Communnity Network');
    expect(results).toHaveTextContent('National');
  });

  it('Wichita (one real local stream) also gets the honest notice', async () => {
    renderPage();
    await ready();
    await userEvent.type(screen.getByRole('searchbox', { name: /where is it happening/i }), 'wichita');
    const hits = await screen.findByRole('list', { name: /matching places/i });
    const top = within(hits).getAllByRole('button')[0];
    if (top === undefined) {
      throw new Error('no place hit');
    }
    await userEvent.click(top);
    expect(await screen.findByTestId('iptv-sparse')).toHaveTextContent(/1 in this city/);
    expect(screen.getByTestId('iptv-results')).toHaveTextContent('WPS TV');
  });

  it('map click selects a subdivision through the same path; near-server is offered', async () => {
    renderPage();
    await ready();
    expect(await screen.findByRole('button', { name: /near this server/i })).toBeInTheDocument();
    await userEvent.click(screen.getByTestId('map-stub'));
    await waitFor(() => { expect(screen.getByTestId('iptv-results')).toHaveTextContent('WPS TV'); });
  });

  it('probe flow shows verdict, facts and events', async () => {
    renderPage();
    await ready();
    await userEvent.type(screen.getByRole('searchbox', { name: /search channels/i }), 'kshb');
    await userEvent.click(await screen.findByRole('button', { name: /show streams of kshb 41/i }));
    await userEvent.click(await screen.findByRole('button', { name: /^probe kshb 41 \(kansas city mo\)$/i }));
    await userEvent.click(await screen.findByRole('button', { name: /details for kshb 41/i }));
    const detail = await screen.findByTestId('iptv-detail');
    expect(await within(detail).findByText('Live')).toBeInTheDocument();
    expect(within(detail).getByText(/1280×720/)).toBeInTheDocument();
    expect(within(detail).getByText('cue_out')).toBeInTheDocument();
    expect(probeBodies[0]).toEqual({ keys: ['s:d4e9ec8986d2'] });
  });

  it('marks already-added streams', async () => {
    renderPage();
    await ready();
    await userEvent.type(screen.getByRole('searchbox', { name: /search channels/i }), 'wps tv');
    await userEvent.click(await screen.findByRole('button', { name: /show streams of wps tv/i }));
    expect((await screen.findAllByText('Already added')).length).toBeGreaterThan(0);
    expect(screen.getByRole('button', { name: /add wps tv as source/i })).toBeDisabled();
  });

  it('Add as source opens the Sources create dialog prefilled with url/UA/referer/origin', async () => {
    renderPage();
    await ready();
    await userEvent.type(screen.getByRole('searchbox', { name: /search channels/i }), 'axn black');
    await userEvent.click(await screen.findByRole('button', { name: /show streams of axn black/i }));
    await userEvent.click(await screen.findByRole('button', { name: /add axn black romania as source/i }));
    const dialog = await screen.findByRole('dialog');
    expect(within(dialog).getByDisplayValue(AXN.url)).toBeInTheDocument();
    expect(within(dialog).getByText(/linked to iptv-org/i)).toBeInTheDocument();
    await userEvent.click(within(dialog).getByRole('button', { name: /^create$/i }));
    await waitFor(() => { expect(created).toHaveLength(1); });
    expect(created[0]?.payload).toMatchObject({
      body: {
        kind: 'hls',
        url: AXN.url,
        user_agent: AXN.ua,
        referrer: AXN.ref,
        playlist_metadata: { tvg_id: 'AXNBlack.us@Romania', name: 'AXN Black Romania' },
        origin: { provider: 'iptv-org', channel: 'AXNBlack.us', follow: 'notify' },
      },
    });
  });

  it('Add as source sends the upstream logo_url, never the page origin', async () => {
    renderPage();
    await ready();
    await userEvent.type(screen.getByRole('searchbox', { name: /search channels/i }), 'axn black');
    await userEvent.click(await screen.findByRole('button', { name: /show streams of axn black/i }));
    await userEvent.click(await screen.findByRole('button', { name: /add axn black romania as source/i }));
    const dialog = await screen.findByRole('dialog');
    await userEvent.click(within(dialog).getByRole('button', { name: /^create$/i }));
    await waitFor(() => { expect(created).toHaveLength(1); });
    const body = (created[0]?.payload as { body: { playlist_metadata: { logo?: string }; epg?: unknown } }).body;
    expect(body.playlist_metadata.logo).toBe('https://i.imgur.com/Peo1QiZ.png');
    expect(body.epg ?? null).toBeNull();
    expect(JSON.stringify(created[0]?.payload)).not.toContain(window.location.origin);
  });

  it('a null logo_url gives no playlist logo', async () => {
    const { toSourceForm } = await import('./toSource');
    const s = (index.streams as unknown as IndexStream[]).find((x) => x.logo_url === null);
    expect(s).toBeDefined();
    expect(toSourceForm({ ...s!, logo: 'l:abc' }, new Set()).metaLogo).toBe('');
  });

  it('shows a "Guide listed" badge with the XMLTV note in the detail', async () => {
    renderPage();
    await ready();
    await userEvent.type(screen.getByRole('searchbox', { name: /search channels/i }), 'axn black');
    await userEvent.click(await screen.findByRole('button', { name: /show streams of axn black/i }));
    await userEvent.click(await screen.findByRole('button', { name: /details for axn black/i }));
    const detail = await screen.findByTestId('iptv-detail');
    expect(within(detail).getByText('Guide listed: tvtv.us')).toBeInTheDocument();
    expect(within(detail).getByText(/supply the XMLTV URL/i)).toBeInTheDocument();
  });

  it('bulk add reports per-item results including a partial failure', async () => {
    renderPage();
    await ready();
    const box = screen.getByRole('searchbox', { name: /search channels/i });
    await userEvent.type(box, 'kshb');
    await userEvent.click(await screen.findByRole('checkbox', { name: /select kshb 41 .* for bulk add/i }));
    await userEvent.clear(box);
    await userEvent.type(box, 'bbc news');
    await userEvent.click(await screen.findByRole('checkbox', { name: /select bbc news uk hd for bulk add/i }));
    await userEvent.click(screen.getByRole('button', { name: /add selected \(2\)/i }));
    const dialog = await screen.findByRole('dialog');
    await userEvent.click(within(dialog).getByRole('button', { name: /add 2 sources/i }));
    await waitFor(() => { expect(within(dialog).getAllByText(/created|failed/i).length).toBeGreaterThanOrEqual(2); });
    expect(created.map((c) => c.id)).toEqual(['nbc-us-kshbtv']);
    expect(within(dialog).getByText(/Failed: Conflict/)).toBeInTheDocument();
  });
});
