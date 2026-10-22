// LoudnessMeter (AUD-8): the live program-bus EBU R128 loudness compliance meter
// on the Audio page. It subscribes to the engine's conflated `audio.loudness`
// topic (useAudioLoudness) and renders momentary / short-term / integrated LUFS,
// loudness range, and true-peak (dBTP) against the program's compliance target,
// with CLIENT-SIDE ballistics (momentary fast-attack/slow-decay + peak hold) and
// colour zones (in-spec / near / out / over-ceiling) per ADR-R006.
//
// Accessibility (WCAG 2.1 AA): the status hue is NEVER the sole carrier of
// meaning — every coloured cell pairs the hue (at low alpha) with `text-foreground`
// plus a textual status label and the numeric value (index.css header rule).
import type { JSX } from 'react';
import { Trans } from '@lingui/react/macro';

import {
  classifyLoudness,
  useAudioLoudness,
} from '../realtime/useAudioLoudness';
import type {
  AudioLoudnessState,
  LoudnessZone,
} from '../realtime/useAudioLoudness';
import { cn } from '../lib/utils';

/** A dash placeholder for an absent measurement (never a fabricated number). */
const ABSENT = '—';

/** Format a LUFS / LU / dBTP reading to one decimal, or a dash when absent. */
function fmt(value: number | undefined): string {
  return value === undefined ? ABSENT : value.toFixed(1);
}

/** The status-token classes for a loudness zone (hue at low alpha + text). */
function zoneClasses(zone: LoudnessZone): string {
  switch (zone) {
    case 'in-spec':
      return 'border-status-live/40 bg-status-live/15 text-foreground';
    case 'near':
      return 'border-status-stale/40 bg-status-stale/15 text-foreground';
    case 'out':
      return 'border-status-nosignal/40 bg-status-nosignal/15 text-foreground';
    case 'absent':
      return 'border-status-offline/40 bg-status-offline/10 text-muted-foreground';
  }
}

/** The textual zone label (carries the meaning; the colour only aids it). */
function ZoneLabel({ zone }: { zone: LoudnessZone }): JSX.Element {
  switch (zone) {
    case 'in-spec':
      return <Trans>In spec</Trans>;
    case 'near':
      return <Trans>Near limit</Trans>;
    case 'out':
      return <Trans>Out of spec</Trans>;
    case 'absent':
      return <Trans>Silent (gated)</Trans>;
  }
}

/** One labelled secondary readout (momentary / short-term / LRA). */
function Readout({
  testid,
  label,
  value,
  unit,
}: {
  testid: string;
  label: JSX.Element;
  value: number | undefined;
  unit: string;
}): JSX.Element {
  return (
    <div
      data-testid={testid}
      className="flex flex-col rounded-md border border-border bg-card px-3 py-2"
    >
      <span className="text-xs text-muted-foreground">{label}</span>
      <span className="font-mono text-lg tabular-nums">
        {fmt(value)}
        {value !== undefined ? <span className="ml-1 text-xs text-muted-foreground">{unit}</span> : null}
      </span>
    </div>
  );
}

/**
 * The presentational meter view: rendered purely from the loudness state, so it
 * is fully testable by injecting a sample (no realtime hook). The container
 * {@link LoudnessMeter} wires it to {@link useAudioLoudness}.
 */
