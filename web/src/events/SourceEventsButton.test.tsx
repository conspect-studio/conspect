import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { act, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { renderWithProviders } from '../test/render';
import { SourceEventsButton } from './SourceEventsButton';
import { nanosecondsToSeconds } from './sourceEvents';
import type { SourceEvent, SourceEvents } from './sourceEvents';

const row = (changes: Partial<SourceEvent> = {}): SourceEvent => ({
  event_id: 'cue-1', epoch: '9007199254740993', origin: 'scte35', vendor: null,
  lifecycle: 'active', timer: { kind: 'remaining', nanoseconds: '9007199254740993' },
  asset_id: null, correlation_id: null, ...changes,
});
const snapshot = (events = [row()]): SourceEvents => ({
  source_id: 'source-a', epoch: '9007199254740993', availability: 'available', degraded: false, events,
});
const server = setupServer();
beforeAll(() => { server.listen({ onUnhandledRequest: 'error' }); });
afterEach(() => { server.resetHandlers(); vi.useRealTimers(); });
afterAll(() => { server.close(); });
const endpoint = '*/api/v1/inputs/:id/events';
const testEndpoint = '*/api/v1/inputs/:id/events/test';
async function open(eventObservation?: 'observing' | 'disabled' | 'not_supported') {
  const view = renderWithProviders(
    eventObservation === undefined
      ? <SourceEventsButton id="source-a" name="Camera A" />
      : <SourceEventsButton id="source-a" name="Camera A" eventObservation={eventObservation} />,
  );
  await userEvent.click(screen.getByRole('button', { name: 'Events: Camera A' }));
  return view;
}

describe('read-only source events', () => {
  it('converts decimal nanoseconds exactly without Number precision loss', () => {
    expect(nanosecondsToSeconds('9007199254740993')).toBe('9007199.254740993');
    expect(nanosecondsToSeconds('18446744073709551615')).toBe('18446744073.709551615');
    expect(nanosecondsToSeconds('0')).toBe('0');
    expect(nanosecondsToSeconds('1e9')).toBeUndefined();
    expect(nanosecondsToSeconds('-1')).toBeUndefined();
    expect(nanosecondsToSeconds('18446744073709551616')).toBeUndefined();
    expect(nanosecondsToSeconds('1'.repeat(10000))).toBeUndefined();
  });

  it('renders known, upcoming, unknown duration and overrun readouts with provenance as plain text', async () => {
    const unsafe = '<img src=x onerror=alert(1)>';
    server.use(http.get(endpoint, () => HttpResponse.json(snapshot([
      row({ asset_id: unsafe }),
      row({ event_id: 'upcoming', lifecycle: 'announced', timer: { kind: 'until_start', nanoseconds: '2000000000' } }),
      row({ event_id: 'unknown', timer: { kind: 'elapsed', nanoseconds: '3000000000' } }),
      row({ event_id: 'marker', lifecycle: 'observed', timer: null, origin: 'vendor_marker', vendor: unsafe }),
      row({ event_id: 'overrun', lifecycle: 'uncertain', timer: { kind: 'overrun', nanoseconds: '4000000000' } }),
      row({ event_id: 'uncertain', lifecycle: 'uncertain', timer: null }),
    ]))));
    await open();
    expect(await screen.findByText('Expected return in: 9007199.254740993 seconds')).toBeInTheDocument();
    expect(screen.getByText('Until start: 2 seconds')).toBeInTheDocument();
    expect(screen.getByText('Elapsed: 3 seconds')).toBeInTheDocument();
    expect(screen.getByText('Past expected return (end unconfirmed): 4 seconds')).toBeInTheDocument();
    expect(screen.getAllByText('Uncertain')).toHaveLength(2);
    expect(screen.getByText(`Asset: ${unsafe}`)).toBeInTheDocument();
    expect(screen.getByText(/Vendor marker \(unverified presence only\)/)).toBeInTheDocument();
    expect(document.querySelector('img')).toBeNull();
    expect(screen.getByText(/Read-only server snapshot/)).toBeInTheDocument();
  });

  it('distinguishes degraded/unavailable evidence from a successful empty snapshot', async () => {
    server.use(http.get(endpoint, () => HttpResponse.json({ ...snapshot([]), availability: 'stale', degraded: true })));
    await open();
    expect(await screen.findByText(/Degraded event evidence/)).toBeInTheDocument();
    expect(screen.getByText(/Timing is uncertain/)).toBeInTheDocument();
    expect(screen.getByText(/No source events in the current server snapshot/)).toBeInTheDocument();
  });

  it.each([
    [401, 'Authentication required'], [403, 'You do not have permission'],
    [404, 'This source was not found'], [503, 'Source event service is unavailable'],
  ])('shows HTTP %s honestly, not as no events', async (status, text) => {
    server.use(http.get(endpoint, () => HttpResponse.json({}, { status })));
    await open();
    expect(await screen.findByRole('alert')).toHaveTextContent(text);
    expect(screen.queryByText(/No source events/)).not.toBeInTheDocument();
  });

  it('polls server updates, clears cancelled timing, and stops requests on close', async () => {
    let calls = 0;
    let response = snapshot();
    server.use(http.get(endpoint, () => { calls++; return HttpResponse.json(response); }));
    await open();
    await screen.findByText(/Expected return in:/);
    response = snapshot([row({ lifecycle: 'cancelled', timer: null })]);
    await screen.findByText('Cancelled', {}, { timeout: 2500 });
    expect(screen.queryByText(/Expected return in:/)).not.toBeInTheDocument();
    await userEvent.keyboard('{Escape}');
    const closedCalls = calls;
    vi.useFakeTimers();
    await act(async () => { await vi.advanceTimersByTimeAsync(3000); });
    expect(calls).toBe(closedCalls);
  });

  it('hides a prior successful readout when a later poll fails', async () => {
    let failed = false;
    server.use(http.get(endpoint, () => failed ? HttpResponse.json({}, { status: 503 }) : HttpResponse.json(snapshot())));
    await open();
    await screen.findByText(/Expected return in:/);
    failed = true;
    expect(await screen.findByRole('alert', {}, { timeout: 2500 })).toHaveTextContent('Source event service is unavailable');
    expect(screen.queryByText(/Expected return in:/)).not.toBeInTheDocument();
  });

  it('does not fetch until opened and aborts an in-flight request on close', async () => {
    let requestSignal: AbortSignal | undefined;
    server.use(http.get(endpoint, async ({ request }) => {
      requestSignal = request.signal;
      await new Promise<void>((resolve) => { request.signal.addEventListener('abort', () => { resolve(); }, { once: true }); });
      return HttpResponse.json(snapshot());
    }));
    renderWithProviders(<SourceEventsButton id="source-a" name="Camera A" />);
    expect(requestSignal).toBeUndefined();
    await userEvent.click(screen.getByRole('button', { name: 'Events: Camera A' }));
    await waitFor(() => { expect(requestSignal).toBeDefined(); });
    await userEvent.keyboard('{Escape}');
    await waitFor(() => { expect(requestSignal?.aborted).toBe(true); });
  });

  it('does not show previous source data after a source switch', async () => {
    server.use(http.get(endpoint, ({ params }) => params.id === 'source-a'
      ? HttpResponse.json(snapshot()) : HttpResponse.json({ ...snapshot([]), source_id: 'source-b' })));
    const view = await open();
    await screen.findByText('cue-1');
    view.rerender(<SourceEventsButton id="source-b" name="Camera B" />);
    expect(screen.queryByText('cue-1')).not.toBeInTheDocument();
    await waitFor(() => { expect(screen.getByText(/No source events in the current server snapshot/)).toBeInTheDocument(); });
  });

  it.each([
    ['observing' as const, /Observing: HLS cue\/DATERANGE/],
    ['disabled' as const, /Observation disabled for this source/],
    ['not_supported' as const, /Not supported for this source kind/],
  ])('shows the per-source observation status honestly (%s)', async (status, pattern) => {
    server.use(http.get(endpoint, () => HttpResponse.json(snapshot([]))));
    await open(status);
    await screen.findByText(pattern);
  });

  it('explains what the empty state means for an observing source', async () => {
    server.use(http.get(endpoint, () => HttpResponse.json(snapshot([]))));
    await open('observing');
    expect(await screen.findByText(/a badge\/countdown overlay will appear automatically/)).toBeInTheDocument();
  });

  it('shows the server-computed marker name/type/detail, falling back to the event id', async () => {
    server.use(http.get(endpoint, () => HttpResponse.json(snapshot([
      row({ event_id: 'named', marker_name: 'Break Start', marker_type_id: '0x22', marker_detail: '2/4' }),
      row({ event_id: 'unnamed-cue' }),
    ]))));
    await open();
    expect(await screen.findByText('Break Start')).toBeInTheDocument();
    expect(screen.getByText(/0x22/)).toBeInTheDocument();
    expect(screen.getByText(/2\/4/)).toBeInTheDocument();
    expect(screen.queryByText('named')).not.toBeInTheDocument();
    expect(screen.getByText('unnamed-cue')).toBeInTheDocument();
  });
});

describe('test-injection aid (admin only, clearly labelled)', () => {
  it('labels the controls as a test aid and injects start/end requests', async () => {
    let lastBody: unknown;
    server.use(
      http.get(endpoint, () => HttpResponse.json(snapshot([]))),
      http.post(testEndpoint, async ({ request }) => {
        lastBody = await request.json();
        return new HttpResponse(null, { status: 202 });
      }),
    );
    await open('observing');
    expect(screen.getByText(/Test aid \(admin only\) — not live evidence/)).toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: 'Inject test break start (30s)' }));
    await waitFor(() => {
      expect(lastBody).toEqual({ boundary: 'start', duration_ns: 30_000_000_000 });
    });
    await userEvent.click(screen.getByRole('button', { name: 'Inject test break end' }));
    await waitFor(() => {
      expect(lastBody).toEqual({ boundary: 'end' });
    });
  });

  it.each([
    [403, /Administer role required/],
    [503, /Test injection is unavailable/],
    [404, /This source has never been active/],
  ])('reports test-injection failure %s honestly', async (status, pattern) => {
    server.use(
      http.get(endpoint, () => HttpResponse.json(snapshot([]))),
      http.post(testEndpoint, () => new HttpResponse(null, { status })),
    );
    await open('observing');
    await userEvent.click(screen.getByRole('button', { name: 'Inject test break start (30s)' }));
    expect(await screen.findByRole('alert')).toHaveTextContent(pattern);
  });

  it('renders a TEST badge on an injected (external-origin, test:-prefixed) event row', async () => {
    server.use(http.get(endpoint, () => HttpResponse.json(snapshot([
      row({ event_id: 'test:break', origin: 'external', lifecycle: 'active' }),
    ]))));
    await open('observing');
    await screen.findByText('test:break');
    expect(screen.getByText('Test')).toBeInTheDocument();
    expect(screen.getByText(/External evidence \(test injection or authenticated hook\)/)).toBeInTheDocument();
  });
});
