// Last-seen rendering for engine-monotonic device timestamps.
//
// `last_seen_ts` is engine-monotonic nanoseconds, not wall time; it can only
// be aged against the engine clock reference the realtime stream maintains.
// Without a reference (no stream yet) the cell shows an em dash with an
// honest explanation — an age is never fabricated.
import { useSyncExternalStore, type JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';

import { lastSeenAgeSeconds } from './api';
import type { EngineClockRef } from '../realtime/useEngineEvents';

const SECONDS_PER_MINUTE = 60;
const SECONDS_PER_HOUR = 3_600;
const NOW_TICK_MS = 1_000;

// A ticking wall-clock store so renders stay pure (react-hooks/purity):
// `Date.now()` is read inside the interval, cached, and exposed as an
// external-store snapshot. The interval only runs while cells are mounted.
let cachedNowMs = Date.now();
const nowListeners = new Set<() => void>();
let nowTimer: ReturnType<typeof setInterval> | undefined;

function subscribeNow(listener: () => void): () => void {
  nowListeners.add(listener);
  nowTimer ??= setInterval(() => {
    cachedNowMs = Date.now();
    for (const notify of nowListeners) {
      notify();
    }
  }, NOW_TICK_MS);
  return () => {
    nowListeners.delete(listener);
    if (nowListeners.size === 0 && nowTimer !== undefined) {
      clearInterval(nowTimer);
      nowTimer = undefined;
    }
  };
}

function readNowMs(): number {
  return cachedNowMs;
}

/** The current wall time in ms, ticking once a second while subscribed. */
function useNowMs(): number {
  return useSyncExternalStore(subscribeNow, readNowMs, readNowMs);
}

/** A human-readable age for a computed last-seen interval. */
export function LastSeenAge({ seconds }: { readonly seconds: number }): JSX.Element {
  if (seconds < SECONDS_PER_MINUTE) {
    return <Trans>{seconds} s ago</Trans>;
  }
  if (seconds < SECONDS_PER_HOUR) {
    const minutes = Math.round(seconds / SECONDS_PER_MINUTE);
    return <Trans>{minutes} min ago</Trans>;
  }
  const hours = Math.round(seconds / SECONDS_PER_HOUR);
  return <Trans>{hours} h ago</Trans>;
}

/** The last-seen table cell / inline readout. */
export function LastSeenCell({
  lastSeenTs,
  clock,
}: {
  readonly lastSeenTs: number | undefined;
  readonly clock: EngineClockRef | undefined;
}): JSX.Element {
  const { t } = useLingui();
  const nowMs = useNowMs();
  if (lastSeenTs === undefined) {
    return (
      <span
        className="text-sm text-muted-foreground"
        title={t`The device has not answered yet.`}
      >
        <span aria-hidden="true">—</span>
        <span className="sr-only">
          <Trans>Never seen.</Trans>
        </span>
      </span>
    );
  }
  const age = lastSeenAgeSeconds(lastSeenTs, clock, nowMs);
  if (age === undefined) {
    return (
      <span
        className="text-sm text-muted-foreground"
        title={t`The device has answered, but without a live event stream its engine timestamp cannot be aged.`}
      >
        <Trans>seen (age unknown)</Trans>
      </span>
    );
  }
  return (
    <span className="text-xs">
      <LastSeenAge seconds={age} />
    </span>
  );
}
