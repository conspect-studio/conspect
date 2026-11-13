// Unit tests for the browser-cast state machine, with the Cast Web Sender SDK
// mocked (no network, no real script injection). PR-1 scope: casting an
// existing HLS output's own playlist URL directly — no backend stream
// endpoint.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { i18n } from '@lingui/core';

import { messages } from '../locales/en/messages';
import { CastController } from './castController';
import type { CastTarget } from './castController';
import { resetCastSdkLoaderForTests } from './sdkLoader';
import { installMockCastSdk } from './testMocks';
import type { MockCastSdk } from './testMocks';

const TARGET: CastTarget = { key: 'output:hls-main', name: 'Program', path: '/hls/main/index.m3u8' };

i18n.load('en', messages);
i18n.activate('en');

describe('CastController', () => {
  let sdk: MockCastSdk;

  beforeEach(() => {
    resetCastSdkLoaderForTests();
    sdk = installMockCastSdk();
  });

  afterEach(() => {
    delete window.cast;
    delete window.chrome;
    delete window.__onGCastApiAvailable;
  });

  it('goes idle → connecting → loading → playing and loads the right media', async () => {
    const controller = new CastController();
    const states: string[] = [];
    controller.subscribe(() => {
      states.push(controller.getSnapshot().status);
    });

    await controller.start(TARGET);

    expect(states).toEqual(['connecting', 'loading', 'playing']);
    expect(sdk.requestSession).toHaveBeenCalledTimes(1);
    expect(sdk.loadMedia).toHaveBeenCalledTimes(1);
    const request = sdk.loadMedia.mock.calls[0]?.[0] as { media: { contentId: string; contentType: string; streamType: string; hlsSegmentFormat: string; hlsVideoSegmentFormat: string } };
    expect(request.media.contentId).toBe(`${window.location.origin}/hls/main/index.m3u8`);
    expect(request.media.contentType).toBe('application/vnd.apple.mpegurl');
    expect(request.media.streamType).toBe('LIVE');
    expect(request.media.hlsSegmentFormat).toBe('TS');
    expect(request.media.hlsVideoSegmentFormat).toBe('MPEG2_TS');

    const snapshot = controller.getSnapshot();
    expect(snapshot.status).toBe('playing');
    expect(snapshot.deviceName).toBe('Living Room TV');
    expect(snapshot.targetKey).toBe(TARGET.key);
  });

  it('does not open a new session if one already exists', async () => {
    sdk.context.getCurrentSession = (): typeof sdk.session => sdk.session;
    const controller = new CastController();
    await controller.start(TARGET);
    expect(sdk.requestSession).not.toHaveBeenCalled();
    expect(controller.getSnapshot().status).toBe('playing');
  });

  it('returns to idle with no error when the user cancels the device picker', async () => {
    sdk.context.getCurrentSession = (): null => null;
    // The CAF sender rejects with the bare chrome.cast.ErrorCode string.
    // eslint-disable-next-line @typescript-eslint/prefer-promise-reject-errors
    sdk.requestSession.mockImplementationOnce(() => Promise.reject('cancel'));
    const controller = new CastController();

    await controller.start(TARGET);

    const snapshot = controller.getSnapshot();
    expect(snapshot.status).toBe('idle');
    expect(snapshot.errorText).toBeNull();
    expect(sdk.loadMedia).not.toHaveBeenCalled();
  });

  it('surfaces a mapped sentence for a receiver_unavailable Cast error', async () => {
    sdk.context.getCurrentSession = (): null => null;
    sdk.requestSession.mockImplementationOnce(() => Promise.reject(Object.assign(new Error('receiver_unavailable'), { code: 'receiver_unavailable' })));
    const controller = new CastController();

    await controller.start(TARGET);

    const snapshot = controller.getSnapshot();
    expect(snapshot.status).toBe('error');
    expect(snapshot.errorText).toBe('No Cast receiver is available on this network.');
  });

  it('stop ends the session and returns to idle', async () => {
    const controller = new CastController();
    await controller.start(TARGET);
    expect(controller.getSnapshot().status).toBe('playing');

    await controller.stop();

    expect(sdk.endSession).toHaveBeenCalledWith(true);
    const snapshot = controller.getSnapshot();
    expect(snapshot.status).toBe('idle');
    expect(snapshot.targetKey).toBeNull();
  });

  it('tears down to idle when the Cast session ends on its own (no error code)', async () => {
    const controller = new CastController();
    await controller.start(TARGET);

    sdk.emitSessionEnded();

    expect(controller.getSnapshot().status).toBe('idle');
  });

  it('tears down to an error state when the Cast session ends with an error code', async () => {
    const controller = new CastController();
    await controller.start(TARGET);

    sdk.emitSessionEnded('session_error');

    const snapshot = controller.getSnapshot();
    expect(snapshot.status).toBe('error');
    expect(snapshot.errorText).toBe('Cast session error.');
  });

  it('stops casting when the media goes IDLE/FINISHED', async () => {
    const controller = new CastController();
    await controller.start(TARGET);

    sdk.emitMediaUpdate({ playerState: 'IDLE', idleReason: 'FINISHED' });
    // The IDLE/FINISHED handler calls stop() asynchronously; flush microtasks.
    await vi.waitFor(() => {
      expect(controller.getSnapshot().status).toBe('idle');
    });
  });

  it('shows an error when the receiver cannot play the stream (IDLE/ERROR)', async () => {
    const controller = new CastController();
    await controller.start(TARGET);

    sdk.emitMediaUpdate({ playerState: 'IDLE', idleReason: 'ERROR' });
    await vi.waitFor(() => {
      expect(controller.getSnapshot().status).toBe('error');
    });
    expect(controller.getSnapshot().errorText).toMatch(/could not play this stream/);
    expect(sdk.endSession).toHaveBeenCalledWith(true);
  });

  it('replaces the loaded media in the same session when starting a different target while playing', async () => {
    const controller = new CastController();
    await controller.start(TARGET);
    expect(sdk.requestSession).toHaveBeenCalledTimes(1);

    const other: CastTarget = { key: 'output:hls-alt', name: 'Alt', path: '/hls/alt/index.m3u8' };
    await controller.start(other);

    // Same session reused — requestSession not called again.
    expect(sdk.requestSession).toHaveBeenCalledTimes(1);
    expect(sdk.loadMedia).toHaveBeenCalledTimes(2);
    const snapshot = controller.getSnapshot();
    expect(snapshot.status).toBe('playing');
    expect(snapshot.targetKey).toBe(other.key);
  });
});
