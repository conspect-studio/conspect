// PatchMatrix (ADR-0110 §8/§12.9, #308): readback-driven glyph/state per cell,
// multi-select arming a set, Plan (SetPlan) → Take all → patch.take outcome,
// take-lock confirm, read-only, stale/disconnected, salvo store/recall, and
// the data-driven Subtitle tab empty state.
import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { act, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import type { Envelope } from '../../realtime/envelope';

type Listener = (envelope: Envelope) => void;
let listeners = new Set<Listener>();
let statusListeners = new Set<(status: string) => void>();
vi.mock('../../realtime/shared', () => ({
  subscribeRealtime: (
    _topic: string,
    listener: Listener,
    onStatus?: (status: string) => void,
  ): (() => void) => {
    listeners.add(listener);
    if (onStatus !== undefined) {
      statusListeners.add(onStatus);
      onStatus('open');
    }
    return () => {
      listeners.delete(listener);
      if (onStatus !== undefined) statusListeners.delete(onStatus);
    };
  },
}));

import { renderWithProviders } from '../../test/render';
import { PatchMatrix } from './PatchMatrix';

function emitStatus(status: string): void {
  act(() => {
    for (const listener of statusListeners) listener(status);
  });
}
function emitEnvelope(envelope: Envelope): void {
  act(() => {
    for (const listener of listeners) listener(envelope);
  });
}

const patchBody = {
  mixer: [],
  outputs: [
    {
      output: 'hls-1',
      format: { width: 1920, height: 1080 },
      video: [{ slot: 0, live: true, source: { layout: 'news-one' }, path: 'P1', pending: null, degraded: false }],
      audio: [{ slot: 0, source: 'follow' }],
    },
    {
      output: 'srt-1',
      format: { width: 1920, height: 1080 },
      video: [{ slot: 0, live: true, source: { source: 'cnn-usa' }, path: 'P2', pending: null, degraded: false }],
      audio: [{ slot: 0, source: 'follow' }],
    },
  ],
};

let takeRequests: unknown[] = [];
let planRequests: unknown[] = [];
let salvoPut: unknown = null;
let planResponse: Record<string, unknown> = { can_take: true, class: 'class2', crosspoints: [] };

const server = setupServer(
  http.get('*/api/v1/patch', () => HttpResponse.json(patchBody)),
  http.get('*/api/v1/sources', () =>
    HttpResponse.json([
      { id: 'cnn-usa', name: 'CNN USA', body: { kind: 'srt', url: 'srt://[::1]:1' } },
      { id: 'abc-news', name: 'ABC News', body: { kind: 'srt', url: 'srt://[::1]:2' } },
    ]),
  ),
  http.get('*/api/v1/layouts', () =>
    HttpResponse.json([{ id: 'news-one', name: 'NewsOne', body: { canvas: { width: 1920, height: 1080 }, cells: [] } }]),
  ),
  http.get('*/api/v1/media/players', () => HttpResponse.json([])),
  http.get('*/api/v1/salvos', () => HttpResponse.json([{ id: 'abc-salvo', display_name: 'ABC switch' }])),
  http.post('*/api/v1/patch/plan', async ({ request }) => {
    planRequests.push(await request.json());
    return HttpResponse.json(planResponse);
  }),
  http.post('*/api/v1/patch/take', async ({ request }) => {
    takeRequests.push(await request.json());
    return HttpResponse.json({ operation_id: 'set-op-1', status: 'accepted' }, { status: 202 });
  }),
  http.post('*/api/v1/salvos/:id/plan', () => HttpResponse.json({ can_take: true, class: 'class2', crosspoints: [] })),
  http.post('*/api/v1/salvos/:id/take', () => HttpResponse.json({ operation_id: 'salvo-op-1', status: 'accepted' }, { status: 202 })),
  http.put('*/api/v1/salvos/:id', async ({ request }) => {
    const body: unknown = await request.json();
    salvoPut = body;
    return HttpResponse.json(body as Record<string, unknown>, { status: 201 });
  }),
);

beforeAll(() => { server.listen({ onUnhandledRequest: 'error' }); });
afterEach(() => {
  server.resetHandlers();
  listeners = new Set();
  statusListeners = new Set();
  takeRequests = [];
  planRequests = [];
  salvoPut = null;
  planResponse = { can_take: true, class: 'class2', crosspoints: [] };
});
afterAll(() => { server.close(); });

function renderMatrix() {
  return renderWithProviders(<PatchMatrix />);
}

describe('PatchMatrix readback', () => {
  it('renders the on-air glyph and passthrough-path suffix for the current source', async () => {
    renderMatrix();
    emitStatus('open');
    const cell = await screen.findByTestId('cell-video-source:cnn-usa-output:srt-1:0');
    await waitFor(() => { expect(cell).toHaveTextContent('●⇢'); });
  });

  it('renders the available glyph for a row not currently patched', async () => {
    renderMatrix();
    emitStatus('open');
    const cell = await screen.findByTestId('cell-video-source:abc-news-output:srt-1:0');
    await waitFor(() => { expect(cell).toHaveTextContent('○'); });
  });

  it('exposes the cell state in the accessible name', async () => {
    renderMatrix();
    emitStatus('open');
    await screen.findByTestId('cell-video-source:cnn-usa-output:srt-1:0');
    expect(screen.getByRole('button', { name: /CNN USA to srt-1: on air/ })).toBeInTheDocument();
  });

  it('shows the Subtitle tab empty-state when no output reports subtitle slots (patch-f note)', async () => {
    const user = userEvent.setup();
    renderMatrix();
    emitStatus('open');
    await screen.findByTestId('cell-video-source:cnn-usa-output:srt-1:0');
    await user.click(screen.getByRole('tab', { name: 'Subtitle' }));
    expect(screen.getByTestId('matrix-subtitle-empty')).toBeInTheDocument();
  });
});

describe('PatchMatrix multi-select set arming', () => {
  it('arms a multi-destination selection and plans a SetPlan body with every member', async () => {
    planResponse = { can_take: true, class: 'class2', crosspoints: [], set: { new_encoders: 1, fits: true } };
    const user = userEvent.setup();
    renderMatrix();
    emitStatus('open');
    await user.click(await screen.findByTestId('cell-video-source:abc-news-output:hls-1:0'));
    await user.click(await screen.findByTestId('cell-video-source:abc-news-output:srt-1:0'));
    expect(screen.getByTestId('matrix-arming')).toHaveTextContent('2 selected');
    await user.click(screen.getByRole('button', { name: 'Classify selection' }));
    await waitFor(() => { expect(planRequests).toHaveLength(1); });
    expect(planRequests[0]).toEqual({
      crosspoints: [
        { plane: 'video', destination: { output: 'hls-1', slot: 0 }, source: { source: 'abc-news' } },
        { plane: 'video', destination: { output: 'srt-1', slot: 0 }, source: { source: 'abc-news' } },
      ],
      on_air: 'protect',
    });
    await waitFor(() => { expect(screen.getByTestId('matrix-plan-line')).toHaveTextContent('+1 encoder'); });
    await user.click(screen.getByRole('button', { name: 'Take all' }));
    await waitFor(() => { expect(takeRequests).toHaveLength(1); });
    expect(takeRequests[0]).toEqual(planRequests[0]);
  });

  it('shows the applied outcome once the matching patch.take event arrives', async () => {
    planResponse = { can_take: true, class: 'class2', crosspoints: [], set: { new_encoders: 0, fits: true } };
    const user = userEvent.setup();
    renderMatrix();
    emitStatus('open');
    await user.click(await screen.findByTestId('cell-video-source:abc-news-output:hls-1:0'));
    await user.click(await screen.findByTestId('cell-video-source:abc-news-output:srt-1:0'));
    await user.click(screen.getByRole('button', { name: 'Classify selection' }));
    await waitFor(() => { expect(screen.getByTestId('matrix-plan-line')).toBeInTheDocument(); });
    await user.click(screen.getByRole('button', { name: 'Take all' }));
    await waitFor(() => { expect(takeRequests).toHaveLength(1); });
    emitEnvelope({
      v: 1, seq: 1, ts: 1, topic: 'outputs', t: 'patch.take',
      data: { operation_id: 'set-op-1', outcome: 'applied', tick: 42, destination: { output: 'hls-1' } },
    });
    await waitFor(() => { expect(screen.getByText(/Patch take applied/)).toBeInTheDocument(); });
  });

  it('shows a refused outcome with its typed kind', async () => {
    planResponse = { can_take: true, class: 'class2', crosspoints: [], set: { new_encoders: 0, fits: true } };
    const user = userEvent.setup();
    renderMatrix();
    emitStatus('open');
    await user.click(await screen.findByTestId('cell-video-source:abc-news-output:hls-1:0'));
    await user.click(await screen.findByTestId('cell-video-source:abc-news-output:srt-1:0'));
    await user.click(screen.getByRole('button', { name: 'Classify selection' }));
    await waitFor(() => { expect(screen.getByTestId('matrix-plan-line')).toBeInTheDocument(); });
    await user.click(screen.getByRole('button', { name: 'Take all' }));
    await waitFor(() => { expect(takeRequests).toHaveLength(1); });
    emitEnvelope({
      v: 1, seq: 1, ts: 1, topic: 'outputs', t: 'patch.take',
      data: { operation_id: 'set-op-1', outcome: 'refused', refusal: 'capacity', tick: 42, destination: { output: 'hls-1' } },
    });
    await waitFor(() => { expect(screen.getByText(/Patch take failed/)).toBeInTheDocument(); });
    expect(screen.getByText(/Refused by the engine/)).toBeInTheDocument();
  });

  it('single-destination selection disables Take all in favour of a single take', async () => {
    const user = userEvent.setup();
    renderMatrix();
    emitStatus('open');
    await user.click(await screen.findByTestId('cell-video-source:abc-news-output:hls-1:0'));
    expect(screen.getByRole('button', { name: 'Classify selection' })).toBeEnabled();
    expect(screen.queryByRole('button', { name: 'Take all' })).toBeNull();
    expect(screen.getByRole('button', { name: 'Take this crosspoint' })).toBeInTheDocument();
  });
});

describe('PatchMatrix take lock', () => {
  it('requires confirmation before a take that changes a Program-tallied output', async () => {
    const programPatch = {
      mixer: [],
      outputs: [
        {
          output: 'hls-1',
          format: null,
          video: [{ slot: 0, live: true, source: { bus: 'program' }, path: 'P1', pending: null, degraded: false }],
          audio: [{ slot: 0, source: 'follow' }],
        },
        { ...patchBody.outputs[1] },
      ],
    };
    server.use(http.get('*/api/v1/patch', () => HttpResponse.json(programPatch)));
    planResponse = { can_take: true, class: 'class2', crosspoints: [], set: { new_encoders: 0, fits: true } };
    const user = userEvent.setup();
    renderMatrix();
    emitStatus('open');
    await user.click(await screen.findByTestId('cell-video-source:abc-news-output:hls-1:0'));
    await user.click(await screen.findByTestId('cell-video-source:abc-news-output:srt-1:0'));
    await user.click(screen.getByRole('button', { name: 'Classify selection' }));
    await waitFor(() => { expect(screen.getByTestId('matrix-plan-line')).toBeInTheDocument(); });
    await user.click(screen.getByRole('button', { name: 'Take all' }));
    expect(await screen.findByText('Confirm a program-affecting take?')).toBeInTheDocument();
    expect(takeRequests).toHaveLength(0);
    await user.click(screen.getByRole('button', { name: 'Confirm take' }));
    await waitFor(() => { expect(takeRequests).toHaveLength(1); });
  });
});

describe('PatchMatrix page states', () => {
  it('is disconnected while the realtime status is reconnecting/closed', async () => {
    renderMatrix();
    emitStatus('reconnecting');
    await waitFor(() => { expect(screen.getByTestId('matrix-page-state')).toHaveTextContent('Disconnected'); });
  });

  it('is read-only on a 403 readback', async () => {
    server.use(http.get('*/api/v1/patch', () => HttpResponse.json({ title: 'forbidden', status: 403 }, { status: 403 })));
    renderMatrix();
    emitStatus('open');
    await waitFor(() => { expect(screen.getByTestId('matrix-page-state')).toHaveTextContent('Read-only'); });
  });
});

describe('PatchMatrix salvo store/recall', () => {
  it('stores the armed selection as a salvo with on_air protect by default', async () => {
    const user = userEvent.setup();
    renderMatrix();
    emitStatus('open');
    await user.click(await screen.findByTestId('cell-video-source:abc-news-output:hls-1:0'));
    await user.click(screen.getByRole('button', { name: 'Store as salvo…' }));
    await user.type(screen.getByLabelText('Salvo id'), 'morning-abc');
    await user.click(screen.getByRole('button', { name: 'Store salvo' }));
    await waitFor(() => { expect(salvoPut).toMatchObject({
      id: 'morning-abc',
      on_air: 'protect',
      patch: [{ plane: 'video', destination: { output: 'hls-1', slot: 0 }, source: { source: 'abc-news' } }],
    }); });
  });

  it('plans then takes a recalled salvo', async () => {
    const user = userEvent.setup();
    renderMatrix();
    emitStatus('open');
    await screen.findByTestId('cell-video-source:cnn-usa-output:srt-1:0');
    await user.click(screen.getByRole('combobox', { name: 'Salvo' }));
    await user.click(await screen.findByRole('option', { name: 'ABC switch' }));
    await user.click(screen.getByRole('button', { name: 'Classify salvo' }));
    await waitFor(() => { expect(screen.getByTestId('matrix-recall-plan-line')).toHaveTextContent('Can take.'); });
    await user.click(screen.getByRole('button', { name: 'Take salvo' }));
    await waitFor(() => {
      expect(screen.getByTestId('matrix-plan-panel')).toBeInTheDocument();
    });
  });
});