export function LoudnessMeterView({
  status,
  current,
  displayMomentary,
  heldPeakDbtp,
}: AudioLoudnessState): JSX.Element {
  if (current === undefined) {
    return (
      <section
        aria-label="Program loudness"
        className="rounded-lg border border-border p-4"
      >
        <h3 className="text-sm font-medium">
          <Trans>Program loudness</Trans>
        </h3>
        <p className="mt-2 text-sm text-muted-foreground" aria-live="polite">
          {status === 'open' ? (
            <Trans>Waiting for loudness telemetry…</Trans>
          ) : (
            <Trans>Waiting for loudness telemetry (realtime offline)…</Trans>
          )}
        </p>
      </section>
    );
  }

  // The headline number is the INTEGRATED loudness (the compliance metric).
  const integratedZone = classifyLoudness(current, current.integrated).loudnessZone;
  // The momentary bar uses the ballistics-shaped value; peakOver flags the dBTP.
  const peakOver = classifyLoudness(current, current.integrated).peakOver;

  return (
    <section aria-label="Program loudness" className="rounded-lg border border-border p-4">
      <header className="flex flex-wrap items-baseline justify-between gap-2">
        <h3 className="text-sm font-medium">
          <Trans>Program loudness</Trans>
        </h3>
        <span className="text-xs text-muted-foreground">
          <Trans>
            Target {current.target_lufs.toFixed(0)} LUFS · ceiling{' '}
            {current.ceiling_dbtp.toFixed(1)} dBTP · ±{current.tolerance_lu.toFixed(0)} LU
          </Trans>
        </span>
      </header>

      {/* Integrated — the headline compliance readout, colour-zoned + labelled. */}
      <div
        data-testid="loudness-integrated"
        className={cn(
          'mt-3 flex flex-wrap items-baseline justify-between gap-2 rounded-md border px-3 py-2',
          zoneClasses(integratedZone),
        )}
      >
        <span className="flex items-baseline gap-2">
          <span className="text-xs uppercase tracking-wide">
            <Trans>Integrated</Trans>
          </span>
          <span className="font-mono text-2xl font-semibold tabular-nums">
            {fmt(current.integrated)}
          </span>
          <span className="text-xs">LUFS</span>
        </span>
        <span className="text-xs font-medium">
          <ZoneLabel zone={integratedZone} />
        </span>
      </div>

      {/* Secondary readouts: momentary (ballistics-shaped), short-term, LRA. */}
      <div className="mt-3 grid grid-cols-2 gap-2 sm:grid-cols-3">
        <Readout
          testid="loudness-momentary"
          label={<Trans>Momentary (400 ms)</Trans>}
          value={displayMomentary ?? current.momentary}
          unit="LUFS"
        />
        <Readout
          testid="loudness-short-term"
          label={<Trans>Short-term (3 s)</Trans>}
          value={current.short_term}
          unit="LUFS"
        />
        <Readout
          testid="loudness-lra"
          label={<Trans>Loudness range</Trans>}
          value={current.lra}
          unit="LU"
        />
      </div>

      {/* True-peak — its own row with the over-ceiling clip flag. */}
      <div
        data-testid="loudness-true-peak"
        className={cn(
          'mt-2 flex flex-wrap items-baseline justify-between gap-2 rounded-md border px-3 py-2',
          peakOver
            ? 'border-status-nosignal/40 bg-status-nosignal/15 text-foreground'
            : 'border-border bg-card',
        )}
      >
        <span className="flex items-baseline gap-2">
          <span className="text-xs uppercase tracking-wide text-muted-foreground">
            <Trans>True peak</Trans>
          </span>
          <span className="font-mono text-lg tabular-nums">
            {fmt(heldPeakDbtp ?? current.true_peak_dbtp)}
          </span>
          <span className="text-xs text-muted-foreground">dBTP</span>
        </span>
        <span className="text-xs font-medium">
          {peakOver ? <Trans>Over ceiling</Trans> : <Trans>Under ceiling</Trans>}
        </span>
      </div>

      {current.gain_db !== undefined ? (
        <p className="mt-2 text-xs text-muted-foreground">
          <Trans>Normaliser gain {current.gain_db.toFixed(1)} dB</Trans>
        </p>
      ) : null}
    </section>
  );
}

/** The live container: subscribes to the loudness topic + renders the view. */
export function LoudnessMeter(): JSX.Element {
  const state = useAudioLoudness();
  return <LoudnessMeterView {...state} />;
}
