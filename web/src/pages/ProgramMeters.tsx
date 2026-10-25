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
import { useAudioLoudness } from '../realtime/useAudioLoudness';
import type { AudioLoudnessSample } from '../realtime/useAudioLoudness';
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

const ABSENT = '—';

function fmt(value: number | undefined): string {
  return value === undefined ? ABSENT : value.toFixed(1);
}

/** Props: the meter state plus the (optional) latest `audio.loudness` sample. */
export type ProgramMetersProps = AudioMeterState & {
  readonly loudness?: AudioLoudnessSample | undefined;
};

function Lufs({ testid, label, value }: { testid: string; label: string; value: number | undefined }): JSX.Element {
  return (
    <div className="flex flex-col rounded-md border border-border bg-card px-2 py-1">
      <span className="text-xs text-muted-foreground">{label}</span>
      <span data-testid={testid} className="font-mono text-sm tabular-nums">
        {fmt(value)}
      </span>
    </div>
  );
}

/** Presentational view: rendered purely from the meter + loudness state. */
export function ProgramMetersView({ status, current, inputs, loudness }: ProgramMetersProps): JSX.Element {
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

      {loudness !== undefined ? (
        <div className="mt-3 grid grid-cols-3 gap-2 sm:grid-cols-5">
          <Lufs testid="meter-lufs-m" label="M LUFS" value={loudness.momentary} />
          <Lufs testid="meter-lufs-s" label="S LUFS" value={loudness.short_term} />
          <Lufs testid="meter-lufs-i" label="I LUFS" value={loudness.integrated} />
          <Lufs testid="meter-lra" label="LRA LU" value={loudness.lra} />
          <Lufs testid="meter-dbtp" label="dBTP" value={loudness.true_peak_dbtp} />
        </div>
      ) : null}

      {inputs.length > 0 ? (
        <ul className="mt-3 space-y-1 border-t border-border pt-3" aria-label="Input meters">
          {inputs.map((m) => {
            const id = m.input_id ?? String(m.track);
            return (
              <li
                key={m.track}
                data-testid={`input-meter-${id}`}
                className="flex items-center gap-3 text-xs"
              >
                <span className="w-24 truncate text-muted-foreground">{id}</span>
                <div className="flex flex-1 flex-col gap-1">
                  {m.peak_db.map((db, i) => (
                    <Bar key={i} db={db} kind="peak" />
                  ))}
                </div>
                <span className="w-28 font-mono tabular-nums">
                  {m.peak_db.map((db) => db.toFixed(1)).join(' / ')} dBFS pk
                </span>
              </li>
            );
          })}
        </ul>
      ) : null}
    </section>
  );
}

/** The live container: subscribes to `audio.meter` + renders the view. */
export function ProgramMeters(): JSX.Element {
  const meter = useAudioMeter();
  const { current: loudness } = useAudioLoudness();
  return <ProgramMetersView {...meter} loudness={loudness} />;
}
