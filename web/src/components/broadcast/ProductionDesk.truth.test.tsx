import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from 'vitest';

vi.mock('../../realtime/shared', () => ({ subscribeRealtime: () => () => undefined }));
const meter = vi.hoisted(() => ({ current: undefined as undefined | { peak_db: number[]; rms_db: number[] } }));
vi.mock('../../realtime/useAudioMeter', () => ({ useAudioMeter: () => ({ status: 'open', current: meter.current, inputs: [] }), meterFillPercent: () => 50 }));
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
import { CastProvider } from '../../cast';
import { ProductionDesk } from './ProductionDesk';

i18n.load('en', messages);
i18n.activate('en');
const src = (id: string, name: string) => ({ id, name, body: { kind: 'srt', url: 'srt://[::1]:1' } });
const server = setupServer(
  http.get('*/api/v1/switcher/me1', () => HttpResponse.json({ program_source: 'a', preview_source: 'b', tick: 7, in_flight: false })),
  http.get('*/api/v1/layouts', () => HttpResponse.json([])),
  http.get('*/api/v1/alarms', () => HttpResponse.json([])),
  http.get('*/api/v1/preview/capabilities', () => HttpResponse.json({ webrtc: false })),
  http.get('*/api/v1/sources', () => HttpResponse.json([src('a', 'CAM A'), src('b', 'CAM B'), src('c', 'CAM C'), src('d', 'CAM D')])),
);
beforeAll(() => { server.listen(); });
afterEach(() => { server.resetHandlers(); meter.current = undefined; });
afterAll(() => { server.close(); });

function page() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  client.setQueryData(['tiles'], {
    t1: { id: 't1', state: 'LIVE', input: 'a' },
    t2: { id: 't2', state: 'RECONNECTING', input: 'b' },
    c: { id: 'c', state: 'NO_SIGNAL' },
  });
  return render(<QueryClientProvider client={client}><I18nProvider i18n={i18n}><MemoryRouter><CastProvider><ProductionDesk /></CastProvider></MemoryRouter></I18nProvider></QueryClientProvider>);
}

describe('ProductionDesk truthful wall, indices and meters', () => {
  it('maps live tile lifecycle onto wall tiles; sources without a tile stay unconfirmed', async () => {
    const { container } = page();
    await screen.findByText('CAM D', { selector: '.bd-wall-tile b' });
    const tile = (n: string): HTMLElement => [...container.querySelectorAll<HTMLElement>('.bd-wall-tile')].find((e) => e.querySelector('b')?.textContent === n) ?? document.body;
    expect(tile('CAM A')).toHaveTextContent('Live');
    expect(tile('CAM B')).toHaveTextContent('Starting');
    expect(tile('CAM C')).toHaveTextContent('No signal');
    expect(tile('CAM D')).toHaveTextContent('Signal unconfirmed');
  });
  it('shows real source-order index numerals on bus buttons', async () => {
    const { container } = page();
    await screen.findAllByText('CAM D', { selector: '.bd-bus button .bd-key-name' });
    const first = container.querySelectorAll('.bd-bus-preview button');
    expect([...first].map((b) => b.querySelector('.bd-idx')?.textContent)).toEqual(['1', '2', '3', '4']);
    // two-line key: name on line 1, index on line 2
    expect([...first].map((b) => b.querySelector('.bd-key-name')?.textContent)).toEqual(['CAM A', 'CAM B', 'CAM C', 'CAM D']);
  });
  it('program meters are explicit unavailable without telemetry, real values with it', async () => {
    page();
    expect(await screen.findByTestId('desk-meters-unavailable')).toBeInTheDocument();
  });
  it('renders L/R meters from audio.meter telemetry', async () => {
    meter.current = { peak_db: [-12.3, -14], rms_db: [-20, -20] };
    page();
    expect(await screen.findByTestId('desk-meters')).toHaveTextContent('-12.3 dBFS');
    expect(screen.queryByTestId('desk-meters-unavailable')).toBeNull();
  });
  it('exposes the waiting state as ordinary text, not hidden behind an image role', async () => {
    page();
    const waiting = await screen.findByText('Waiting for a frame…');
    expect(waiting.closest('[role="img"]')).toBeNull();
  });
  it('renders the PGM TC / SHOW ELAPSED panel as explicit unavailable with the real engine tick', async () => {
    page();
    const tc = await screen.findByTestId('desk-tc-unavailable');
    expect(tc).toHaveTextContent('PGM TC');
    expect(tc).toHaveTextContent('unavailable');
    expect(tc).toHaveAttribute('title', 'No timecode source in the engine API');
    expect(screen.getByTestId('desk-elapsed-unavailable')).toHaveTextContent('SHOW ELAPSED');
    expect(tc.closest('.bd-desk-clock')).toHaveTextContent('ENGINE TICK');
    expect(await screen.findByText('7', { selector: '.bd-desk-clock b' })).toBeInTheDocument();
    expect(tc.textContent).not.toMatch(/\d\d:\d\d/);
  });
  it('Help pill toggles the help panel', async () => {
    page();
    const btn = await screen.findByRole('button', { name: /Help/ });
    expect(btn).toHaveAttribute('aria-expanded', 'false');
    btn.click();
    await screen.findByText(/Tally vs signal/);
    expect(btn).toHaveAttribute('aria-expanded', 'true');
  });
});
