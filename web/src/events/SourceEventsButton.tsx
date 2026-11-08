import { useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { Button } from '../components/ui/button';
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from '../components/ui/dialog';
import { OperationApiError } from '../api/operations';
import { nanosecondsToSeconds, useSourceEvents } from './sourceEvents';
import type { SourceEvent } from './sourceEvents';

function EventRow({ event, timingAvailable }: { readonly event: SourceEvent; readonly timingAvailable: boolean }): JSX.Element {
  const { t } = useLingui();
  const states = {
    announced: t`Upcoming`, active: t`Active`, ended: t`Ended`, cancelled: t`Cancelled`,
    superseded: t`Superseded`, uncertain: t`Uncertain`, observed: t`Observed`,
  };
  const origins = {
    scte35: t`SCTE-35 signalling`, timed_id3: t`Timed ID3`, manifest: t`Manifest signalling`,
    vendor_marker: t`Vendor marker (unverified presence only)`, external: t`External evidence`,
  };
  const timerLabels = {
    until_start: t`Until start`, remaining: t`Expected return in`,
    elapsed: t`Elapsed`, overrun: t`Past expected return (end unconfirmed)`,
  };
  const seconds = event.timer ? nanosecondsToSeconds(event.timer.nanoseconds) : undefined;
  const showTimer = !['cancelled', 'ended', 'superseded'].includes(event.lifecycle)
    && (event.lifecycle !== 'uncertain' || event.timer?.kind === 'overrun');
  return <li className="space-y-1 rounded border p-3">
    <p className="break-all font-medium">{event.event_id}</p>
    <p>{states[event.lifecycle]}</p>
    <p><Trans>Provenance</Trans>: {origins[event.origin]}{event.vendor ? <> — {event.vendor}</> : null}</p>
    <p><Trans>Epoch</Trans>: {event.epoch}</p>
    {event.asset_id ? <p className="break-all"><Trans>Asset</Trans>: {event.asset_id}</p> : null}
    {event.correlation_id ? <p className="break-all"><Trans>Correlation</Trans>: {event.correlation_id}</p> : null}
    {showTimer && timingAvailable && event.timer && seconds !== undefined
      ? <p>{timerLabels[event.timer.kind]}: <Trans>{seconds} seconds</Trans></p>
      : showTimer ? <p><Trans>No reliable timing readout.</Trans></p> : null}
  </li>;
}

function SourceEventsPanel({ id }: { readonly id: string }): JSX.Element {
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
  return <div className="space-y-3">
    {snapshot.availability !== 'available' ? <p role="status"><Trans>Presentation timing is unavailable or stale. Timing is uncertain.</Trans></p> : null}
    {snapshot.degraded ? <p role="status"><Trans>Degraded event evidence: information may be incomplete.</Trans></p> : null}
    {snapshot.events.length === 0 ? <p><Trans>No source events in the current server snapshot.</Trans></p>
      : <ul className="max-h-96 space-y-2 overflow-auto">{snapshot.events.map((event, index) => <EventRow key={`${event.epoch}:${event.event_id}:${String(index)}`} event={event} timingAvailable={snapshot.availability === 'available'} />)}</ul>}
  </div>;
}

/** Read-only: opening the panel is the only action; no automation is armed. */
export function SourceEventsButton({ id, name }: { readonly id: string; readonly name: string }): JSX.Element {
  const { t } = useLingui();
  const [open, setOpen] = useState(false);
  return <>
    <Button variant="outline" size="sm" onClick={() => { setOpen(true); }} aria-label={t`Events: ${name}`}><Trans>Events</Trans></Button>
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle><Trans>Source events</Trans> — {name}</DialogTitle>
          <DialogDescription><Trans>Read-only server snapshot, refreshed every second while open. No automation is armed. Timing uses the source presentation clock, not browser receipt time.</Trans></DialogDescription>
        </DialogHeader>
        {open ? <SourceEventsPanel key={id} id={id} /> : null}
      </DialogContent>
    </Dialog>
  </>;
}
