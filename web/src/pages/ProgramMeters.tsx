// ProgramMeters (AUD-8 / SW-14): live program-bus peak/RMS bar meters on the
// Audio page, fed by the engine's conflated (~30 Hz, drop-oldest) `audio.meter`
// topic via useAudioMeter. The LUFS / dBTP compliance readout sits beside it in
// LoudnessMeter. UI-only: it consumes telemetry and never touches the engine
// path (invariant #10).
//
// Accessibility: bar fill is decorative; the numeric dBFS value and an explicit
// "Clip" text label carry the meaning (colour is never the sole signal).
import type { JSX } from 'react';
import { Trans } from '@lingui/react/macro';

import { meterFillPercent, useAudioMeter } from '../realtime/useAudioMeter';
import type { AudioMeterState } from '../realtime/useAudioMeter';
import { cn } from '../lib/utils';

function channelLabel(index: number, count: number): string {
  if (count === 2) {
    return index === 0 ? 'L' : 'R';
  }
  return String(index + 1);
}

function Bar({ db, kind }: { db: number; kind: 'peak' | 'rms' }): JSX.Element {
  return (
    <div className="h-2 flex-1 overflow-hidden rounded bg-muted" aria-hidden="true">
      <div
        className={cn('h-full', kind === 'peak' ? 'bg-status-live' : 'bg-status-live/50')}
        style={{ width: `${String(meterFillPercent(db))}%` }}
      />
    </div>
  );
}

/** Presentational view: rendered purely from the meter state. */
export function ProgramMetersView({ status, current }: AudioMeterState): JSX.Element {
  if (current === undefined) {
    return (
      <section aria-label="Program meters" className="rounded-lg border border-border p-4">
        <h3 className="text-sm font-medium">
          <Trans>Program meters</Trans>
        </h3>
        <p className="mt-2 text-sm text-muted-foreground" aria-live="polite">
          {status === 'open' ? (
            <Trans>Waiting for meter telemetry…</Trans>
          ) : (
            <Trans>Waiting for meter telemetry (realtime offline)…</Trans>
          )}
        </p>
      </section>
    );
  }
  const count = current.peak_db.length;
  return (
    <section aria-label="Program meters" className="rounded-lg border border-border p-4">
      <header className="flex items-baseline justify-between gap-2">
        <h3 className="text-sm font-medium">
          <Trans>Program meters</Trans>
        </h3>
        {current.clip ? (
          <span className="rounded border border-status-nosignal/40 bg-status-nosignal/15 px-2 text-xs font-medium text-foreground">
            <Trans>Clip</Trans>
          </span>
        ) : null}
      </header>
      <ul className="mt-3 space-y-2">
        {current.peak_db.map((peak, i) => {
          const rms = current.rms_db[i] ?? peak;
          return (
            <li key={i} className="flex items-center gap-3">
              <span className="w-4 text-xs text-muted-foreground">{channelLabel(i, count)}</span>
              <div className="flex flex-1 flex-col gap-1">
                <Bar db={peak} kind="peak" />
                <Bar db={rms} kind="rms" />
              </div>
              <span className="flex w-28 flex-col font-mono text-xs tabular-nums">
                <span>
                  <span data-testid={`meter-peak-${String(i)}`}>{peak.toFixed(1)}</span> dBFS pk
                </span>
                <span className="text-muted-foreground">
                  <span data-testid={`meter-rms-${String(i)}`}>{rms.toFixed(1)}</span> dBFS rms
                </span>
              </span>
            </li>
          );
        })}
      </ul>
    </section>
  );
}

/** The live container: subscribes to `audio.meter` + renders the view. */
export function ProgramMeters(): JSX.Element {
  return <ProgramMetersView {...useAudioMeter()} />;
}
