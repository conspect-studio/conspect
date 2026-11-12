// The Outputs page shortcut for output feed routing (ADR-0108 §8): a Video
// feed row (grouped select → Plan → cost line → Take, outcome via
// OperationFeedback) and a read-only Audio row, plus the tally ("PGM" follows
// the switcher vs "direct: <id>").
//
// Truth rules (broadcast/README.md): a 202 is *accepted*, never *applied*; the
// applied/refused verdict comes only from the `route.output` event correlated
// by operation id; no timer here infers a failure.
import { useEffect, useMemo, useState } from 'react';
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
  readonly group: 'buses' | 'players' | 'generators' | 'sources';
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
}

interface Outcome {
  readonly operationId: string;
  readonly outcome: string;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

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
}: OutputFeedRowProps): JSX.Element {
  const { t } = useLingui();
  const { locked } = useTakeLock();
  const current = route === undefined ? undefined : feedOf(route.video);
  const currentKey = current === undefined ? '' : feedKey(current);
  const [selected, setSelected] = useState<string>('');
  const [plan, setPlan] = useState<OutputPlan | undefined>();
  const [planError, setPlanError] = useState<string | undefined>();
  const [planning, setPlanning] = useState(false);
  const [taking, setTaking] = useState(false);
  const [confirm, setConfirm] = useState(false);
  const [operationId, setOperationId] = useState<string | undefined>();
  const [outcome, setOutcome] = useState<Outcome | undefined>();
  const [takeError, setTakeError] = useState<string | undefined>();

  useEffect(() => {
    if (operationId === undefined) return undefined;
    return subscribeRealtime('outputs', (envelope) => {
      if (envelope.t !== 'route.output') return;
      const data = envelope.data;
      if (!isRecord(data) || data.operation_id !== operationId) return;
      if (typeof data.outcome === 'string') {
        setOutcome({ operationId, outcome: data.outcome });
      }
    });
  }, [operationId]);

  const allChoices = useMemo<readonly FeedChoice[]>(() => {
    const buses: FeedChoice[] = [
      { key: 'bus:program', label: t`Program (follows the switcher)`, group: 'buses' },
      // Without an M/E the plan refuses `switcher-required`; its text becomes
      // the Take key's disabled reason.
      { key: 'bus:preview', label: t`Preview`, group: 'buses' },
    ];
    return [...buses, ...choices];
  }, [choices, t]);

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
          : selected === currentKey
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
      const accepted = await takeOutputFeed(output, selectedFeed);
      setOperationId(accepted.operation_id);
      if (typeof accepted.outcome === 'string') {
        setOutcome({ operationId: accepted.operation_id, outcome: accepted.outcome });
      }
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
      : cost.new_encoders === 0
        ? t`+0 encoders`
        : t`+1 encoder`;
  const sessions = cost?.encoder_sessions ?? undefined;
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
    if (current !== undefined && 'source' in current) {
      const id = current.source;
      return t`direct: ${id}`;
    }
    return t`Hidden`;
  })();
  const groups: readonly (readonly [FeedChoice['group'], string])[] = [
    ['buses', t`Buses`],
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
            <Trans>Plan: {costLine}</Trans>
          ) : (
            <Trans>Plan: {costLine} ({sessions} encoder sessions)</Trans>
          )}
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
      <div className="flex flex-wrap items-center gap-2">
        <span className="font-medium"><Trans>Audio</Trans></span>
        <span data-testid="output-audio">{audioText}</span>
      </div>
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
