// CapabilitiesPage: a focused render test over MSW. Verifies the page reads the
// build capability + licence surface and renders the backend availability, the
// compositor class, and the effective licence.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { screen, within } from '@testing-library/react';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';

import { CapabilitiesPage } from './CapabilitiesPage';
import { I18nProvider as AppI18nProvider } from '../i18n/I18nProvider';
import { renderWithProviders } from '../test/render';

function renderCapabilities(): void {
  renderWithProviders(
    <AppI18nProvider>
      <CapabilitiesPage />
    </AppI18nProvider>,
  );
}

const REPORT = {
  backends: [
    {
      kind: 'software',
      stage: 'decode',
      available: true,
      max_resolution: { width: 7680, height: 4320 },
    },
    { kind: 'cuda', stage: 'decode', available: false },
    { kind: 'software', stage: 'encode', available: true },
  ],
  compositor: { class: 'none' },
  build: {
    effective_license: 'LGPL-clean',
    redistributable: true,
    features: ['software'],
    ndi: false,
  },
};

const server = setupServer(
  http.get('http://localhost:3000/api/v1/system/capabilities', () =>
    HttpResponse.json(REPORT),
  ),
);

beforeAll(() => {
  server.listen({ onUnhandledRequest: 'error' });
});
afterEach(() => {
  server.resetHandlers();
});
afterAll(() => {
  server.close();
});

describe('CapabilitiesPage', () => {
  it('renders backend availability, the compositor class and the effective licence', async () => {
    renderCapabilities();

    // Anchor on the unique probed resolution to find the software decode row
    // (there are two `software` rows: decode + encode).
    const resolutionCell = await screen.findByText('7680×4320');
    const softwareRow = resolutionCell.closest('tr');
    expect(softwareRow).not.toBeNull();
    expect(within(softwareRow as HTMLElement).getByText('software')).toBeInTheDocument();
    expect(within(softwareRow as HTMLElement).getByText('Available')).toBeInTheDocument();

    // A compiled-out hardware backend reads as unavailable, not missing.
    const cudaRow = screen.getByText('cuda').closest('tr');
    expect(cudaRow).not.toBeNull();
    expect(within(cudaRow as HTMLElement).getByText('Not available')).toBeInTheDocument();

    // The compliance surface: the exact licence string + compositor class.
    expect(screen.getByText('LGPL-clean')).toBeInTheDocument();
    expect(screen.getByText('none')).toBeInTheDocument();
  });

  it('shows the NDI attribution only when the ndi feature is compiled', async () => {
    server.use(
      http.get('http://localhost:3000/api/v1/system/capabilities', () =>
        HttpResponse.json({
          ...REPORT,
          build: { ...REPORT.build, ndi: true, features: ['software', 'ndi'] },
          ndi_attribution: {
            trademark: 'NDI® is a registered trademark of Vizrt NDI AB',
            url: 'https://ndi.video',
          },
        }),
      ),
    );
    renderCapabilities();

    expect(
      await screen.findByText('NDI® is a registered trademark of Vizrt NDI AB'),
    ).toBeInTheDocument();
  });
});
