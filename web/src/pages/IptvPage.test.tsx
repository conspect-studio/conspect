// IPTV playlist page + M3U import helpers (epg-playlist).
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { MemoryRouter } from 'react-router-dom';

import { IptvPage } from './IptvPage';
import { candidateToSourceBody, moveEntry, slugify, uniqueId } from '../api/iptv';
import type { IptvState } from '../api/iptv';
import { renderWithProviders } from '../test/render';

describe('M3U import helpers', () => {
  it('slugifies names and suffixes collisions', () => {
    expect(slugify('CNN US (1080p) — backup')).toBe('cnn-us-1080p-backup');
    expect(slugify('Español')).toBe('espanol');
    expect(slugify('***')).toBe('source');
    const taken = new Set(['cnn-us']);
    expect(uniqueId('CNN US', taken)).toBe('cnn-us-2');
    expect(uniqueId('CNN US', taken)).toBe('cnn-us-3');
  });

  it('builds an HLS body with manual EPG and playlist metadata', () => {
    const body = candidateToSourceBody(
      { name: 'CNN US', url: 'https://cdn/x.m3u8', tvg_id: 'CNN.us', tvg_logo: 'https://l/c.png', group: 'News' },
      'cnn-us',
      'https://epg/guide.xml',
    );
    expect(body).toEqual({
      id: 'cnn-us',
      kind: 'hls',
      url: 'https://cdn/x.m3u8',
      epg: { mode: 'manual', url: 'https://epg/guide.xml', channel_id: 'CNN.us' },
      playlist_metadata: { tvg_id: 'CNN.us', name: 'CNN US', logo: 'https://l/c.png', group: 'News' },
    });
    const bare = candidateToSourceBody({ name: 'X', url: 'https://cdn/y.m3u8' }, 'x', undefined);
    expect(bare).toEqual({ id: 'x', kind: 'hls', url: 'https://cdn/y.m3u8', playlist_metadata: { name: 'X' } });
  });

  it('writes the activation an import chose', () => {
    const body = candidateToSourceBody({ name: 'X', url: 'https://cdn/y.m3u8' }, 'x', undefined, 'on_demand');
    expect(body.activation).toBe('on_demand');
  });

  it('reorders entries', () => {
    expect(moveEntry(['a', 'b', 'c'], 2, -1)).toEqual(['a', 'c', 'b']);
    expect(moveEntry(['a', 'b'], 0, -1)).toEqual(['a', 'b']);
  });
});

let current: IptvState = {
  config: { enabled: false, entries: [] },
  entries: [],
  feeds: [],
  playlist_path: '/iptv/playlist.m3u8',
  guide_path: '/iptv/guide.xml',
};
let lastPut: unknown;
let lastIfMatch: string | null = null;

const server = setupServer(
  http.get('*/api/v1/iptv', () => HttpResponse.json(current, { headers: { ETag: '"iptv-1"' } })),
  http.put('*/api/v1/iptv', async ({ request }) => {
    lastPut = await request.json();
    lastIfMatch = request.headers.get('If-Match');
    current = { ...current, config: lastPut as IptvState['config'] };
    return HttpResponse.json(current, { headers: { ETag: '"iptv-2"' } });
  }),
  http.get('*/api/v1/outputs', () =>
    HttpResponse.json([{ id: 'output-0', name: 'hls', body: { kind: 'hls', path: '/x/program.m3u8' } }]),
  ),
  http.get('*/api/v1/sources', () => HttpResponse.json([])),
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

describe('IptvPage', () => {
  it('enables publication, adds an output and saves with If-Match', async () => {
    renderWithProviders(
      <MemoryRouter>
        <IptvPage />
      </MemoryRouter>,
    );
    expect(await screen.findByText(/\/iptv\/playlist\.m3u8/)).toBeInTheDocument();
    const user = userEvent.setup();
    await user.click(screen.getByLabelText(/Serve the playlist and guide/));
    await waitFor(() => {
      expect(screen.getByRole('option', { name: /Output: hls/ })).toBeInTheDocument();
    });
    await user.selectOptions(screen.getByLabelText(/Add an output or source/), 'output:output-0');
    await user.click(screen.getByRole('button', { name: /^Add$/ }));
    await user.click(screen.getByRole('button', { name: /^Save$/ }));
    await waitFor(() => {
      expect(lastPut).toEqual({
        enabled: true,
        public_base_url: null,
        entries: [{ kind: 'output', ref: 'output-0', allow_direct_source_url: false }],
      });
    });
    expect(lastIfMatch).toBe('"iptv-1"');
  });
});
