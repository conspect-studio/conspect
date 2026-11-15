// SubtitlePatchPanel (ADR-0110 §12.8, #345 part F3): matrix rendering (incl.
// redaction and live:false), the plan+take request bodies and
// Idempotency-Key, the `source-has-no-captions` note, typed refusal display,
// the realtime `subtitle:applied` / `subtitle:refused:<reason>` outcome
// (including buffered-before-202 delivery), output-name resolution, the
// always-present per-column status region, and per-destination attempt
// ownership (a column disabled while pending; an older attempt's late
// response never overwrites a newer attempt's result — review UI2-1).
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
import { EMPTY_SUBTITLE_ATTEMPTS, subtitleAttemptsReducer } from './subtitlePatchAttempts';

function emit(data: Record<string, unknown>): void {
  act(() => {
    for (const listener of listeners) {
      listener({ v: 1, t: 'route.output', topic: 'outputs', seq: 1, ts: 1, data });
    }
  });
}

/**
 * Deliver several `route.output` envelopes inside ONE `act()` (review
 * UI3-1): a naive side-channel implementation settled the first
 * destination correctly but could strand a second destination settling
 * in the same batch; the reducer (one state transition per dispatched
 * action, no data carried outside it) must not.
 */
function emitAll(events: readonly Record<string, unknown>[]): void {
  act(() => {
    for (const data of events) {
      for (const listener of listeners) {
        listener({ v: 1, t: 'route.output', topic: 'outputs', seq: 1, ts: 1, data });
      }
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
      // The unified `GET /patch` output shape (E3 + patch-f): the server always
      // reports the video/audio readback alongside the subtitle slots.
      video: [],
      audio: [],
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

  // UI2-1 (review round 2): two attempts on the SAME destination used to
  // overwrite each other's feedback regardless of order. The disabled
  // button (tested separately below) prevents a user-driven double click
  // from starting a second attempt while one is outstanding, but the
  // attempt-token guard itself — exercised directly here, independent of
  // the disabled-button affordance — is the actual fix: a newer attempt's
  // settled result (here, a plan refusal) must never be clobbered by an
  // older attempt's late-arriving realtime outcome.
  it('an older operation id\'s outcome is ignored once a newer attempt on the same destination has settled (UI2-1 fix)', async () => {
    holdTake = true;
    takeOperationId = 'op-A';
    renderWithProviders(<SubtitlePatchPanel />);
    const cell = await screen.findByTestId('subtitle-cell-cam-1-out-1:1');
    const button = within(cell).getByRole('button');

    // Attempt A: accepted as op-A, held before the 202 arrives.
    const clickA = userEvent.click(button);
    await waitFor(() => {
      expect(takeBody).not.toBeNull();
    });
    expect(screen.getByTestId('subtitle-status-out-1:1').textContent).toBe('pending');
    expect(button).toBeDisabled();

    // Release A's take response so it resolves (op-A accepted) and settle
    // it via its own realtime outcome, so the destination becomes editable
    // again and attempt B can legitimately start.
    releaseTake?.();
    await clickA;
    emit({ operation_id: 'op-A', outcome: 'subtitle:applied', output: 'out-1', tick: 1 });
    await waitFor(() => {
      expect(button).not.toBeDisabled();
    });

    // Attempt B on the SAME destination, now that A is no longer pending:
    // refused in planning. B is logically newer and must win.
    planRefusal = { type: 'on-air-protected', detail: 'B refused' };
    takeOperationId = 'op-B';
    await userEvent.click(button);
    await waitFor(() => {
      expect(screen.getByTestId('subtitle-status-out-1:1').textContent).toBe('B refused');
    });

    // op-A's outcome now arrives late: it must not overwrite B's refusal,
    // because the cell's `operationId` was overwritten away from op-A
    // (first to '' by attempt B's start, then never back to op-A) the
    // moment attempt B started — the realtime listener's `operationId ===
    // event.operation_id` match therefore no longer matches this cell.
    emit({ operation_id: 'op-A', outcome: 'subtitle:applied', output: 'out-1', tick: 1 });
    expect(screen.getByTestId('subtitle-status-out-1:1').textContent).toBe('B refused');
  });

  it('disables a destination\'s buttons (aria-disabled) while an attempt on it is pending (UI2-1 fix)', async () => {
    holdTake = true;
    renderWithProviders(<SubtitlePatchPanel />);
    const patchCell = await screen.findByTestId('subtitle-cell-cam-1-out-1:1');
    const noneCell = screen.getByTestId('subtitle-cell-none-out-1:1');
    const patchButton = within(patchCell).getByRole('button');
    const noneButton = within(noneCell).getByRole('button');

    expect(patchButton).not.toBeDisabled();
    expect(noneButton).not.toBeDisabled();
    await userEvent.click(patchButton);
    await waitFor(() => {
      expect(takeBody).not.toBeNull();
    });
    // The whole destination (both the patch button and the None button for
    // this slot) is disabled while the attempt is in flight.
    expect(patchButton).toBeDisabled();
    expect(patchButton.getAttribute('aria-disabled')).toBe('true');
    expect(noneButton).toBeDisabled();
    // A different destination (a different slot) is unaffected.
    const otherSlotButton = within(screen.getByTestId('subtitle-cell-cam-1-out-1:0')).getByRole('button');
    expect(otherSlotButton).not.toBeDisabled();

    // Releasing the take response lets `takePatch` resolve and the cell's
    // `operationId` get set, but that happens asynchronously (a few
    // microtasks through `fetch`/`response.json()`); "pending" alone does
    // not distinguish "take still in flight" from "accepted, awaiting
    // outcome" (both read "pending" in the status region), so the outcome
    // is re-emitted inside `waitFor` until it actually lands on the right
    // operation id — i.e. until the take's own state update has landed.
    releaseTake?.();
    expect(patchButton).toBeDisabled();
    expect(noneButton).toBeDisabled();
    await waitFor(() => {
      emit({ operation_id: takeOperationId, outcome: 'subtitle:applied', output: 'out-1', tick: 2 });
      expect(screen.getByTestId('subtitle-status-out-1:1').textContent).toBe('subtitle:applied');
    });
    expect(patchButton).not.toBeDisabled();
    expect(noneButton).not.toBeDisabled();
  });

  // UI3-1 (review round 3): outcome settlement used to go through a side
  // channel (an array populated inside a `setCells` updater and consumed
  // right after the `setCells` call) that could desynchronize from a
  // SECOND state (`pendingKeys`) when two destinations settle in the same
  // batch — the first destination's updater ran and its side-channel array
  // was drained, but the second updater (React may run an updater more
  // than once, or defer it) produced its own result after the drain had
  // already happened, stranding that destination "busy" forever despite a
  // successful outcome. The reducer fixes this by making `busy` part of
  // the SAME state transition as the outcome, for every destination the
  // one `outcome` action settles, in one `act()`.
  it('settles two different pending destinations whose outcomes are delivered in ONE act(), re-enabling both (UI3-1 fix)', async () => {
    holdTake = true;
    renderWithProviders(<SubtitlePatchPanel />);
    const cellA = await screen.findByTestId('subtitle-cell-cam-1-out-1:0');
    const cellB = screen.getByTestId('subtitle-cell-none-out-1:1');
    const buttonA = within(cellA).getByRole('button');
    const buttonB = within(cellB).getByRole('button');

    // Start attempt A on destination out-1:0 (accepted as op-X), holding
    // its take.
    takeOperationId = 'op-X';
    const clickA = userEvent.click(buttonA);
    await waitFor(() => {
      expect(takeBody).not.toBeNull();
    });
    releaseTake?.();
    await clickA;
    await waitFor(() => {
      expect(screen.getByTestId('subtitle-status-out-1:0').textContent).toBe('pending');
    });
    expect(buttonA).toBeDisabled();

    // Start attempt B on a DIFFERENT destination, out-1:1 (accepted as
    // op-Y), holding its take too.
    holdTake = true;
    takeOperationId = 'op-Y';
    const clickB = userEvent.click(buttonB);
    await waitFor(() => {
      expect(takeBody).toEqual({
        crosspoints: [{ plane: 'subtitle', destination: { output: 'out-1', slot: 1 }, source: 'none' }],
      });
    });
    releaseTake?.();
    await clickB;
    await waitFor(() => {
      expect(screen.getByTestId('subtitle-status-out-1:1').textContent).toBe('pending');
    });
    expect(buttonB).toBeDisabled();

    // Both destinations are now accepted and pending (busy), each with its
    // own operation id. Deliver BOTH outcomes inside ONE act().
    emitAll([
      { operation_id: 'op-X', outcome: 'subtitle:applied', output: 'out-1', tick: 1 },
      { operation_id: 'op-Y', outcome: 'subtitle:applied', output: 'out-1', tick: 2 },
    ]);

    expect(screen.getByTestId('subtitle-status-out-1:0').textContent).toBe('subtitle:applied');
    expect(screen.getByTestId('subtitle-status-out-1:1').textContent).toBe('subtitle:applied');
    expect(buttonA).not.toBeDisabled();
    expect(buttonB).not.toBeDisabled();
  });

  describe('subtitleAttemptsReducer (pure): the attempt-token guard directly', () => {
    // UI3-1 (review round 3) + UI2-1: "a direct attempt-token test where
    // A's take is still held when B starts on the same destination via
    // the guard". The UI disables a destination's buttons while an
    // attempt is pending (tested above through the real component), so
    // reproducing "B starts while A's take is still held" at the DOM
    // level would require bypassing that disabling; the reducer's own
    // actions are "the component's API" the attempt-token guard is built
    // on (`runTake` dispatches exactly these actions), so driving them
    // directly exercises the guard itself, independent of the button.
    it('A\'s late "accepted" is ignored once B has started on the same destination (A\'s take still conceptually held)', () => {
      const started = subtitleAttemptsReducer(EMPTY_SUBTITLE_ATTEMPTS, {
        type: 'start',
        key: 'out-1:1',
        attempt: 1, // attempt A dispatched; its take is now in flight ("held").
      });
      expect(started['out-1:1']).toEqual({ attempt: 1, busy: true, operationId: '' });

      // B starts on the SAME destination while A's take has not resolved
      // (no `accepted`/`takeFailed`/`planRefused` action for A has been
      // dispatched yet) — exactly "A's take is still held when B starts".
      const bStarted = subtitleAttemptsReducer(started, {
        type: 'start',
        key: 'out-1:1',
        attempt: 2,
      });
      expect(bStarted['out-1:1']).toEqual({ attempt: 2, busy: true, operationId: '' });

      // A's take now (late) resolves with a 202: its `accepted` action
      // carries attempt 1, no longer current (current is 2) — ignored.
      const aAccepted = subtitleAttemptsReducer(bStarted, {
        type: 'accepted',
        key: 'out-1:1',
        attempt: 1,
        operationId: 'op-A',
      });
      expect(aAccepted).toBe(bStarted); // same reference: a true no-op.

      // B is refused in planning: its action carries the current attempt
      // (2) and is applied.
      const bRefused = subtitleAttemptsReducer(aAccepted, {
        type: 'planRefused',
        key: 'out-1:1',
        attempt: 2,
        message: 'B refused',
      });
      expect(bRefused['out-1:1']).toEqual({ attempt: 2, busy: false, operationId: '', error: 'B refused' });

      // A's operation id (op-A) now delivers its realtime outcome, after
      // B's start: ignored, because no destination's CURRENT `operationId`
      // is `op-A` (A's `accepted` never wrote it — the no-op above).
      const afterLateOutcome = subtitleAttemptsReducer(bRefused, {
        type: 'outcome',
        operationId: 'op-A',
        outcome: 'subtitle:applied',
      });
      expect(afterLateOutcome).toBe(bRefused); // unchanged: B's refusal stands.
    });
  });

  describe('subtitleAttemptsReducer (pure)', () => {
    it('accepted with no attempt match (a newer attempt already started) leaves state unchanged', () => {
      const state = { 'out-1:0': { attempt: 2, busy: true, operationId: '' } };
      const next = subtitleAttemptsReducer(state, {
        type: 'accepted',
        key: 'out-1:0',
        attempt: 1, // stale: the current attempt is 2
        operationId: 'op-stale',
      });
      expect(next).toBe(state); // same reference: no-op.
    });

    it('outcome settles every destination whose current operationId matches, in one transition', () => {
      const state = {
        'out-1:0': { attempt: 1, busy: true, operationId: 'op-X' },
        'out-1:1': { attempt: 1, busy: true, operationId: 'op-Y' },
      };
      const afterX = subtitleAttemptsReducer(state, {
        type: 'outcome',
        operationId: 'op-X',
        outcome: 'subtitle:applied',
      });
      const afterY = subtitleAttemptsReducer(afterX, {
        type: 'outcome',
        operationId: 'op-Y',
        outcome: 'subtitle:applied',
      });
      expect(afterY['out-1:0']).toEqual({ attempt: 1, busy: false, operationId: 'op-X', outcome: 'subtitle:applied' });
      expect(afterY['out-1:1']).toEqual({ attempt: 1, busy: false, operationId: 'op-Y', outcome: 'subtitle:applied' });
    });

    it('an outcome for a stale operationId (superseded by a newer attempt) touches nothing', () => {
      const state = {
        'out-1:0': { attempt: 2, busy: false, operationId: 'op-B', error: 'B refused' },
      };
      const next = subtitleAttemptsReducer(state, {
        type: 'outcome',
        operationId: 'op-A', // belongs to attempt 1, no longer current
        outcome: 'subtitle:applied',
      });
      expect(next).toBe(state);
    });

    it('EMPTY_SUBTITLE_ATTEMPTS is the initial reducer state', () => {
      expect(EMPTY_SUBTITLE_ATTEMPTS).toEqual({});
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
