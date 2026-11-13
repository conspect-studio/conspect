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
import { BroadcastShell } from './BroadcastShell';
import { ProductionDesk } from './ProductionDesk';

i18n.load('en', messages);
i18n.activate('en');
let tick = 7;
let advance = false;
const server = setupServer(
  http.get('*/api/v1/switcher/me1', () => { if (advance) tick += 1; return HttpResponse.json({ program_source: 'a', preview_source: 'b', tick, in_flight: false }); }),
  http.get('*/api/v1/layouts', () => HttpResponse.json([])),
  http.get('*/api/v1/alarms', () => HttpResponse.json([])),
  http.get('*/api/v1/preview/capabilities', () => HttpResponse.json({ webrtc: false })),
  http.get('*/api/v1/sources', () => HttpResponse.json([{ id: 'a', name: 'CAM A', body: { kind: 'srt', url: 'srt://[::1]:1' } }])),
);
beforeAll(() => { server.listen(); });
afterEach(() => { server.resetHandlers(); });
afterAll(() => { server.close(); });

function page() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return { client, ...render(<QueryClientProvider client={client}><I18nProvider i18n={i18n}><MemoryRouter><CastProvider><BroadcastShell managementLabel="Connected"><ProductionDesk /></BroadcastShell></CastProvider></MemoryRouter></I18nProvider></QueryClientProvider>) };
}

describe('header ProducerStatus and Desk share one tick observation', () => {
  it('a frozen tick leaves BOTH header and desk unconfirmed', async () => {
    advance = false;
    const { container } = page();
    await screen.findByText(/PROGRAM — unconfirmed/);
    const badge = container.querySelector('[data-producer-badge]');
    expect(badge).toHaveTextContent('Unknown');
    expect(container.querySelector('.bd-top')).toHaveTextContent('No output tick observed');
    expect(screen.queryByText(/ON AIR — PROGRAM/)).toBeNull();
  });
  it('an advancing tick confirms BOTH header and desk', async () => {
    advance = true;
    const { container } = page();
    await screen.findByText(/ON AIR — PROGRAM/, undefined, { timeout: 8000 });
    expect(container.querySelector('[data-producer-badge]')).toHaveTextContent('Producing');
    expect(container.querySelector('.bd-top')).not.toHaveTextContent('No output tick observed');
  }, 15000);
});
