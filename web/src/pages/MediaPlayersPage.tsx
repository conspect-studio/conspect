// Media players — the VT (video-tape) transport panel (ADR-0057 + ADR-0097).
//
// Each configured media player is a pre-declared, bus-selectable channel that
// rolls an asset with VT-style transport. This page lists the players
// (`GET /api/v1/media/players`) as compact broadcast cards (TaskWorkspace):
// the title, an always-visible status line and the 48px transport keys stay
// in the phone fold; the vamp-exit triad and the configured-default facts sit
// behind the per-card Details disclosure. On the phone ONE focused slot shows
// its console while the other slots collapse to a 48px switch row (broadcast
// design round 3); from `md:` up every console is in flow.
//
// Every command is submitted through the shared QueryClient operation tracker
// (`useSubmitTrackedOperation`, fed by the ONE shell-owned WebSocket
// subscription) with the existing transport hook as its real POST:
//   * tracker capacity is reserved BEFORE the POST. A capacity refusal shows
//     the localized actionable message and nothing is ever sent;
//   * the real 202's operation id — minted by the server, never here — binds
//     the tracker record. Correlated verbs (play/pause/stop/cue and the
//     vamp-exit triad, mirroring `CorrKey::for_command`) resolve
//     pending→applied ONLY on exact terminal evidence (corr + topic +
//     resource + event + the verb's own expected terminal state). Uncorrelated
//     verbs (load/seek — ambiguous outcome server-side) stay
//     accepted-unobserved: no applied event exists for them, so no expected
//     media state is ever guessed;
//   * a definitive HTTP 4xx/5xx rejection is `failed` (dismissable); anything
//     unverifiable — a transport error after send, a realtime gap or the
//     tracker deadline — is `unknown`, never a failure or a rollback claim.
//     Only the duplicate of the possibly-in-flight verb is blocked; every
//     other verb stays usable — no deadlock;
//   * the per-player latest feedback and earlier unresolved commands persist
//     in a credential-partitioned QueryClient store across navigation; a
//     changed identity drops the prior store and its in-flight callbacks. The transport
//     result may carry a completion promise; it is deliberately ignored — no
//     second feedback channel and no premature success claim.
//
// The player's live transport state, playhead and loaded asset arrive ONLY on
// the realtime `media.player_state` stream (engine isolation, inv #10): a
// stale or absent stream shows "no live state" and never blocks, and a
// missing observation never infers a state (a silent player is not "stopped";
// a boot-idle player with a configured default has loaded nothing — the
// configured default is shown as its own separate fact). The playhead renders
// as raw integer frames: the event carries no cadence, so no timecode is
// invented here.
//
// Trim (in/out/vamp frames) is a property of the referenced ASSET, not the
// player, so it is not shown per-player here — the live playhead + transport
// state is the authoritative VT readout. The load dialog takes an asset id
// only (the shipped backend): no asset inventory exists to browse, which the
// dialog says honestly; save/unload verbs land with ML4 and are absent here.
import { useEffect, useMemo, useState, useSyncExternalStore } from 'react';
import type { JSX } from 'react';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { Trans, useLingui } from '@lingui/react/macro';
import {
  CheckCircle2,
  CircleStop,
  FlagTriangleRight,
  FolderInput,
  Pause,
  Play,
  Repeat,
  SkipBack,
  XCircle,
} from 'lucide-react';

