// The browser-cast state machine against a mocked CAF sender (REAL CAF call
// shapes, see testMocks.ts) and a fake cast-stream API. No network.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { i18n } from '@lingui/core';

import { messages } from '../locales/en/messages';
import { CastController, TV_UNREACHABLE_MS } from './castController';
import type { CastTarget } from './castController';
import { CAST_NAMESPACE } from './castReport';
import { resetCastSdkLoaderForTests } from './sdkLoader';
import { fakeApi, installMemoryStorage, refusal, streamDoc, TOKEN_PATH } from './testApi';
import type { FakeApi } from './testApi';
import { installMockCastSdk } from './testMocks';
import type { MockCastSdk } from './testMocks';

i18n.load('en', messages);
i18n.activate('en');

const PROGRAM: CastTarget = { key: 'feed:bus:program', name: 'Program', feed: { bus: 'program' } };
const APP_ID = '8E05190C';

interface LoadArg {
  media: {
    contentId: string;
    contentUrl?: string;
    contentType: string;
    streamType: string;
    hlsSegmentFormat?: string;
    hlsVideoSegmentFormat?: string;
    customData?: Record<string, unknown>;
    metadata?: { title?: string };
  };
}

function loadArg(sdk: MockCastSdk, call = 0): LoadArg {
  return sdk.loadMedia.mock.calls[call]?.[0] as LoadArg;
}

async function flush(): Promise<void> {
  await vi.advanceTimersByTimeAsync(0);
}

