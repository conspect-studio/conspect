// Every 202 command POST — including those with a JSON body — sends an
// Idempotency-Key minted by the shared submit helper (SW-19, #45).
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';

import { cueMediaPlayer, loadMediaPlayer } from './media-players';
import { setCastVolume } from './cast-sessions';
import { setOverride } from './tally';
import { takeRoute } from './routing';
import { applyLayoutCommand } from '../layout/applyLayout';
import { setDeviceMode } from '../devices/api';

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
});
