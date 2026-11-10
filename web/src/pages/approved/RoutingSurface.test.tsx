// RoutingSurface: PGM readout truth, picker states, composed page wiring.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { MemoryRouter } from 'react-router-dom';

import { RoutingPage } from '../RoutingPage';
import { OutputsPage } from '../OutputsPage';
import { CrosspointGrid, DestinationPicker, RouteTruthStrip, RoutingHead, SourcePicker } from './RoutingSurface';
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

  it('help disclosure toggles and states 202 is not applied', async () => {
    const user = userEvent.setup();
    renderWithProviders(<RoutingHead />);
    const btn = screen.getByRole('button', { name: /Help/ });
    expect(btn).toHaveAttribute('aria-expanded', 'false');
    await user.click(btn);
    expect(btn).toHaveAttribute('aria-expanded', 'true');
    expect(screen.getByText(/202 is not applied/)).toBeVisible();
  });

  it('crosspoint grid uses real destinations only, Unknown state, selection feeds callback', async () => {
    const user = userEvent.setup();
    const picks: string[] = [];
    renderWithProviders(
      <CrosspointGrid
        sources={[{ id: 'a', name: 'A' }]}
        dests={[{ kind: 'video', id: 'cell-1', label: 'cell-1' }, { kind: 'audio', id: 'trk', label: 'trk' }]}
        selectedSource=""
        selectedDest=""
        planned={false}
        onPick={(s, d) => picks.push(`${s}>${d.kind}:${d.id}`)}
      />,
    );
    const cell = screen.getByTestId('xpt-a-cell-1');
    expect(cell).toHaveTextContent('?');
    expect(cell).not.toHaveTextContent('ON');
    await user.click(cell);
    expect(picks).toEqual(['a>video:cell-1']);
  });

  it('crosspoint grid is replaced by a note when no destinations are known', () => {
    renderWithProviders(<CrosspointGrid sources={[{ id: 'a', name: 'A' }]} dests={[]} selectedSource="" selectedDest="" planned={false} onPick={() => undefined} />);
    expect(screen.getByTestId('crosspoint-empty')).toBeInTheDocument();
  });

  it('phone destination stepper shows step 1 list then step 2 with back', async () => {
    const user = userEvent.setup();
    const dests = [{ kind: 'video' as const, id: 'c1', label: 'c1' }];
    const { rerender } = renderWithProviders(
      <DestinationPicker dests={dests} selected="" step={1} onPick={() => undefined} onBack={() => undefined} />,
    );
    expect(screen.getByTestId('route-dest-c1')).toHaveTextContent('Crosspoint: Unknown');
    let backed = false;
    rerender(<DestinationPicker dests={dests} selected="c1" step={2} onPick={() => undefined} onBack={() => { backed = true; }} />);
    await user.click(screen.getByRole('button', { name: /Back/ }));
    expect(backed).toBe(true);
  });

  it('output sink card shows Requested vs Effective with Unknown readback', async () => {
    renderWithProviders(<MemoryRouter><RoutingPage /></MemoryRouter>);
    const card = await screen.findByTestId('output-card-program');
    expect(within(card).getByTestId('output-requested')).toHaveTextContent('h264');
    expect(within(card).getByTestId('output-effective')).toHaveTextContent(/Unknown/);
    expect(within(card).getByTestId('output-restart')).toHaveTextContent(/Unknown/);
  });

  it('shows the plan panel and asks for confirmation when a locked take targets the PGM source', async () => {
    const user = userEvent.setup();
    renderWithProviders(<MemoryRouter><RoutingPage /></MemoryRouter>);
    await user.click(await screen.findByTestId('route-source-cam-a'));
    await user.type(screen.getByLabelText('Target video cell'), 'cell-a');
    expect(screen.getByTestId('plan-panel')).toHaveAttribute('data-selected', 'true');
    await user.click(screen.getByRole('button', { name: 'Take' }));
    expect(await screen.findByText('Confirm a program-affecting take?')).toBeInTheDocument();
  });
});
