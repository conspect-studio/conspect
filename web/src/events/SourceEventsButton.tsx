import { useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { Button } from '../components/ui/button';
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from '../components/ui/dialog';
import { OperationApiError } from '../api/operations';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '../components/ui/tabs';
import { AutomationPanel } from './AutomationPanel';
import { JournalPanel } from './JournalPanel';
import { ClockNote, Countdown } from './eventTime';
import { durationSeconds, localTime, nanosecondsToSeconds, useInjectTestEvent, useSourceEvents } from './sourceEvents';
import type { SourceClock, SourceEvent, SourceMarker } from './sourceEvents';
import type { SourceView } from '../resources/types';

function EventRow({ event, timingAvailable }: { readonly event: SourceEvent; readonly timingAvailable: boolean }): JSX.Element {
  const { t } = useLingui();
  const states = {
    announced: t`Upcoming`, active: t`Active`, ended: t`Ended`, cancelled: t`Cancelled`,
    superseded: t`Superseded`, uncertain: t`Uncertain`, observed: t`Observed`,
  };
  const origins = {
    scte35: t`SCTE-35 signalling`, timed_id3: t`Timed ID3`, manifest: t`Manifest signalling`,
    vendor_marker: t`Vendor marker (unverified presence only)`, external: t`External evidence (test injection or authenticated hook)`,
  };
  const timerLabels = {
    until_start: t`Until start`, remaining: t`Expected return in`,
    elapsed: t`Elapsed`, overrun: t`Past expected return (end unconfirmed)`,
  };
  const seconds = event.timer ? nanosecondsToSeconds(event.timer.nanoseconds) : undefined;
  const showTimer = !['cancelled', 'ended', 'superseded'].includes(event.lifecycle)
    && (event.lifecycle !== 'uncertain' || event.timer?.kind === 'overrun');
  const isTest = event.event_id.startsWith('test:');
  // Marker name/detail are server-computed free text (SCTE-35 segmentation
  // names, splice_insert direction, HLS asset labels, ID3 frame names) — they
  // cannot be run through the i18n macro as a static literal. Only the
  // surrounding field labels are translated; an absent marker_name means the
  // evidence was honestly unidentified, not that the server omitted it.
  return <li className="space-y-1 rounded border p-3">
    <p className="break-all font-medium">
      {event.marker_name ?? event.event_id}
      {isTest ? <span className="ml-2 rounded bg-amber-200 px-1.5 py-0.5 text-xs font-semibold uppercase text-amber-900"><Trans>Test</Trans></span> : null}
    </p>
    <p>{states[event.lifecycle]}</p>
    <p><Trans>Provenance</Trans>: {origins[event.origin]}{event.vendor ? <> — {event.vendor}</> : null}</p>
    {event.marker_type_id ? <p className="break-all text-xs text-muted-foreground"><Trans>Marker type</Trans>: {event.marker_type_id}</p> : null}
    {event.marker_detail ? <p className="break-all text-xs text-muted-foreground"><Trans>Detail</Trans>: {event.marker_detail}</p> : null}
    <p><Trans>Epoch</Trans>: {event.epoch}</p>
    {event.asset_id ? <p className="break-all"><Trans>Asset</Trans>: {event.asset_id}</p> : null}
    {event.correlation_id ? <p className="break-all"><Trans>Correlation</Trans>: {event.correlation_id}</p> : null}
    {showTimer && timingAvailable && event.timer && seconds !== undefined
      ? <p>{timerLabels[event.timer.kind]}: <Trans>{seconds} seconds</Trans></p>
      : showTimer ? <p><Trans>No reliable timing readout.</Trans></p> : null}
  </li>;
}

/** One decoded marker: what it is and when it happens, in local time. */
function MarkerRow({ marker, now }: { readonly marker: SourceMarker; readonly now: number }): JSX.Element {
  return <li className="space-y-1 rounded border p-3">
    <p className="break-all font-medium">{marker.name}</p>
    {marker.start_at
      ? <p>
          {localTime(marker.start_at)}{marker.end_at ? <> → {localTime(marker.end_at)}</> : null}
          {' '}<Countdown at={marker.start_at} now={now} />
        </p>
      : <p><Trans>Time not known.</Trans></p>}
    {typeof marker.duration_ms === 'number' ? <p><Trans>Duration</Trans>: <Trans>{durationSeconds(marker.duration_ms)} seconds</Trans></p> : null}
    {marker.airs_at
      ? <p><Trans>Airs on program at</Trans> {localTime(marker.airs_at)}{marker.airs_end_at ? <> → {localTime(marker.airs_end_at)}</> : null} <Countdown at={marker.airs_at} now={now} /></p>
      : null}
    {marker.event_id ? <p className="break-all text-xs text-muted-foreground"><Trans>Event ID</Trans>: {marker.event_id}</p> : null}
    {marker.upid ? <p className="break-all text-xs text-muted-foreground"><Trans>UPID</Trans>: {marker.upid}</p> : null}
    <p className="text-xs text-muted-foreground"><ClockNote source={marker.clock_source} uncertaintyMs={marker.uncertainty_ms} /></p>
  </li>;
}

/** The source's embedded clock: a reference line, never an event row. */
function SourceClockLine({ clock }: { readonly clock: SourceClock }): JSX.Element {
  const offset = (clock.offset_ms / 1000).toFixed(1);
  return <p className="text-sm">
    <Trans>Source clock</Trans>: {localTime(clock.utc)} ({clock.utc.replace(/\.\d+/, '')}), <Trans>{offset} s against this host</Trans>
    {' — '}<ClockNote source={clock.clock_source} />
  </p>;
}

/** What observation status text to show, independent of the live snapshot. */
function observationStatusText(status: SourceView['eventObservation']): JSX.Element {
  if (status === 'observing') return <Trans>Observing: HLS cue/DATERANGE, SCTE-35 and timed ID3 are watched for this source.</Trans>;
  if (status === 'disabled') return <Trans>Observation disabled for this source (events.observe = false in its configuration).</Trans>;
  return <Trans>Not supported for this source kind: only HLS (and YouTube, an HLS wrapper) sources carry cue surfaces this tracker understands.</Trans>;
}

/** Operator/admin-only test-injection controls (ADR-0183 item 5). Clearly
 * labelled as a test aid, never presented as live evidence. */
function TestInjectionControls({ id }: { readonly id: string }): JSX.Element {
  const inject = useInjectTestEvent(id);
  const forbidden = inject.error instanceof OperationApiError && inject.error.status === 403;
  const unavailable = inject.error instanceof OperationApiError && inject.error.status === 503;
  const notFound = inject.error instanceof OperationApiError && inject.error.status === 404;
  return <div className="space-y-2 rounded border border-dashed p-3">
    <p className="text-xs font-semibold uppercase text-muted-foreground"><Trans>Test aid (admin only) — not live evidence</Trans></p>
    <p className="text-xs text-muted-foreground"><Trans>Inject a clearly-labelled synthetic break so you can see the overlay/automation work without waiting for a live ad break. If automation is armed, a test start takes the replacement and a test end restores it.</Trans></p>
    <div className="flex gap-2">
      <Button
        type="button"
        variant="outline"
        size="sm"
        disabled={inject.isPending}
        onClick={() => { inject.mutate({ boundary: 'start', duration_ns: 30_000_000_000 }); }}
      >
        <Trans>Inject test break start (30s)</Trans>
      </Button>
      <Button
        type="button"
        variant="outline"
        size="sm"
        disabled={inject.isPending}
        onClick={() => { inject.mutate({ boundary: 'end' }); }}
      >
        <Trans>Inject test break end</Trans>
      </Button>
    </div>
    {inject.isError ? <p role="alert" className="text-xs text-destructive">
      {forbidden ? <Trans>Administer role required to inject test events.</Trans>
        : unavailable ? <Trans>Test injection is unavailable (no active generation for this source, or the queue is full).</Trans>
        : notFound ? <Trans>This source has never been active; connect it before injecting a test event.</Trans>
        : <Trans>Test event injection failed.</Trans>}
    </p> : null}
  </div>;
}

function SourceEventsPanel({ id, eventObservation }: { readonly id: string; readonly eventObservation: SourceView['eventObservation'] }): JSX.Element {
  const { t } = useLingui();
  const query = useSourceEvents(id);
  if (query.isError) {
    const status = query.error instanceof OperationApiError ? query.error.status : undefined;
    return <p role="alert">{status === 401 ? <Trans>Authentication required to read source events.</Trans>
      : status === 403 ? <Trans>You do not have permission to read this source's events.</Trans>
      : status === 404 ? <Trans>This source was not found.</Trans>
      : status === 503 ? <Trans>Source event service is unavailable.</Trans>
      : <Trans>Source events could not be read.</Trans>}</p>;
  }
  if (query.data === undefined) return <p role="status"><Trans>Loading source events…</Trans></p>;
  const snapshot = query.data;
  const markers = snapshot.markers ?? [];
  // A decoded marker says everything an untimed presence row says, and more.
  const events = markers.length === 0
    ? snapshot.events
    : snapshot.events.filter((event) => !event.event_id.startsWith('untimed-scte:'));
  return <div className="space-y-3">
    <p className="text-sm text-muted-foreground">{observationStatusText(eventObservation)}</p>
    {snapshot.availability !== 'available' ? <p role="status"><Trans>Presentation timing is unavailable or stale. Timing is uncertain.</Trans></p> : null}
    {snapshot.degraded ? <p role="status"><Trans>Degraded event evidence: information may be incomplete.</Trans></p> : null}
    {snapshot.source_clock ? <SourceClockLine clock={snapshot.source_clock} /> : null}
    {markers.length > 0
      ? <ul aria-label={t`Decoded markers`} className="max-h-96 space-y-2 overflow-auto">{markers.map((marker) => <MarkerRow key={marker.record_id} marker={marker} now={query.dataUpdatedAt} />)}</ul>
      : null}
    {events.length === 0 && markers.length === 0
      ? <p><Trans>No source events in the current server snapshot. When this source airs an ad break, a badge/countdown overlay will appear automatically on its multiview tile.</Trans></p>
      : null}
    {events.length > 0
      ? <ul className="max-h-96 space-y-2 overflow-auto">{events.map((event, index) => <EventRow key={`${event.epoch}:${event.event_id}:${String(index)}`} event={event} timingAvailable={snapshot.availability === 'available'} />)}</ul>
      : null}
    <AutomationPanel id={id} />
    <TestInjectionControls id={id} />
  </div>;
}

/** Read-only snapshot + an admin-only test-injection aid; no automation is
 * armed by opening this panel. `eventObservation` is optional so existing
 * callers (e.g. the monitoring tile, which has no cached source list) still
 * work — the empty-state status line then falls back to `'not_supported'`'s
 * neutral phrasing being skipped rather than shown wrongly; callers that DO
 * have the source record should pass it for an honest status line. */
export function SourceEventsButton({ id, name, eventObservation }: {
  readonly id: string;
  readonly name: string;
  readonly eventObservation?: SourceView['eventObservation'];
}): JSX.Element {
  const { t } = useLingui();
  const [open, setOpen] = useState(false);
  return <>
    <Button variant="outline" size="sm" onClick={() => { setOpen(true); }} aria-label={t`Events: ${name}`}><Trans>Events</Trans></Button>
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogContent className="max-h-[90vh] max-w-4xl overflow-auto">
        <DialogHeader>
          <DialogTitle><Trans>Source events</Trans> — {name}</DialogTitle>
          <DialogDescription><Trans>Read-only server snapshot of source events, refreshed every second while open. Ad-break automation is configured below and must be armed explicitly. Lifecycle timers use the source presentation clock. Marker times are predictions from the clock each one names, and their countdowns run against this browser's clock.</Trans></DialogDescription>
        </DialogHeader>
        {open
          ? <Tabs defaultValue="events">
              <TabsList>
                <TabsTrigger value="events"><Trans>Events</Trans></TabsTrigger>
                <TabsTrigger value="journal"><Trans>Journal</Trans></TabsTrigger>
              </TabsList>
              <TabsContent value="events"><SourceEventsPanel key={id} id={id} eventObservation={eventObservation ?? 'observing'} /></TabsContent>
              <TabsContent value="journal"><JournalPanel key={id} id={id} /></TabsContent>
            </Tabs>
          : null}
      </DialogContent>
    </Dialog>
  </>;
}
