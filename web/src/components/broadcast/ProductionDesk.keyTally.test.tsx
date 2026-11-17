// Wall tile PGM/PVW KEY badges from the engine tally (design note §9/§10,
// K5): a source that is a DSK fill/key and that the tally currently carries
// shows `● PGM KEY` / `▲ PVW KEY` next to its ordinary bus tally.
import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from 'vitest';

vi.mock('../../realtime/shared', () => ({ subscribeRealtime: () => () => undefined }));
vi.mock('../../realtime/useEngineEvents', () => ({ useEngineEvents: () => ({ status: 'open' }), TILES_QUERY_KEY: ['tiles'] }));
vi.mock('../../preview/PreviewSurface', () => ({ PreviewSurface: () => null }));
vi.mock('../../preview/useJpegPreview', () => ({ useJpegPreview: () => undefined, useJpegStill: () => ({ url: undefined, state: 'loading' }) }));
import { render, screen } from '@testing-library/react';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { i18n } from '@lingui/core';
import { I18nProvider } from '@lingui/react';
import { MemoryRouter } from 'react-router-dom';
import { messages } from '../../locales/en/messages';
import { ProductionDesk } from './ProductionDesk';

i18n.load('en', messages);
i18n.activate('en');

const DSK1 = {
  id: 'dsk1', family: 'dsk', fill: 'gfx-1', key: null, type: 'linear',
  fly: null, mask: null, on_air: true, tie: false, ramp: null, rate: 15,
  loss: 'drop_to_off_air', stale: false, pending: false,
};
const baseState = { program_source: 'cam-1', preview_source: 'cam-2', tick: 7, in_flight: false, keyers: [DSK1], tally: { program: ['cam-1', 'gfx-1'], preview: ['cam-2'] } };
let polls = 0;
const server = setupServer(
  http.get('*/api/v1/switcher/me1', () => { polls += 1; return HttpResponse.json({ ...baseState, tick: baseState.tick + polls }); }),
  http.get('*/api/v1/layouts', () => HttpResponse.json([])),
  http.get('*/api/v1/alarms', () => HttpResponse.json([])),
  http.get('*/api/v1/preview/capabilities', () => HttpResponse.json({ webrtc: false })),
  http.get('*/api/v1/sources', () => HttpResponse.json([
    { id: 'cam-1', name: 'CAM 1', body: { kind: 'srt', url: 'srt://[::1]:1' } },
    { id: 'cam-2', name: 'CAM 2', body: { kind: 'srt', url: 'srt://[::1]:2' } },
    { id: 'gfx-1', name: 'GFX 1', body: { kind: 'srt', url: 'srt://[::1]:3' } },
  ])),
);
beforeAll(() => { server.listen(); });
afterEach(() => { server.resetHandlers(); polls = 0; });
afterAll(() => { server.close(); });

function page() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(<QueryClientProvider client={client}><I18nProvider i18n={i18n}><MemoryRouter><ProductionDesk /></MemoryRouter></I18nProvider></QueryClientProvider>);
}

describe('ProductionDesk wall tile KEY tally badges (K5)', () => {
  it('badges a DSK fill carried on Program with ● PGM KEY once the engine confirms', async () => {
    page();
    await screen.findByText('● ON AIR — PROGRAM', undefined, { timeout: 6000 });
    expect(await screen.findByTestId('tile-key-badge-gfx-1')).toHaveTextContent('● PGM KEY');
  }, 10000);

  it('does not badge a source that is neither a keyer fill/key nor on the tally', async () => {
    page();
    await screen.findByText('● ON AIR — PROGRAM', undefined, { timeout: 6000 });
    expect(screen.queryByTestId('tile-key-badge-cam-2')).toBeNull();
  }, 10000);
});
