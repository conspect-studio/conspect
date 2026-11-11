// The ticking wall clock for producer tick ages. `Date.now()` may not be read
// during render (react-hooks/purity), so — exactly like the devices' last-seen
// clock in `src/devices/lastSeen.tsx` — the impure read happens inside a
// shared interval, is cached, and is exposed as a `useSyncExternalStore`
// snapshot. The first subscription after idle refreshes the cached time
// before React re-reads the snapshot, including after a delayed first mount. One interval serves every mounted consumer and stops when the
// last one unmounts. Tests bypass it by injecting `now` into
// {@link ProducerStatus} for a deterministic age.
import { useSyncExternalStore } from 'react';

const NOW_TICK_MS = 1_000;

let cachedNowMs = Date.now();
const nowListeners = new Set<() => void>();
let nowTimer: ReturnType<typeof setInterval> | undefined;

function subscribeNow(listener: () => void): () => void {
  if (nowListeners.size === 0) {
    cachedNowMs = Date.now();
  }
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

/**
 * The current wall time in ms, ticking once a second while any consumer is
 * mounted. Pure to render against; deterministic tests inject their own
 * clock instead.
 */
export function useProducerNowMs(): number {
  return useSyncExternalStore(subscribeNow, readNowMs, readNowMs);
}
