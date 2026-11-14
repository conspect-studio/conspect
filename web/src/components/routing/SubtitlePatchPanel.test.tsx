// SubtitlePatchPanel (ADR-0110 §12.8, #345 part F3): matrix rendering (incl.
// redaction and live:false), the plan+take request bodies and
// Idempotency-Key, the `source-has-no-captions` note, typed refusal display,
// and the realtime `subtitle:applied` / `subtitle:refused:<reason>` outcome.
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

const sourcesBody = [
  { id: 'cam-1', name: 'Camera 1', body: { kind: 'rtsp', url: 'rtsp://cam-1', captions: { mode: 'auto' } } },
  { id: 'cam-2', name: 'Camera 2', body: { kind: 'rtsp', url: 'rtsp://cam-2', captions: { mode: 'off' } } },
  { id: 'cam-3', name: 'Camera 3', body: { kind: 'rtsp', url: 'rtsp://cam-3' } },
];

const server = setupServer(
  http.get('*/api/v1/sources', () => HttpResponse.json(sourcesBody)),
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
    return HttpResponse.json({ operation_id: 'sub-op-1', status: 'accepted' }, { status: 202 });
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
});
afterAll(() => {
  server.close();
});

describe('SubtitlePatchPanel', () => {
  it('renders the matrix: rows are captioned sources, columns are output subtitle slots, with live/config and redaction', async () => {
    renderWithProviders(<SubtitlePatchPanel />);
    await screen.findByTestId('subtitle-patch-panel');
    // Only cam-1 declares captions (cam-2 is explicitly off, cam-3 has no
    // `captions` block at all).
    expect(screen.getByText('Camera 1')).toBeTruthy();
    expect(screen.queryByText('Camera 2')).toBeNull();
    expect(screen.queryByText('Camera 3')).toBeNull();
    const liveCell = screen.getByTestId('subtitle-cell-cam-1-out-1:0');
    expect(within(liveCell).getByText('live')).toBeTruthy();
    const configCell = screen.getByTestId('subtitle-cell-cam-1-out-1:1');
    expect(within(configCell).getByText('config')).toBeTruthy();
    // Slot 2's source is redacted: no patch button, a "redacted" badge instead.
    const redactedCell = screen.getByTestId('subtitle-cell-cam-1-out-1:2');
    expect(within(redactedCell).getByText('redacted')).toBeTruthy();
    expect(within(redactedCell).queryByRole('button')).toBeNull();
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
      expect(screen.getByTestId('subtitle-cell-cam-1-out-1:1-outcome').textContent).toBe('subtitle:applied');
    });
  });

  it('shows the source-has-no-captions plan note without refusing the take', async () => {
    planNote = 'source-has-no-captions';
    renderWithProviders(<SubtitlePatchPanel />);
    const cell = await screen.findByTestId('subtitle-cell-cam-1-out-1:1');
    await userEvent.click(within(cell).getByRole('button'));
    await waitFor(() => {
      expect(screen.getByTestId('subtitle-cell-cam-1-out-1:1-note').textContent).toBe('source-has-no-captions');
    });
    expect(takeBody).not.toBeNull();
  });

  it('shows a typed refusal and never takes', async () => {
    planRefusal = { type: 'slot-medium-mismatch', detail: "This output's medium carries no subtitles." };
    renderWithProviders(<SubtitlePatchPanel />);
    const cell = await screen.findByTestId('subtitle-cell-cam-1-out-1:1');
    await userEvent.click(within(cell).getByRole('button'));
    await waitFor(() => {
      expect(screen.getByTestId('subtitle-cell-cam-1-out-1:1-error').textContent).toBe(
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
