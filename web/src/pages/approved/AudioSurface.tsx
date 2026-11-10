// Approved-design composition for the Audio page (lane A). Presentation only:
// every control, draft, ETag and Save rule stays in AudioPage. These pieces
// provide the approved hierarchy — heading + help + program loudness on top,
// desktop strip bank with a right-hand overview/master/monitor rail, and a
// phone/tablet PROGRAM summary — and say plainly where the production control
// plane has no capability (master gain/mute, headphone LISTEN/PFL).
import { useEffect, useState } from 'react';
import type { JSX, ReactNode } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';

import { Button } from '../../components/ui/button';

import type { AudioLoudnessState } from '../../realtime/useAudioLoudness';
import './audio-approved.css';

/** A sample older than this is stale: the readout reverts to Unknown. */
export const LOUDNESS_STALE_MS = 6000;

/** Fresh live sample, or undefined when disconnected/stalled/never received. */
function useFreshSample(
  state: AudioLoudnessState,
): AudioLoudnessState['current'] {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const id = setInterval(() => {
      setNow(Date.now());
    }, 1000);
    return (): void => {
      clearInterval(id);
    };
  }, []);
  if (state.status !== 'open' || state.receivedAt === undefined) {
    return undefined;
  }
  return now - state.receivedAt > LOUDNESS_STALE_MS
    ? undefined
    : state.current;
}

/** The PROGRAM LOUDNESS readout: live telemetry or an honest Unknown. */
export function AudioLoudnessReadout({
  loudness,
  label,
}: {
  readonly loudness: AudioLoudnessState;
  readonly label: ReactNode;
}): JSX.Element {
  const sample = useFreshSample(loudness);
  const value = sample?.short_term;
  const momentary = sample?.momentary;
  return (
    <div className="audio-meta" data-testid="audio-loudness-readout">
      <span className="lbl">{label}</span>
      <span className="clock-mono">
        {value === undefined ? '—' : value.toFixed(1)} LUFS
      </span>
      {value === undefined ? (
        <span className="sr-only">
          <Trans>Unknown: no current loudness telemetry</Trans>
        </span>
      ) : null}
      {momentary === undefined ? null : (
        <>
          <span className="lbl">
            <Trans>MOMENTARY</Trans>
          </span>
          <span className="clock-mono" data-testid="audio-momentary-readout">
            {momentary.toFixed(1)} LUFS
          </span>
        </>
      )}
    </div>
  );
}

/** Page root: local tokens + heading/help/loudness header + content. */
export function AudioSurface({
  sub,
  loudness,
  children,
}: {
  readonly sub: ReactNode;
  readonly loudness: AudioLoudnessState;
  readonly children: ReactNode;
}): JSX.Element {
  return (
    <div data-approved-page="audio" className="audio-surface">
      <div className="screen-head">
        <div>
          <h1>
            <Trans>Audio mixer</Trans>
          </h1>
          <p className="screen-sub">{sub}</p>
        </div>
        <AudioLoudnessReadout
          loudness={loudness}
          label={<Trans>PROGRAM LOUDNESS (short-term)</Trans>}
        />
      </div>
      {children}
    </div>
  );
}

/** Desktop: strip bank + right rail. Below xl the rail stacks after the strip. */
export function AudioMixerLayout({
  bank,
  focused,
  rail,
}: {
  readonly bank: ReactNode;
  readonly focused: ReactNode;
  readonly rail: ReactNode;
}): JSX.Element {
  return (
    <div className="mixer">
      <div className="mixer-main">
        {bank}
        {focused}
      </div>
      <aside className="master-col">{rail}</aside>
    </div>
  );
}

/** Pinned PROGRAM summary for phone/tablet (below xl). */
export function AudioPhoneMaster({
  loudness,
}: {
  readonly loudness: AudioLoudnessState;
}): JSX.Element {
  const value = useFreshSample(loudness)?.short_term;
  return (
    <div
      className="phone-master"
      data-testid="audio-phone-master"
      aria-label="PROGRAM"
    >
      <span className="lbl">
        <Trans>PROGRAM</Trans>
      </span>
      <span className="clock-mono">
        {value === undefined ? '—' : value.toFixed(1)} LUFS
      </span>
      <span className="strip-status">
        <Trans>master mute unavailable</Trans>
      </span>
    </div>
  );
}

/** Desktop master rail: the approved VERTICAL PROGRAM strip (fader track,
 * nudges, MUTE, status) rendered DISABLED — the control plane has no master
 * gain/mute — with the real program meters/loudness beneath (children). */
export function AudioMasterRegion({
  children,
}: {
  readonly children?: ReactNode;
}): JSX.Element {
  const { t } = useLingui();
  const reason = t`Master gain/mute unavailable: control plane exposes only per-input gain and mute in the saved audio document`;
  return (
    <>
      <section
        className="strip-master"
        data-testid="audio-master-region"
        aria-labelledby="audio-master-heading"
      >
        <h2 id="audio-master-heading" className="strip-name">
          <Trans>PROGRAM</Trans>
        </h2>
        <div className="master-fader-row" title={reason}>
          <div className="audio-fader-box fader-track master-track" data-testid="audio-master-fader" role="group" tabIndex={-1} aria-disabled="true" aria-label={t`PROGRAM master gain — unavailable`} title={reason} />
          <div className="audio-nudge-col">
            {(['+6', '+1', '−1', '−6'] as const).map((label) => (
              <Button key={label} type="button" variant="outline" disabled className="audio-nudge-btn h-12 min-w-12 p-0 font-mono" aria-label={t`PROGRAM gain ${label} dB — unavailable`}>
                {label}
              </Button>
            ))}
          </div>
        </div>
        <div className="master-keys">
          <Button type="button" variant="outline" disabled title={reason} aria-label={t`PROGRAM MUTE — unavailable`} className="h-12 min-w-12 px-3">
            <Trans>MUTE</Trans>
          </Button>
        </div>
        <p className="strip-status" data-testid="audio-master-unavailable">
          {reason}.
        </p>
        {children}
      </section>
      <div className="mix-note">
        <p className="monitor-line" data-testid="audio-monitor-unavailable">
          <Trans>
            MONITOR OUT — LISTEN (PFL) is unavailable: no headphone monitoring
            path exists on this control plane.
          </Trans>
        </p>
        <p>
          <Trans>
            Input meters are pre-fader presence; they do not move with the
            staged gain. PGM = include in the program bus.
          </Trans>
        </p>
      </div>
    </>
  );
}
