// RoutingPage: a focused render test over MSW. Verifies that Plan POSTs the
// composed crosspoint to /routing/plan and shows the returned class banner, and
// that Take POSTs to /routing/{kind}/take and surfaces a hot vs migration result.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { act, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { MemoryRouter } from 'react-router-dom';

import { RoutingPage } from './RoutingPage';
import { renderWithProviders } from '../test/render';

let lastPlanBody: unknown = null;
let lastTakePath = '';
let lastTakeIdempotency: string | null = null;

const server = setupServer(
  // The composed page also reads the PGM state, sources and outputs.
  http.get('*/api/v1/switcher/me1', () =>
    HttpResponse.json({ in_flight: false, preview_source: 'cam-b', program_source: 'cam-a', tick: 1 }),
  ),
  http.get('*/api/v1/sources', () => HttpResponse.json([])),
  http.get('*/api/v1/outputs', () => HttpResponse.json([])),
  http.get('*/api/v1/layouts', () => HttpResponse.json([])),
  http.get('*/api/v1/audio-routing', () => HttpResponse.json({ state: { selectable_tracks: [] } })),
  http.post('http://localhost:3000/api/v1/routing/plan', async ({ request }) => {
    lastPlanBody = await request.json();
    return HttpResponse.json({ class: 'class2', coerced: false });
  }),
  http.post('http://localhost:3000/api/v1/routing/:kind/take', ({ request }) => {
    lastTakePath = new URL(request.url).pathname;
    lastTakeIdempotency = request.headers.get('Idempotency-Key');
    return HttpResponse.json(
      { applied: true, class: 'class1', coerced: false, operation_id: 'op-1' },
      { status: 200 },
    );
  }),
);

beforeAll(() => {
  server.listen({ onUnhandledRequest: 'error' });
});
afterEach(() => {
  server.resetHandlers();
  lastPlanBody = null;
  lastTakePath = '';
  lastTakeIdempotency = null;
});
afterAll(() => {
  server.close();
});

describe('RoutingPage', () => {
  it('plans a video take and shows the classified class', async () => {
    const user = userEvent.setup();
    renderWithProviders(<MemoryRouter><RoutingPage /></MemoryRouter>);

    await user.type(screen.getByLabelText('Source input id'), 'cam-north');
    await user.type(screen.getByLabelText('Target video cell'), 'cell-a');
    await user.click(screen.getByRole('button', { name: /Plan/i }));

    // The plan banner shows the returned class as text.
    expect(await screen.findByText('class2')).toBeInTheDocument();
    expect(lastPlanBody).toEqual({
      source: { input_id: 'cam-north', kind: { kind: 'video' }, selector: { by: 'best' } },
      target: { kind: 'video_cell', cell: 'cell-a' },
    });
  });

  it('takes the crosspoint with a fresh Idempotency-Key', async () => {
    const user = userEvent.setup();
    renderWithProviders(<MemoryRouter><RoutingPage /></MemoryRouter>);

    await user.type(screen.getByLabelText('Source input id'), 'cam-north');
    await user.type(screen.getByLabelText('Target video cell'), 'cell-a');
    await user.click(screen.getByRole('button', { name: 'Take' }));
    // PGM is unconfirmed (frozen tick) and takes are locked: the fail-safe
    // asks for confirmation before sending.
    await user.click(await screen.findByRole('button', { name: 'Confirm take' }));

    await waitFor(() => {
      expect(lastTakePath).toBe('/api/v1/routing/video/take');
    });
    expect(lastTakeIdempotency).not.toBeNull();
    expect(lastTakeIdempotency).not.toBe('');
  });

  it('repeated Take submit and Confirm events produce exactly one request', async () => {
    let takes = 0;
    server.use(http.post('*/api/v1/routing/video/take', async () => {
      takes += 1;
      await new Promise((r) => setTimeout(r, 150));
      return HttpResponse.json({ status: 'applied', applied: { class: 'hot', operation_id: 'op-1' } });
    }));
    const user = userEvent.setup();
    renderWithProviders(<MemoryRouter><RoutingPage /></MemoryRouter>);
    await user.type(screen.getByLabelText('Source input id'), 'cam-north');
    await user.type(screen.getByLabelText('Target video cell'), 'cell-a');
    await user.click(screen.getByRole('button', { name: 'Take' }));
    const confirm = await screen.findByRole('button', { name: 'Confirm take' });
    act(() => { confirm.click(); confirm.click(); });
    await waitFor(() => { expect(takes).toBe(1); });
    await new Promise((r) => setTimeout(r, 400));
    expect(takes).toBe(1);
  });

  it('disables Plan and Take until an input and target are given', () => {
    renderWithProviders(<MemoryRouter><RoutingPage /></MemoryRouter>);
    expect(screen.getByRole('button', { name: /Plan/i })).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Take' })).toBeDisabled();
  });

  it('reads PGM as Unknown and confirms a locked take when the switcher is unreadable', async () => {
    server.use(
      http.get('*/api/v1/switcher/me1', () => HttpResponse.json({ error: 'down' }, { status: 500 })),
    );
    const user = userEvent.setup();
    renderWithProviders(<MemoryRouter><RoutingPage /></MemoryRouter>);
    await waitFor(() => {
      expect(screen.getByTestId('pgm-readout')).toHaveTextContent('Unknown');
    });
    await user.type(screen.getByLabelText('Source input id'), 'cam-north');
    await user.type(screen.getByLabelText('Target video cell'), 'cell-a');
    await user.click(screen.getByRole('button', { name: 'Take' }));
    expect(await screen.findByText('Confirm a program-affecting take?')).toBeInTheDocument();
    expect(lastTakePath).toBe('');
  });

  it('never keeps a picked discrete track once the target is typed: the route is a program-bus channel', async () => {
    server.use(
      http.get('*/api/v1/audio-routing', () =>
        HttpResponse.json(
          { configured: true, routing: null, selectable_tracks: ['prog', 'trk-1'] },
          { headers: { ETag: 'W/"1"' } },
        ),
      ),
    );
    const user = userEvent.setup();
    renderWithProviders(<MemoryRouter><RoutingPage /></MemoryRouter>);
    await user.click(await screen.findByTestId('route-dest-trk-1'));
    await user.type(screen.getByLabelText('Source input id'), 'cam-north');
    const target = screen.getByLabelText(/Target/);
    await user.clear(target);
    await user.type(target, 'main-L');
    await user.click(screen.getByRole('button', { name: /Plan/i }));
    await screen.findByText('class2');
    expect(lastPlanBody).toMatchObject({ target: { kind: 'audio_program_bus', channel: 'main-L' } });
  });

  it('sizes Plan and Take to the 48px floor', () => {
    renderWithProviders(<MemoryRouter><RoutingPage /></MemoryRouter>);
    expect(screen.getByRole('button', { name: /Plan/i })).toHaveClass('min-h-12');
    expect(screen.getByRole('button', { name: 'Take' })).toHaveClass('min-h-12');
  });
});
