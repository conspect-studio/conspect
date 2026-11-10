// Approved-design composition for the Audio page (lane A). Presentation only:
// every control, draft, ETag and Save rule stays in AudioPage. These pieces
// provide the approved hierarchy — heading + help + program loudness on top,
// desktop strip bank with a right-hand overview/master/monitor rail, and a
// phone/tablet PROGRAM summary — and say plainly where the production control
// plane has no capability (master gain/mute, headphone LISTEN/PFL).
import type { JSX, ReactNode } from 'react';
import { Trans } from '@lingui/react/macro';

import type { AudioLoudnessState } from '../../realtime/useAudioLoudness';
import './audio-approved.css';

/** Real short-term program loudness, or undefined (never a fabricated value). */
function programLufs(state: AudioLoudnessState): number | undefined {
  return state.current?.short_term ?? state.current?.momentary;
}

/** The PROGRAM LOUDNESS readout: live telemetry or an honest dash. */
export function AudioLoudnessReadout({
  loudness,
  label,
}: {
  readonly loudness: AudioLoudnessState;
  readonly label: ReactNode;
}): JSX.Element {
  const value = programLufs(loudness);
  return (
    <div className="audio-meta" data-testid="audio-loudness-readout">
      <span className="lbl">{label}</span>
      <span className="clock-mono">
        {value === undefined ? '—' : value.toFixed(1)} LUFS
      </span>
      {value === undefined ? (
        <span className="sr-only">
          <Trans>No loudness telemetry</Trans>
        </span>
      ) : null}
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
  const value = programLufs(loudness);
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

/** Desktop master + monitor regions: visible, with specific unavailable reasons. */
export function AudioMasterRegion(): JSX.Element {
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
        <p className="strip-status" data-testid="audio-master-unavailable">
          <Trans>
            Master gain and mute are unavailable: the control plane exposes
            only per-input gain and mute in the saved audio document.
          </Trans>
        </p>
      </section>
      <div className="mix-note">
        <p className="monitor-line" data-testid="audio-monitor-unavailable">
          <Trans>
            Monitor / LISTEN (PFL) is unavailable: no headphone monitoring
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
