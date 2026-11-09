// mediaPlayerPresentation — the pure derivations behind MediaPlayersPage:
// the config-vs-live honesty helpers, the live-only vamp gates, the focused
// slot rule, and the per-player operation-record model that drives the page's
// OperationFeedback region and its duplicate-action gating.
//
// TRACKER JOIN (landed): the shared QueryClient operation tracker
// (../components/broadcast/operationTracker, fed by the ONE shell-owned
// WebSocket subscription) is now the authority this page reads. The join rules
// the page-side model must obey:
//   * a `202` alone is `accepted` — OperationFeedback `pending`, never
//     `applied`. Only the tracker record settled by EXACT terminal evidence
//     (corr + topic + resource + event + the verb's expected terminal state)
//     may turn it into `applied`;
//   * the expected terminal state mirrors `CorrKey::for_command`
//     (crates/multiview-control/src/realtime.rs): play→playing, pause→paused,
//     stop→stopped, cue→cued, arm/take-exit→vamping with exitArmed, cancel-exit
//     →vamping without it. `load` and `seek` are NOT correlated server-side
//     (ambiguous outcome): they stay `accepted-unobserved` forever and no
//     expected media state is ever guessed for them;
//   * an unobserved outcome (realtime gap/disconnect or the tracker deadline)
//     is `unknown`, never `failed` and never a rollback claim; a definitive
//     HTTP 4xx/5xx rejection is the ONLY `failed`;
//   * a tracker capacity refusal happens BEFORE any POST: it is a local
//     `refused` record (actionable, localized), never a failure and never an
//     unknown outcome — nothing was sent.
import type { QueryClient } from '@tanstack/react-query';
import { getStoredToken } from '../api/token';
import type { MediaPlayer, TransportAction } from '../api/media-playersQueries';
import { OperationApiError } from '../api/operations';
import type { OperationRecord } from '../components/broadcast/operationOutcomes';
import { OperationOutcomeError } from '../components/broadcast/operationOutcomes';
import type { OperationExpectation } from '../components/broadcast/operationTracker';
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
 * The realtime terminal predicate for one transport verb, mirroring
 * `CorrKey::for_command` (crates/multiview-control/src/realtime.rs): only
 * verbs with a single unambiguous outcome state are correlated server-side.
 * `load` (Loading then Cued) and `seek` (keeps the current state) are
 * deliberately UNCORRELATED — `undefined` here means the submission is
 * accepted-unobserved and no media state may be guessed as its outcome.
 */
export function transportExpectation(
  playerId: string,
  verb: TransportAction,
): OperationExpectation | undefined {
  switch (verb) {
    case 'play':
      return playerStateExpectation(playerId, 'playing');
    case 'pause':
      return playerStateExpectation(playerId, 'paused');
    case 'stop':
      return playerStateExpectation(playerId, 'stopped');
    case 'cue':
      return playerStateExpectation(playerId, 'cued');
    case 'arm-exit':
    case 'take-exit':
      return {
        topic: 'switcher',
        resource: playerId,
        event: 'media.player_state',
        terminal: { kind: 'player-state', state: 'vamping', exitArmed: true },
      };
    case 'cancel-exit':
      return {
        topic: 'switcher',
        resource: playerId,
        event: 'media.player_state',
        terminal: { kind: 'player-state', state: 'vamping', exitArmed: false },
      };
    case 'load':
    case 'seek':
      return undefined;
  }
}

function playerStateExpectation(
  playerId: string,
  state: 'playing' | 'paused' | 'stopped' | 'cued',
): OperationExpectation {
  return {
    topic: 'switcher',
    resource: playerId,
    event: 'media.player_state',
    terminal: { kind: 'player-state', state },
  };
}

/**
 * What the page knows about one player's latest submitted transport command.
 * The phases before and after the real `202` are deliberately distinct:
 * `sending` is a purely local observation (nothing has been accepted yet),
 * while `accepted` exists only once the server's 202 body arrived.
 * `refused` is a local, pre-POST refusal (the shared tracker was out of
 * capacity): no command outcome exists at all. Only the shared tracker's
 * exact terminal evidence may later resolve an `accepted` record.
 */
export type MediaPlayerOperation = (
  | { readonly phase: 'sending'; readonly verb: TransportAction; readonly label: string }
  | {
      readonly phase: 'accepted';
      readonly operationId: string;
      readonly verb: TransportAction;
      readonly label: string;
      /** True for uncorrelated verbs (load/seek): accepted, never applied. */
      readonly uncorrelated: boolean;
    }
  | { readonly phase: 'refused'; readonly reason: string; readonly label: string }
  | { readonly phase: 'failed'; readonly reason: string; readonly label: string }
  | {
      readonly phase: 'unknown';
      readonly verb: TransportAction;
      readonly reason: string;
      readonly label: string;
    }
) & {
  readonly requestId?: number;
  /** Earlier outcomes remain evidence even when another verb is the latest feedback. */
  readonly prior?: readonly MediaPlayerOperation[] | undefined;
};

