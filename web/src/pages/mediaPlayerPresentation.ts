// mediaPlayerPresentation — the pure derivations behind MediaPlayersPage:
// the config-vs-live honesty helpers, the live-only vamp gates, the focused
// slot rule, and the per-player operation-record model that drives the page's
// OperationFeedback region and its duplicate-action gating.
//
// TRACKER ADAPTER SEAM (read before wiring): the realtime operation tracker
// is a separate workstream; its hook API is being written and is deliberately
// NOT imported or guessed here. The page keeps its own latest-wins record per
// player. When the tracker lands it should resolve `accepted` records against
// authoritative evidence — the sanctioned join point is
// `../components/broadcast/operationOutcomes.ts` (a `media.player_state`
// frame's envelope `corr` is minted from the 202's operation id by the
// control plane). Until that join exists, `accepted` maps to
// OperationFeedback's `pending` phase and NEVER to `applied`: no
// uncorrelated state may claim success, and no record here can age itself
// into a failure (an unobserved outcome stays `unknown`).
import type { MediaPlayer, TransportAction } from '../api/media-playersQueries';
import { OperationApiError } from '../api/operations';
import type { BadgeProps } from '../components/ui/badge';
import type { MediaPlayerEvent } from '../realtime/generated-types';

/** The transport-state `kind` discriminants we render badges for. */
export type StateKind = MediaPlayerEvent['state']['kind'];

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

/** The configured-default asset id on a player body, if declared. */
export function configuredDefaultAsset(player: MediaPlayer): string | undefined {
  const body: unknown = player.body;
  if (isRecord(body) && typeof body.default === 'string') {
    return body.default;
  }
  return undefined;
}

/** Whether the player's configured default loop is on (vamp-on-start). */
export function configuredLoop(player: MediaPlayer): boolean {
  const body: unknown = player.body;
  return isRecord(body) && body.loop_default === true;
}

/** Map a transport-state kind to a status-badge variant. */
export function stateVariant(kind: StateKind): BadgeProps['variant'] {
  switch (kind) {
    case 'playing':
    case 'vamping':
      return 'live';
    case 'cued':
    case 'paused':
      return 'reconnecting';
    case 'loading':
      return 'stale';
    case 'eof':
      return 'nosignal';
    case 'stopped':
      return 'offline';
  }
}

/** Whether the live event says the player is mid-vamp (in its loop window). */
export function isVamping(live: MediaPlayerEvent | undefined): boolean {
  return live?.state.kind === 'vamping';
}

/** Whether a clean vamp exit is currently armed — live truth only. */
export function isExitArmed(live: MediaPlayerEvent | undefined): boolean {
  return live?.state.kind === 'vamping' && live.state.exit_armed;
}

/**
 * Why one vamp-exit key is unavailable. Gates derive from the live
 * `media.player_state` ONLY: a missing observation gates as `no-live-state`
 * and never claims the player is "not vamping" or stopped.
 */
export type VampGateReason =
  | 'no-live-state'
  | 'not-vamping'
  | 'exit-already-armed'
  | 'no-armed-exit';

/** The vamp-exit key being gated (arm / take / cancel). */
export type VampExitKey = 'arm' | 'take' | 'cancel';

/** Gate one vamp-exit key against the live state; `undefined` means available. */
export function vampGate(
  live: MediaPlayerEvent | undefined,
  key: VampExitKey,
): VampGateReason | undefined {
  if (live === undefined) {
    return 'no-live-state';
  }
  switch (key) {
    case 'arm':
      if (live.state.kind !== 'vamping') {
        return 'not-vamping';
      }
      return live.state.exit_armed ? 'exit-already-armed' : undefined;
    case 'take':
      return live.state.kind !== 'vamping' ? 'not-vamping' : undefined;
    case 'cancel':
      return live.state.kind === 'vamping' && live.state.exit_armed
        ? undefined
        : 'no-armed-exit';
  }
}