import {
  useMediaPlayerTransport,
  useMediaPlayers,
} from '../api/media-playersQueries';
import type {
  MediaPlayer,
  TransportAction,
  TransportVars,
} from '../api/media-playersQueries';
import { useMediaPlayerStates } from '../realtime/useEngineEvents';
import type { MediaPlayerEvent } from '../realtime/generated-types';
import { OperationFeedback, TaskWorkspace, TouchAction } from '../components/broadcast';
import { useSubmitTrackedOperation } from '../components/broadcast/useOperationFeedback';
import { getOperationTracker, OPERATION_TRACKER_QUERY_KEY } from '../components/broadcast/operationTracker';
import { createOperationOutcomeState } from '../components/broadcast/operationOutcomes';
import type { OperationOutcomeState } from '../components/broadcast/operationOutcomes';
import { getStoredToken } from '../api/token';
import { PageHeader } from '../components/PageHeader';
import { Badge } from '../components/ui/badge';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '../components/ui/dialog';
import { Input } from '../components/ui/input';
import { Label } from '../components/ui/label';
import {
  classifySubmissionError,
  configuredDefaultAsset,
  configuredLoop,
  effectiveSelectedPlayerId,
  isExitArmed,
  mediaPlayerIdentity,
  mediaPlayerIdentitySnapshot,
  mediaPlayerOperationsKey,
  operationFeedbackView,
  readMediaPlayerOperations,
  replaceMediaPlayerOperation,
  unresolvedMediaPlayerOperations,
  stateVariant,
  transportExpectation,
  updateMediaPlayerOperation,
  vampGate,
  verbBlockedByOperation,
} from './mediaPlayerPresentation';
import type {
  MediaPlayerOperation,
  MediaPlayerOperations,
  StateKind,
  VampGateReason,
} from './mediaPlayerPresentation';

/** Props for one player's compact VT card (a component so the per-operation tracker read stays hook-legal). */
interface MediaPlayerCardProps {
  readonly player: MediaPlayer;
  readonly live: MediaPlayerEvent | undefined;
  /** Latest feedback and preserved earlier command evidence. */
  readonly op: MediaPlayerOperation | undefined;
  readonly tracker: OperationOutcomeState;
  readonly focused: boolean;
  readonly detailsOpen: boolean;
  readonly onDetailsToggle: (open: boolean) => void;
  readonly onFocus: () => void;
  readonly onSubmit: (player: MediaPlayer, vars: TransportVars, label: string) => void;
  readonly onOpenLoad: (player: MediaPlayer) => void;
  readonly onDismissOutcome: (playerId: string) => void;
  readonly stateLabel: (kind: StateKind) => string;
  readonly operationLabel: (verb: TransportAction, player: MediaPlayer) => string;
}

/**
 * One player's compact VT card (TaskWorkspace) with its transport console and
 * the tracker-joined feedback region. The shared tracker record for the card's
 * latest operation id is read here (per-card, never in a loop) and joined
 * with the page's own record: `applied` and tracker-`unknown` verdicts come
 * from that record alone.
 */
