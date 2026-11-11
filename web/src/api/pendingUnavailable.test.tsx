// Capability honesty: main does not serve the pending routes, so a 404/405/501
// must surface as a typed "unavailable (backend not deployed)" state - never a
// generic failure, never success.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { renderHook, waitFor } from '@testing-library/react';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';

import { createApiClient } from './client';
import { BackendUnavailableError, isBackendUnavailable } from './operations';
import { getSwitcher, planSwitcher, commandSwitcher } from './switcher';
import { getSourceStatus, postSourceAction } from './sourceStatus';
import { listChannels, listItems } from './graphics';
import { listWeatherLocations, deleteWeatherLocation } from './weather';
import { getIptv, importM3u } from './iptv';
import { fetchStatus } from './catalogue';
import { useEventJournal, useAutomation } from '../events/sourceEvents';
import { useOutputPlayback } from '../resources/outputPlayback';
import { getOutputLink } from '../resources/api';
import { TestProviders } from '../test/render';

const server = setupServer();
beforeAll(() => { server.listen({ onUnhandledRequest: 'error' }); });
afterEach(() => { server.resetHandlers(); });
afterAll(() => { server.close(); });

const routeMissing = () => new HttpResponse(null, { status: 404 });
const BASE = { baseUrl: 'http://localhost' };

async function rejection(p: Promise<unknown>): Promise<unknown> {
  try { await p; } catch (e) { return e; }
  throw new Error('expected rejection');
}

describe.each([404, 405, 501])('HTTP %i from main = unavailable (backend not deployed)', (status) => {
  const respond = () => new HttpResponse(null, { status });
  it('switcher state read, plan and command', async () => {
    server.use(
      http.get('*/api/v1/switcher/me1', respond),
      http.post('*/api/v1/switcher/plan', respond),
      http.post('*/api/v1/switcher/commands/cut', respond),
    );
    const req: Parameters<typeof planSwitcher>[0] = { me: 'me1', program: 'main', action: 'cut' };
    for (const p of [getSwitcher(BASE), planSwitcher(req, BASE), commandSwitcher('cut', req, BASE)]) {
      const e = await rejection(p);
      expect(e).toBeInstanceOf(BackendUnavailableError);
      expect((e as BackendUnavailableError).status).toBe(status);
    }
  });
  it('source status and start/stop', async () => {
    server.use(http.get('*/api/v1/sources/status', respond), http.post('*/api/v1/sources/x/start', respond));
    expect(isBackendUnavailable(await rejection(getSourceStatus(BASE)))).toBe(true);
    expect(isBackendUnavailable(await rejection(postSourceAction('x', 'start', BASE)))).toBe(true);
  });
  it('graphics channels and items', async () => {
    server.use(http.get('*/api/v1/graphics/channels', respond), http.get('*/api/v1/graphics/items', respond));
    expect(isBackendUnavailable(await rejection(listChannels()))).toBe(true);
    expect(isBackendUnavailable(await rejection(listItems()))).toBe(true);
  });
  it('weather list; delete is NOT treated as already gone', async () => {
    server.use(
      http.get('*/api/v1/weather/locations', respond),
      http.delete('*/api/v1/weather/locations/syd', respond),
    );
    expect(isBackendUnavailable(await rejection(listWeatherLocations(BASE)))).toBe(true);
    expect(isBackendUnavailable(await rejection(deleteWeatherLocation('syd', BASE)))).toBe(true);
  });
  it('IPTV, M3U import and catalogue', async () => {
    server.use(
      http.get('*/api/v1/iptv', respond),
      http.post('*/api/v1/sources/import/m3u', respond),
      http.get('*/api/v1/catalogues/iptv-org/status', respond),
    );
    expect(isBackendUnavailable(await rejection(getIptv(BASE)))).toBe(true);
    expect(isBackendUnavailable(await rejection(importM3u({ text: '#EXTM3U' }, BASE)))).toBe(true);
    expect(isBackendUnavailable(await rejection(fetchStatus(BASE)))).toBe(true);
  });
  it('event journal and automation (typed client)', async () => {
    server.use(
      http.get('*/api/v1/inputs/s1/events/journal', respond),
      http.get('*/api/v1/inputs/s1/events/automation', respond),
    );
    const j = renderHook(() => useEventJournal('s1', { origin: '', from: '', to: '' }), { wrapper: TestProviders });
    await waitFor(() => { expect(j.result.current.isError).toBe(true); });
    expect(isBackendUnavailable(j.result.current.error)).toBe(true);
    const a = renderHook(() => useAutomation('s1'), { wrapper: TestProviders });
    await waitFor(() => { expect(a.result.current.isError).toBe(true); });
    expect(isBackendUnavailable(a.result.current.error)).toBe(true);
  });
  it('output playback resolves to unavailable, not an empty success', async () => {
    server.use(http.get('*/api/v1/outputs/o1/playback', respond));
    const h = renderHook(() => useOutputPlayback('o1'), { wrapper: TestProviders });
    await waitFor(() => { expect(h.result.current.isSuccess).toBe(true); });
    expect(h.result.current.data).toEqual({ urls: [], unavailable: true });
  });
  it('output link: route missing throws unavailable (not "no link")', async () => {
    server.use(http.get('*/api/v1/outputs/o1/link', respond));
    expect(isBackendUnavailable(await rejection(getOutputLink('o1', BASE)))).toBe(true);
  });
});

describe('a deployed backend still distinguishes resource 404s and real failures', () => {
  it('output link problem-body 404 is "no link" (null)', async () => {
    server.use(http.get('*/api/v1/outputs/o1/link', () => HttpResponse.json({ title: 'not found', status: 404 }, { status: 404 })));
    expect(await getOutputLink('o1', BASE)).toBeNull();
  });
  it('a 500 stays a generic failure', async () => {
    server.use(http.get('*/api/v1/switcher/me1', () => HttpResponse.json({ title: 'boom', status: 500 }, { status: 500 })));
    const e = await rejection(getSwitcher(BASE));
    expect(isBackendUnavailable(e)).toBe(false);
    expect((e as { status: number }).status).toBe(500);
  });
  it('typed client reaches pending paths (AllPaths)', async () => {
    server.use(http.get('*/api/v1/switcher/me1', routeMissing));
    const { response } = await createApiClient({ baseUrl: 'http://localhost' }).GET('/api/v1/switcher/{me}', { params: { path: { me: 'me1' } } });
    expect(response.status).toBe(404);
  });
});
