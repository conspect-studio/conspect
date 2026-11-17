// ActiveCastCard: the Video/Audio/Subtitles pickers (decision 2/7) — hidden
// with one choice, visible and wired to the controller otherwise.
import { act, render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { TestProviders } from '../test/render';
import type { CastOutput } from '../api/castStreams';
import { ActiveCastCard } from './ActiveCastCard';
import { CastController } from './castController';
import { CastProvider } from './CastProvider';
import { CAST_NAMESPACE } from './castReport';
import { resetCastSdkLoaderForTests } from './sdkLoader';
import { fakeApi, installMemoryStorage, refusal } from './testApi';
import type { FakeApi } from './testApi';
import { installMockCastSdk } from './testMocks';
import type { MockCastSdk } from './testMocks';

const OUTPUTS: CastOutput[] = [
  { id: 'cast-1', active: true, ready: true, streams: 1 },
  { id: 'cast-2', active: false, ready: true, streams: 0 },
];

describe('ActiveCastCard', () => {
  let sdk: MockCastSdk;
  let api: FakeApi;
  let controller: CastController;

  beforeEach(() => {
    installMemoryStorage();
    resetCastSdkLoaderForTests();
    sdk = installMockCastSdk({ deviceName: 'Living Room TV' });
    api = fakeApi();
    controller = new CastController(api);
    controller.activate();
    controller.configure({ appId: '8E05190C', namespace: CAST_NAMESPACE });
  });

  afterEach(() => {
    delete window.cast;
    delete window.chrome;
    delete window.__onGCastApiAvailable;
  });

  function renderCard(outputs: readonly CastOutput[] = OUTPUTS): ReturnType<typeof render> {
    return render(
      <TestProviders>
        <CastProvider controller={controller}>
          <ActiveCastCard options={[]} outputs={outputs} />
        </CastProvider>
      </TestProviders>,
    );
  }

  it('renders nothing while idle', () => {
    renderCard();
    expect(screen.queryByText('Casting from this browser')).toBeNull();
  });

  it('hides Video with a single cast output, Audio/Subtitles with at most one choice', async () => {
    await controller.start({ key: 'feed:bus:program', name: 'Program', feed: { bus: 'program' } });
    renderCard([OUTPUTS[0]!]);
    expect(await screen.findByText('Casting from this browser')).toBeInTheDocument();
    expect(screen.queryByLabelText('Video')).toBeNull();
    expect(screen.queryByLabelText('Audio')).toBeNull();
    expect(screen.queryByLabelText('Subtitles')).toBeNull();
  });

  it('shows Video with more than one cast output, and Audio/Subtitles once MEDIA_STATUS lists more than one choice', async () => {
    await controller.start({ key: 'feed:bus:program', name: 'Program', feed: { bus: 'program' } });
    renderCard(OUTPUTS);
    expect(await screen.findByLabelText('Video')).toBeInTheDocument();
    // One audio track, no text tracks yet: both hidden.
    act(() => {
      sdk.emitTracks([{ trackId: 1, type: 'AUDIO', name: 'English', language: 'en' }], [1]);
    });
    expect(screen.queryByLabelText('Audio')).toBeNull();
    expect(screen.queryByLabelText('Subtitles')).toBeNull();
    // A second audio track and one text track: both appear ("Off" always counts for subtitles).
    act(() => {
      sdk.emitTracks(
        [
          { trackId: 1, type: 'AUDIO', name: 'English', language: 'en' },
          { trackId: 2, type: 'AUDIO', name: 'French', language: 'fr' },
          { trackId: 10, type: 'TEXT', name: 'English CC', language: 'en' },
        ],
        [1],
      );
    });
    expect(await screen.findByLabelText('Audio')).toBeInTheDocument();
    expect(await screen.findByLabelText('Subtitles')).toBeInTheDocument();
  });

  it('selecting an audio track calls selectAudio via editTracksInfo', async () => {
    const user = userEvent.setup();
    await controller.start({ key: 'feed:bus:program', name: 'Program', feed: { bus: 'program' } });
    renderCard(OUTPUTS);
    act(() => {
      sdk.emitTracks(
        [
          { trackId: 1, type: 'AUDIO', name: 'English', language: 'en' },
          { trackId: 2, type: 'AUDIO', name: 'French', language: 'fr' },
        ],
        [1],
      );
    });
    const select = await screen.findByLabelText('Audio');
    await user.selectOptions(select, '2');
    await waitFor(() => {
      expect(sdk.editTracksInfo).toHaveBeenCalledWith(
        expect.objectContaining({ activeTrackIds: [2] }),
        expect.anything(),
        expect.anything(),
      );
    });
  });

  it('selecting "Off" for subtitles calls selectText(null)', async () => {
    const user = userEvent.setup();
    await controller.start({ key: 'feed:bus:program', name: 'Program', feed: { bus: 'program' } });
    renderCard(OUTPUTS);
    act(() => {
      sdk.emitTracks(
        [
          { trackId: 1, type: 'AUDIO', name: 'English', language: 'en' },
          { trackId: 2, type: 'AUDIO', name: 'French', language: 'fr' },
          { trackId: 10, type: 'TEXT', name: 'English CC', language: 'en' },
        ],
        [1, 10],
      );
    });
    const select = await screen.findByLabelText('Subtitles');
    await user.selectOptions(select, 'Off');
    await waitFor(() => {
      expect(sdk.editTracksInfo).toHaveBeenCalledWith(
        expect.objectContaining({ activeTrackIds: [1] }),
        expect.anything(),
        expect.anything(),
      );
    });
  });

  it('switching Video calls switchOutput: creates, LOADs, then deletes the old stream', async () => {
    const user = userEvent.setup();
    api = fakeApi();
    api.create.mockResolvedValueOnce({
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
    });
    controller = new CastController(api);
    controller.activate();
    controller.configure({ appId: '8E05190C', namespace: CAST_NAMESPACE });
    await controller.start({ key: 'feed:bus:program', name: 'Program', feed: { bus: 'program' } });

    const order: string[] = [];
    api.create.mockImplementationOnce(() => {
      order.push('create');
      return Promise.resolve({
        id: 'cs-2',
        output: 'cast-2',
        feed: {},
        path: '/cast/token-2/master.m3u8',
        content_type: 'application/vnd.apple.mpegurl',
        hls_segment_format: 'fmp4',
        hls_video_segment_format: 'fmp4',
        ready: true,
        segments: 12,
        expires_unix_ms: 0,
        fetches: { playlists: 0, segments: 0, cast_device: false },
      });
    });
    api.remove.mockImplementation((id: string) => {
      order.push(`remove:${id}`);
      return Promise.resolve();
    });
    const originalLoad = sdk.loadMedia.getMockImplementation();
    sdk.loadMedia.mockImplementation((...args: unknown[]) => {
      order.push('load');
      return originalLoad?.(...(args as [unknown])) ?? Promise.resolve();
    });

    renderCard(OUTPUTS);
    const select = await screen.findByLabelText('Video');
    await user.selectOptions(select, 'cast-2');
    await waitFor(() => {
      expect(order).toEqual(['create', 'load', 'remove:cs-1']);
    });
  });

  it('a refused video switch shows the refusal sentence', async () => {
    const user = userEvent.setup();
    await controller.start({ key: 'feed:bus:program', name: 'Program', feed: { bus: 'program' } });
    renderCard(OUTPUTS);
    api.create.mockRejectedValueOnce(refusal(409, 'cast-capacity', 'busy'));
    const select = await screen.findByLabelText('Video');
    await user.selectOptions(select, 'cast-2');
    expect(await screen.findByText(/Every cast output is in use/)).toBeInTheDocument();
  });
});
