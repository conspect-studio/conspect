import type { JSX } from 'react';
import { Trans } from '@lingui/react/macro';
import { secondsUntil } from './sourceEvents';
import type { ClockSource } from './sourceEvents';

/** "(in 12 s)" or "(12 s ago)" against the given instant. */
export function Countdown({ at, now }: { readonly at: string; readonly now: number }): JSX.Element {
  const seconds = secondsUntil(at, now);
  const ago = Math.abs(seconds);
  return seconds >= 0 ? <span><Trans>(in {seconds} s)</Trans></span> : <span><Trans>({ago} s ago)</Trans></span>;
}

/** Which clock a time came from, and how far to trust it. */
export function ClockNote({ source, uncertaintyMs }: {
  readonly source: ClockSource | null | undefined;
  readonly uncertaintyMs?: number | null | undefined;
}): JSX.Element | null {
  if (!source) return null;
  const margin = typeof uncertaintyMs === 'number' ? (uncertaintyMs / 1000).toFixed(1) : undefined;
  return <span>
    {source === 'pdt' ? <Trans>Clock source: PDT</Trans>
      : source === 'id3_utc' ? <Trans>Clock source: ID3 UTC</Trans>
      : <Trans>Clock source: receive time</Trans>}
    {margin === undefined ? null : <> <Trans>(±{margin} s)</Trans></>}
  </span>;
}
