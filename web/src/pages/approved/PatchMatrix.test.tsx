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

import { i18n } from '@lingui/core';
import { t } from '@lingui/core/macro';
import { messages as pseudoMessages } from '../../locales/pseudo/messages';
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

function isEmptyPlan(body: unknown): boolean {
  return typeof body === 'object' && body !== null && 'crosspoints' in body
    && Array.isArray(body.crosspoints) && body.crosspoints.length === 0;
}

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
    const body: unknown = await request.json();
    // The server's role → count order: an empty list is the page's Write
    // probe and answers 422 (count) for a Write principal, planning nothing.
    if (isEmptyPlan(body)) return HttpResponse.json({ title: 'validation', status: 422 }, { status: 422 });
    planRequests.push(body);
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

// ---- Review findings E3-01 … E3-11 ------------------------------------------

const mixerPatch = (tick = 7) => ({
  mixer: [{
    me: 'me1', inputs_version: 1, tick,
    inputs: [
      { number: 1, label: 'in1', video: { source: 'cnn-usa' }, on_program: true, on_preview: false },
      { number: 2, label: 'in2', video: { source: 'abc-news' }, on_program: false, on_preview: true },
    ],
  }],
  outputs: patchBody.outputs,
});

async function armAndClassify(user: ReturnType<typeof userEvent.setup>, ...testIds: string[]): Promise<void> {
  for (const id of testIds) await user.click(await screen.findByTestId(id));
  await user.click(screen.getByRole('button', { name: 'Classify selection' }));
}

describe('E3-01 salvo recall goes through the Program confirm gate', () => {
  it('a recalled salvo touching a Program output opens the confirm and takes only after it', async () => {
    const programPatch = {
      mixer: [],
      outputs: [
        { ...patchBody.outputs[0]!, video: [{ slot: 0, live: true, source: { bus: 'program' }, path: 'P1', pending: null, degraded: false }] },
        patchBody.outputs[1]!,
      ],
    };
    let salvoTakes = 0;
    server.use(
      http.get('*/api/v1/patch', () => HttpResponse.json(programPatch)),
      http.get('*/api/v1/salvos', () => HttpResponse.json([{
        id: 'abc-salvo', display_name: 'ABC switch', on_air: 'protect',
        patch: [{ plane: 'video', destination: { output: 'hls-1', slot: 0 }, source: { source: 'abc-news' } }],
      }])),
      http.post('*/api/v1/salvos/:id/take', () => {
        salvoTakes += 1;
        return HttpResponse.json({ operation_id: 'salvo-op-1', status: 'accepted' }, { status: 202 });
      }),
    );
    const user = userEvent.setup();
    renderMatrix();
    await screen.findByTestId('cell-video-source:cnn-usa-output:srt-1:0');
    await user.click(screen.getByRole('combobox', { name: 'Salvo' }));
    await user.click(await screen.findByRole('option', { name: 'ABC switch' }));
    await user.click(screen.getByRole('button', { name: 'Classify salvo' }));
    await waitFor(() => { expect(screen.getByTestId('matrix-recall-plan-line')).toHaveTextContent('needs confirmation'); });
    await user.click(screen.getByRole('button', { name: 'Take salvo' }));
    expect(await screen.findByText('Confirm a program-affecting take?')).toBeInTheDocument();
    expect(salvoTakes).toBe(0);
    await user.click(screen.getByRole('button', { name: 'Confirm take' }));
    await waitFor(() => { expect(salvoTakes).toBe(1); });
  });

  it('Take salvo stays disabled until the salvo is classified', async () => {
    const user = userEvent.setup();
    renderMatrix();
    await screen.findByTestId('cell-video-source:cnn-usa-output:srt-1:0');
    await user.click(screen.getByRole('combobox', { name: 'Salvo' }));
    await user.click(await screen.findByRole('option', { name: 'ABC switch' }));
    expect(screen.getByRole('button', { name: 'Take salvo' })).toBeDisabled();
    expect(screen.getByText('Classify the salvo first')).toBeInTheDocument();
  });

  it('a protect salvo whose plan hits the on-air guard is disabled with that reason, not confirmed', async () => {
    server.use(http.post('*/api/v1/salvos/:id/plan', () => HttpResponse.json({
      can_take: false, class: 'class1',
      crosspoints: [{ destination: { mixer: 'me1', input: 1 }, path: 'mixer-input', on_air: true, refusal: { type: 'on-air-protected', detail: 'input 1 is on Program' } }],
    })));
    const user = userEvent.setup();
    renderMatrix();
    await screen.findByTestId('cell-video-source:cnn-usa-output:srt-1:0');
    await user.click(screen.getByRole('combobox', { name: 'Salvo' }));
    await user.click(await screen.findByRole('option', { name: 'ABC switch' }));
    await user.click(screen.getByRole('button', { name: 'Classify salvo' }));
    await waitFor(() => { expect(screen.getByRole('button', { name: 'Take salvo' })).toBeDisabled(); });
    expect(screen.getByText(/stored on-air: protect/)).toBeInTheDocument();
  });
});

