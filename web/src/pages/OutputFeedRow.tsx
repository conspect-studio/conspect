// The Outputs page shortcut for output feed routing (ADR-0108 §8): a Video
// feed row (grouped select → Plan → cost line → Take, outcome via
// OperationFeedback) and an Audio row (the editable OutputAudioRow when the
// page passes one, else a read-only line), plus the tally ("PGM" follows
// the switcher vs "direct: <id>").
//
// Truth rules (broadcast/README.md): a 202 is *accepted*, never *applied*; the
// applied/refused verdict comes only from the `route.output` event correlated
// by operation id; no timer here infers a failure.
import { useEffect, useMemo, useRef, useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { OperationFeedback, TouchAction } from '../components/broadcast';
import type { OperationPhase } from '../components/broadcast';
import { useTakeLock } from '../components/broadcast/BroadcastShell';
import { Button } from '../components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '../components/ui/dialog';
import {
  feedFromKey,
  feedKey,
  feedOf,
  isRedacted,
  planOutputFeed,
  takeOutputFeed,
} from '../api/outputRouting';
import type { OutputPlan, OutputRouteState, OutputVideoFeed } from '../api/outputRouting';
import { OperationApiError } from '../api/operations';
import { subscribeRealtime } from '../realtime/shared';

/** A feed the select offers, with an optional reason it cannot be taken. */
export interface FeedChoice {
  readonly key: string;
  readonly label: string;
  readonly group: 'buses' | 'layouts' | 'players' | 'generators' | 'sources';
  readonly disabledReason?: string | undefined;
}

/** Props for {@link OutputFeedRow}. */
export interface OutputFeedRowProps {
  /** The output API id. */
  readonly output: string;
  /** Display name for labels. */
  readonly outputName: string;
  /** This output's routing row, if readable (undefined while loading). */
  readonly route: OutputRouteState | undefined;
  /** The feeds to offer, grouped. */
  readonly choices: readonly FeedChoice[];
  /**
   * The editable Audio row (OutputAudioRow); replaces the read-only audio
   * line when given.
   */
  readonly audioRow?: JSX.Element | undefined;
}

interface Outcome {
  readonly operationId: string;
  readonly outcome: string;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

// `encoder_sessions` is "<in use> of <max> in use" (whole-system principals
// only); the numbers are re-worded through Lingui, anything else shown as is.
const SESSIONS = /^(\d+) of (\d+) in use$/;

/** Outcomes remembered for operation ids not yet known to the row. */
const RECENT_OUTCOMES = 16;

function phaseOf(outcome: string | undefined): OperationPhase {
  if (outcome === undefined) return 'pending';
  if (outcome === 'applied') return 'applied';
  if (outcome === 'superseded') return 'cancelled';
  return 'failed';
}

export function OutputFeedRow({
  output,
  outputName,
  route,
  choices,
  audioRow,
}: OutputFeedRowProps): JSX.Element {
  const { t } = useLingui();
  const { locked } = useTakeLock();
  // `video` is what is ON AIR (the engine's live bus); `configured_video` the
  // persisted feed. They differ after a refused start-up take or a fallback.
  const current = route === undefined ? undefined : feedOf(route.video);
  const configured = route === undefined ? undefined : feedOf(route.configured_video);
  const configuredNote =
    current !== undefined && configured !== undefined && feedKey(configured) !== feedKey(current)
      ? configured
      : undefined;
  const currentKey = current === undefined ? '' : feedKey(current);
  // The feed on air: the readback's, or the one an applied take just put there
  // (before the readback catches up). The select starts on it and returns to
  // it whenever it changes, so an on-air output never reads "Choose a feed…".
  const [onAirKey, setOnAirKey] = useState<string>(currentKey);
  const [selected, setSelected] = useState<string>(currentKey);
  const [plan, setPlan] = useState<OutputPlan | undefined>();
  // A new readback feed resets the select (adjusting state while rendering,
  // React's pattern for state derived from a changed prop).
  const [seenKey, setSeenKey] = useState<string>(currentKey);
  if (seenKey !== currentKey) {
    setSeenKey(currentKey);
    setOnAirKey(currentKey);
    setSelected(currentKey);
    setPlan(undefined);
  }
  const [planError, setPlanError] = useState<string | undefined>();
  const [planning, setPlanning] = useState(false);
  const [taking, setTaking] = useState(false);
  const [confirm, setConfirm] = useState(false);
  const [operationId, setOperationIdState] = useState<string | undefined>();
  const [outcome, setOutcome] = useState<Outcome | undefined>();
  const [takeError, setTakeError] = useState<string | undefined>();

  // Subscribed for the row's lifetime, not from the 202 on: an outcome can
  // arrive before the take's response (or before an effect keyed on the new
  // operation id runs). Recent outcomes for this output are kept by id
  // (bounded) and matched once the operation id is known.
  const recent = useRef(new Map<string, string>());
  const operationRef = useRef<string | undefined>(undefined);
  // The feed key of the take in flight (what goes on air when it applies).
  const takenRef = useRef<string | undefined>(undefined);
  const setOperationId = (id: string | undefined): void => {
    operationRef.current = id;
    setOperationIdState(id);
  };
  // Record an outcome for the current operation; `applied` puts the taken
  // feed on air (the select shows it, the done change's plan is cleared).
  const settle = (id: string, result: string): void => {
    setOutcome({ operationId: id, outcome: result });
    const taken = takenRef.current;
    if (result === 'applied' && taken !== undefined) {
      setOnAirKey(taken);
      setSelected(taken);
      setPlan(undefined);
    }
  };
  const settleRef = useRef(settle);
  useEffect(() => {
    settleRef.current = settle;
  });
  useEffect(
    () =>
      subscribeRealtime('outputs', (envelope) => {
        if (envelope.t !== 'route.output') return;
        const data = envelope.data;
        if (!isRecord(data) || data.output !== output) return;
        if (typeof data.operation_id !== 'string' || typeof data.outcome !== 'string') return;
        recent.current.set(data.operation_id, data.outcome);
        while (recent.current.size > RECENT_OUTCOMES) {
          const oldest = recent.current.keys().next().value;
          if (oldest === undefined) break;
          recent.current.delete(oldest);
        }
        if (data.operation_id === operationRef.current) {
          settleRef.current(data.operation_id, data.outcome);
        }
      }),
    [output],
  );

  const allChoices = useMemo<readonly FeedChoice[]>(() => {
    const buses: FeedChoice[] = [
      { key: 'bus:program', label: t`Program (follows the switcher)`, group: 'buses' },
      // Without an M/E the plan refuses `switcher-required` for Preview and
      // Clean; its text becomes the Take key's disabled reason.
      { key: 'bus:preview', label: t`Preview`, group: 'buses' },
      { key: 'bus:clean', label: t`Clean (Program without labels and tally)`, group: 'buses' },
    ];
    const all = [...buses, ...choices];
    // The on-air feed may be one the page does not list (e.g. a removed
    // source): still show it rather than a blank select.
    const onAir = feedFromKey(onAirKey);
    if (onAir !== undefined && 'source' in onAir && !all.some((c) => c.key === onAirKey)) {
      all.push({ key: onAirKey, label: onAir.source, group: 'sources' });
    }
    if (onAir !== undefined && 'layout' in onAir && !all.some((c) => c.key === onAirKey)) {
      all.push({ key: onAirKey, label: onAir.layout, group: 'layouts' });
    }
    return all;
  }, [choices, onAirKey, t]);

  const selectedFeed: OutputVideoFeed | undefined = feedFromKey(selected);
  const redacted = route !== undefined && isRedacted(route.video);
  const refusal = plan?.refusals[0];
  const takeDisabledReason =
    selectedFeed === undefined
      ? t`Choose a feed and plan it first`
      : plan === undefined
        ? t`Plan this feed first`
        : refusal !== undefined
          ? refusal.detail
          : selected === onAirKey
            ? t`This output already carries this feed`
            : undefined;

  const runPlan = async (key: string): Promise<void> => {
    const feed = feedFromKey(key);
    setPlan(undefined);
    setPlanError(undefined);
    if (feed === undefined) return;
    setPlanning(true);
    try {
      setPlan(await planOutputFeed(output, feed));
    } catch (error) {
      setPlanError(error instanceof OperationApiError ? (error.detail ?? error.message) : t`Plan failed`);
    } finally {
      setPlanning(false);
    }
  };

  const runTake = async (): Promise<void> => {
    if (selectedFeed === undefined) return;
    setConfirm(false);
    setTaking(true);
    setTakeError(undefined);
    setOutcome(undefined);
    try {
      takenRef.current = selected;
      const accepted = await takeOutputFeed(output, selectedFeed);
      setOperationId(accepted.operation_id);
      // A replay carries its recorded outcome; an event may also have beaten
      // the 202.
      const known = accepted.outcome ?? recent.current.get(accepted.operation_id);
      if (typeof known === 'string') settle(accepted.operation_id, known);
    } catch (error) {
      setOperationId(undefined);
      setTakeError(error instanceof OperationApiError ? (error.detail ?? error.message) : t`Take failed`);
    } finally {
      setTaking(false);
    }
  };

  const onTake = (): void => {
    // The take lock guards a change to an output that currently carries Program.
    if (locked && route?.tally === 'program') {
      setConfirm(true);
      return;
    }
    void runTake();
  };

  const cost = plan?.cost;
  const costLine =
    cost === undefined
      ? undefined
      : cost.new_aux_canvases === 1
        ? cost.new_encoders === 0
          ? t`+0 encoders, +1 layout canvas`
          : t`+1 encoder, +1 layout canvas`
        : cost.new_encoders === 0
          ? t`+0 encoders`
          : t`+1 encoder`;
  // Which audio the output carries on the planned feed when audio follows.
  const plannedAudio =
    cost?.follow_audio === 'program'
      ? t`Audio follows: program mix`
      : cost?.follow_audio === 'source'
        ? selected.startsWith('aux:')
          ? t`Audio follows: one cell's source audio (set by the layout)`
          : t`Audio follows: this source's own audio`
        : cost?.follow_audio === 'preview'
          ? t`Audio follows: the preview source's audio`
          : cost?.follow_audio === 'none'
            ? t`Audio follows: silence (this layout carries no audio)`
            : undefined;
  const feedLabel = allChoices.find((c) => c.key === selected)?.label ?? selected;
  const sessionsRaw = cost?.encoder_sessions ?? undefined;
  const sessions = (() => {
    if (sessionsRaw === undefined) return undefined;
    const match = SESSIONS.exec(sessionsRaw);
    if (match === null) return sessionsRaw;
    const inUse = match[1] ?? '';
    const max = match[2] ?? '';
    return t`${inUse} of ${max} encoder sessions in use`;
  })();
  const effectiveAudio: unknown = route?.effective_audio;
  const audioText = (() => {
    if (route === undefined || isRedacted(effectiveAudio)) return t`Follow`;
    if (effectiveAudio === 'program') return t`Follow (program mix)`;
    if (effectiveAudio === 'preview_source') return t`Follow (preview source)`;
    if (isRecord(effectiveAudio) && typeof effectiveAudio.source === 'string') {
      const id = effectiveAudio.source;
      return t`Follow (${id})`;
    }
    return t`Follow`;
  })();
  const phase: OperationPhase = takeError !== undefined
    ? 'failed'
    : operationId === undefined
      ? 'idle'
      : phaseOf(outcome?.operationId === operationId ? outcome.outcome : undefined);
  const selectId = `feed-${output}`;
  // Tally: "PGM" follows the switcher; "direct: <id>" ignores it (§8).
  const tally = (() => {
    if (route === undefined) return t`Unknown`;
    if (route.tally === 'program') return t`PGM`;
    if (route.tally === 'preview') return t`PVW`;
    if (route.tally === 'clean') return t`CLEAN`;
    if (current !== undefined && 'source' in current) {
      const id = current.source;
      return t`direct: ${id}`;
    }
    if (current !== undefined && 'layout' in current) {
      const id = current.layout;
      return t`layout: ${id}`;
    }
    return t`Hidden`;
  })();
  const groups: readonly (readonly [FeedChoice['group'], string])[] = [
    ['buses', t`Buses`],
    ['layouts', t`Layouts`],
    ['players', t`Players`],
    ['generators', t`Generators`],
    ['sources', t`Sources`],
  ];

  return (
    <div className="grid gap-2 text-sm" data-testid={`output-feed-${output}`}>
      <div className="flex flex-wrap items-center gap-2">
        <label htmlFor={selectId} className="font-medium">
          <Trans>Video</Trans>
        </label>
        <select
          id={selectId}
          className="min-h-12 min-w-0 rounded border bg-background px-2"
          value={selected}
          disabled={redacted}
          aria-label={t`Video feed for ${outputName}`}
          onChange={(event): void => {
            setSelected(event.target.value);
            setOperationId(undefined);
            setOutcome(undefined);
            setTakeError(undefined);
            void runPlan(event.target.value);
          }}
        >
          <option value="">{t`Choose a feed…`}</option>
          {groups.map(([group, label]) => {
            const items = allChoices.filter((c) => c.group === group);
            if (items.length === 0) return null;
            return (
              <optgroup key={group} label={label}>
                {items.map((c) => (
                  <option key={c.key} value={c.key} disabled={c.disabledReason !== undefined}>
                    {c.disabledReason === undefined ? c.label : `${c.label} — ${c.disabledReason}`}
                  </option>
                ))}
              </optgroup>
            );
          })}
        </select>
        <span className="pill" data-testid="output-tally">
          {tally}
        </span>
        {configuredNote !== undefined ? (
          <span className="text-xs text-muted-foreground" data-testid="output-configured-feed">
            {(() => {
              const label =
                'source' in configuredNote
                  ? configuredNote.source
                  : 'layout' in configuredNote
                    ? (allChoices.find((c) => c.key === feedKey(configuredNote))?.label ?? configuredNote.layout)
                    : (allChoices.find((c) => c.key === feedKey(configuredNote))?.label ?? configuredNote.bus);
              return t`configured: ${label} (not on air)`;
            })()}
          </span>
        ) : null}
        {route?.state !== undefined && route.state !== 'configured' ? (
          <span className="text-xs text-muted-foreground" data-testid="output-route-state">
            {route.state === 'on_air' ? t`on air` : route.state === 'pending' ? t`take pending` : route.state === 'degraded' ? t`degraded (holding last picture)` : route.state}
          </span>
        ) : null}
      </div>
      {planning ? (
        <p className="text-xs text-muted-foreground"><Trans>Planning…</Trans></p>
      ) : null}
      {planError !== undefined ? (
        <p className="break-words text-xs text-destructive" role="alert">{planError}</p>
      ) : null}
      {costLine !== undefined ? (
        <p className="break-words text-xs" data-testid="output-plan-cost">
          {sessions === undefined ? (
            <Trans>{feedLabel} → {costLine}</Trans>
          ) : (
            <Trans>{feedLabel} → {costLine} ({sessions})</Trans>
          )}
        </p>
      ) : null}
      {plannedAudio !== undefined ? (
        <p className="break-words text-xs text-muted-foreground" data-testid="output-plan-audio">
          {plannedAudio}
        </p>
      ) : null}
      <div className="flex flex-wrap items-start gap-2">
        <TouchAction
          onPress={onTake}
          pending={taking}
          disabled={takeDisabledReason !== undefined}
          disabledReason={takeDisabledReason}
          aria-label={t`Take feed to ${outputName}`}
        >
          <Trans>Take</Trans>
        </TouchAction>
      </div>
      <OperationFeedback
        phase={phase}
        operationLabel={t`Feed change on ${outputName}`}
        correlationId={operationId}
        failureReason={takeError ?? (outcome !== undefined && outcome.outcome !== 'applied' ? outcome.outcome : undefined)}
        {...(phase === 'failed' || phase === 'cancelled'
          ? {
              onDismiss: (): void => {
                setOperationId(undefined);
                setOutcome(undefined);
                setTakeError(undefined);
              },
            }
          : {})}
      />
      {audioRow ?? (
        <div className="flex flex-wrap items-center gap-2">
          <span className="font-medium"><Trans>Audio</Trans></span>
          <span data-testid="output-audio">{audioText}</span>
        </div>
      )}
      <Dialog open={confirm} onOpenChange={setConfirm}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>
              <Trans>Confirm a take on a Program output?</Trans>
            </DialogTitle>
            <DialogDescription>
              <Trans>
                Takes are locked and {outputName} currently carries Program. Confirm to change its feed.
              </Trans>
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button
              variant="outline"
              className="min-h-12"
              onClick={(): void => {
                setConfirm(false);
              }}
            >
              <Trans>Cancel</Trans>
            </Button>
            <Button
              className="min-h-12"
              onClick={(): void => {
                void runTake();
              }}
            >
              <Trans>Confirm take</Trans>
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
