import { useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { Button } from '../components/ui/button';
import { OperationApiError } from '../api/operations';
import { ClockNote } from './eventTime';
import { downloadJournal, durationSeconds, localTime, rawHex, useEventJournal } from './sourceEvents';
import type { JournalFilters, JournalRecord } from './sourceEvents';

const NO_FILTERS: JournalFilters = { origin: '', from: '', to: '' };

function originOf(value: string): JournalFilters['origin'] {
  return value === 'scte35' || value === 'id3' || value === 'hls' ? value : '';
}

/** A `datetime-local` value as RFC 3339, or empty. */
function toInstant(value: string): string {
  const at = new Date(value);
  return value === '' || Number.isNaN(at.getTime()) ? '' : at.toISOString();
}

function TimingFacts({ record }: { readonly record: JournalRecord }): JSX.Element | null {
  const timing = record.timing;
  if (!timing) return null;
  return <dl className="grid grid-cols-[auto_1fr] gap-x-3 text-xs">
    {typeof timing.splice_pts === 'number' ? <><dt><Trans>Splice PTS</Trans></dt><dd>{timing.splice_pts}</dd></> : null}
    {typeof timing.pre_roll_ms === 'number' ? <><dt><Trans>Pre-roll</Trans></dt><dd><Trans>{durationSeconds(timing.pre_roll_ms)} seconds</Trans></dd></> : null}
    {timing.start_at ? <><dt><Trans>Splice (UTC)</Trans></dt><dd>{timing.start_at}</dd></> : null}
    {timing.end_at ? <><dt><Trans>Break end (UTC)</Trans></dt><dd>{timing.end_at}</dd></> : null}
    {timing.airs_at ? <><dt><Trans>Airs on program (UTC)</Trans></dt><dd>{timing.airs_at}</dd></> : null}
    <dt><Trans>Clock</Trans></dt><dd><ClockNote source={timing.clock_source} uncertaintyMs={timing.uncertainty_ms} /></dd>
  </dl>;
}

function JournalRow({ record }: { readonly record: JournalRecord }): JSX.Element {
  const { t } = useLingui();
  const [open, setOpen] = useState(false);
  const repeats = record.repeats ?? 0;
  return <>
    <tr className="border-t align-top">
      <td className="whitespace-nowrap p-1">{localTime(record.received_at)}</td>
      <td className="p-1">{record.origin}</td>
      <td className="break-all p-1">{record.name}{repeats > 0 ? <> ×{repeats + 1}</> : null}</td>
      <td className="break-all p-1">
        {record.summary}
        {record.error ? <p role="alert" className="text-destructive">{record.error}</p> : null}
      </td>
      <td className="p-1">
        <Button type="button" variant="ghost" size="sm" aria-expanded={open} aria-label={t`Details: ${record.record_id}`} onClick={() => { setOpen(!open); }}>
          {open ? <Trans>Hide</Trans> : <Trans>Show</Trans>}
        </Button>
      </td>
    </tr>
    {open
      ? <tr>
          <td colSpan={5} className="space-y-2 p-2">
            <TimingFacts record={record} />
            <p className="text-xs font-semibold"><Trans>Decoded</Trans></p>
            <pre className="max-h-64 overflow-auto rounded bg-muted p-2 text-xs">{JSON.stringify({ transport: record.transport, source_pts: record.source_pts, decoded: record.decoded }, null, 2)}</pre>
            <p className="text-xs font-semibold"><Trans>Raw</Trans></p>
            <pre className="max-h-40 overflow-auto rounded bg-muted p-2 text-xs">{rawHex(record)}</pre>
          </td>
        </tr>
      : null}
  </>;
}

/** Time-ordered raw marker history for one source, with its download. */
export function JournalPanel({ id }: { readonly id: string }): JSX.Element {
  const { t } = useLingui();
  const [filters, setFilters] = useState<JournalFilters>(NO_FILTERS);
  const [downloadFailed, setDownloadFailed] = useState(false);
  const query = useEventJournal(id, filters);
  const records = query.data?.pages.flatMap((page) => page.records) ?? [];
  const stats = query.data?.pages.at(-1)?.stats;
  const status = query.error instanceof OperationApiError ? query.error.status : undefined;
  return <div className="space-y-3">
    <div className="flex flex-wrap items-end gap-2 text-sm">
      <label className="flex flex-col"><Trans>Origin</Trans>
        <select
          className="rounded border bg-background p-1"
          value={filters.origin}
          onChange={(event) => { setFilters({ ...filters, origin: originOf(event.target.value) }); }}
        >
          <option value="">{t`All`}</option>
          <option value="scte35">SCTE-35</option>
          <option value="id3">ID3</option>
          <option value="hls">HLS</option>
        </select>
      </label>
      <label className="flex flex-col"><Trans>From</Trans>
        <input type="datetime-local" className="rounded border bg-background p-1" onChange={(event) => { setFilters({ ...filters, from: toInstant(event.target.value) }); }} />
      </label>
      <label className="flex flex-col"><Trans>To</Trans>
        <input type="datetime-local" className="rounded border bg-background p-1" onChange={(event) => { setFilters({ ...filters, to: toInstant(event.target.value) }); }} />
      </label>
      <Button type="button" variant="outline" size="sm" onClick={() => { void query.refetch(); }}><Trans>Refresh</Trans></Button>
      <Button
        type="button"
        variant="outline"
        size="sm"
        onClick={() => {
          setDownloadFailed(false);
          downloadJournal(id, filters).catch(() => { setDownloadFailed(true); });
        }}
      >
        <Trans>Download</Trans>
      </Button>
    </div>
    {downloadFailed ? <p role="alert"><Trans>The journal could not be downloaded.</Trans></p> : null}
    {stats && stats.dropped > 0 ? <p role="status"><Trans>{stats.dropped} observations were dropped since this server started, so this history has gaps.</Trans></p> : null}
    {query.isError
      ? <p role="alert">{status === 403 ? <Trans>You do not have permission to read this source's journal.</Trans>
        : status === 404 ? <Trans>This source has no journal yet.</Trans>
        : status === 503 ? <Trans>The event journal is busy or unavailable. Try again shortly.</Trans>
        : <Trans>The event journal could not be read.</Trans>}</p>
      : query.data === undefined ? <p role="status"><Trans>Loading the journal…</Trans></p>
      : records.length === 0 ? <p><Trans>No journal records match these filters.</Trans></p>
      : <table className="w-full text-left text-xs">
          <thead>
            <tr>
              <th className="p-1"><Trans>Time</Trans></th>
              <th className="p-1"><Trans>Origin</Trans></th>
              <th className="p-1"><Trans>Name</Trans></th>
              <th className="p-1"><Trans>Summary</Trans></th>
              <th className="p-1"><span className="sr-only"><Trans>Details</Trans></span></th>
            </tr>
          </thead>
          <tbody>{records.map((record) => <JournalRow key={record.record_id} record={record} />)}</tbody>
        </table>}
    {query.hasNextPage
      ? <Button type="button" variant="outline" size="sm" disabled={query.isFetchingNextPage} onClick={() => { void query.fetchNextPage(); }}><Trans>Load older records</Trans></Button>
      : null}
  </div>;
}