describe('E3-02 on-air plan vs confirmed take bodies', () => {
  it('a locked PGM-input plan shows needs-confirmation (Take enabled) and the confirmed take sends allow', async () => {
    server.use(http.get('*/api/v1/patch', () => HttpResponse.json(mixerPatch())));
    planResponse = {
      can_take: false, class: 'class1',
      crosspoints: [{ destination: { mixer: 'me1', input: 1 }, path: 'mixer-input', on_air: true, refusal: { type: 'on-air-protected', detail: 'input 1 is on Program' } }],
    };
    const user = userEvent.setup();
    renderMatrix();
    await armAndClassify(user, 'cell-video-source:abc-news-mixer:me1:1');
    await waitFor(() => { expect(planRequests).toHaveLength(1); });
    expect(planRequests[0]).toEqual({
      crosspoints: [{ plane: 'video', destination: { mixer: 'me1', input: 1 }, source: { source: 'abc-news' } }],
      on_air: 'protect',
    });
    await waitFor(() => { expect(screen.getByTestId('matrix-plan-line')).toHaveTextContent('needs confirmation'); });
    const take = screen.getByRole('button', { name: 'Take this crosspoint' });
    expect(take).toBeEnabled();
    await user.click(take);
    await user.click(await screen.findByRole('button', { name: 'Confirm take' }));
    await waitFor(() => { expect(takeRequests).toHaveLength(1); });
    expect(takeRequests[0]).toEqual({
      crosspoints: [{ plane: 'video', destination: { mixer: 'me1', input: 1 }, source: { source: 'abc-news' } }],
      on_air: 'allow',
    });
  });

  it('a hard refusal (not the on-air guard) still disables Take', async () => {
    planResponse = {
      can_take: false, class: 'class2',
      crosspoints: [{ destination: { output: 'hls-1', slot: 0 }, path: 'P3', on_air: false, refusal: { type: 'capacity', detail: 'needs 1 encoder, 0 free' } }],
    };
    const user = userEvent.setup();
    renderMatrix();
    await armAndClassify(user, 'cell-video-source:abc-news-output:hls-1:0');
    await waitFor(() => { expect(screen.getByTestId('matrix-plan-line')).toHaveTextContent('needs 1 encoder, 0 free'); });
    expect(screen.getByRole('button', { name: 'Take this crosspoint' })).toBeDisabled();
  });
});