/**
 * What the page knows about one player's latest submitted transport command.
 * The phases before and after the real `202` are deliberately distinct:
 * `sending` is a purely local observation (nothing has been accepted yet),
 * while `accepted` exists only once the server's 202 body arrived. The
 * tracker bridge (see the module header) is the only thing that may later
 * resolve an `accepted` record with an authoritative outcome.
 */
export type MediaPlayerOperation =
  | { readonly phase: 'sending'; readonly verb: TransportAction; readonly label: string }
  | { readonly phase: 'accepted'; readonly operationId: string; readonly label: string }
  | { readonly phase: 'failed'; readonly reason: string; readonly label: string }
  | {
      readonly phase: 'unknown';
      readonly verb: TransportAction;
      readonly reason: string;
      readonly label: string;
    };

/** The OperationFeedback view of one operation (never `applied`: see header). */
export type OperationFeedbackView =
  | { readonly phase: 'pending'; readonly correlationId: string }
  | { readonly phase: 'failed'; readonly failureReason: string }
  | { readonly phase: 'unknown'; readonly unknownReason: string };

/**
 * Derive the feedback region's view of the latest operation. `sending`
 * renders NOTHING through OperationFeedback: its pending wording says
 * "accepted", and before the real 202 nothing has been accepted — the page
 * keeps a separate local sending status instead.
 */
export function operationFeedbackView(
  op: MediaPlayerOperation | undefined,
): OperationFeedbackView | undefined {
  switch (op?.phase) {
    case undefined:
    case 'sending':
      return undefined;
    case 'accepted':
      return { phase: 'pending', correlationId: op.operationId };
    case 'failed':
      return { phase: 'failed', failureReason: op.reason };
    case 'unknown':
      return { phase: 'unknown', unknownReason: op.reason };
  }
}

/**
 * Whether the latest operation blocks the given verb's key for the same
 * player — the duplicate-action guard for the windows without an
 * authoritative event join:
 *  * `sending` blocks every verb (a POST is genuinely in flight; bounded by
 *    its own settle — it can never deadlock);
 *  * `unknown` blocks only the duplicate of the possibly-in-flight verb;
 *    every other verb stays usable, so the page never deadlocks while the
 *    tracker bridge is still pending;
 *  * `accepted` (a real 202) and `failed` (a definitive rejection) block
 *    nothing — the transport stays usable exactly as before.
 */
export function verbBlockedByOperation(
  op: MediaPlayerOperation | undefined,
  verb: TransportAction,
): boolean {
  if (op === undefined) {
    return false;
  }
  if (op.phase === 'sending') {
    return true;
  }
  if (op.phase === 'unknown') {
    return op.verb === verb;
  }
  return false;
}

/**
 * Classify a transport error into the record model. A definitive HTTP
 * rejection (a problem document with a status) is a `failed`; everything
 * else — a transport-level fetch failure, or a 2xx whose body could not be
 * read — is an UNKNOWN outcome: the command may or may not have been
 * applied, and nothing here ever claims a failure or a rollback.
 * `unknownExplanation` is the caller-localized sentence shown under the
 * unknown line; the raw error text is appended for reconciliation.
 */
export function classifyTransportError(
  error: unknown,
  verb: TransportAction,
  label: string,
  unknownExplanation: string,
): MediaPlayerOperation {
  if (error instanceof OperationApiError && error.status !== undefined) {
    const reason =
      error.detail !== undefined ? `${error.message}: ${error.detail}` : error.message;
    return { phase: 'failed', reason, label };
  }
  const raw = error instanceof Error ? error.message : String(error);
  const reason = raw !== '' ? `${unknownExplanation} (${raw})` : unknownExplanation;
  return { phase: 'unknown', verb, reason, label };
}

/**
 * The slot whose transport stays in the phone fold: the operator's
 * controlled pick, else the first configured player. A picked slot that
 * vanishes (config refetch) falls back to the first — the focus never
 * dangles on a player that no longer exists.
 */
export function effectiveSelectedPlayerId(
  selected: string | null | undefined,
  players: readonly MediaPlayer[],
): string | undefined {
  if (selected !== null && selected !== undefined && players.some((p) => p.id === selected)) {
    return selected;
  }
  return players[0]?.id;
}
