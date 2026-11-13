// The Outputs page "Audio" row (ADR-0108 §8: `Audio [ Follow (program mix) ▾ ]`):
// a grouped select of the output's audio feed → Plan (`stream: audio`) → cost
// line → Take, the outcome via OperationFeedback — the same flow as the video
// row (OutputFeedRow), for an audio-only take that leaves the video untouched.
//
// Truth rules (broadcast/README.md): a 202 is *accepted*, never *applied*; the
// verdict comes only from the `route.output` event correlated by operation id.
import { useEffect, useMemo, useRef, useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { OperationFeedback, TouchAction } from '../components/broadcast';
import type { OperationPhase } from '../components/broadcast';
import {
  audioFromKey,
  audioKey,
  audioOf,
  isRedacted,
  planOutputAudio,
  takeOutputAudio,
} from '../api/outputRouting';
import type { OutputAudioFeed, OutputPlan, OutputRouteState } from '../api/outputRouting';
import { OperationApiError } from '../api/operations';
import { subscribeRealtime } from '../realtime/shared';

/** A source the Audio select offers. */
export interface AudioSourceChoice {
  readonly id: string;
  readonly label: string;
}

/** Props for {@link OutputAudioRow}. */
export interface OutputAudioRowProps {
  /** The output API id. */
  readonly output: string;
  /** Display name for labels. */
  readonly outputName: string;
  /** This output's routing row, if readable (undefined while loading). */
  readonly route: OutputRouteState | undefined;
  /** Sources whose own audio may be routed. */
  readonly sources: readonly AudioSourceChoice[];
  /**
   * Declared discrete tracks (`audio-routing` `selectable_tracks` without the
   * program bus); `undefined` while unknown, which omits the Tracks group.
   */
  readonly tracks: readonly string[] | undefined;
}

const RECENT_OUTCOMES = 16;

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

function phaseOf(outcome: string | undefined): OperationPhase {
  if (outcome === undefined) return 'pending';
  if (outcome === 'applied') return 'applied';
  if (outcome === 'superseded') return 'cancelled';
  return 'failed';
}

interface Choice {
  readonly key: string;
  readonly label: string;
  readonly group: 'follow' | 'program' | 'silence' | 'sources' | 'tracks';
}

export function OutputAudioRow({
  output,
  outputName,
  route,
  sources,
  tracks,
}: OutputAudioRowProps): JSX.Element {
  const { t } = useLingui();
  const current: OutputAudioFeed | undefined = route === undefined ? undefined : audioOf(route.audio);
  const currentKey = current === undefined ? '' : audioKey(current);
  const [onAirKey, setOnAirKey] = useState<string>(currentKey);
  const [selected, setSelected] = useState<string>(currentKey);
  const [plan, setPlan] = useState<OutputPlan | undefined>();
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
  const [operationId, setOperationIdState] = useState<string | undefined>();
  const [outcome, setOutcome] = useState<{ operationId: string; outcome: string } | undefined>();
  const [takeError, setTakeError] = useState<string | undefined>();

  const recent = useRef(new Map<string, string>());
  const operationRef = useRef<string | undefined>(undefined);
  const takenRef = useRef<string | undefined>(undefined);
  const setOperationId = (id: string | undefined): void => {
    operationRef.current = id;
    setOperationIdState(id);
  };
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

  // "Follow (<what it carries now>)".
  const effective: unknown = route?.effective_audio;
  const followLabel = (() => {
    if (route === undefined || isRedacted(effective)) return t`Follow`;
    if (effective === 'program') return t`Follow (program mix)`;
    if (effective === 'preview_source') return t`Follow (preview source)`;
    if (effective === 'none') return t`Follow (silence)`;
    if (isRecord(effective) && typeof effective.source === 'string') {
      const id = effective.source;
      return t`Follow (${id})`;
    }
    return t`Follow`;
  })();

  const choices = useMemo<readonly Choice[]>(() => {
    const all: Choice[] = [
      { key: 'follow', label: followLabel, group: 'follow' },
      { key: 'program', label: t`Program mix`, group: 'program' },
      { key: 'none', label: t`Silence`, group: 'silence' },
      ...sources.map((s): Choice => ({ key: `src:${s.id}`, label: s.label, group: 'sources' })),
      ...(tracks ?? []).map((name): Choice => ({ key: `track:${name}`, label: name, group: 'tracks' })),
    ];
    // The on-air value may be one the page does not list: still show it.
    const onAir = audioFromKey(onAirKey);
    if (onAir !== undefined && typeof onAir !== 'string' && !all.some((c) => c.key === onAirKey)) {
      all.push(
        'source' in onAir
          ? { key: onAirKey, label: onAir.source, group: 'sources' }
          : { key: onAirKey, label: onAir.tracks.join(', '), group: 'tracks' },
      );
    }
    return all;
  }, [followLabel, sources, tracks, onAirKey, t]);

  const selectedAudio = audioFromKey(selected);
  const redacted = route !== undefined && isRedacted(route.audio);
  const refusal = plan?.refusals[0];
  const takeDisabledReason =
    selectedAudio === undefined
      ? t`Choose an audio feed and plan it first`
      : plan === undefined
        ? t`Plan this audio feed first`
        : refusal !== undefined
          ? refusal.detail
          : selected === onAirKey
            ? t`This output already carries this audio`
            : undefined;

  const runPlan = async (key: string): Promise<void> => {
    const audio = audioFromKey(key);
    setPlan(undefined);
    setPlanError(undefined);
    if (audio === undefined) return;
    setPlanning(true);
    try {
      setPlan(await planOutputAudio(output, audio));
    } catch (error) {
      setPlanError(error instanceof OperationApiError ? (error.detail ?? error.message) : t`Plan failed`);
    } finally {
      setPlanning(false);
    }
  };

  const runTake = async (): Promise<void> => {
    if (selectedAudio === undefined) return;
    setTaking(true);
    setTakeError(undefined);
    setOutcome(undefined);
    try {
      takenRef.current = selected;
      const accepted = await takeOutputAudio(output, selectedAudio);
      setOperationId(accepted.operation_id);
      const known = accepted.outcome ?? recent.current.get(accepted.operation_id);
      if (typeof known === 'string') settle(accepted.operation_id, known);
    } catch (error) {
      setOperationId(undefined);
      setTakeError(error instanceof OperationApiError ? (error.detail ?? error.message) : t`Take failed`);
    } finally {
      setTaking(false);
    }
  };

  const audioCost = plan?.cost.audio_cost ?? undefined;
  const costLine =
    plan === undefined ? undefined : audioCost !== undefined && audioCost !== '' ? audioCost : t`No extra encoder`;
  const choiceLabel = choices.find((c) => c.key === selected)?.label ?? selected;
  const phase: OperationPhase = takeError !== undefined
    ? 'failed'
    : operationId === undefined
      ? 'idle'
      : phaseOf(outcome?.operationId === operationId ? outcome.outcome : undefined);
  const selectId = `audio-${output}`;
  const groups: readonly (readonly [Choice['group'], string])[] = [
    ['follow', t`Follow`],
    ['program', t`Program mix`],
    ['silence', t`Silence`],
    ['sources', t`Sources`],
    ['tracks', t`Tracks`],
  ];

  return (
    <div className="grid gap-2 text-sm" data-testid={`output-audio-row-${output}`}>
      <div className="flex flex-wrap items-center gap-2">
        <label htmlFor={selectId} className="font-medium">
          <Trans>Audio</Trans>
        </label>
        <select
          id={selectId}
          className="min-h-12 min-w-0 rounded border bg-background px-2"
          value={selected}
          disabled={redacted}
          aria-label={t`Audio feed for ${outputName}`}
          onChange={(event): void => {
            setSelected(event.target.value);
            setOperationId(undefined);
            setOutcome(undefined);
            setTakeError(undefined);
            void runPlan(event.target.value);
          }}
        >
          {current === undefined ? <option value="">{t`Choose an audio feed…`}</option> : null}
          {groups.map(([group, label]) => {
            const items = choices.filter((c) => c.group === group);
            if (items.length === 0) return null;
            return (
              <optgroup key={group} label={label}>
                {items.map((c) => (
                  <option key={c.key} value={c.key}>
                    {c.label}
                  </option>
                ))}
              </optgroup>
            );
          })}
        </select>
        <span className="text-xs text-muted-foreground" data-testid="output-audio">
          {redacted ? t`Hidden` : (choices.find((c) => c.key === onAirKey)?.label ?? followLabel)}
        </span>
        {tracks === undefined ? (
          <span className="text-xs text-muted-foreground" data-testid="output-audio-no-tracks">
            <Trans>Tracks unavailable (audio routing not loaded)</Trans>
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
        <p className="break-words text-xs" data-testid="output-audio-plan-cost">
          <Trans>{choiceLabel} → {costLine}</Trans>
        </p>
      ) : null}
      <div className="flex flex-wrap items-start gap-2">
        <TouchAction
          onPress={(): void => {
            void runTake();
          }}
          pending={taking}
          disabled={takeDisabledReason !== undefined}
          disabledReason={takeDisabledReason}
          aria-label={t`Take audio to ${outputName}`}
        >
          <Trans>Take</Trans>
        </TouchAction>
      </div>
      <OperationFeedback
        phase={phase}
        operationLabel={t`Audio change on ${outputName}`}
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
    </div>
  );
}