describe('E3-03 write capability and connection gate every action', () => {
  it('is read-only when the principal lacks Write (the plan role check answers 403)', async () => {
    server.use(http.post('*/api/v1/patch/plan', () => HttpResponse.json({ title: 'forbidden', status: 403 }, { status: 403 })));
    renderMatrix();
    await waitFor(() => { expect(screen.getByTestId('matrix-page-state')).toHaveTextContent('Read-only'); });
    expect(await screen.findByTestId('cell-video-source:abc-news-output:hls-1:0')).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Classify selection' })).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Classify salvo' })).toBeDisabled();
    expect(screen.getAllByText('Read-only: your role lacks Write').length).toBeGreaterThan(0);
  });

  it('disconnected disables cells, plan, take, store and recall', async () => {
    const user = userEvent.setup();
    renderMatrix();
    await user.click(await screen.findByTestId('cell-video-source:abc-news-output:hls-1:0'));
    emitStatus('reconnecting');
    await waitFor(() => { expect(screen.getByTestId('matrix-page-state')).toHaveTextContent('Disconnected'); });
    expect(screen.getByTestId('cell-video-source:abc-news-output:srt-1:0')).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Classify selection' })).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Take this crosspoint' })).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Store as salvo…' })).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Classify salvo' })).toBeDisabled();
  });

  it('an already-open confirm dialog closes on disconnect and sends nothing', async () => {
    server.use(http.get('*/api/v1/patch', () => HttpResponse.json(mixerPatch())));
    planResponse = { can_take: true, class: 'class1', crosspoints: [{ destination: { mixer: 'me1', input: 1 }, path: 'mixer-input', on_air: true, refusal: null }] };
    const user = userEvent.setup();
    renderMatrix();
    await armAndClassify(user, 'cell-video-source:abc-news-mixer:me1:1');
    await waitFor(() => { expect(screen.getByTestId('matrix-plan-line')).toBeInTheDocument(); });
    await user.click(screen.getByRole('button', { name: 'Take this crosspoint' }));
    expect(await screen.findByRole('button', { name: 'Confirm take' })).toBeInTheDocument();
    emitStatus('closed');
    await waitFor(() => { expect(screen.queryByRole('button', { name: 'Confirm take' })).toBeNull(); });
    expect(takeRequests).toHaveLength(0);
  });
});

describe('E3-04 settle per request path', () => {
  it('a single-output take settles on route.output (and ignores a patch.take for the same id)', async () => {
    server.use(http.post('*/api/v1/patch/take', async ({ request }) => {
      takeRequests.push(await request.json());
      return HttpResponse.json({ operation_id: 'out-op-1', status: 'accepted' }, { status: 202 });
    }));
    const user = userEvent.setup();
    renderMatrix();
    await armAndClassify(user, 'cell-video-source:abc-news-output:hls-1:0');
    await waitFor(() => { expect(screen.getByTestId('matrix-plan-line')).toBeInTheDocument(); });
    await user.click(screen.getByRole('button', { name: 'Take this crosspoint' }));
    await waitFor(() => { expect(takeRequests).toHaveLength(1); });
    emitEnvelope({ v: 1, seq: 1, ts: 1, topic: 'outputs', t: 'patch.take', data: { operation_id: 'out-op-1', outcome: 'refused', tick: 1, destination: { output: 'hls-1' } } });
    expect(screen.queryByText(/Patch take failed/)).toBeNull();
    emitEnvelope({ v: 1, seq: 2, ts: 2, topic: 'outputs', id: 'hls-1', corr: 'out-op-1', t: 'route.output', data: { output: 'hls-1', operation_id: 'out-op-1', tick: 9, outcome: 'applied' } });
    await waitFor(() => { expect(screen.getByText(/Patch take applied/)).toBeInTheDocument(); });
  });

  it('a mixer-only take settles on switcher.operation', async () => {
    server.use(
      http.get('*/api/v1/patch', () => HttpResponse.json(mixerPatch())),
      http.post('*/api/v1/patch/take', async ({ request }) => {
        takeRequests.push(await request.json());
        return HttpResponse.json({ operation_id: 'mix-op-1', status: 'accepted' }, { status: 202 });
      }),
    );
    planResponse = { can_take: true, class: 'class1', crosspoints: [{ destination: { mixer: 'me1', input: 2 }, path: 'mixer-input', on_air: false, refusal: null }] };
    const user = userEvent.setup();
    renderMatrix();
    await armAndClassify(user, 'cell-video-source:cnn-usa-mixer:me1:2');
    await waitFor(() => { expect(screen.getByTestId('matrix-plan-line')).toBeInTheDocument(); });
    await user.click(screen.getByRole('button', { name: 'Take this crosspoint' }));
    await waitFor(() => { expect(takeRequests).toHaveLength(1); });
    emitEnvelope({ v: 1, seq: 1, ts: 1, topic: 'switcher', id: 'me1', corr: 'mix-op-1', t: 'switcher.operation', data: { me: 'me1', program: 'cnn-usa', tick: 9, operation_id: 'mix-op-1', completed: true } });
    await waitFor(() => { expect(screen.getByText(/Patch take applied/)).toBeInTheDocument(); });
  });
});

