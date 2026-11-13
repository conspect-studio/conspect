// A wall tile with no picture must state why (issue #300): a failed source shows
// a no-signal slate carrying the engine's reason, a starting source says
// "Starting", a live cell and a normal still are unchanged, and the reason is
// plain text — a detail containing markup shows literally, never as HTML.
//
// No test here touches the network for stills: `useJpegStill` is stubbed per
// path, and the small number of JSON reads go through MSW. Every test unmounts
// (cleanup) before its `server.resetHandlers`, so no poll outlives its stub.
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('../../realtime/shared', () => ({ subscribeRealtime: () => () => undefined }));
vi.mock('../../realtime/useEngineEvents', () => ({ useEngineEvents: () => ({ status: 'open' }), TILES_QUERY_KEY: ['tiles'] }));
vi.mock('../../preview/PreviewSurface', () => ({ PreviewSurface: () => null }));

/** Stills are stubbed per path; the mock records whether polling was enabled. */
const stills = vi.hoisted(() => ({
  byPath: new Map<string, { url: string | undefined; state: string }>(),
  enabled: [] as { path: string; enabled: boolean }[],
}));
vi.mock('../../preview/useJpegPreview', () => ({
  useJpegPreview: () => undefined,
  useJpegStill: (path: string, _refreshMs: number, enabled = true) => {
    stills.enabled.push({ path, enabled });
    return stills.byPath.get(path) ?? { url: undefined, state: 'loading' };
  },
}));

/** Only these source ids have a live wall cell. */
const wall = vi.hoisted(() => ({ live: new Set<string>() }));
vi.mock('../../preview/WallStream', () => ({
  WallStreamProvider: ({ children }: { readonly children: unknown }) => children,
  WallCrop: ({ source }: { readonly source: string }) => <canvas className="bd-source-picture" data-testid="wall-crop" data-source={source} />,
  useWallStatus: () => ({ live: false, reason: undefined, transport: 'stills' }),
  useWallSurface: (source?: string | null) => (wall.live.has(source ?? '')
    ? { live: true, cell: { x: 0, y: 0, w: 1, h: 1 }, previewSource: null }
    : { live: false, cell: undefined, previewSource: null }),
}));

import { cleanup, render, screen } from '@testing-library/react';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { i18n } from '@lingui/core';
import { I18nProvider } from '@lingui/react';
import { MemoryRouter } from 'react-router-dom';
import { messages } from '../../locales/en/messages';
import { CastProvider } from '../../cast';
import { ProductionDesk } from './ProductionDesk';
import type { SourceDemand } from '../../api/sourceStatus';

i18n.load('en', messages);
i18n.activate('en');

const src = (id: string, name: string) => ({ id, name, body: { kind: 'srt', url: 'srt://[::1]:1' } });
const demand = (id: string, phase: SourceDemand['phase'], extra: Partial<SourceDemand> = {}): SourceDemand => ({
  id,
  phase,
  video_wanted: true,
  audio_wanted: false,
  video_running: false,
  audio_running: false,
  consumers: [],
  activation: 'on_demand',
  ...extra,
});

const server = setupServer(
  http.get('*/api/v1/switcher/me1', () => HttpResponse.json({ program_source: 'cam-a', preview_source: 'cam-b', tick: 7, in_flight: false })),
  http.get('*/api/v1/layouts', () => HttpResponse.json([])),
  http.get('*/api/v1/alarms', () => HttpResponse.json([])),
  http.get('*/api/v1/preview/capabilities', () => HttpResponse.json({ webrtc: false })),
  http.get('*/api/v1/sources', () => HttpResponse.json([src('cam-a', 'CAM A'), src('cam-b', 'CAM B'), src('cam-c', 'CAM C'), src('cam-d', 'CAM D')])),
  http.get('*/api/v1/sources/status', () => HttpResponse.json([])),
);
beforeAll(() => { server.listen(); });
beforeEach(() => { stills.byPath.clear(); stills.enabled = []; wall.live.clear(); });
afterEach(() => {
  // Unmount first: a live poll must never outlive the stubs it answers from.
  cleanup();
  server.resetHandlers();
});
afterAll(() => { server.close(); });

function page() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  client.setQueryData(['tiles'], {});
  return render(<QueryClientProvider client={client}><I18nProvider i18n={i18n}><MemoryRouter><CastProvider><ProductionDesk /></CastProvider></MemoryRouter></I18nProvider></QueryClientProvider>);
}

/** The wall tile whose source-name is `name`. */
function tile(container: HTMLElement, name: string): HTMLElement {
  return [...container.querySelectorAll<HTMLElement>('.bd-wall-tile')].find((e) => e.querySelector('b')?.textContent === name) ?? document.body;
}

const stillPath = (id: string): string => `/api/v1/preview/inputs/${id}.jpg`;

