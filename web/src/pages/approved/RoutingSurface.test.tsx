// RoutingSurface: PGM readout truth, picker states, composed page wiring.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { MemoryRouter } from 'react-router-dom';

import { RoutingPage } from '../RoutingPage';
import { OutputsPage } from '../OutputsPage';
import { RouteTruthStrip, SourcePicker } from './RoutingSurface';
import { renderWithProviders } from '../../test/render';

const server = setupServer(
  http.get('*/api/v1/switcher/me1', () =>
    HttpResponse.json({ in_flight: false, preview_source: 'cam-b', program_source: 'cam-a', tick: 1 }),
  ),
  http.get('*/api/v1/sources', () =>
    HttpResponse.json([
      { id: 'cam-a', name: 'Camera A', body: { kind: 'srt', url: 'srt://[::1]:1' } },
      { id: 'cam-b', name: 'Camera B', body: { kind: 'srt', url: 'srt://[::1]:2' } },
    ]),
  ),
  http.get('*/api/v1/outputs', () =>
    HttpResponse.json([{ id: 'program', name: 'Program HLS', body: { kind: 'hls', path: '/p.m3u8', codec: 'h264' } }]),
  ),
  http.get('*/api/v1/outputs/:id/link', () => HttpResponse.json({ state: 'idle' })),
  http.get('*/api/v1/devices', () => HttpResponse.json([])),
  http.get('*/api/v1/audio-routing', () => HttpResponse.json({ state: { selectable_tracks: [] } })),
);
beforeAll(() => {
  server.listen({ onUnhandledRequest: 'bypass' });
});
afterEach(() => {
  server.resetHandlers();
});
afterAll(() => {
  server.close();
});

describe('RoutingSurface', () => {
  it('reads PGM only from the switcher and never fabricates restart state', () => {
    renderWithProviders(<RouteTruthStrip pgm={{ programSource: undefined, loading: false }} />);
    expect(within(screen.getByTestId('pgm-readout')).getByText('Unknown')).toBeInTheDocument();
    expect(screen.getByTestId('pending-summary')).toHaveTextContent(/Unknown/);
  });

  it('marks PGM chip only on the program source and reports picker failure', () => {
    const { rerender } = renderWithProviders(
      <SourcePicker sources={[{ id: 'a', name: 'A' }, { id: 'b', name: 'B' }]} selected="b" programSource="a" status="ready" onPick={() => undefined} />,
    );
    expect(screen.getByTestId('route-source-a')).toHaveTextContent('● on PGM');
    expect(screen.getByTestId('route-source-b')).toHaveTextContent('PGM: not this source');
    expect(screen.getByTestId('route-source-b')).toHaveAttribute('aria-pressed', 'true');
    rerender(<SourcePicker sources={[]} selected="" programSource={undefined} status="error" onPick={() => undefined} />);
    expect(screen.getByText(/Source list unavailable/)).toBeInTheDocument();
  });

  it('composes PGM readout, source picker (fills input id) and output sinks', async () => {
    const user = userEvent.setup();
    renderWithProviders(<MemoryRouter><RoutingPage /></MemoryRouter>);
    await waitFor(() => {
      expect(within(screen.getByTestId('pgm-readout')).getByText('cam-a')).toBeInTheDocument();
    });
    await user.click(await screen.findByTestId('route-source-cam-b'));
    expect(screen.getByLabelText('Source input id')).toHaveValue('cam-b');
    expect(await screen.findByTestId('output-card-program')).toBeInTheDocument();
    expect(screen.getByRole('heading', { name: 'Output sinks' })).toBeInTheDocument();
    expect(screen.getByTestId('crud-new')).toBeInTheDocument();
  });

  it('keeps the standalone Outputs page with its own heading', async () => {
    renderWithProviders(<MemoryRouter><OutputsPage /></MemoryRouter>);
    expect(await screen.findByRole('heading', { name: 'Outputs' })).toBeInTheDocument();
    expect(await screen.findByTestId('output-card-program')).toBeInTheDocument();
  });
});