describe('E3-05 events before the 202', () => {
  it('reconciles a terminal event that arrives before the POST resolves', async () => {
    planResponse = { can_take: true, class: 'class2', crosspoints: [], set: { new_encoders: 0, fits: true } };
    server.use(http.post('*/api/v1/patch/take', async ({ request }) => {
      takeRequests.push(await request.json());
      for (const listener of listeners) {
        listener({ v: 1, seq: 1, ts: 1, topic: 'outputs', t: 'patch.take', data: { operation_id: 'early-op', outcome: 'applied', tick: 4, destination: { output: 'hls-1' } } });
      }
      return HttpResponse.json({ operation_id: 'early-op', status: 'accepted' }, { status: 202 });
    }));
    const user = userEvent.setup();
    renderMatrix();
    await armAndClassify(user, 'cell-video-source:abc-news-output:hls-1:0', 'cell-video-source:abc-news-output:srt-1:0');
    await waitFor(() => { expect(screen.getByTestId('matrix-plan-line')).toBeInTheDocument(); });
    await user.click(screen.getByRole('button', { name: 'Take all' }));
    await waitFor(() => { expect(screen.getByText(/Patch take applied/)).toBeInTheDocument(); });
  });

  it('ignores an early event for an unrelated operation id', async () => {
    planResponse = { can_take: true, class: 'class2', crosspoints: [], set: { new_encoders: 0, fits: true } };
    const user = userEvent.setup();
    renderMatrix();
    await screen.findByTestId('cell-video-source:abc-news-output:hls-1:0');
    emitEnvelope({ v: 1, seq: 1, ts: 1, topic: 'outputs', t: 'patch.take', data: { operation_id: 'someone-else', outcome: 'applied', tick: 4, destination: { output: 'hls-1' } } });
    await armAndClassify(user, 'cell-video-source:abc-news-output:hls-1:0', 'cell-video-source:abc-news-output:srt-1:0');
    await waitFor(() => { expect(screen.getByTestId('matrix-plan-line')).toBeInTheDocument(); });
    await user.click(screen.getByRole('button', { name: 'Take all' }));
    await waitFor(() => { expect(takeRequests).toHaveLength(1); });
    expect(screen.queryByText(/Patch take applied/)).toBeNull();
    expect(document.querySelector('[data-phase="pending"]')).not.toBeNull();
  });
});

describe('E3-06 stale follows the engine tick', () => {
  it('a frozen tick (> 2 s) is stale: warning, unknown cells, takes still enabled', async () => {
    server.use(http.get('*/api/v1/patch', () => HttpResponse.json(mixerPatch(7))));
    const user = userEvent.setup();
    renderMatrix();
    await user.click(await screen.findByTestId('cell-video-source:abc-news-output:hls-1:0'));
    await waitFor(() => { expect(screen.getByTestId('matrix-page-state')).toHaveTextContent('stale'); }, { timeout: 5000 });
    expect(screen.getByTestId('matrix-stale-warning')).toBeInTheDocument();
    expect(screen.getByTestId('cell-video-source:cnn-usa-output:srt-1:0')).toHaveTextContent('?');
    expect(screen.getByRole('button', { name: 'Classify selection' })).toBeEnabled();
  }, 10_000);

  it('an advancing tick stays live even though each fetch is fresh', async () => {
    let tick = 100;
    server.use(http.get('*/api/v1/patch', () => { tick += 30; return HttpResponse.json(mixerPatch(tick)); }));
    renderMatrix();
    await screen.findByTestId('cell-video-source:cnn-usa-output:srt-1:0');
    await new Promise((resolve) => { setTimeout(resolve, 2600); });
    expect(screen.getByTestId('matrix-page-state')).toHaveTextContent('Live');
  }, 10_000);
});