/**
 * The OperationFeedback view of one operation, joined with the shared
 * tracker's record for its operation id. `applied` appears ONLY via a settled
 * tracker record; `accepted-unobserved` is the honest terminal rest state of
 * an uncorrelated verb (load/seek — no applied event will ever arrive).
 */
export type OperationFeedbackView =
  | {
      readonly phase: 'pending' | 'accepted-unobserved' | 'applied';
      readonly correlationId: string;
    }
  | { readonly phase: 'failed'; readonly failureReason?: string; readonly correlationId?: string }
  | { readonly phase: 'unknown'; readonly unknownReason: string; readonly correlationId?: string };

/**
 * Derive the feedback region's view of the latest operation, joining the
 * page's record with the tracker record for the same operation id.
 * `sending` (before the real 202) and `refused` (no command was submitted)
 * render NOTHING through OperationFeedback — the page keeps separate local
 * lines for them. `trackerUnknownReason` is the caller-localized sentence
 * shown when the tracker reports the outcome unobserved (deadline/gap).
 */
export function operationFeedbackView(
  op: MediaPlayerOperation | undefined,
  tracked: OperationRecord | undefined,
  trackerUnknownReason: string,
): OperationFeedbackView | undefined {
  switch (op?.phase) {
    case undefined:
    case 'sending':
    case 'refused':
      return undefined;
    case 'accepted':
      if (op.uncorrelated) {
        return { phase: 'accepted-unobserved', correlationId: op.operationId };
      }
      switch (tracked?.phase) {
        case undefined:
        case 'accepted':
          return { phase: 'pending', correlationId: op.operationId };
        case 'applied':
          return { phase: 'applied', correlationId: op.operationId };
        case 'unknown':
          return {
            phase: 'unknown',
            unknownReason: trackerUnknownReason,
            correlationId: op.operationId,
          };
        case 'failed':
          // Authoritative failure verdict without a server-supplied reason.
          return { phase: 'failed', correlationId: op.operationId };
      }
      return { phase: 'pending', correlationId: op.operationId };
    case 'failed':
      return { phase: 'failed', failureReason: op.reason };
    case 'unknown':
      return { phase: 'unknown', unknownReason: op.reason };
  }
}

/**
 * The transport verb whose previous outcome is still unresolved for this
 * player, if any: a locally unverifiable submission, or an accepted command
 * the shared tracker reports unobserved (deadline/gap). Repeating THAT verb
 * could duplicate an in-flight command; every other verb stays safe.
 */
export function unresolvedVerb(
  op: MediaPlayerOperation | undefined,
  tracked: OperationRecord | undefined,
): TransportAction | undefined {
  if (op?.phase === 'unknown') {
    return op.verb;
  }
  if (
    op?.phase === 'accepted' &&
    !op.uncorrelated &&
    tracked?.phase === 'unknown'
  ) {
    return op.verb;
  }
  return undefined;
}

/**
 * Whether the latest operation blocks the given verb's key for the same
 * player — the duplicate-action guard:
 *  * `sending` blocks every verb (a POST is genuinely in flight; bounded by
 *    its own settle — it can never deadlock);
 *  * an unresolved verb (unknown transport error, or the tracker reporting
 *    the outcome unobserved) blocks only its own duplicate; every other verb
 *    stays usable, so the page never deadlocks;
 *  * `accepted` (a real 202), an `applied` join, a definitive rejection and a
 *    pre-POST `refused` block nothing — the transport stays usable.
 */
export function verbBlockedByOperation(
  op: MediaPlayerOperation | undefined,
  verb: TransportAction,
  tracked?: OperationRecord,
): boolean {
  if (op === undefined) {
    return false;
  }
  if (op.phase === 'sending') {
    return true;
  }
  return unresolvedVerb(op, tracked) === verb;
}

/** The localized message when the error is (or wraps) a tracker capacity refusal. */
function capacityRefusalMessage(error: unknown): string | undefined {
  if (error instanceof OperationOutcomeError && error.code === 'capacity') {
    return error.message;
  }
  if (
    error instanceof Error &&
    error.cause instanceof OperationOutcomeError &&
    error.cause.code === 'capacity'
  ) {
    return error.message;
  }
  return undefined;
}

/**
 * Classify a tracked submission's error into the record model. A tracker
 * capacity refusal (thrown BEFORE the POST ran) is a local `refused` carrying
 * the localized actionable message — never a `failed` (the server rejected
 * nothing) and never an `unknown` (no command was submitted, so there is no
 * uncertain outcome to guard). Everything else falls through to
 * {@link classifyTransportError}.
 */
export function classifySubmissionError(
  error: unknown,
  verb: TransportAction,
  label: string,
  unknownExplanation: string,
): MediaPlayerOperation {
  const capacity = capacityRefusalMessage(error);
  if (capacity !== undefined) {
    return { phase: 'refused', reason: capacity, label };
  }
  return classifyTransportError(error, verb, label, unknownExplanation);
}