describe('desk wall tile states', () => {
  it('states a failed source with a no-signal slate and the engine reason as literal text', async () => {
    server.use(http.get('*/api/v1/sources/status', () => HttpResponse.json([
      // A bidi override and a zero-width space must not survive into the tile.
      demand('cam-c', 'failed', { detail: 'no media yet; still retrying — <b>not markup</b>\u202E\u200B' }),
    ])));
    const { container } = page();
    const failed = await screen.findByTestId('tile-failed');
    expect(failed).toHaveTextContent('No signal');
    expect(tile(container, 'CAM C')).toContainElement(failed);
    const detail = await screen.findByTestId('tile-failed-detail');
    expect(detail).toHaveTextContent('no media yet; still retrying — <b>not markup</b>');
    // The reason is a text node: the markup is shown, not parsed.
    expect(detail.querySelector('b')).toBeNull();
    expect(detail.textContent).not.toContain('\u202E');
    expect(detail.textContent).not.toContain('\u200B');
    expect(container.querySelectorAll('.bd-wall-tile b')).toHaveLength(4);
    // A failed source keeps its still poll: that poll holds the 5 s preview
    // demand, and dropping it would lapse the lease and flip the tile to idle.
    expect(stills.enabled.filter((c) => c.path === stillPath('cam-c')).some((c) => c.enabled)).toBe(true);
  });

  it('bounds a long source reason without splitting a character', async () => {
    // 159 ASCII characters put an astral character exactly on the 160-char cut.
    const long = `${'a'.repeat(159)}😀${'b'.repeat(50)}`;
    server.use(http.get('*/api/v1/sources/status', () => HttpResponse.json([
      demand('cam-c', 'failed', { detail: long }),
    ])));
    page();
    const detail = await screen.findByTestId('tile-failed-detail');
    const text = detail.textContent;
    expect(text).toBe(`${'a'.repeat(159)}😀…`);
    expect(Array.from(text).length).toBeLessThanOrEqual(161);
  });

  it('says Starting for a starting source and for an idle source something wants', async () => {
    server.use(http.get('*/api/v1/sources/status', () => HttpResponse.json([
      demand('cam-c', 'starting'),
      demand('cam-d', 'idle', { video_wanted: true }),
    ])));
    const { container } = page();
    await vi.waitFor(() => { expect(screen.getAllByTestId('tile-starting')).toHaveLength(2); });
    expect(tile(container, 'CAM C')).toHaveTextContent('Starting');
    expect(tile(container, 'CAM D')).toHaveTextContent('Starting');
  });

  it('draws the last good frame under the Starting mark when one exists', async () => {
    server.use(http.get('*/api/v1/sources/status', () => HttpResponse.json([demand('cam-c', 'starting')])));
    stills.byPath.set(stillPath('cam-c'), { url: 'blob:last-good', state: 'stale' });
    const { container } = page();
    const overlay = await screen.findByTestId('tile-starting');
    expect(overlay.tagName).toBe('EM');
    expect(overlay).toHaveTextContent('Starting');
    expect(tile(container, 'CAM C').querySelector('img')).toHaveAttribute('src', 'blob:last-good');
  });

  it('leaves a live wall cell alone', async () => {
    wall.live.add('cam-c');
    const { container } = page();
    await screen.findByText('CAM C', { selector: '.bd-wall-tile b' });
    expect(tile(container, 'CAM C').querySelector('[data-testid=wall-crop]')).not.toBeNull();
    expect(screen.queryByTestId('tile-failed')).toBeNull();
    expect(screen.queryByTestId('tile-starting')).toBeNull();
  });

  it('leaves a normal still alone, and a stale still keeps its Stale frame mark', async () => {
    server.use(http.get('*/api/v1/sources/status', () => HttpResponse.json([
      demand('cam-a', 'ready'),
      demand('cam-c', 'ready'),
    ])));
    stills.byPath.set(stillPath('cam-a'), { url: 'blob:fresh', state: 'fresh' });
    stills.byPath.set(stillPath('cam-c'), { url: 'blob:old', state: 'stale' });
    const { container } = page();
    await screen.findByText('CAM A', { selector: '.bd-wall-tile b' });
    expect(tile(container, 'CAM A').querySelector('img')).toHaveAttribute('src', 'blob:fresh');
    expect(tile(container, 'CAM A').querySelector('[data-testid=tile-stale]')).toBeNull();
    await vi.waitFor(() => {
      expect(tile(container, 'CAM C').querySelector('[data-testid=tile-stale]')).toHaveTextContent('Stale frame');
    });
  });

  it('states an idle source nothing wants instead of showing an empty box', async () => {
    server.use(http.get('*/api/v1/sources/status', () => HttpResponse.json([
      demand('cam-d', 'idle', { video_wanted: false }),
    ])));
    page();
    expect(await screen.findByTestId('tile-idle')).toHaveTextContent('Idle — not in use');
  });
});
