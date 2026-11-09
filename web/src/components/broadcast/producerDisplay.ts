// Output-producer display derivation — the truth separation the correction-4
// safety review pinned (criteria 7): the producer verdict comes from OBSERVED
// OUTPUT TICKS ONLY. A management connection — however healthy — never feeds
// it; a missing tick is UNKNOWN; an old tick is FROZEN / STALE past an exact
// threshold; a future timestamp cannot verify freshness, so it is UNKNOWN
// too; and reading or changing the state enum never fabricates a tick.
// Pure functions only: no React, no i18n, no side effects.

/** The producer state the control plane reports. */
export type ProducerState = 'producing' | 'stopped' | 'failed' | 'unknown';

/** The derived operator-facing display state. */
export type ProducerDisplay =
  | 'producing'
  | 'stopped'
  | 'failed'
  | 'stale'
  | 'unknown';

/** Why a display resolved to `unknown` — each reason is user-visible copy. */
export type ProducerUnknownReason =
  | 'no-tick'
  | 'future-tick'
  | 'state-unknown';

export interface DerivedProducerDisplay {
  readonly display: ProducerDisplay;
  /** Age of the last observed tick in ms, when computable and not negative. */
  readonly ageMs: number | null;
  /** Populated only when the display is `unknown`, saying why. */
  readonly reason: ProducerUnknownReason | null;
}

function tickAge(lastTickAt: number | null, nowMs: number): number | null {
  if (lastTickAt === null) {
    return null;
  }
  const age = nowMs - lastTickAt;
  return age >= 0 ? age : null;
}

/**
 * Derive the producer display from the reported state and the last observed
 * output tick. Precedence follows the prototype ledger exactly: `failed` and
 * `stopped` report their own state; a missing tick is unknown even when the
 * enum claims producing; a future tick is unverifiable (clock skew), never
 * fresh; a tick older than `staleAfterMs` is frozen/stale even while the
 * enum still says producing; and a fresh tick with an unreported state stays
 * unknown — a tick alone is not a state claim.
 */
export function deriveProducerDisplay(
  state: ProducerState,
  lastTickAt: number | null,
  nowMs: number,
  staleAfterMs: number,
): DerivedProducerDisplay {
  if (state === 'failed') {
    return { display: 'failed', ageMs: tickAge(lastTickAt, nowMs), reason: null };
  }
  if (state === 'stopped') {
    return { display: 'stopped', ageMs: tickAge(lastTickAt, nowMs), reason: null };
  }
  if (lastTickAt === null) {
    return { display: 'unknown', ageMs: null, reason: 'no-tick' };
  }
  const ageMs = nowMs - lastTickAt;
  if (ageMs < 0) {
    return { display: 'unknown', ageMs: null, reason: 'future-tick' };
  }
  if (ageMs >= staleAfterMs) {
    return { display: 'stale', ageMs, reason: null };
  }
  if (state === 'producing') {
    return { display: 'producing', ageMs, reason: null };
  }
  return { display: 'unknown', ageMs, reason: 'state-unknown' };
}
