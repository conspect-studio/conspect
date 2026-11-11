// Demand-driven sources on Monitoring: an idle source is shown as Idle (not a
// fault) and its thumbnail never polls, so opening the page starts nothing.
import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { screen, waitFor } from '@testing-library/react';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { MemoryRouter } from 'react-router-dom';

import { renderWithProviders } from '../test/render';
import { MonitoringPage } from './MonitoringPage';

vi.mock('../preview/PreviewSurface', () => ({ PreviewSurface: (): null => null }));
vi.mock('../realtime/useEngineEvents', () => ({ useEngineEvents: () => ({ status: 'open' }) }));
vi.mock('../resources/useLiveTiles', () => ({ useLiveTiles: () => new Map() }));

const jpegPolls: string[] = [];
const server = setupServer(
  http.get('*/api/v1/preview/inputs', () => HttpResponse.json(['idle', 'live'])),
  http.get('*/api/v1/preview/capabilities', () => HttpResponse.json({
    webrtc: false,
    scopes: {
      program: { whep: false, fidelity: 'pre-encode-canvas-approx' },
      inputs: { whep: false },
      outputs: { whep: false },
    },
  })),
  http.get('*/api/v1/outputs', () => HttpResponse.json([])),
  http.get('*/api/v1/sources/status', () =>
    HttpResponse.json([
      {
        id: 'idle', phase: 'idle', video_wanted: false, audio_wanted: false,
        video_running: false, audio_running: false, consumers: [], activation: 'on_demand',
      },
      {
        id: 'live', phase: 'ready', video_wanted: true, audio_wanted: true, activation: 'always_on',
        video_running: true, audio_running: true,
        consumers: [{ kind: 'layout_cell', label: 'cell-1' }],
      },
    ]),
  ),
  http.get('*/api/v1/preview/inputs/:file', ({ params }) => {
    jpegPolls.push(String(params.file));
    return new HttpResponse(null, { status: 503 });
  }),
);
beforeAll(() => { server.listen(); });
afterEach(() => { server.resetHandlers(); jpegPolls.length = 0; });
afterAll(() => { server.close(); });

describe('MonitoringPage demand states', () => {
  it('shows an idle source as idle and never polls it, while a consumed one polls', async () => {
    renderWithProviders(<MemoryRouter><MonitoringPage /></MemoryRouter>);
    expect(await screen.findByTestId('input-idle')).toBeInTheDocument();
    expect(screen.getByText('Idle')).toBeInTheDocument();
    expect(screen.getByText('Ready')).toBeInTheDocument();
    await waitFor(() => { expect(jpegPolls).toContain('live.jpg'); });
    expect(jpegPolls).not.toContain('idle.jpg');
    expect(screen.queryByText(/No signal/i)).toBeNull();
  });
});