function MediaPlayerCard({
  player,
  live,
  op,
  tracker,
  focused,
  detailsOpen,
  onDetailsToggle,
  onFocus,
  onSubmit,
  onOpenLoad,
  onDismissOutcome,
  stateLabel,
  operationLabel,
}: MediaPlayerCardProps): JSX.Element {
  const { t } = useLingui();
  // Shared tracker records are read for both the latest feedback and all
  // earlier uncertain outcomes; load/seek have no correlated tracker record.
  const tracked = op?.phase === 'accepted' && !op.uncorrelated
    ? tracker.records.get(op.operationId) : undefined;
  const unresolved = unresolvedMediaPlayerOperations(op, (id) => tracker.records.get(id));
  const feedback = operationFeedbackView(
    op,
    tracked,
    t`The applied event was not observed on the realtime stream. The command may or may not have been applied.`,
  );
  const kind = live?.state.kind;
  const configuredDefault = configuredDefaultAsset(player);
  const gates = {
    arm: vampGate(live, 'arm'),
    take: vampGate(live, 'take'),
    cancel: vampGate(live, 'cancel'),
  };
  const keyBlocked = (verb: TransportAction): boolean =>
    verbBlockedByOperation(op, verb, tracked) || unresolved.some((entry) => 'verb' in entry && entry.verb === verb);
  const keyPending = (verb: TransportAction): boolean =>
    op?.phase === 'sending' && op.verb === verb;
  const duplicateReason = (verb: TransportAction): string | undefined =>
    unresolved.some((entry) => 'verb' in entry && entry.verb === verb)
      ? t`The previous command's outcome is unknown — repeating it could duplicate the command.`
      : undefined;
  const runVerb = (verb: Exclude<TransportAction, 'load' | 'cue' | 'seek'>): void => {
    onSubmit(player, { id: player.id, action: verb }, operationLabel(verb, player));
  };

  /** Localized, human-readable reason for a vamp gate. */
  const gateReason = (gate: VampGateReason | undefined): string | undefined => {
    switch (gate) {
      case undefined:
        return undefined;
      case 'no-live-state':
        return t`No live state yet — the player's vamp status is unknown.`;
      case 'not-vamping':
        return t`Only while the player is vamping.`;
      case 'exit-already-armed':
        return t`An exit is already armed.`;
      case 'no-armed-exit':
        return t`No exit is armed.`;
    }
  };

  return (
    <TaskWorkspace
      title={
        <span className="flex min-w-0 flex-wrap items-baseline gap-x-2 break-words">
          <span lang="" dir="auto" className="font-medium">
            {player.name}
          </span>
          <code className="text-xs text-muted-foreground">{player.id}</code>
        </span>
      }
      status={
        <span className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1 break-words">
          {kind !== undefined ? (
            <Badge variant={stateVariant(kind)}>
              {stateLabel(kind)}
              {isExitArmed(live) ? ` · ${t`exit armed`}` : ''}
            </Badge>
          ) : (
            <span>
              <Trans>No live state</Trans>
            </span>
          )}
          <span className="min-w-0">
            <Trans>Loaded</Trans>:{' '}
            {live?.asset !== undefined && live.asset !== '' ? (
              <code className="break-all rounded bg-muted px-1 py-0.5 font-mono">
                {live.asset}
              </code>
            ) : (
              <span>
                <Trans>Not loaded</Trans>
              </span>
            )}
          </span>
          <span className="tabular-nums">
            <Trans>Position</Trans>:{' '}
            {live !== undefined ? `${String(live.position_frames)} ${t`frames`}` : '—'}
          </span>
        </span>
      }
      details={
        <div className="grid gap-3">
          <div className="flex flex-wrap items-start gap-2">
            <TouchAction
              variant="outline"
              disabled={gates.arm !== undefined || keyBlocked('arm-exit')}
              pending={keyPending('arm-exit')}
              disabledReason={gateReason(gates.arm) ?? duplicateReason('arm-exit')}
              aria-label={`${t`Arm vamp exit`}: ${player.id}`}
              onPress={(): void => {
                runVerb('arm-exit');
              }}
            >
              <FlagTriangleRight aria-hidden="true" />
              <Trans>Arm exit</Trans>
            </TouchAction>
            <TouchAction
              variant="outline"
              disabled={gates.take !== undefined || keyBlocked('take-exit')}
              pending={keyPending('take-exit')}
              disabledReason={gateReason(gates.take) ?? duplicateReason('take-exit')}
              aria-label={`${t`Take vamp exit`}: ${player.id}`}
              onPress={(): void => {
                runVerb('take-exit');
              }}
            >
              <Play aria-hidden="true" />
              <Trans>Take exit</Trans>
            </TouchAction>
            <TouchAction
              variant="ghost"
              disabled={gates.cancel !== undefined || keyBlocked('cancel-exit')}
              pending={keyPending('cancel-exit')}
              disabledReason={gateReason(gates.cancel) ?? duplicateReason('cancel-exit')}
              aria-label={`${t`Cancel vamp exit`}: ${player.id}`}
              onPress={(): void => {
                runVerb('cancel-exit');
              }}
            >
              <XCircle aria-hidden="true" />
              <Trans>Cancel exit</Trans>
            </TouchAction>
          </div>
          <div className="grid gap-1.5 text-xs text-muted-foreground">
            <p className="flex min-w-0 flex-wrap break-words">
              <Trans>Configured default</Trans>:{' '}
              {configuredDefault !== undefined && configuredDefault !== '' ? (
                <code className="break-all rounded bg-muted px-1 py-0.5 font-mono">
                  {configuredDefault}
                </code>
              ) : (
                <Trans>None</Trans>
              )}
            </p>
            {configuredLoop(player) ? (
              <Badge variant="secondary" className="gap-1 self-start">
                <Repeat aria-hidden="true" className="size-3" />
                <Trans>Loop default</Trans>
              </Badge>
            ) : null}
          </div>
        </div>
      }
      detailsOpen={detailsOpen}
      onDetailsToggle={onDetailsToggle}
    >
      {!focused ? (
        <TouchAction
          variant="outline"
          className="md:hidden"
          aria-label={`${t`Focus player transport`}: ${player.id}`}
          onPress={onFocus}
        >
          <Trans>Focus transport</Trans>
        </TouchAction>
      ) : null}
      <div
        data-testid={`player-console-${player.id}`}
        className={focused ? 'block' : 'hidden md:block'}
      >
          <div className="flex flex-wrap items-start gap-2">
            <TouchAction
              variant="outline"
              disabled={keyBlocked('load')}
              pending={keyPending('load')}
              disabledReason={duplicateReason('load')}
              aria-label={`${t`Load asset into player`}: ${player.id}`}
              onPress={(): void => {
                onOpenLoad(player);
              }}
            >
              <FolderInput aria-hidden="true" />
              <Trans>Load</Trans>
            </TouchAction>
            <TouchAction
              variant="outline"
              disabled={keyBlocked('cue')}
              pending={keyPending('cue')}
              disabledReason={duplicateReason('cue')}
              aria-label={`${t`Cue player to in-point`}: ${player.id}`}
              onPress={(): void => {
                onSubmit(player, { id: player.id, action: 'cue' }, operationLabel('cue', player));
              }}
            >
              <SkipBack aria-hidden="true" />
              <Trans>Cue</Trans>
            </TouchAction>
            <TouchAction
              variant="outline"
              disabled={keyBlocked('play')}
              pending={keyPending('play')}
              disabledReason={duplicateReason('play')}
              aria-label={`${t`Play player`}: ${player.id}`}
              onPress={(): void => {
                runVerb('play');
              }}
            >
              <Play aria-hidden="true" />
              <Trans>Play</Trans>
            </TouchAction>
            <TouchAction
              variant="outline"
              disabled={keyBlocked('pause')}
              pending={keyPending('pause')}
              disabledReason={duplicateReason('pause')}
              aria-label={`${t`Pause player`}: ${player.id}`}
              onPress={(): void => {
                runVerb('pause');
              }}
            >
              <Pause aria-hidden="true" />
              <Trans>Pause</Trans>
            </TouchAction>
            <TouchAction
              variant="ghost"
              disabled={keyBlocked('stop')}
              pending={keyPending('stop')}
              disabledReason={duplicateReason('stop')}
              aria-label={`${t`Stop player`}: ${player.id}`}
              onPress={(): void => {
                runVerb('stop');
              }}
            >
              <CircleStop aria-hidden="true" />
              <Trans>Stop</Trans>
            </TouchAction>
          </div>
          {unresolved.filter((entry) => entry !== op).length > 0 ? (
            <div role="status" className="mt-2 rounded-md border border-amber-500/50 p-2 text-xs break-words">
              <Trans>Earlier unresolved commands</Trans>:
              <ul className="list-inside list-disc">
                {unresolved.filter((entry) => entry !== op).map((entry, index) => (
                  <li key={`${String(entry.requestId ?? index)}-${'verb' in entry ? entry.verb : ''}`}>
                    {entry.label} — <Trans>outcome unknown; do not repeat</Trans>
                    {entry.phase === 'accepted' ? <code className="ml-1 break-all">{entry.operationId}</code> : null}
                    {entry.phase === 'unknown' ? <span className="ml-1">{entry.reason}</span> : null}
                  </li>
                ))}
              </ul>
            </div>
          ) : null}
          {op?.phase === 'sending' ? (
            // A separate local submitting status: before the real 202 nothing
            // says accepted (the OperationFeedback pending wording is only
            // honest once the server's 202 body has arrived).
            <p role="status" className="mt-2 text-xs text-muted-foreground">
              {t`Sending ${op.label}…`}
            </p>
          ) : null}
          {op?.phase === 'refused' ? (
            // The tracker refused BEFORE any POST: an actionable, localized
            // message — never a raw exception, and never a failed/unknown
            // command outcome (nothing was sent).
            <p role="alert" className="mt-2 text-sm text-destructive">
              {op.reason}
            </p>
          ) : null}
          {op !== undefined && feedback !== undefined ? (
            feedback.phase === 'accepted-unobserved' ? (
              // Uncorrelated verb (load/seek): accepted, but the control
              // plane correlates no outcome for it, so honestly no applied
              // event will ever arrive — the "waiting for the applied event"
              // wording would be a lie here.
              <div
                role="status"
                aria-live="polite"
                data-phase="accepted-unobserved"
                className="mt-2 flex min-w-0 flex-col gap-1 break-words rounded-md border bg-muted/40 p-2"
              >
                <p className="flex min-w-0 items-center gap-2 text-sm [&>span]:min-w-0 [&>span]:break-all">
                  <CheckCircle2 className="size-4 shrink-0" aria-hidden="true" />
                  <span>
                    <Trans>{op.label} accepted.</Trans>
                  </span>
                </p>
                <p className="text-sm text-muted-foreground">
                  <Trans>
                    The control plane does not correlate this command's
                    outcome, so no applied event will arrive.
                  </Trans>
                </p>
                <p className="text-xs text-muted-foreground">
                  <Trans>Correlation</Trans>{' '}
                  <code className="break-all rounded bg-muted px-1 py-0.5 font-mono">
                    {feedback.correlationId}
                  </code>
                </p>
              </div>
            ) : (
              <div className="mt-2">
                <OperationFeedback
                  phase={feedback.phase}
                  operationLabel={op.label}
                  correlationId={feedback.correlationId}
                  failureReason={
                    feedback.phase === 'failed' ? feedback.failureReason : undefined
                  }
                  unknownReason={
                    feedback.phase === 'unknown' ? feedback.unknownReason : undefined
                  }
                  onDismiss={
                    op.phase === 'failed' && feedback.phase === 'failed'
                      ? (): void => {
                          onDismissOutcome(player.id);
                        }
                      : undefined
                  }
                />
              </div>
            )
          ) : null}
        </div>
    </TaskWorkspace>
  );
}

