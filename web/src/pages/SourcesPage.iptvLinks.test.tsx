// Sources page: Link column, linked-origin panel round-trip, Review changes.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { MemoryRouter } from 'react-router-dom';

import { SourcesPage } from './SourcesPage';
import { renderWithProviders } from '../test/render';

const ORIGIN = {
  provider: 'iptv-org', channel: 'NBC.us', feed: 'KSHBTV',
  stream_url: 'https://old.test/kshb.m3u8', labels: [], linked_at: '2026-11-01T00:00:00Z', follow: 'notify',
};
const BODY = { id: 'kshb', kind: 'hls', url: 'https://old.test/kshb.m3u8', display_name: 'KSHB 41 (Kansas City MO)', origin: ORIGIN, extra_unknown: 1 };

let put: unknown;
const server = setupServer(
  http.get('*/api/v1/sources', () =>
    HttpResponse.json([
      { id: 'kshb', name: 'KSHB 41 (Kansas City MO)', body: BODY },
      { id: 'plain', name: 'Plain', body: { id: 'plain', kind: 'hls', url: 'https://x.test/a.m3u8' } },
    ]),
  ),
  http.get('*/api/v1/sources/kshb', () =>
    HttpResponse.json({ id: 'kshb', name: 'KSHB 41 (Kansas City MO)', body: BODY }, { headers: { ETag: '"7"' } }),
  ),
  http.put('*/api/v1/sources/kshb', async ({ request }) => {
    put = { ifMatch: request.headers.get('If-Match'), body: await request.json() };
    return HttpResponse.json({ id: 'kshb', name: 'KSHB 41 (Kansas City MO)', body: BODY }, { headers: { ETag: '"8"' } });
  }),
  http.get('*/api/v1/catalogues/iptv-org/links', () =>
    HttpResponse.json({
      links: [
        {
          source_id: 'kshb', status: 'updated', checked_at: null, replaced_by: null, alternatives: [],
          upstream: { url: 'https://new.test/kshb.m3u8', user_agent: 'UA/2', referrer: null, labels: ['Geo-blocked'] },
        },
      ],
    }),
  ),
);
beforeAll(() => { server.listen(); });
afterEach(() => { server.resetHandlers(); put = undefined; });
afterAll(() => { server.close(); });

describe('Sources page IPTV-ORG integration', () => {
  it('shows a Browse button, the link status as text, and "—" for unlinked rows', async () => {
    renderWithProviders(<MemoryRouter><SourcesPage /></MemoryRouter>);
    expect(await screen.findByRole('link', { name: /browse iptv-org/i })).toHaveAttribute('href', '/sources/iptv-org');
    await userEvent.click(await screen.findByRole('button', { name: /^details: kshb/i }));
    expect(await screen.findByText('Updated upstream')).toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: /^details: plain/i }));
    expect(screen.getByText('Not linked to IPTV-ORG')).toBeInTheDocument();
  });

  it('Review changes shows the diff and Apply PUTs url/headers/labels with If-Match', async () => {
    renderWithProviders(<MemoryRouter><SourcesPage /></MemoryRouter>);
    await userEvent.click(await screen.findByRole('button', { name: /^details: kshb/i }));
    await userEvent.click(await screen.findByRole('button', { name: /review changes: kshb/i }));
    const dialog = await screen.findByRole('dialog');
    expect(within(dialog).getByText('https://new.test/kshb.m3u8')).toBeInTheDocument();
    expect(within(dialog).getByText('UA/2')).toBeInTheDocument();
    await userEvent.click(within(dialog).getByRole('button', { name: 'Apply' }));
    await waitFor(() => { expect(put).toBeDefined(); });
    expect(put).toMatchObject({
      ifMatch: '"7"',
      body: { body: { url: 'https://new.test/kshb.m3u8', user_agent: 'UA/2', origin: { stream_url: 'https://new.test/kshb.m3u8', labels: ['Geo-blocked'], follow: 'notify' } } },
    });
  });

  it('editing a linked source shows the read-only panel and round-trips origin', async () => {
    renderWithProviders(<MemoryRouter><SourcesPage /></MemoryRouter>);
    await userEvent.click(await screen.findByRole('button', { name: /^details: kshb/i }));
    await userEvent.click(await screen.findByRole('button', { name: /edit source: kshb/i }));
    const dialog = await screen.findByRole('dialog');
    expect(await within(dialog).findByText('Linked to IPTV-ORG')).toBeInTheDocument();
    await userEvent.click(within(dialog).getByRole('button', { name: /^save$/i }));
    await waitFor(() => { expect(put).toBeDefined(); });
    expect(put).toMatchObject({ body: { body: { extra_unknown: 1, origin: ORIGIN } } });
  });
});
