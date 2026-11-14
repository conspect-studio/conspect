// Every 202 command POST — including those with a JSON body — sends an
// Idempotency-Key minted by the shared submit helper (SW-19, #45).
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';

import { cueMediaPlayer, loadMediaPlayer, loadMediaPlayerArtifact } from './media-players';
import { setCastVolume } from './cast-sessions';
import { setOverride } from './tally';
import { takeRoute } from './routing';
import { applyLayoutCommand } from '../layout/applyLayout';
import { setDeviceMode } from '../devices/api';
import { cancelCapture, createCapture } from './web-capture';

const keys = new Map<string, string | null>();

function accept(name: string, extra: Record<string, unknown> = {}) {
  return ({ request }: { request: Request }) => {
    keys.set(name, request.headers.get('idempotency-key'));
    return HttpResponse.json({ operation_id: `op-${name}`, kind: 'x', ...extra }, { status: 202 });
  };
}

const server = setupServer(
  http.post('*/api/v1/media/players/p1/load', accept('load')),
  http.post('*/api/v1/media/players/p1/cue', accept('cue')),
  http.post('*/api/v1/cast/sessions/c1/volume', accept('volume')),
  http.put('*/api/v1/tally/override', accept('tally')),
  http.post('*/api/v1/routing/video/take', accept('take')),
  http.post('*/api/v1/commands/apply-layout', accept('layout')),
  http.post('*/api/v1/devices/d1/set-mode', accept('mode', { impact: 'dev', detail: 'd' })),
  http.post('*/api/v1/media/captures', ({ request }) => {
    keys.set('capture', request.headers.get('idempotency-key'));
    return HttpResponse.json({ job_id: 1, status_url: '/x' }, { status: 202 });
  }),
  http.post('*/api/v1/media/captures/1/cancel', ({ request }) => {
    keys.set('capture-cancel', request.headers.get('idempotency-key'));
    return new HttpResponse(null, { status: 202 });
  }),
);

beforeAll(() => {
  server.listen();
});
afterEach(() => {
  keys.clear();
});
afterAll(() => {
  server.close();
});

describe('Idempotency-Key on JSON-body commands', () => {
  it('Load with default options sends an Idempotency-Key', async () => {
    await loadMediaPlayer('p1', 'asset-1');
    expect(keys.get('load')).toBeTruthy();
  });

  it('Cue, cast volume, tally override, route take, apply-layout, set-mode send one too', async () => {
    await cueMediaPlayer('p1', undefined);
    await setCastVolume('c1', { level_percent: 50 });
    await setOverride({ kind: 'tile', index: 0 }, 'Red');
    await takeRoute('video', { source: { input_id: 'i', kind: { kind: 'video' } }, target: { kind: 'video_cell', cell: 'c' } });
    await applyLayoutCommand('l1');
    await setDeviceMode('d1', 'm');
    for (const name of ['cue', 'volume', 'tally', 'take', 'layout', 'mode']) {
      expect(keys.get(name), name).toBeTruthy();
    }
  });

  it('keeps an explicit key', async () => {
    await loadMediaPlayer('p1', 'a', { idempotencyKey: 'mine' });
    expect(keys.get('load')).toBe('mine');
  });

  it('web-capture create/cancel and the artifactRef load send one too', async () => {
    await createCapture({
      fill_id: 'vt-p1-windy',
      target_player: 'p1',
      view: { lat: 0, lon: 0, zoom: 1, overlay: 'radar', width: 1920, height: 1080, fps: 10, frames: 60, public: true },
      terms: { accepted: true, version: 'windy-2023-09-01' },
    });
    expect(keys.get('capture')).toBeTruthy();
    await cancelCapture(1);
    expect(keys.get('capture-cancel')).toBeTruthy();
    await loadMediaPlayerArtifact('p1', { captureJobId: 1, artifactId: 'a1' }, 'resume');
    expect(keys.get('load')).toBeTruthy();
  });
});
