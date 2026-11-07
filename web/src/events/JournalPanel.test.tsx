import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { renderWithProviders } from '../test/render';
import { SourceEventsButton } from './SourceEventsButton';
import { instantMs, rawHex, secondsUntil } from './sourceEvents';
import type { JournalRecord, SourceEvents, SourceMarker } from './sourceEvents';

const record = (changes: Partial<JournalRecord> = {}): JournalRecord => ({
  record_id: '01793000000000000000-000001', source_id: 'source-a',
  received_at: '2026-11-03T21:14:58.123456789Z', received_ns: '1793000000000000000',
  origin: 'scte35', name: 'Break Start', summary: 'time_signal segmentation_event_id=7',
  transport: { pid: 483 }, source_pts: 900000, raw_encoding: 'base64', raw: '/DA=',
  decoded: { splice_command_name: 'time_signal' }, timing: null, error: null,
  clock_reference: false, repeats: 0, ...changes,
});
const page = (records: JournalRecord[], next: string | null = null) => ({
  source_id: 'source-a', records, next_cursor: next,
  stats: { dropped: 0, written: 1, repeats: 0, write_errors: 0 },
});
const marker: SourceMarker = {
  record_id: 'r1', name: 'Provider Placement Opportunity Start', received_at: '2026-11-03T21:14:58Z',
  start_at: new Date(Date.now() + 3_600_000).toISOString(), end_at: null, airs_at: null, airs_end_at: null,
  duration_ms: 30_000, clock_source: 'id3_utc', uncertainty_ms: 1000,
  event_id: '1207959694', upid: 'AiringID: 0x2CA0A18A',
};
const snapshot = (changes: Partial<SourceEvents> = {}): SourceEvents => ({
  source_id: 'source-a', epoch: '1', availability: 'available', degraded: false, events: [], ...changes,
});
const events = '*/api/v1/inputs/:id/events';
const journal = '*/api/v1/inputs/:id/events/journal';
const server = setupServer(
  http.get('*/api/v1/inputs/:id/events/automation', () => HttpResponse.json({
    rule: null, log: [], bars_sources: [], executor_running: true,
    status: { phase: 'disarmed', armed: false, test: false, replaced: [] },
  })),
  http.get(events, () => HttpResponse.json(snapshot())),
);
beforeAll(() => { server.listen({ onUnhandledRequest: 'error' }); });
afterEach(() => { server.resetHandlers(); });
afterAll(() => { server.close(); });

async function open() {
  renderWithProviders(<SourceEventsButton id="source-a" name="Camera A" />);
  await userEvent.click(screen.getByRole('button', { name: 'Events: Camera A' }));
}
async function openJournal() {
  await open();
  await userEvent.click(screen.getByRole('tab', { name: 'Journal' }));
}

describe('event time helpers', () => {
  it('reads nanosecond timestamps and counts whole seconds', () => {
    expect(instantMs('2026-11-03T21:14:58.123456789Z')).toBe(Date.UTC(2026, 10, 3, 21, 14, 58, 123));
    expect(secondsUntil('2026-11-03T21:14:58.000Z', Date.UTC(2026, 10, 3, 21, 14, 46))).toBe(12);
    expect(secondsUntil('2026-11-03T21:14:58.000Z', Date.UTC(2026, 10, 3, 21, 15, 0))).toBe(-2);
  });
  it('renders base64 and text payloads as hex', () => {
    expect(rawHex({ raw: '/DA=', raw_encoding: 'base64' })).toBe('fc 30');
    expect(rawHex({ raw: '#E', raw_encoding: 'text' })).toBe('23 45');
    expect(rawHex({ raw: btoa('x'.repeat(17)), raw_encoding: 'base64' }).split('\n')).toHaveLength(2);
  });
});

