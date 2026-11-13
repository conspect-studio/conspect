// The desk's Program monitor Cast button (PR 1 scope): casts the first
// HLS/LL-HLS output in configuration order; disabled with an honest reason
// when none is configured.
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('../../realtime/shared', () => ({ subscribeRealtime: () => () => undefined }));
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

let outputs: readonly { id: string; name: string; body: Record<string, unknown> }[] = [];

const server = setupServer(
  http.get('*/api/v1/switcher/me1', () => HttpResponse.json({ program_source: 'cam-1', preview_source: 'cam-2', tick: 1, in_flight: false })),
  http.get('*/api/v1/layouts', () => HttpResponse.json([])),
  http.get('*/api/v1/alarms', () => HttpResponse.json([])),
  http.get('*/api/v1/preview/capabilities', () => HttpResponse.json({ webrtc: false })),
  http.get('*/api/v1/sources', () => HttpResponse.json([])),
  http.get('*/api/v1/outputs', () => HttpResponse.json(outputs)),
);

beforeAll(() => {
  server.listen();
});
beforeEach(() => {
  window.chrome = {};
  outputs = [];
});
afterEach(() => {
  server.resetHandlers();
  delete window.chrome;
});
afterAll(() => {
  server.close();
});

function page(): ReturnType<typeof render> {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <I18nProvider i18n={i18n}>
        <MemoryRouter>
          <CastProvider>
            <ProductionDesk />
          </CastProvider>
        </MemoryRouter>
      </I18nProvider>
    </QueryClientProvider>,
  );
}

describe('ProductionDesk — Program Cast button', () => {
  it('is disabled with an honest reason when no HLS output is configured', async () => {
    page();
    expect(await screen.findByRole('button', { name: 'Cast needs an HLS output that shows Program' })).toBeDisabled();
  });

  it('casts the first configured HLS output once one exists', async () => {
    outputs = [
      { id: 'rtsp-1', name: 'RTSP mirror', body: { kind: 'rtsp_server', mount: '/x' } },
      { id: 'hls-main', name: 'Main HLS', body: { kind: 'hls', path: '/srv/index.m3u8', codec: 'h264' } },
    ];
    page();
    expect(await screen.findByRole('button', { name: /^Cast: Program$/ })).toBeInTheDocument();
  });
});
