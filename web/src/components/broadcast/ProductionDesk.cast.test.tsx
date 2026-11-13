// The desk's monitor captions carry a browser Cast button: Program casts
// Program, Preview casts Preview (both through a cast stream for that bus).
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('../../realtime/shared', () => ({ subscribeRealtime: () => () => undefined }));
vi.mock('../../realtime/useEngineEvents', () => ({ useEngineEvents: () => ({ status: 'open' }), TILES_QUERY_KEY: ['tiles'] }));
vi.mock('../../preview/PreviewSurface', () => ({ PreviewSurface: () => null }));
vi.mock('../../preview/useJpegPreview', () => ({ useJpegPreview: () => undefined, useJpegStill: () => ({ url: undefined, state: 'loading' }) }));

import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { MemoryRouter } from 'react-router-dom';

import { renderWithProviders } from '../../test/render';
import { resetCastSdkLoaderForTests } from '../../cast/sdkLoader';
import { installMemoryStorage } from '../../cast/testApi';
import { installMockCastSdk } from '../../cast/testMocks';
import { ProductionDesk } from './ProductionDesk';

const created: unknown[] = [];

const server = setupServer(
  http.get('*/api/v1/switcher/me1', () => HttpResponse.json({ program_source: 'cam-1', preview_source: 'cam-2', tick: 1, in_flight: false })),
  http.get('*/api/v1/layouts', () => HttpResponse.json([])),
  http.get('*/api/v1/alarms', () => HttpResponse.json([])),
  http.get('*/api/v1/preview/capabilities', () => HttpResponse.json({ webrtc: false })),
  http.get('*/api/v1/sources', () => HttpResponse.json([])),
  http.get('*/api/v1/cast/config', () =>
    HttpResponse.json({ receiver_app_id: '8E05190C', receiver_path: '/cast/receiver/', namespace: 'urn:x-cast:studio.conspect.cast', outputs: [{ id: 'cast-1', active: false, ready: true, streams: 0 }] }),
  ),
  http.get('*/api/v1/cast/streams', () => HttpResponse.json([])),
  http.post('*/api/v1/cast/streams', async ({ request }) => {
    created.push(await request.json());
    return HttpResponse.json({ type: '/problems/cast-capacity', title: 'Cast refused', status: 409, detail: 'busy' }, { status: 409 });
  }),
);

beforeAll(() => {
  server.listen();
});
beforeEach(() => {
  installMemoryStorage();
  resetCastSdkLoaderForTests();
  installMockCastSdk();
  created.length = 0;
});
afterEach(() => {
  server.resetHandlers();
  delete window.cast;
  delete window.chrome;
  delete window.__onGCastApiAvailable;
});
afterAll(() => {
  server.close();
});

describe('ProductionDesk Cast buttons', () => {
  it('puts a Cast button in the Program and Preview monitor captions, each casting its bus', async () => {
    const user = userEvent.setup();
    const { container } = renderWithProviders(<MemoryRouter><ProductionDesk /></MemoryRouter>);
    const pgmSlot = container.querySelector<HTMLElement>('[data-monitor-actions="program"]');
    const pvwSlot = container.querySelector<HTMLElement>('[data-monitor-actions="preview"]');
    if (pgmSlot === null || pvwSlot === null) throw new Error('monitor action slots missing');
    const pgm = await within(pgmSlot).findByRole('button', { name: 'Cast: Program' });
    const pvw = within(pvwSlot).getByRole('button', { name: 'Cast: Preview' });
    await waitFor(() => {
      expect(pgm).toBeEnabled();
    });
    await user.click(pvw);
    await waitFor(() => {
      expect(created).toEqual([{ feed: { bus: 'preview' } }]);
    });
    // The refusal shows as the Retry button's name/title, never opens the picker.
    expect(await within(pvwSlot).findByRole('button', { name: 'Retry cast: Preview' })).toBeInTheDocument();
    expect(screen.getAllByRole('alert').some((n) => n.textContent.includes('Every cast output is in use'))).toBe(true);
  });
});
