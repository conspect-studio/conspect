// Pure attempt-token + busy-flag state machine for SubtitlePatchPanel
// (ADR-0110 §12.8, #345 part F3; review rounds 2–3, UI2-1/UI3-1).
//
// Kept in its own module (not co-exported from the component file) so Fast
// Refresh stays happy there and so this reducer is independently unit-
// testable without rendering anything.
//
// Per-destination (output + slot) attempt state, keyed by `${output}:
// ${slot}`. `attempt` is the destination's latest dispatched attempt
// number: every action below carries the attempt it belongs to and is a
// no-op unless it still matches `state[key].attempt`, so a superseded
// attempt's plan/take/outcome can never clobber a newer attempt's result,
// and `busy` — which both drives the disabled buttons and the "pending"
// status text — is updated in the SAME state transition as everything
// else: there is no second piece of state (a parallel `pendingKeys` set,
// or an array mutated inside a `setState` updater and consumed after the
// call, review UI3-1) that could observe a different, stale generation of
// this destination's data. One state per destination, not per source row:
// the destination is what a take applies to, so its status is shown once
// per column (review UI-5), covering both the captioned-source rows and
// the None (unpatch) row.
export interface SubtitleAttemptState {
  readonly attempt: number;
  readonly busy: boolean;
  readonly operationId: string;
  readonly outcome?: string;
  readonly note?: string;
  readonly error?: string;
}

/** The whole panel's attempt state: one {@link SubtitleAttemptState} per destination key. */
export type SubtitleAttempts = Readonly<Record<string, SubtitleAttemptState>>;

/** No destination has ever been attempted. */
export const EMPTY_SUBTITLE_ATTEMPTS: SubtitleAttempts = {};

/**
 * Actions the reducer accepts. `start`/`planRefused`/`accepted`/
 * `takeFailed` carry the attempt they belong to (stamped at dispatch from
 * a per-destination counter) and are ignored unless that attempt is still
 * `state[key].attempt` — i.e. no newer attempt has started on the same
 * destination since. `outcome` carries no attempt: a realtime event is
 * identified by `operation_id`, which (by construction: `accepted` is the
 * only writer of `operationId`, and only when it is still current) is
 * never set to anything but the current attempt's own id, so matching on
 * it is already attempt-exact.
 */
export type SubtitleAttemptAction =
  | { readonly type: 'start'; readonly key: string; readonly attempt: number }
  | { readonly type: 'planRefused'; readonly key: string; readonly attempt: number; readonly message: string }
  | {
      readonly type: 'accepted';
      readonly key: string;
      readonly attempt: number;
      readonly operationId: string;
      readonly note?: string;
      readonly outcome?: string;
    }
  | { readonly type: 'takeFailed'; readonly key: string; readonly attempt: number; readonly message: string }
  | { readonly type: 'outcome'; readonly operationId: string; readonly outcome: string };

function isCurrentAttempt(state: SubtitleAttempts, key: string, attempt: number): boolean {
  return state[key]?.attempt === attempt;
}

/**
 * The attempt-token + busy-flag reducer (review UI2-1, UI3-1): every
 * destination's `busy`/`operationId`/`outcome`/`note`/`error` move
 * together, in one state transition per action, so two destinations
 * settling from two realtime events delivered in the same batch each
 * apply their own, independently correct update — neither can observe or
 * depend on a value the other's update produced in the same pass (the
 * earlier bug: a side channel array populated inside one `setState`
 * updater and drained after the call, which a second, batched updater
 * invocation could run past before the first drain executed).
 */
export function subtitleAttemptsReducer(
  state: SubtitleAttempts,
  action: SubtitleAttemptAction,
): SubtitleAttempts {
  switch (action.type) {
    case 'start':
      return { ...state, [action.key]: { attempt: action.attempt, busy: true, operationId: '' } };
    case 'planRefused':
      if (!isCurrentAttempt(state, action.key, action.attempt)) return state;
      return {
        ...state,
        [action.key]: { attempt: action.attempt, busy: false, operationId: '', error: action.message },
      };
    case 'accepted': {
      if (!isCurrentAttempt(state, action.key, action.attempt)) return state;
      return {
        ...state,
        [action.key]: {
          attempt: action.attempt,
          // Already settled (its outcome was buffered before this
          // `accepted` action landed, review UI-1) ⇒ no longer busy;
          // otherwise stay busy until a later `outcome` action settles it.
          busy: action.outcome === undefined,
          operationId: action.operationId,
          ...(action.note !== undefined ? { note: action.note } : {}),
          ...(action.outcome !== undefined ? { outcome: action.outcome } : {}),
        },
      };
    }
    case 'takeFailed':
      if (!isCurrentAttempt(state, action.key, action.attempt)) return state;
      return {
        ...state,
        [action.key]: { attempt: action.attempt, busy: false, operationId: '', error: action.message },
      };
    case 'outcome': {
      // Every destination whose CURRENT attempt's operationId matches,
      // not yet settled. Processed as one state transition: whichever
      // destinations match are all updated together here, not through a
      // side channel consumed by separate code after this dispatch.
      let changed: Record<string, SubtitleAttemptState> | undefined;
      for (const [key, dest] of Object.entries(state)) {
        if (dest.operationId === action.operationId && dest.outcome === undefined) {
          changed = { ...(changed ?? state), [key]: { ...dest, busy: false, outcome: action.outcome } };
        }
      }
      return changed ?? state;
    }
    default:
      return state;
  }
}
