// Outputs page: a browser Cast button renders only on HLS/LL-HLS output
// rows — other output kinds get no Cast control (PR 1 scope: casting an
// existing HLS output's own playlist URL).
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it } from 'vitest';
import { screen, within } from '@testing-library/react';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { MemoryRouter } from 'react-router-dom';

import { OutputsPage } from './OutputsPage';
import { renderWithProviders } from '../test/render';

const OUTPUTS = [
  { id: 'program', name: 'Program HLS', body: { kind: 'hls', path: '/srv/index.m3u8', codec: 'h264' } },
  { id: 'll', name: 'Low-latency HLS', body: { kind: 'll-hls', path: '/srv/ll.m3u8', codec: 'h264' } },
  { id: 'out-rtsp', name: 'Studio RTSP', body: { kind: 'rtsp_server', mount: '/multiview', codec: 'h264' } },
];

const server = setupServer(
  http.get('*/api/v1/outputs', () => HttpResponse.json(OUTPUTS)),
  http.get('*/api/v1/outputs/:id/playback', () => HttpResponse.json({ urls: [] })),
  http.get('*/api/v1/routing', () => HttpResponse.json({ outputs: [] })),
  http.get('*/api/v1/sources', () => HttpResponse.json([])),
  http.get('*/api/v1/media/players', () => HttpResponse.json([])),
  http.get('*/api/v1/devices', () => HttpResponse.json([])),
  http.get('*/api/v1/audio-routing', () => HttpResponse.json({ configured: false })),
);

beforeAll(() => {
  server.listen({ onUnhandledRequest: 'warn' });
});
beforeEach(() => {
  // Cast support detection needs `window.chrome`; these tests assert on the
  // Cast button itself, not browser-support gating (see `CastButton.test.tsx`).
  window.chrome = {};
});
afterEach(() => {
  server.resetHandlers();
  delete window.chrome;
});
afterAll(() => {
  server.close();
});

describe('OutputsPage — Cast buttons', () => {
  it('shows a Cast button on each HLS/LL-HLS row and none on other kinds', async () => {
    renderWithProviders(<MemoryRouter><OutputsPage /></MemoryRouter>);

    const hlsCard = await screen.findByTestId('output-card-program');
    expect(within(hlsCard).getByRole('button', { name: /^Cast: Program HLS$/ })).toBeInTheDocument();

    const llCard = await screen.findByTestId('output-card-ll');
    expect(within(llCard).getByRole('button', { name: /^Cast: Low-latency HLS$/ })).toBeInTheDocument();

    const rtspCard = await screen.findByTestId('output-card-out-rtsp');
    expect(within(rtspCard).queryByRole('button', { name: /^Cast:/ })).not.toBeInTheDocument();
    expect(within(rtspCard).queryByRole('button', { name: 'Cast needs Chrome or Edge' })).not.toBeInTheDocument();
  });
});
