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
async function open() {
  const view = renderWithProviders(<SourceEventsButton id="source-a" name="Camera A" />);
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
});
