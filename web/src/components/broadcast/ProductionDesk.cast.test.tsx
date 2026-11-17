// The desk's monitor captions carry a browser Cast button: Program casts
// Program, Preview casts Preview (both through a cast stream for that bus).
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('../../realtime/shared', () => ({ subscribeRealtime: () => () => undefined }));
vi.mock('../../realtime/useEngineEvents', () => ({ useEngineEvents: () => ({ status: 'open' }), TILES_QUERY_KEY: ['tiles'] }));
vi.mock('../../preview/PreviewSurface', () => ({ PreviewSurface: () => null }));
vi.mock('../../preview/useJpegPreview', () => ({ useJpegPreview: () => undefined, useJpegStill: () => ({ url: undefined, state: 'loading' }) }));

import { act, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { MemoryRouter } from 'react-router-dom';

import { renderWithProviders } from '../../test/render';
import { resetCastSdkLoaderForTests } from '../../cast/sdkLoader';
import { installMemoryStorage } from '../../cast/testApi';
import { installMockCastSdk } from '../../cast/testMocks';
import type { MockCastSdk } from '../../cast/testMocks';
import { ProductionDesk } from './ProductionDesk';

const created: unknown[] = [];
let refuseCreate = true;

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
    if (refuseCreate) {
      return HttpResponse.json({ type: '/problems/cast-capacity', title: 'Cast refused', status: 409, detail: 'busy' }, { status: 409 });
    }
    return HttpResponse.json({
      id: 'cs-1',
      output: 'cast-1',
      feed: {},
      path: '/cast/token-1/master.m3u8',
      content_type: 'application/vnd.apple.mpegurl',
      hls_segment_format: 'fmp4',
      hls_video_segment_format: 'fmp4',
      ready: true,
      segments: 12,
      expires_unix_ms: 0,
      fetches: { playlists: 0, segments: 0, cast_device: false },
    }, { status: 201 });
  }),
);

beforeAll(() => {
  server.listen();
});
let sdk: MockCastSdk;

beforeEach(() => {
  installMemoryStorage();
  resetCastSdkLoaderForTests();
  sdk = installMockCastSdk();
  created.length = 0;
  refuseCreate = true;
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
    // Findable: a visible "Cast" word beside the icon on wide screens, and a tooltip that says what it does.
    expect(pgm).toHaveTextContent('Cast');
    expect(pgm.querySelector('.md\\:inline')).not.toBeNull();
    expect(pgm).toHaveAttribute('title', 'Cast Program to a TV (Google Cast)');
    await user.click(pvw);
    await waitFor(() => {
      expect(created).toEqual([{ feed: { bus: 'preview' } }]);
    });
    // The refusal shows as the Retry button's name/title, never opens the picker.
    expect(await within(pvwSlot).findByRole('button', { name: 'Retry cast: Preview' })).toBeInTheDocument();
    expect(screen.getAllByRole('alert').some((n) => n.textContent.includes('Every cast output is in use'))).toBe(true);
  });

  it('opens the Cast panel (Video/Audio/Subtitles pickers) from the desk once a cast is active', async () => {
    refuseCreate = false;
    const user = userEvent.setup();
    const { container } = renderWithProviders(<MemoryRouter><ProductionDesk /></MemoryRouter>);
    const pgmSlot = container.querySelector<HTMLElement>('[data-monitor-actions="program"]');
    if (pgmSlot === null) throw new Error('monitor action slot missing');
    const pgm = await within(pgmSlot).findByRole('button', { name: 'Cast: Program' });
    await waitFor(() => {
      expect(pgm).toBeEnabled();
    });
    await user.click(pgm);
    await waitFor(() => {
      expect(sdk.loadMedia).toHaveBeenCalledTimes(1);
    });
    const settingsButton = await within(pgmSlot).findByRole('button', { name: 'Cast settings' });
    await user.click(settingsButton);
    expect(await screen.findByText('Casting from this browser')).toBeInTheDocument();
    // Only one audio track so far: Audio/Subtitles stay hidden.
    expect(screen.queryByLabelText('Audio')).toBeNull();
    act(() => {
      sdk.emitTracks(
        [
          { trackId: 1, type: 'AUDIO', name: 'English', language: 'en' },
          { trackId: 2, type: 'AUDIO', name: 'French', language: 'fr' },
        ],
        [1],
      );
    });
    expect(await screen.findByLabelText('Audio')).toBeInTheDocument();
  });
});