describe('decoded event summary', () => {
  it('shows the marker with its countdown, identity, clock note and the source clock', async () => {
    server.use(http.get(events, () => HttpResponse.json(snapshot({
      markers: [marker],
      source_clock: { utc: '2026-11-03T21:14:58.000000000Z', offset_ms: -2500, clock_source: 'id3_utc', observed_at: '2026-11-03T21:15:00Z' },
      events: [{
        event_id: 'untimed-scte:0', epoch: '1', origin: 'scte35', vendor: null, lifecycle: 'observed',
        timer: null, asset_id: null, correlation_id: null,
      }],
    }))));
    await open();
    const list = await screen.findByRole('list', { name: 'Decoded markers' });
    expect(within(list).getByText('Provider Placement Opportunity Start')).toBeInTheDocument();
    expect(within(list).getByText(/UPID/)).toHaveTextContent('AiringID: 0x2CA0A18A');
    expect(within(list).getByText(/\(in \d+ s\)/)).toBeInTheDocument();
    expect(within(list).getByText(/Clock source: ID3 UTC/)).toHaveTextContent('(±1.0 s)');
    expect(screen.getByText(/-2\.5 s against this host/)).toBeInTheDocument();
    expect(screen.queryByText('untimed-scte:0')).not.toBeInTheDocument();
  });

  it('keeps presence rows when nothing has been decoded', async () => {
    server.use(http.get(events, () => HttpResponse.json(snapshot({
      events: [{
        event_id: 'untimed-scte:0', epoch: '1', origin: 'scte35', vendor: null, lifecycle: 'observed',
        timer: null, asset_id: null, correlation_id: null,
      }],
    }))));
    await open();
    expect(await screen.findByText('untimed-scte:0')).toBeInTheDocument();
  });
});

describe('journal tab', () => {
  it('lists records, expands the decode and hex, and pages older records', async () => {
    const cursors: (string | null)[] = [];
    server.use(http.get(journal, ({ request }) => {
      const cursor = new URL(request.url).searchParams.get('cursor');
      cursors.push(cursor);
      return HttpResponse.json(cursor === null
        ? page([record({ repeats: 2 })], '01793000000000000000-000001')
        : page([record({ record_id: '01792000000000000000-000001', name: 'Older' })]));
    }));
    await openJournal();
    expect(await screen.findByText(/Break Start/)).toHaveTextContent('×3');
    await userEvent.click(screen.getByRole('button', { name: 'Details: 01793000000000000000-000001' }));
    expect(screen.getByText('fc 30')).toBeInTheDocument();
    expect(screen.getByText(/"splice_command_name": "time_signal"/)).toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: 'Load older records' }));
    expect(await screen.findByText('Older')).toBeInTheDocument();
    expect(cursors).toEqual([null, '01793000000000000000-000001']);
    expect(screen.queryByRole('button', { name: 'Load older records' })).not.toBeInTheDocument();
  });

  it('sends the origin filter and shows a record error', async () => {
    const origins: (string | null)[] = [];
    server.use(http.get(journal, ({ request }) => {
      origins.push(new URL(request.url).searchParams.get('origin'));
      return HttpResponse.json(page([record({ error: 'CRC_32 mismatch' })]));
    }));
    await openJournal();
    expect(await screen.findByRole('alert')).toHaveTextContent('CRC_32 mismatch');
    await userEvent.selectOptions(screen.getByLabelText('Origin'), 'hls');
    await waitFor(() => { expect(origins).toEqual([null, 'hls']); });
  });

  it('warns about gaps and explains a forbidden read', async () => {
    server.use(http.get(journal, () => HttpResponse.json({
      ...page([record()]), stats: { dropped: 4, written: 1, repeats: 0, write_errors: 0 },
    })));
    await openJournal();
    expect(await screen.findByText(/4 observations were dropped/)).toBeInTheDocument();
  });

  it('reports a forbidden journal', async () => {
    server.use(http.get(journal, () => new HttpResponse(null, { status: 403 })));
    await openJournal();
    expect(await screen.findByRole('alert')).toHaveTextContent("permission to read this source's journal");
  });

  it('downloads through the authenticated client', async () => {
    const created = vi.fn(() => 'blob:journal');
    URL.createObjectURL = created;
    URL.revokeObjectURL = vi.fn();
    const click = vi.spyOn(HTMLAnchorElement.prototype, 'click').mockImplementation(() => undefined);
    server.use(
      http.get(journal, () => HttpResponse.json(page([record()]))),
      http.get('*/api/v1/inputs/:id/events/journal.jsonl', () => new HttpResponse('{}\n')),
    );
    await openJournal();
    await screen.findByText(/Break Start/);
    await userEvent.click(screen.getByRole('button', { name: 'Download' }));
    await waitFor(() => { expect(click).toHaveBeenCalledOnce(); });
    expect(created).toHaveBeenCalledOnce();
    click.mockRestore();
  });
});