let nextMediaPlayerRequestId = 0;

/** The media-player (VT) transport panel. */
export function MediaPlayersPage(): JSX.Element {
  const { t } = useLingui();
  const queryClient = useQueryClient();
  const players = useMediaPlayers();
  const transport = useMediaPlayerTransport();
  const liveStates = useMediaPlayerStates();
  const submitTracked = useSubmitTrackedOperation();

  const [loadFor, setLoadFor] = useState<MediaPlayer | null>(null);
  const [assetDraft, setAssetDraft] = useState('');
  const [assetMissing, setAssetMissing] = useState(false);
  // The focused slot and each card's Details disclosure are caller-owned
  // state (the TaskWorkspace controlled-disclosure contract).
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [detailsOpen, setDetailsOpen] = useState<Record<string, boolean>>({});
  // Latest feedback plus preserved unresolved evidence per player, scoped to
  // an opaque credential epoch. Navigation within an identity retains it;
  // callbacks from another epoch cannot publish into the new operator view.
  const identityEpoch = useSyncExternalStore(
    (notify) => {
      const listener = (): void => { notify(); };
      window.addEventListener('storage', listener);
      // Query creation/observer attachment can occur during render. Those
      // events are not credential signals and must not synchronously notify
      // React while it is constructing the incoming epoch's query.
      const unsubscribe = queryClient.getQueryCache().subscribe((event) => {
        if (event.type === 'updated') listener();
      });
      return (): void => { window.removeEventListener('storage', listener); unsubscribe(); };
    },
    () => mediaPlayerIdentitySnapshot(queryClient),
    () => mediaPlayerIdentitySnapshot(queryClient),
  );
  useEffect(() => {
    mediaPlayerIdentity(queryClient);
    getOperationTracker(queryClient).resetIdentity(getStoredToken());
  }, [queryClient, identityEpoch]);
  const operationsQuery = useQuery<MediaPlayerOperations>({
    queryKey: mediaPlayerOperationsKey(identityEpoch),
    queryFn: (): MediaPlayerOperations => ({}),
    enabled: false,
    initialData: {},
  });
  // `initialData` makes `data` always defined — the empty store is the
  // clean slate, never a loading state.
  // Never render an old observer's data during a token-change render. The
  // token is compared only in memory, never copied into query keys/devtools.
  const ops: MediaPlayerOperations = readMediaPlayerOperations(queryClient);
  void operationsQuery.data;
  const trackerQuery = useQuery<OperationOutcomeState>({
    queryKey: OPERATION_TRACKER_QUERY_KEY,
    queryFn: () => getOperationTracker(queryClient).snapshot(),
    enabled: false,
    initialData: createOperationOutcomeState,
  });
  // snapshot() also resets tracker identity and publishes; never call it in
  // render. Old records cannot be joined because ops is credential-guarded.
  const tracker = queryClient.getQueryData<OperationOutcomeState>(OPERATION_TRACKER_QUERY_KEY)
    ?? createOperationOutcomeState();
  void operationsQuery;
  void trackerQuery;

  const data = useMemo<MediaPlayer[]>(() => players.data ?? [], [players.data]);
  const focusedId = effectiveSelectedPlayerId(selectedId, data);

  /** Localized, human-readable label for a transport-state kind. */
  const stateLabel = (kind: StateKind): string => {
    switch (kind) {
      case 'loading':
        return t`Loading`;
      case 'cued':
        return t`Cued`;
      case 'playing':
        return t`Playing`;
      case 'paused':
        return t`Paused`;
      case 'stopped':
        return t`Stopped`;
      case 'vamping':
        return t`Vamping`;
      case 'eof':
        return t`End of file`;
    }
  };

  /** What the feedback region calls one submitted operation. */
  const operationLabel = (verb: TransportAction, player: MediaPlayer): string => {
    switch (verb) {
      case 'load':
        return t`Load ${player.name}`;
      case 'cue':
        return t`Cue ${player.name}`;
      case 'play':
        return t`Play ${player.name}`;
      case 'pause':
        return t`Pause ${player.name}`;
      case 'stop':
        return t`Stop ${player.name}`;
      case 'seek':
        return t`Seek ${player.name}`;
      case 'arm-exit':
        return t`Arm vamp exit on ${player.name}`;
      case 'take-exit':
        return t`Take vamp exit on ${player.name}`;
      case 'cancel-exit':
        return t`Cancel vamp exit on ${player.name}`;
    }
  };

  /**
   * Submit one transport verb for one player through the shared tracker. The
   * local `sending` record is written BEFORE the POST; the real 202 (and only
   * the real 202) replaces it with the accepted record carrying the server's
   * operation id. The verb's expected terminal state mirrors
   * `CorrKey::for_command` — `undefined` for the uncorrelated load/seek, which
   * stay accepted-unobserved. The transport result's optional completion
   * promise is deliberately ignored: the shared tracker is the single
   * feedback authority (no double feedback, no premature success).
   */
  const submit = (player: MediaPlayer, vars: TransportVars, label: string): void => {
    const verb = vars.action;
    const token = getStoredToken();
    const epoch = mediaPlayerIdentity(queryClient);
    const requestId = ++nextMediaPlayerRequestId;
    const currentIdentity = (): boolean => {
      const same = token === getStoredToken() && epoch === mediaPlayerIdentity(queryClient);
      if (!same) mediaPlayerIdentity(queryClient);
      return same;
    };
    const prior = readMediaPlayerOperations(queryClient)[player.id];
    if (prior?.phase === 'sending' || unresolvedMediaPlayerOperations(
      prior, (id) => getOperationTracker(queryClient).snapshot().records.get(id),
    ).some((entry) => 'verb' in entry && entry.verb === verb)) return;
    updateMediaPlayerOperation(queryClient, player.id, (previous) =>
      replaceMediaPlayerOperation(previous, { phase: 'sending', verb, label, requestId },
        (id) => getOperationTracker(queryClient).snapshot().records.get(id)));
    void submitTracked(
      () => transport.mutateAsync(vars),
      transportExpectation(player.id, verb),
    ).then(
      (result): void => {
        if (!currentIdentity()) return;
        updateMediaPlayerOperation(queryClient, player.id, (previous) =>
          previous?.requestId === requestId
            ? { ...previous, phase: 'accepted', operationId: result.operationId,
                verb, label, uncorrelated: result.kind === 'accepted-unobserved' }
            : previous);
      },
      (error: unknown): void => {
        // A definitive HTTP rejection failed. A capacity refusal (before any
        // POST) is a local refusal. Anything else (a transport-level failure,
        // an unreadable 2xx) is an UNKNOWN outcome: the command may or may
        // not have been applied, and nothing here ever claims a rollback.
        if (!currentIdentity()) return;
        updateMediaPlayerOperation(queryClient, player.id, (previous) =>
          previous?.requestId === requestId
            ? { ...classifySubmissionError(
                error, verb, label,
                t`The control plane did not answer. The command may or may not have been applied.`,
              ), requestId, prior: previous.prior }
            : previous,
        );
      },
    );
  };

  /** Dismiss a definitive failure — the only record that clears here. */
  const dismissOutcome = (playerId: string): void => {
    updateMediaPlayerOperation(queryClient, playerId, (previous) =>
      previous?.phase === 'failed' ? undefined : previous,
    );
  };

  const openLoad = (player: MediaPlayer): void => {
    // The configured default prefills the DRAFT as a starting point — never
    // as a loaded-asset claim (rule 27).
    setAssetDraft(configuredDefaultAsset(player) ?? '');
    setAssetMissing(false);
    setLoadFor(player);
  };

  const loadOp = loadFor !== null ? ops[loadFor.id] : undefined;
  const dialogPending = loadOp?.phase === 'sending' && loadOp.verb === 'load';

  const submitLoad = (): void => {
    const player = loadFor;
    if (player === null) {
      return;
    }
    const asset = assetDraft.trim();
    if (asset === '') {
      setAssetMissing(true);
      return;
    }
    // Close the dialog: the outcome is tracked persistently in the card's
    // inline feedback region, so nothing needs to stay open to be seen.
    setLoadFor(null);
    submit(player, { id: player.id, action: 'load', asset }, operationLabel('load', player));
  };

  const renderPlayer = (player: MediaPlayer): JSX.Element => (
    <MediaPlayerCard
      key={player.id}
      player={player}
      live={liveStates[player.id]}
      op={ops[player.id]}
      tracker={tracker}
      focused={focusedId === player.id}
      detailsOpen={detailsOpen[player.id] === true}
      onDetailsToggle={(open: boolean): void => {
        setDetailsOpen((prev) => ({ ...prev, [player.id]: open }));
      }}
      onFocus={(): void => {
        setSelectedId(player.id);
      }}
      onSubmit={submit}
      onOpenLoad={openLoad}
      onDismissOutcome={dismissOutcome}
      stateLabel={stateLabel}
      operationLabel={operationLabel}
    />
  );

  return (
    <>
      <PageHeader
        title={<Trans>Media players</Trans>}
        description={
          <Trans>
            VT transport for the configured media-player channels. Load an
            asset, cue, play, pause, stop, and — for a vamp/loop fill — arm,
            take, or cancel a clean exit at the next vamp boundary. Each
            command is accepted asynchronously; the player's live state
            and playhead arrive on the realtime stream.
          </Trans>
        }
      />

      {players.isPending ? (
        <p
          role="status"
          aria-live="polite"
          className="text-sm text-muted-foreground"
        >
          <Trans>Loading media players…</Trans>
        </p>
      ) : players.isError ? (
        <p role="alert" className="text-sm text-destructive">
          <Trans>Could not load media players:</Trans> {players.error.message}
        </p>
      ) : data.length === 0 ? (
        <div className="rounded-md border border-dashed p-8 text-center">
          <p className="text-sm text-muted-foreground">
            <Trans>
              No media players are configured. Declare them under
              <code className="mx-1">media_players</code> in the config.
            </Trans>
          </p>
        </div>
      ) : (
        <div>
          <p className="mb-3 text-xs text-muted-foreground">
            <Trans>All configured media players.</Trans>
          </p>
          <div className="grid gap-4">
            {data.map((player) => renderPlayer(player))}
          </div>
        </div>
      )}

      <Dialog
        open={loadFor !== null}
        onOpenChange={(open: boolean): void => {
          if (!open) {
            setLoadFor(null);
          }
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>
              <Trans>Load asset</Trans>
            </DialogTitle>
            <DialogDescription>
              <Trans>
                Load a media-library asset into this player. It cues to the
                asset's in-point, ready to take.
              </Trans>
            </DialogDescription>
          </DialogHeader>
          {loadFor !== null ? (
            <div className="grid gap-4">
              <p className="text-sm">
                <Trans>Player</Trans>: <code>{loadFor.id}</code>
              </p>
              <div className="grid gap-1.5">
                <Label htmlFor="media-player-asset">
                  <Trans>Asset id</Trans>
                </Label>
                <Input
                  id="media-player-asset"
                  value={assetDraft}
                  placeholder={t`Media-library asset id`}
                  onChange={(e): void => {
                    setAssetDraft(e.target.value);
                    setAssetMissing(false);
                  }}
                />
              </div>
              {/* Honest capability label: the library backend workstream has
                  not shipped an asset inventory this page could browse, so
                  there is no picker — the asset id is typed exactly as the
                  media library defines it. No fake browse/save/unload
                  affordances exist here (save/unload land with ML4). */}
              <p className="text-xs text-muted-foreground">
                <Trans>
                  No asset browser is available here yet: this control plane
                  has no library listing to pick from, so the asset id must be
                  typed exactly as the media library defines it.
                </Trans>
              </p>
              {assetMissing ? (
                <p role="alert" className="text-sm text-destructive">
                  <Trans>An asset id is required</Trans>
                </p>
              ) : null}
            </div>
          ) : null}
          <DialogFooter>
            <TouchAction
              variant="outline"
              onPress={(): void => {
                setLoadFor(null);
              }}
            >
              <Trans>Cancel</Trans>
            </TouchAction>
            <TouchAction
              pending={dialogPending}
              disabled={dialogPending}
              onPress={(): void => {
                submitLoad();
              }}
            >
              <Trans>Load</Trans>
            </TouchAction>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}
