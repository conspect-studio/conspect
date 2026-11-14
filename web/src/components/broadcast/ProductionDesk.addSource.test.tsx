// #419 item 3: the Desk offers a keyboard-accessible "Add source" entry point
// that reaches the New source dialog (via /sources?new=1).
import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from 'vitest';

vi.mock('../../realtime/shared', () => ({ subscribeRealtime: () => () => undefined }));
import { render, screen } from '@testing-library/react';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { i18n } from '@lingui/core';
import { I18nProvider } from '@lingui/react';
import { MemoryRouter } from 'react-router-dom';
import { messages } from '../../locales/en/messages';
import { ProductionDesk } from './ProductionDesk';

vi.mock('../../realtime/useEngineEvents', () => ({ useEngineEvents: () => ({ status: 'open' }), TILES_QUERY_KEY: ['tiles'] }));
vi.mock('../../preview/PreviewSurface', () => ({ PreviewSurface: () => null }));
vi.mock('../../preview/useJpegPreview', () => ({ useJpegPreview: () => undefined, useJpegStill: () => ({ url: undefined, state: 'loading' }) }));

i18n.load('en', messages);
i18n.activate('en');

const server = setupServer(
  http.get('*/api/v1/switcher/me1', () => HttpResponse.json({ program_source: undefined, preview_source: undefined, tick: 1, in_flight: false })),
  http.get('*/api/v1/layouts', () => HttpResponse.json([])),
  http.get('*/api/v1/alarms', () => HttpResponse.json([])),
  http.get('*/api/v1/preview/capabilities', () => HttpResponse.json({ webrtc: false })),
  http.get('*/api/v1/sources', () => HttpResponse.json([])),
  http.get('*/api/v1/sources/status', () => HttpResponse.json([])),
);
beforeAll(() => { server.listen(); });
afterEach(() => { server.resetHandlers(); });
afterAll(() => { server.close(); });

function page(): void {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  render(
    <QueryClientProvider client={client}>
      <I18nProvider i18n={i18n}>
        <MemoryRouter>
          <ProductionDesk />
        </MemoryRouter>
      </I18nProvider>
    </QueryClientProvider>,
  );
}

describe('ProductionDesk "Add source" entry point', () => {
  it('offers a keyboard-reachable link to the New source flow', async () => {
    page();
    const link = await screen.findByRole('link', { name: /Add a source/ });
    expect(link).toHaveAttribute('href', '/sources?new=1');
    // Links are real anchors: they are keyboard-focusable by default (tabIndex
    // is not -1), satisfying the keyboard-accessibility requirement.
    expect(link).not.toHaveAttribute('tabindex', '-1');
  });
});
