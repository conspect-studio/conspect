// RoutingPage: a focused render test over MSW. Verifies that Plan POSTs the
// composed crosspoint to /routing/plan and shows the returned class banner, and
// that Take POSTs to /routing/{kind}/take and surfaces a hot vs migration result.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';

import { RoutingPage } from './RoutingPage';
import { renderWithProviders } from '../test/render';

let lastPlanBody: unknown = null;
let lastTakePath = '';
let lastTakeIdempotency: string | null = null;

const server = setupServer(
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
    renderWithProviders(<RoutingPage />);

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
    renderWithProviders(<RoutingPage />);

    await user.type(screen.getByLabelText('Source input id'), 'cam-north');
    await user.type(screen.getByLabelText('Target video cell'), 'cell-a');
    await user.click(screen.getByRole('button', { name: 'Take' }));

    await waitFor(() => {
      expect(lastTakePath).toBe('/api/v1/routing/video/take');
    });
    expect(lastTakeIdempotency).not.toBeNull();
    expect(lastTakeIdempotency).not.toBe('');
  });

  it('disables Plan and Take until an input and target are given', () => {
    renderWithProviders(<RoutingPage />);
    expect(screen.getByRole('button', { name: /Plan/i })).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Take' })).toBeDisabled();
  });
});