/**
 * Classify a transport error into the record model. A definitive HTTP
 * rejection (a problem document with a 4xx/5xx status — exactly what the
 * shared tracker treats as definitive) is a `failed`; everything else — a
 * transport-level fetch failure, or a 2xx whose body could not be read — is
 * an UNKNOWN outcome: the command may or may not have been applied, and
 * nothing here ever claims a failure or a rollback. `unknownExplanation` is
 * the caller-localized sentence shown under the unknown line; the raw error
 * text is appended for reconciliation.
 */
export function classifyTransportError(
  error: unknown,
  verb: TransportAction,
  label: string,
  unknownExplanation: string,
): MediaPlayerOperation {
  if (
    error instanceof OperationApiError &&
    error.status !== undefined &&
    error.status >= 400 &&
    error.status < 600
  ) {
    const reason =
      error.detail !== undefined ? `${error.message}: ${error.detail}` : error.message;
    return { phase: 'failed', reason, label };
  }
  const raw = error instanceof Error ? error.message : String(error);
  const reason = raw !== '' ? `${unknownExplanation} (${raw})` : unknownExplanation;
  return { phase: 'unknown', verb, reason, label };
}

/**
 * Base key of the page's QueryClient-backed operation records. An opaque
 * in-memory credential epoch partitions the key without exposing the token;
 * navigation within the same identity retains the feedback and evidence.
 */
export const MEDIA_PLAYER_OPERATIONS_QUERY_KEY = ['media-players', 'operations'] as const;

// Credentials never enter query keys, data or developer-facing labels. A
// QueryClient-scoped epoch isolates callbacks issued under an earlier token.
const identities = new WeakMap<QueryClient, { token: string | undefined; epoch: number }>();
export function mediaPlayerIdentity(client: QueryClient): number {
  const token = getStoredToken();
  const current = identities.get(client);
  if (current === undefined) {
    identities.set(client, { token, epoch: 0 });
    return 0;
  }
  if (current.token !== token) {
    const epoch = current.epoch + 1;
    identities.set(client, { token, epoch });
    client.removeQueries({ queryKey: MEDIA_PLAYER_OPERATIONS_QUERY_KEY });
    return epoch;
  }
  return current.epoch;
}

/** Do not put the bearer token in the key, even transiently. */
export function mediaPlayerOperationsKey(epoch: number): readonly (string | number)[] {
  return epoch === 0 ? MEDIA_PLAYER_OPERATIONS_QUERY_KEY : [...MEDIA_PLAYER_OPERATIONS_QUERY_KEY, epoch];
}

/** The per-player latest-operation records the page persists. */
export type MediaPlayerOperations = Record<string, MediaPlayerOperation | undefined>;

/** Read the persisted per-player operation records (empty when none yet). */
export function readMediaPlayerOperations(client: QueryClient): MediaPlayerOperations {
  return client.getQueryData<MediaPlayerOperations>(mediaPlayerOperationsKey(mediaPlayerIdentity(client))) ?? {};
}

/** Keep every earlier command's evidence separate from the latest visible feedback. */
export function replaceMediaPlayerOperation(
  previous: MediaPlayerOperation | undefined,
  next: MediaPlayerOperation,
  tracked: (id: string) => OperationRecord | undefined = () => undefined,
): MediaPlayerOperation {
  if (previous === undefined) return next;
  const { prior = [], ...latest } = previous;
  const retain = (entry: MediaPlayerOperation): boolean =>
    entry.phase === 'unknown' || (entry.phase === 'accepted' && !entry.uncorrelated &&
      tracked(entry.operationId)?.phase !== 'applied' && tracked(entry.operationId)?.phase !== 'failed');
  return { ...next, prior: [...prior, latest].filter(retain) };
}

/** Every unresolved verb, including earlier correlated tracker-unknown outcomes. */
export function unresolvedMediaPlayerOperations(
  op: MediaPlayerOperation | undefined,
  tracked: (id: string) => OperationRecord | undefined,
): MediaPlayerOperation[] {
  if (op === undefined) return [];
  return [...(op.prior ?? []), op].filter((entry) =>
    unresolvedVerb(entry, entry.phase === 'accepted' ? tracked(entry.operationId) : undefined) !== undefined,
  );
}

/**
 * Update one player's latest-operation record in the QueryClient store.
 * `update` receives the player's previous record so guards (e.g. a dismiss
 * clearing only a definitive failure) stay pure and race-free.
 */
export function updateMediaPlayerOperation(
  client: QueryClient,
  playerId: string,
  update: (previous: MediaPlayerOperation | undefined) => MediaPlayerOperation | undefined,
): void {
  const epoch = mediaPlayerIdentity(client);
  client.setQueryData<MediaPlayerOperations>(mediaPlayerOperationsKey(epoch), (previous) => {
    const base: MediaPlayerOperations = previous ?? {};
    return { ...base, [playerId]: update(base[playerId]) };
  });
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
