// #419 item 2: the Kind select shows human labels with the raw id as
// secondary text, e.g. "Colour bars (bars)", "YouTube live (youtube)".
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { MemoryRouter } from 'react-router-dom';

import { SourcesPage } from './SourcesPage';
import { renderWithProviders } from '../test/render';

const server = setupServer(http.get('*/api/v1/sources', () => HttpResponse.json([])));

beforeAll(() => {
  server.listen({ onUnhandledRequest: 'bypass' });
});
afterEach(() => {
  server.resetHandlers();
});
afterAll(() => {
  server.close();
});

const EXPECTED: readonly [string, string][] = [
  ['bars', 'Colour bars (bars)'],
  ['solid', 'Solid colour (solid)'],
  ['clock', 'Clock (clock)'],
  ['timer', 'Timer (timer)'],
  ['rtsp', 'RTSP camera / encoder (rtsp)'],
  ['hls', 'HLS stream (hls)'],
  ['youtube', 'YouTube live (youtube)'],
  ['ts', 'MPEG-TS (UDP/HTTP) (ts)'],
  ['srt', 'SRT (srt)'],
  ['rist', 'RIST (rist)'],
  ['rtmp', 'RTMP push (rtmp)'],
  ['ndi', 'NDI (ndi)'],
  ['file', 'File (file)'],
  ['webrtc', 'WebRTC (WHIP) (webrtc)'],
];

describe('SourcesPage Kind select — human labels (#419)', () => {
  it('shows a human label with the raw id as secondary text for every kind', async () => {
    renderWithProviders(
      <MemoryRouter>
        <SourcesPage />
      </MemoryRouter>,
    );
    await userEvent.click(await screen.findByRole('button', { name: 'New source' }));
    const dialog = await screen.findByRole('dialog');
    const kindTrigger = within(dialog).getByRole('combobox', { name: 'Kind' });
    await userEvent.click(kindTrigger);

    for (const [, label] of EXPECTED) {
      expect(await screen.findByRole('option', { name: label })).toBeInTheDocument();
    }
  });
});