describe('E3-07 malformed responses', () => {
  it('a video slot without pending is a readback error state, not a crash', async () => {
    server.use(http.get('*/api/v1/patch', () => HttpResponse.json({
      mixer: [],
      outputs: [{ output: 'hls-1', format: null, video: [{ slot: 0, live: true, source: { source: 'x' }, path: 'P1', degraded: false }], audio: [] }],
    })));
    renderMatrix();
    await waitFor(() => { expect(screen.getByTestId('matrix-page-state')).toHaveTextContent('Readback unavailable'); });
    expect(screen.getByTestId('matrix-readback-error')).toHaveTextContent('Invalid patch readback');
  });

  it('a plan with crosspoints: [null] shows an error and leaves Take disabled', async () => {
    planResponse = { can_take: true, class: 'class2', crosspoints: [null] };
    const user = userEvent.setup();
    renderMatrix();
    await armAndClassify(user, 'cell-video-source:abc-news-output:hls-1:0');
    expect(await screen.findByText('Invalid patch plan')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Take this crosspoint' })).toBeDisabled();
  });
});

describe('E3-08 plans bind to the selection revision', () => {
  it('a plan answered after the selection changed does not enable Take', async () => {
    let release: () => void = () => undefined;
    server.use(http.post('*/api/v1/patch/plan', async ({ request }) => {
      const body: unknown = await request.json();
      if (isEmptyPlan(body)) return HttpResponse.json({ status: 422 }, { status: 422 });
      planRequests.push(body);
      await new Promise<void>((resolve) => { release = resolve; });
      return HttpResponse.json({ can_take: true, class: 'class2', crosspoints: [] });
    }));
    const user = userEvent.setup();
    renderMatrix();
    await armAndClassify(user, 'cell-video-source:abc-news-output:hls-1:0');
    await waitFor(() => { expect(planRequests).toHaveLength(1); });
    await user.click(screen.getByTestId('cell-video-source:abc-news-output:srt-1:0'));
    release();
    await waitFor(() => { expect(screen.getByRole('button', { name: 'Classify selection' })).toBeEnabled(); });
    expect(screen.queryByTestId('matrix-plan-line')).toBeNull();
    expect(screen.getByRole('button', { name: 'Take all' })).toBeDisabled();
  });

  it('a recall plan does not survive choosing another salvo', async () => {
    server.use(http.get('*/api/v1/salvos', () => HttpResponse.json([
      { id: 'abc-salvo', display_name: 'ABC switch' },
      { id: 'cnn-salvo', display_name: 'CNN switch' },
    ])));
    const user = userEvent.setup();
    renderMatrix();
    await screen.findByTestId('cell-video-source:cnn-usa-output:srt-1:0');
    await user.click(screen.getByRole('combobox', { name: 'Salvo' }));
    await user.click(await screen.findByRole('option', { name: 'ABC switch' }));
    await user.click(screen.getByRole('button', { name: 'Classify salvo' }));
    await waitFor(() => { expect(screen.getByTestId('matrix-recall-plan-line')).toBeInTheDocument(); });
    await user.click(screen.getByRole('combobox', { name: 'Salvo' }));
    await user.click(await screen.findByRole('option', { name: 'CNN switch' }));
    expect(screen.queryByTestId('matrix-recall-plan-line')).toBeNull();
    expect(screen.getByRole('button', { name: 'Take salvo' })).toBeDisabled();
  });
});

describe('E3-09 planned and disabled glyphs', () => {
  it('an armed, planned member shows ◐ with its path; a refused member shows ⊘ with the refusal', async () => {
    planResponse = {
      can_take: false, class: 'class2', set: { new_encoders: 1, fits: true },
      crosspoints: [
        { destination: { output: 'hls-1', slot: 0 }, path: 'P3', on_air: false, refusal: null },
        { destination: { output: 'srt-1', slot: 0 }, path: 'P2', on_air: false, refusal: { type: 'feed-not-fresh', detail: 'abc-news is not fresh' } },
      ],
    };
    const user = userEvent.setup();
    renderMatrix();
    await armAndClassify(user, 'cell-video-source:abc-news-output:hls-1:0', 'cell-video-source:abc-news-output:srt-1:0');
    const planned = screen.getByTestId('cell-video-source:abc-news-output:hls-1:0');
    await waitFor(() => { expect(planned).toHaveTextContent('◐◇'); });
    const refused = screen.getByTestId('cell-video-source:abc-news-output:srt-1:0');
    expect(refused).toHaveTextContent('⊘⇢');
    expect(refused).toHaveAccessibleName(/disabled: abc-news is not fresh/);
    expect(refused).toBeEnabled();
  });
});

describe('E3-10 audio and subtitle planes', () => {
  it('the Audio tab lists audio feeds; only slot-0 Follow is armable, others carry the reason', async () => {
    const user = userEvent.setup();
    renderMatrix();
    await screen.findByTestId('cell-video-source:abc-news-output:hls-1:0');
    await user.click(screen.getByRole('tab', { name: 'Audio' }));
    expect(screen.getByTestId('cell-audio-audio:follow-output:hls-1:0')).toHaveTextContent('●');
    const source = screen.getByTestId('cell-audio-source:abc-news-output:hls-1:0');
    expect(source).toBeDisabled();
    expect(source).toHaveAccessibleName(/only audio slot 0 Follow is patchable/);
    expect(screen.queryByTestId('cell-audio-layout:news-one-output:hls-1:0')).toBeNull();
  });

  it('audio Follow alone cannot be planned; with its output video it is sent as plane audio follow', async () => {
    const programAudio = {
      mixer: [],
      outputs: [{ ...patchBody.outputs[0]!, audio: [{ slot: 0, source: 'program' }] }, patchBody.outputs[1]!],
    };
    server.use(http.get('*/api/v1/patch', () => HttpResponse.json(programAudio)));
    const user = userEvent.setup();
    renderMatrix();
    await screen.findByTestId('cell-video-source:abc-news-output:hls-1:0');
    await user.click(screen.getByRole('tab', { name: 'Audio' }));
    await user.click(screen.getByTestId('cell-audio-audio:follow-output:hls-1:0'));
    expect(screen.getByRole('button', { name: 'Classify selection' })).toBeDisabled();
    expect(screen.getAllByText(/Audio Follow rides its output's video take/).length).toBeGreaterThan(0);
    await user.click(screen.getByRole('tab', { name: 'Video' }));
    await user.click(screen.getByTestId('cell-video-source:abc-news-output:hls-1:0'));
    await user.click(screen.getByRole('button', { name: 'Classify selection' }));
    await waitFor(() => { expect(planRequests).toHaveLength(1); });
    expect(planRequests[0]).toEqual({
      crosspoints: [
        { plane: 'audio', destination: { output: 'hls-1', slot: 0 }, source: 'follow' },
        { plane: 'video', destination: { output: 'hls-1', slot: 0 }, source: { source: 'abc-news' } },
      ],
      on_air: 'protect',
    });
  });

  it('Subtitle cells are disabled with the reason and never arm a crosspoint', async () => {
    const withSubtitle = {
      mixer: [],
      outputs: [{ ...patchBody.outputs[0]!, subtitle: [{ slot: 0, format: 'webvtt', name: 'English', source: 'none' }] }],
    };
    server.use(
      http.get('*/api/v1/patch', () => HttpResponse.json(withSubtitle)),
      http.get('*/api/v1/sources', () => HttpResponse.json([
        { id: 'cnn-usa', name: 'CNN USA', body: { kind: 'srt', url: 'srt://[::1]:1', captions: { mode: 'cea608' } } },
      ])),
    );
    const user = userEvent.setup();
    renderMatrix();
    await screen.findByTestId('cell-video-source:cnn-usa-output:hls-1:0');
    await user.click(screen.getByRole('tab', { name: 'Subtitle' }));
    const cell = await screen.findByTestId('cell-subtitle-source:cnn-usa-output:hls-1:0');
    expect(cell).toBeDisabled();
    expect(cell).toHaveAccessibleName(/subtitle patching is not available/);
    expect(screen.getByRole('button', { name: 'Classify selection' })).toBeDisabled();
  });
});

describe('E3-11 group headings and built-in rows go through Lingui', () => {
  it('translates group headings and the built-in bus names at render', async () => {
    i18n.load('pseudo', pseudoMessages);
    i18n.activate('pseudo');
    try {
      renderMatrix();
      await screen.findByTestId('cell-video-source:cnn-usa-output:srt-1:0');
      const headings = screen.getAllByRole('columnheader').map((h) => h.textContent);
      expect(headings).not.toContain('Buses');
      expect(headings.length).toBeGreaterThan(3);
      const pseudoBuses = t`Buses`;
      expect(pseudoBuses).not.toBe('Buses');
      expect(headings).toContain(pseudoBuses);
      expect(screen.getAllByRole('rowheader').map((h) => h.textContent)).toContain(t`Program`);
      expect(headings).not.toContain('Sources');
      const rowHeaders = screen.getAllByRole('rowheader').map((h) => h.textContent);
      expect(rowHeaders).not.toContain('Program');
      expect(rowHeaders).toContain('CNN USA');
    } finally {
      i18n.activate('en');
    }
  });
});