describe('CastController', () => {
  let sdk: MockCastSdk;
  let api: FakeApi;
  let controller: CastController;

  function make(appId: string | null = APP_ID, theApi: FakeApi = api): CastController {
    controller = new CastController(theApi, () => Date.now());
    controller.activate();
    controller.configure({ appId, namespace: CAST_NAMESPACE });
    return controller;
  }

  beforeEach(() => {
    vi.useFakeTimers();
    installMemoryStorage();
    resetCastSdkLoaderForTests();
    sdk = installMockCastSdk();
    api = fakeApi();
  });

  afterEach(() => {
    controller.dispose();
    vi.useRealTimers();
    delete window.cast;
    delete window.chrome;
    delete window.__onGCastApiAvailable;
  });

  it('creates the stream, opens the picker, then LOADs the absolute token URL on the Conspect receiver', async () => {
    make();
    const states: string[] = [];
    controller.subscribe(() => {
      const status = controller.getSnapshot().status;
      if (states.at(-1) !== status) states.push(status);
    });
    await controller.start(PROGRAM);

    expect(sdk.context.setOptions).toHaveBeenCalledWith({ receiverApplicationId: APP_ID, autoJoinPolicy: 'origin_scoped' });
    expect(api.create).toHaveBeenCalledWith({ feed: { bus: 'program' } });
    expect(api.create.mock.invocationCallOrder[0]).toBeLessThan(sdk.requestSession.mock.invocationCallOrder[0] ?? 0);
    expect(states).toEqual(['preparing', 'connecting', 'loading', 'buffering']);
    const media = loadArg(sdk).media;
    const url = `${window.location.origin}${TOKEN_PATH}`;
    expect(media.contentId).toBe(url);
    expect(media.contentUrl).toBe(url);
    expect(media.contentType).toBe('application/vnd.apple.mpegurl');
    expect(media.streamType).toBe('LIVE');
    expect(media.hlsSegmentFormat).toBe('fmp4');
    expect(media.hlsVideoSegmentFormat).toBe('fmp4');
    expect(media.customData).toEqual({ title: 'Program' });
    expect(media.metadata?.title).toBe('Program');
    const snap = controller.getSnapshot();
    expect(snap.receiver).toBe('conspect');
    expect(snap.deviceName).toBe('Living Room TV');
    expect(snap.streamId).toBe('cs-1');
    expect(sdk.sendMessage).toHaveBeenCalledWith(CAST_NAMESPACE, { type: 'ping' });
  });

  it('opens the picker before the stream is ready, and LOADs only once both are done', async () => {
    let ready = false;
    api = fakeApi(streamDoc({ ready: false, segments: 1 }), () => streamDoc({ ready, segments: ready ? 6 : 2 }));
    make();
    const started = controller.start(PROGRAM);
    await flush();
    expect(sdk.requestSession).toHaveBeenCalledTimes(1);
    expect(sdk.loadMedia).not.toHaveBeenCalled();
    expect(controller.getSnapshot().status).toBe('connecting');
    ready = true;
    await vi.advanceTimersByTimeAsync(600);
    await started;
    expect(sdk.loadMedia).toHaveBeenCalledTimes(1);
  });

  it("falls back to Google's Default Media Receiver and waits for 8 segments", async () => {
    let segments = 5;
    api = fakeApi(streamDoc({ segments }), () => streamDoc({ segments }));
    make(null);
    const started = controller.start(PROGRAM);
    await vi.advanceTimersByTimeAsync(600);
    expect(sdk.loadMedia).not.toHaveBeenCalled();
    segments = 8;
    await vi.advanceTimersByTimeAsync(600);
    await started;
    expect(sdk.context.setOptions).toHaveBeenCalledWith({ receiverApplicationId: 'CC1AD845', autoJoinPolicy: 'origin_scoped' });
    expect(controller.getSnapshot().receiver).toBe('default');
    expect(loadArg(sdk).media.contentId).toBe(`${window.location.origin}${TOKEN_PATH}`);
  });

  it('deletes the stream when the picker is cancelled and returns to idle', async () => {
    // eslint-disable-next-line @typescript-eslint/prefer-promise-reject-errors -- CAF rejects with the bare code string
    sdk.requestSession.mockImplementationOnce(() => Promise.reject('cancel'));
    make();
    await controller.start(PROGRAM);
    expect(api.remove).toHaveBeenCalledWith('cs-1');
    expect(controller.getSnapshot()).toMatchObject({ status: 'idle', errorText: null });
    expect(sdk.loadMedia).not.toHaveBeenCalled();
  });

  it.each([
    ['cast-capacity', 409, 'Every cast output is in use'],
    ['cast-unavailable', 409, 'no usable cast output'],
    ['cast-no-encoder', 409, 'No free encoder'],
    ['switcher-required', 409, 'need a switcher'],
  ])('shows the %s refusal and never opens the picker', async (reason, status, sentence) => {
    api.create.mockRejectedValueOnce(refusal(status, reason, 'server detail'));
    make();
    await controller.start(PROGRAM);
    expect(sdk.requestSession).not.toHaveBeenCalled();
    const snap = controller.getSnapshot();
    expect(snap.status).toBe('error');
    expect(snap.errorText).toContain(sentence);
    expect(snap.errorDetail).toBe('server detail');
  });

  it('gives up after 15 s when the stream never becomes ready', async () => {
    api = fakeApi(streamDoc({ ready: false, segments: 0 }));
    make();
    const started = controller.start(PROGRAM);
    await vi.advanceTimersByTimeAsync(16_000);
    await started;
    const snap = controller.getSnapshot();
    expect(snap.status).toBe('error');
    expect(snap.errorText).toContain('15 seconds');
    expect(api.remove).toHaveBeenCalledWith('cs-1');
    expect(sdk.endSession).toHaveBeenCalledWith(true);
    expect(sdk.loadMedia).not.toHaveBeenCalled();
  });

  it('follows the receiver report: buffering, playing with resolution/latency, error with codes', async () => {
    make();
    await controller.start(PROGRAM);
    sdk.emitMessage(CAST_NAMESPACE, { v: 1, state: 'buffering', error: null, stats: null });
    expect(controller.getSnapshot().status).toBe('buffering');
    sdk.emitMessage(CAST_NAMESPACE, { v: 1, state: 'playing', error: null, stats: { width: 1920, height: 1080, latency_s: 7.2 } });
    expect(controller.getSnapshot()).toMatchObject({ status: 'playing', report: { width: 1920, height: 1080, latencyS: 7.2 } });
    sdk.emitMessage(CAST_NAMESPACE, {
      v: 1,
      state: 'error',
      error: { code: 905, detailed: 905, reason: 'INVALID_REQUEST', shaka_code: null, message: 'This TV cannot decode H.264 High 5.1.' },
      stats: null,
    });
    const snap = controller.getSnapshot();
    expect(snap.status).toBe('error');
    expect(snap.errorText).toBe('This TV cannot decode H.264 High 5.1.');
    expect(snap.errorDetail).toBe('code 905 · INVALID_REQUEST');
  });

  it('ignores a malformed custom message', async () => {
    make();
    await controller.start(PROGRAM);
    sdk.emitMessage(CAST_NAMESPACE, '{not json');
    sdk.emitMessage(CAST_NAMESPACE, { state: 'exploded' });
    expect(controller.getSnapshot().status).toBe('buffering');
  });

  it('uses media status and the polled stream report with the default receiver', async () => {
    let doc = streamDoc();
    api = fakeApi(doc, () => doc);
    make(null);
    await controller.start(PROGRAM);
    sdk.emitMediaUpdate({ playerState: 'PLAYING' });
    expect(controller.getSnapshot().status).toBe('playing');
    sdk.emitMediaUpdate({ playerState: 'IDLE', idleReason: 'ERROR' });
    expect(controller.getSnapshot().status).toBe('error');
    doc = streamDoc({ last_report: { received_unix_ms: 5, report: { state: 'playing', stats: { width: 1280, height: 720 } } as unknown as Record<string, never> } });
    await vi.advanceTimersByTimeAsync(2_100);
    expect(controller.getSnapshot()).toMatchObject({ status: 'playing', report: { width: 1280 } });
  });

  it('says the TV has not reached this server after 10 s of LOAD with no playlist fetch', async () => {
    make();
    await controller.start(PROGRAM);
    await vi.advanceTimersByTimeAsync(TV_UNREACHABLE_MS - 2_500);
    expect(controller.getSnapshot().tvUnreachable).toBe(false);
    await vi.advanceTimersByTimeAsync(4_000);
    expect(controller.getSnapshot().tvUnreachable).toBe(true);
  });

  it('does not raise the hint once the TV fetched a playlist', async () => {
    api = fakeApi(streamDoc(), () => streamDoc({ fetches: { playlists: 3, segments: 9, cast_device: true } }));
    make();
    await controller.start(PROGRAM);
    await vi.advanceTimersByTimeAsync(14_000);
    expect(controller.getSnapshot().tvUnreachable).toBe(false);
  });

  it('stop ends the Cast session, deletes the stream and detaches every listener', async () => {
    make();
    await controller.start(PROGRAM);
    expect(sdk.messageListenerCount()).toBe(1);
    await controller.stop();
    expect(sdk.endSession).toHaveBeenCalledWith(true);
    expect(api.remove).toHaveBeenCalledWith('cs-1');
    expect(controller.getSnapshot().status).toBe('idle');
    expect(sdk.messageListenerCount()).toBe(0);
    expect(sdk.hasSessionStateHandler()).toBe(false);
  });

  it('re-sends an unanswered Stop DELETE with keepalive on pagehide', async () => {
    make();
    await controller.start(PROGRAM);
    api.remove.mockImplementationOnce(() => new Promise<void>(() => undefined));
    void controller.stop();
    await flush();
    window.dispatchEvent(new Event('pagehide'));
    expect(api.remove).toHaveBeenLastCalledWith('cs-1', { keepalive: true });
  });

  it('a closed tab (pagehide without Stop) deletes nothing', async () => {
    make();
    await controller.start(PROGRAM);
    window.dispatchEvent(new Event('pagehide'));
    expect(api.remove).not.toHaveBeenCalled();
  });

  it('a TV-side end (SESSION_ENDED, errorCode null) deletes the stream and goes idle', async () => {
    make();
    await controller.start(PROGRAM);
    sdk.emitSessionEnded();
    expect(controller.getSnapshot().status).toBe('idle');
    expect(api.remove).toHaveBeenCalledWith('cs-1');
  });

  it('sets the receiver volume and mute and follows VOLUME_CHANGED', async () => {
    make();
    await controller.start(PROGRAM);
    expect(controller.getSnapshot().volume).toBe(0.5);
    await controller.setVolume(0.3);
    expect(sdk.setVolume).toHaveBeenCalledWith(0.3);
    await controller.setMuted(true);
    expect(sdk.setMute).toHaveBeenCalledWith(true);
    sdk.emitVolume(0.8, false);
    expect(controller.getSnapshot()).toMatchObject({ volume: 0.8, muted: false });
  });

  describe('dispose during every async step leaves idle with no leaked listener and no LOAD', () => {
    it('while the stream is being created', async () => {
      let resolve: (value: ReturnType<typeof streamDoc>) => void = () => undefined;
      api.create.mockImplementationOnce(() => new Promise((r) => { resolve = r; }));
      make();
      const started = controller.start(PROGRAM);
      await flush();
      controller.dispose();
      resolve(streamDoc());
      await started;
      expect(controller.getSnapshot().status).toBe('idle');
      expect(sdk.requestSession).not.toHaveBeenCalled();
      expect(api.remove).toHaveBeenCalledWith('cs-1');
    });

    it('while the picker is open', async () => {
      let pick: () => void = () => undefined;
      sdk.requestSession.mockImplementationOnce(() => new Promise<void>((r) => { pick = r; }));
      make();
      const started = controller.start(PROGRAM);
      await flush();
      controller.dispose();
      pick();
      await started;
      expect(controller.getSnapshot().status).toBe('idle');
      expect(sdk.loadMedia).not.toHaveBeenCalled();
      expect(api.remove).toHaveBeenCalledWith('cs-1');
    });

    it('while waiting for ready', async () => {
      api = fakeApi(streamDoc({ ready: false }));
      make();
      const started = controller.start(PROGRAM);
      await flush();
      controller.dispose();
      await vi.advanceTimersByTimeAsync(1_000);
      await started;
      expect(sdk.loadMedia).not.toHaveBeenCalled();
      expect(controller.getSnapshot().status).toBe('idle');
    });

    it('while the LOAD is in flight', async () => {
      let loaded: () => void = () => undefined;
      sdk.loadMedia.mockImplementationOnce(() => new Promise<void>((r) => { loaded = r; }));
      make();
      const started = controller.start(PROGRAM);
      await flush();
      controller.dispose();
      loaded();
      await started;
      expect(controller.getSnapshot().status).toBe('idle');
      expect(sdk.messageListenerCount()).toBe(0);
      expect(sdk.hasSessionStateHandler()).toBe(false);
    });

    it('while playing: the cast keeps playing (no endSession, no DELETE), listeners gone, re-armable', async () => {
      make();
      await controller.start(PROGRAM);
      controller.dispose();
      expect(sdk.endSession).not.toHaveBeenCalled();
      expect(api.remove).not.toHaveBeenCalled();
      expect(sdk.messageListenerCount()).toBe(0);
      controller.activate();
      await flush();
      // StrictMode re-mount: the same controller re-attaches to the cast.
      expect(controller.getSnapshot()).toMatchObject({ status: 'buffering', streamId: 'cs-1' });
      expect(sdk.messageListenerCount()).toBe(1);
    });
  });

  it('re-attaches after a reload to the tab session and its listed stream', async () => {
    make();
    await controller.start(PROGRAM);
    controller.dispose();
    // A new page: a new controller, the SDK auto-joined the session.
    sdk.context.getCurrentSession = (): typeof sdk.session => sdk.session;
    resetCastSdkLoaderForTests();
    make();
    await flush();
    expect(api.list).toHaveBeenCalled();
    expect(controller.getSnapshot()).toMatchObject({ status: 'buffering', streamId: 'cs-1', targetName: 'Program' });
    sdk.emitMessage(CAST_NAMESPACE, { state: 'playing', stats: null });
    expect(controller.getSnapshot().status).toBe('playing');
  });

  it('does not re-attach when the stream is no longer listed', async () => {
    make();
    await controller.start(PROGRAM);
    controller.dispose();
    sdk.context.getCurrentSession = (): typeof sdk.session => sdk.session;
    const empty = fakeApi();
    empty.list.mockResolvedValue([]);
    make(APP_ID, empty);
    await flush();
    expect(controller.getSnapshot().status).toBe('idle');
  });
});
