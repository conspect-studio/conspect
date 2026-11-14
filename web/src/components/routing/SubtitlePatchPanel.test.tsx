// SubtitlePatchPanel (ADR-0110 §12.8, #345 part F3): matrix rendering (incl.
// redaction and live:false), the plan+take request bodies and
// Idempotency-Key, the `source-has-no-captions` note, typed refusal display,
// the realtime `subtitle:applied` / `subtitle:refused:<reason>` outcome
// (including buffered-before-202 delivery), output-name resolution and the
// always-present per-column status region.
import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { act, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import type { Envelope } from '../../realtime/envelope';

type Listener = (envelope: Envelope) => void;
const listeners = new Set<Listener>();
vi.mock('../../realtime/shared', () => ({
  subscribeRealtime: (_topic: string, listener: Listener): (() => void) => {
    listeners.add(listener);
    return () => listeners.delete(listener);
  },
}));

import { renderWithProviders } from '../../test/render';
import { SubtitlePatchPanel } from './SubtitlePatchPanel';

function emit(data: Record<string, unknown>): void {
  act(() => {
    for (const listener of listeners) {
      listener({ v: 1, t: 'route.output', topic: 'outputs', seq: 1, ts: 1, data });
    }
  });
}

let takeBody: unknown = null;
let takeKey: string | null = null;
let planBody: unknown = null;
let planNote: string | undefined;
let planRefusal: { type: string; detail: string } | undefined;
let takeOperationId = 'sub-op-1';
/** When true, the take handler blocks until `releaseTake()` is called. */
let holdTake = false;
let releaseTake: (() => void) | undefined;

const patchBody = {
  mixer: [],
  outputs: [
    {
      output: 'out-1',
      subtitle: [
        { slot: 0, format: 'webvtt', language: 'en', name: null, live: true, source: { source: 'cam-1' } },
        { slot: 1, format: 'webvtt', language: 'es', name: null, live: false, source: null },
        { slot: 2, format: 'webvtt', language: 'fr', name: null, live: true, source: { redacted: true } },
      ],
    },
  ],
};

// UI-4: `out-1` is used as BOTH an output id and (deliberately) a source
// id, with different names, to prove the column label comes from the
// output resource list, never from a same-id source.
const sourcesBody = [
  { id: 'cam-1', name: 'Camera 1', body: { kind: 'rtsp', url: 'rtsp://cam-1', captions: { mode: 'auto' } } },
  { id: 'cam-2', name: 'Camera 2', body: { kind: 'rtsp', url: 'rtsp://cam-2', captions: { mode: 'off' } } },
  { id: 'cam-3', name: 'Camera 3', body: { kind: 'rtsp', url: 'rtsp://cam-3' } },
  { id: 'out-1', name: 'Decoy Source Named Like The Output Id', body: { kind: 'rtsp', url: 'rtsp://decoy' } },
];
const outputsBody = [{ id: 'out-1', name: 'Main HLS Output', body: { kind: 'hls', path: '/srv/hls' } }];

const server = setupServer(
  http.get('*/api/v1/sources', () => HttpResponse.json(sourcesBody)),
  http.get('*/api/v1/outputs', () => HttpResponse.json(outputsBody)),
  http.get('*/api/v1/patch', () => HttpResponse.json(patchBody)),
  http.post('*/api/v1/patch/plan', async ({ request }) => {
    planBody = await request.json();
    const crosspoint = (planBody as { crosspoints: unknown[] }).crosspoints[0];
    return HttpResponse.json({
      can_take: planRefusal === undefined,
      class: 'class1',
      crosspoints: [
        {
          destination: (crosspoint as { destination: unknown }).destination,
          on_air: false,
          path: 'subtitle',
          ...(planNote !== undefined ? { note: planNote } : {}),
          ...(planRefusal !== undefined ? { refusal: planRefusal } : {}),
        },
      ],
    });
  }),
  http.post('*/api/v1/patch/take', async ({ request }) => {
    takeBody = await request.json();
    takeKey = request.headers.get('Idempotency-Key');
    if (holdTake) {
      await new Promise<void>((resolve) => {
        releaseTake = resolve;
      });
    }
    return HttpResponse.json({ operation_id: takeOperationId, status: 'accepted' }, { status: 202 });
  }),
);

beforeAll(() => {
  server.listen({ onUnhandledRequest: 'error' });
});
afterEach(() => {
  server.resetHandlers();
  listeners.clear();
  takeBody = null;
  takeKey = null;
  planBody = null;
  planNote = undefined;
  planRefusal = undefined;
  takeOperationId = 'sub-op-1';
  holdTake = false;
  releaseTake = undefined;
});
afterAll(() => {
  server.close();
});

describe('SubtitlePatchPanel', () => {
  it('renders the matrix: rows are captioned sources, columns are output subtitle slots, with live/config and redaction', async () => {
    renderWithProviders(<SubtitlePatchPanel />);
    await screen.findByTestId('subtitle-patch-panel');
    // Only cam-1 declares captions (cam-2 is explicitly off, cam-3 has no
    // `captions` block at all; the decoy `out-1` source also has none).
    expect(screen.getByText('Camera 1')).toBeTruthy();
    expect(screen.queryByText('Camera 2')).toBeNull();
    expect(screen.queryByText('Camera 3')).toBeNull();
    expect(screen.queryByText('Decoy Source Named Like The Output Id')).toBeNull();
    const liveCell = screen.getByTestId('subtitle-cell-cam-1-out-1:0');
    expect(within(liveCell).getByText('live')).toBeTruthy();
    const configCell = screen.getByTestId('subtitle-cell-cam-1-out-1:1');
    expect(within(configCell).getByText('config')).toBeTruthy();
    // Slot 2's source is redacted: no patch button, a "redacted" badge instead.
    const redactedCell = screen.getByTestId('subtitle-cell-cam-1-out-1:2');
    expect(within(redactedCell).getByText('redacted')).toBeTruthy();
    expect(within(redactedCell).queryByRole('button')).toBeNull();
  });

  // UI-4 (review): the column label must come from GET /outputs, never
  // from a same-id source — otherwise an operator could patch the wrong
  // output under a misleading label.
  it('labels the column from the output resource, not a same-id source (UI-4 fix)', async () => {
    renderWithProviders(<SubtitlePatchPanel />);
    await screen.findByTestId('subtitle-patch-panel');
    const headers = screen.getAllByRole('columnheader', { name: /Main HLS Output/ });
    expect(headers.length).toBeGreaterThan(0);
    expect(screen.queryByRole('columnheader', { name: /Decoy Source/ })).toBeNull();
    expect(
      within(screen.getByTestId('subtitle-cell-cam-1-out-1:0')).getByRole('button').getAttribute('aria-label'),
    ).toContain('Main HLS Output');
  });

  it('plans then takes with a fresh Idempotency-Key on a cell click, and shows the applied outcome', async () => {
    renderWithProviders(<SubtitlePatchPanel />);
    const cell = await screen.findByTestId('subtitle-cell-cam-1-out-1:1');
    await userEvent.click(within(cell).getByRole('button'));
    await waitFor(() => {
      expect(takeBody).toEqual({
        crosspoints: [
          { plane: 'subtitle', destination: { output: 'out-1', slot: 1 }, source: { source: 'cam-1' } },
        ],
      });
    });
    expect(planBody).toEqual({
      crosspoints: [
        { plane: 'subtitle', destination: { output: 'out-1', slot: 1 }, source: { source: 'cam-1' } },
      ],
    });
    expect(takeKey).toBeTruthy();
    emit({ operation_id: 'sub-op-1', outcome: 'subtitle:applied', output: 'out-1', tick: 7 });
    await waitFor(() => {
      expect(screen.getByTestId('subtitle-status-out-1:1').textContent).toBe('subtitle:applied');
    });
  });

  it('shows the source-has-no-captions plan note without refusing the take', async () => {
    planNote = 'source-has-no-captions';
    renderWithProviders(<SubtitlePatchPanel />);
    const cell = await screen.findByTestId('subtitle-cell-cam-1-out-1:1');
    await userEvent.click(within(cell).getByRole('button'));
    await waitFor(() => {
      expect(screen.getByTestId('subtitle-status-out-1:1').textContent).toBe('source-has-no-captions');
    });
    expect(takeBody).not.toBeNull();
  });

  it('shows a typed refusal and never takes', async () => {
    planRefusal = { type: 'slot-medium-mismatch', detail: "This output's medium carries no subtitles." };
    renderWithProviders(<SubtitlePatchPanel />);
    const cell = await screen.findByTestId('subtitle-cell-cam-1-out-1:1');
    await userEvent.click(within(cell).getByRole('button'));
    await waitFor(() => {
      expect(screen.getByTestId('subtitle-status-out-1:1').textContent).toBe(
        "This output's medium carries no subtitles.",
      );
    });
    expect(takeBody).toBeNull();
  });

  it('unpatches via the None row', async () => {
    renderWithProviders(<SubtitlePatchPanel />);
    const cell = await screen.findByTestId('subtitle-cell-none-out-1:0');
    await userEvent.click(within(cell).getByRole('button'));
    await waitFor(() => {
      expect(takeBody).toEqual({
        crosspoints: [{ plane: 'subtitle', destination: { output: 'out-1', slot: 0 }, source: 'none' }],
      });
    });
  });

  // UI-1 (review): the realtime outcome can arrive before `takePatch`'s own
  // HTTP response resolves. A listener that only matches a cell already
  // carrying the operation id drops this outcome forever — a refusal would
  // then show nothing at all. Both applied and refused variants are
  // checked.
  it('reconciles an outcome that arrives before the 202 response (applied) (UI-1 fix)', async () => {
    holdTake = true;
    renderWithProviders(<SubtitlePatchPanel />);
    const cell = await screen.findByTestId('subtitle-cell-cam-1-out-1:1');
    const click = userEvent.click(within(cell).getByRole('button'));
    // Wait for the take POST to be in flight (held) before the event
    // arrives — the realtime outcome beats the HTTP response.
    await waitFor(() => {
      expect(takeBody).not.toBeNull();
    });
    emit({ operation_id: 'sub-op-1', outcome: 'subtitle:applied', output: 'out-1', tick: 9 });
    releaseTake?.();
    await click;
    await waitFor(() => {
      expect(screen.getByTestId('subtitle-status-out-1:1').textContent).toBe('subtitle:applied');
    });
  });

  it('reconciles an outcome that arrives before the 202 response (refused) (UI-1 fix)', async () => {
    holdTake = true;
    renderWithProviders(<SubtitlePatchPanel />);
    const cell = await screen.findByTestId('subtitle-cell-cam-1-out-1:1');
    const click = userEvent.click(within(cell).getByRole('button'));
    await waitFor(() => {
      expect(takeBody).not.toBeNull();
    });
    emit({ operation_id: 'sub-op-1', outcome: 'subtitle:refused:not-in-run', output: 'out-1', tick: 0 });
    releaseTake?.();
    await click;
    await waitFor(() => {
      expect(screen.getByTestId('subtitle-status-out-1:1').textContent).toBe('subtitle:refused:not-in-run');
    });
  });

  // UI-5 (review): with zero captioned sources, only the None row exists,
  // and its own destination had no visible place to show a failure before
  // this fix (status lived only inside caption-source rows).
  it('shows a failed unpatch even with zero captioned sources, via the always-present status row (UI-5 fix)', async () => {
    server.use(http.get('*/api/v1/sources', () => HttpResponse.json([])));
    planRefusal = { type: 'on-air-protected', detail: 'This slot is protected.' };
    renderWithProviders(<SubtitlePatchPanel />);
    await screen.findByTestId('subtitle-patch-panel');
    expect(screen.getByText('No source declares captions.')).toBeTruthy();
    const noneCell = screen.getByTestId('subtitle-cell-none-out-1:0');
    await userEvent.click(within(noneCell).getByRole('button'));
    const statusCell = await screen.findByTestId('subtitle-status-out-1:0');
    await waitFor(() => {
      expect(statusCell.textContent).toBe('This slot is protected.');
    });
    // The status cell is an accessible live region, not merely text.
    expect(within(statusCell).getByRole('status')).toBeTruthy();
  });

  it('every column has an accessible, always-present status region (aria-live) even with no pending change', async () => {
    renderWithProviders(<SubtitlePatchPanel />);
    await screen.findByTestId('subtitle-patch-panel');
    const statusRow = screen.getByTestId('subtitle-status-row');
    const regions = within(statusRow).getAllByRole('status');
    expect(regions.length).toBe(3); // one per output subtitle slot (out-1 declares 3).
    for (const region of regions) {
      expect(region.getAttribute('aria-live')).toBe('polite');
    }
  });

  it('every cell button and table header is keyboard-operable and labelled', async () => {
    renderWithProviders(<SubtitlePatchPanel />);
    await screen.findByTestId('subtitle-patch-panel');
    const table = screen.getByRole('table');
    expect(within(table).getAllByRole('columnheader').length).toBeGreaterThan(0);
    const button = within(screen.getByTestId('subtitle-cell-cam-1-out-1:0')).getByRole('button');
    expect(button).toHaveAccessibleName();
    button.focus();
    expect(document.activeElement).toBe(button);
  });
});
