// The Routing page as the patch matrix (ADR-0110 §8, §12.9, #308).
//
// Video | Audio | Subtitle tabs; rows are sources grouped Buses / Layouts /
// Players / Generators / Sources; columns are mixer inputs then output slots.
// A cell shows the §8 glyph + path; selecting cells across 2+ destinations
// arms a salvo ("Take all"). The take lock and the on-air confirm guard any
// crosspoint that would change Program. Page states: loading / live / stale
// (>2s) / disconnected / read-only / salvo-arming (§8).
import { Fragment, useEffect, useMemo, useRef, useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { useQuery, useQueryClient } from '@tanstack/react-query';

import { OperationFeedback, TouchAction } from '../../components/broadcast';
import type { OperationPhase } from '../../components/broadcast';
import { useTakeLock } from '../../components/broadcast/BroadcastShell';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '../../components/ui/dialog';
import { Button } from '../../components/ui/button';
import { Input } from '../../components/ui/input';
import { Label } from '../../components/ui/label';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '../../components/ui/select';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '../../components/ui/tabs';
import { subscribeRealtime } from '../../realtime/shared';
import type { RealtimeStatus } from '../../realtime/connection';
import { OperationApiError, newIdempotencyKey } from '../../api/operations';
import { getPatch, parseCrosspoints, planPatch, probePatchWrite, takePatch } from '../../api/patch';
import type { OnAir, PatchCrosspoint, PatchPlanResponse, PatchRequest, PatchView } from '../../api/patch';
import { planSalvo, takePatchSalvo } from '../../api/salvos';
import type { Salvo } from '../../api/salvos';
import { useSalvos, useSaveSalvo } from '../../api/salvosQueries';
import { useSources } from '../../resources/queries';
import { useLayouts } from '../../api/queries';
import { useMediaPlayers } from '../../api/media-playersQueries';
import { createApiClient } from '../../api/client';
import { useSubtitleSources } from './useSubtitleSources';
import {
  ON_AIR_REFUSAL,
  affectsProgram,
  audioCell,
  audioRows,
  buildRows,
  crosspointsAffectProgram,
  displayCell,
  hasSubtitleData,
  isSetSelection,
  membersOf,
  mixerCell,
  mixerColumns,
  outputAudioColumns,
  outputSlotCell,
  outputSubtitleColumns,
  outputVideoColumns,
  selectionKey,
  selectionProblem,
  subtitleRows,
  takePathOf,
} from './patchMatrixModel';
import type { ArmedSelection, DisabledReason, MatrixCell, MatrixColumn, MatrixRow, PlannedMember, TakePath } from './patchMatrixModel';
import { EarlyEvidence, mergeEvidence, takeEvidenceOf } from './patchTakeEvidence';
import type { TakeEvidence } from './patchTakeEvidence';

const PATCH_KEY = ['patch', 'matrix'] as const;
const WRITE_KEY = ['patch', 'write-capability'] as const;
/** §8: a frozen engine tick for more than this many ms is `stale`, not `live`. */
const STALE_MS = 2000;
/** Readback cadence: well under {@link STALE_MS}, so a healthy engine is seen advancing. */
const READBACK_MS = 1000;

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

/** Coarse page state (§8); `salvo-arming` is layered on top once a selection exists. */
export type MatrixPageState = 'loading' | 'live' | 'stale' | 'disconnected' | 'read-only' | 'error';

type Plane = 'video' | 'audio' | 'subtitle';

/** §8's glyph per cell state; the accessible name carries the state in words too. */
function glyphOf(cell: MatrixCell): string {
  switch (cell.state) {
    case 'on-air': return '●';
    case 'planned': return '◐';
    case 'pending': return '◌';
    case 'available': return '○';
    case 'disabled': return '⊘';
    case 'na': return '—';
    case 'unknown': return '?';
  }
}

function pathGlyph(cell: MatrixCell): string {
  switch (cell.path) {
    case 'passthrough': return '⇢';
    case 'shared': return '=';
    case 'converted': return '◇';
    default: return '';
  }
}

function columnKey(column: MatrixColumn): string {
  return column.kind === 'mixer' ? `mixer:${column.me}:${String(column.input)}` : `output:${column.output}:${String(column.slot)}`;
}

/** A plan bound to the exact request (and lock state) it answered (E3-08). */
interface BoundPlan {
  readonly key: string;
  readonly plan: PatchPlanResponse;
}

/** One accepted take the page is waiting on: its id and the event family it settles on (E3-04). */
interface PendingTake {
  readonly operationId: string;
  readonly path: TakePath;
  readonly label: 'patch' | 'salvo';
}

/** Whether a plan refuses only on the on-air guard (a confirmed `allow` take may still apply it). */
function planNeedsConfirm(plan: PatchPlanResponse): boolean {
  return plan.crosspoints.some((xp) => xp.refusal?.type === ON_AIR_REFUSAL || xp.on_air);
}
/** The first refusal other than the on-air guard, if any. */
function hardRefusal(plan: PatchPlanResponse): { readonly type: string; readonly detail: string } | undefined {
  const refusal = plan.crosspoints.find((xp) => xp.refusal !== null && xp.refusal !== undefined && xp.refusal.type !== ON_AIR_REFUSAL)?.refusal;
  return refusal ?? undefined;
}
/** Whether a take of this plan can proceed (possibly after an on-air confirm). */
function planTakeable(plan: PatchPlanResponse): boolean {
  if (hardRefusal(plan) !== undefined) return false;
  if (plan.set !== undefined && !plan.set.fits) return false;
  return plan.can_take || plan.crosspoints.some((xp) => xp.refusal?.type === ON_AIR_REFUSAL);
}

/** One matrix tab's grid: rows × columns, with click-to-arm cells. */
function MatrixGrid({
  plane,
  rows,
  columns,
  cellOf,
  isArmed,
  onToggle,
  interactive,
  rowLabel,
}: {
  readonly plane: Plane;
  readonly rows: readonly MatrixRow[];
  readonly columns: readonly MatrixColumn[];
  readonly cellOf: (row: MatrixRow, column: MatrixColumn) => MatrixCell;
  readonly isArmed: (row: MatrixRow, column: MatrixColumn) => boolean;
  readonly onToggle: (row: MatrixRow, column: MatrixColumn) => void;
  readonly interactive: boolean;
  readonly rowLabel: (row: MatrixRow) => string;
}): JSX.Element {
  const { t } = useLingui();
  const groupLabel = (group: MatrixRow['group']): string => {
    switch (group) {
      case 'feeds': return t`Audio feeds`;
      case 'buses': return t`Buses`;
      case 'layouts': return t`Layouts`;
      case 'players': return t`Players`;
      case 'cg': return t`CG`;
      case 'generators': return t`Generators`;
      case 'sources': return t`Sources`;
    }
  };
  const reasonLabel = (cell: MatrixCell): string | undefined => {
    const reason: DisabledReason | undefined = cell.disabledReason;
    switch (reason) {
      case undefined: return undefined;
      case 'not-fresh': return t`not fresh`;
      case 'would-cycle': return t`would cycle`;
      case 'capacity': return t`capacity`;
      case 'not-authorized': return t`not authorized`;
      case 'switcher-required': return t`switcher required`;
      case 'unroutable-output': return t`unroutable output`;
      case 'audio-fixed': return t`fixed by the output's configuration; only audio slot 0 Follow is patchable in this release`;
      case 'subtitle-unsupported': return t`subtitle patching is not available in this release (#345)`;
      case 'refused': return cell.refusal?.detail ?? t`refused by the plan`;
    }
  };
  /** The accessible-name state fragment (§8); needs `t` in this component's scope. */
  const stateLabel = (cell: MatrixCell): string => {
    switch (cell.state) {
      case 'on-air': return t`on air`;
      case 'planned': return cell.needsConfirm === true ? t`planned, needs on-air confirmation` : t`planned`;
      case 'pending': return cell.heldTicks !== undefined
        ? t`pending (${cell.heldTicks} of 50 ticks)`
        : t`pending`;
      case 'available': return t`available`;
      case 'disabled': {
        const reason = reasonLabel(cell);
        return reason !== undefined ? t`disabled: ${reason}` : t`disabled`;
      }
      case 'na': return t`not applicable`;
      case 'unknown': return t`unknown`;
    }
  };
  if (rows.length === 0 || columns.length === 0) {
    return (
      <p role="status" data-testid={`matrix-empty-${plane}`} className="rp-note">
        {plane === 'subtitle' ? (
          <Trans>
            No subtitle columns or caption sources yet: the Subtitle tab fills
            in once outputs declare subtitle slots.
          </Trans>
        ) : (
          <Trans>No rows or columns to show yet.</Trans>
        )}
      </p>
    );
  }
  const groupHeaders = rows.map((row, index) => index === 0 || row.group !== rows[index - 1]?.group);
  return (
    <div className="patch-wrap" data-testid={`matrix-grid-${plane}`}>
      <table className="patch-table">
        <thead>
          <tr>
            <th className="src-h" scope="col">
              <Trans>Source ↓ / Dest →</Trans>
            </th>
            {columns.map((col) => (
              <th key={columnKey(col)} scope="col" className="dest-h" title={col.label}>
                {col.label}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((row, index) => {
            const groupHeader = groupHeaders[index] ?? false;
            const label = rowLabel(row);
            return (
              <Fragment key={row.id}>
                {groupHeader ? (
                  <tr key={`group-${row.group}`}>
                    <th className="grp-h" colSpan={columns.length + 1} scope="colgroup">
                      {groupLabel(row.group)}
                    </th>
                  </tr>
                ) : null}
                <tr key={row.id}>
                  <th className="patch-src" scope="row" title={label}>
                    {label}
                  </th>
                  {columns.map((col) => {
                    const cell = cellOf(row, col);
                    const armed = isArmed(row, col);
                    // An armed cell stays pressable so it can be disarmed.
                    const disabled = !interactive || cell.state === 'na' || (cell.state === 'disabled' && !armed);
                    return (
                      <td key={columnKey(col)}>
                        <button
                          type="button"
                          className={armed ? 'xpt plan' : 'xpt'}
                          aria-pressed={armed}
                          disabled={disabled}
                          aria-label={t`${label} to ${col.label}: ${stateLabel(cell)}`}
                          title={reasonLabel(cell) ?? stateLabel(cell)}
                          data-testid={`cell-${plane}-${row.id}-${columnKey(col)}`}
                          data-state={cell.state}
                          onClick={(): void => {
                            onToggle(row, col);
                          }}
                        >
                          {glyphOf(cell)}{pathGlyph(cell)}
                        </button>
                      </td>
                    );
                  })}
                </tr>
              </Fragment>
            );
          })}
        </tbody>
      </table>
      <p className="patch-legend">
        <Trans>
          ● on air · ◐ planned · ◌ pending · ○ available · ⊘ disabled · — n/a ·
          ? unknown. Path: ⇢ passthrough · = shared · ◇ converted.
        </Trans>
      </p>
    </div>
  );
}

/** The engine tick the readback carries (the mixer's last composed tick), if any. */
function readbackTick(view: PatchView | undefined): number | undefined {
  const ticks = (view?.mixer ?? []).map((m) => m.tick);
  return ticks.length === 0 ? undefined : Math.max(...ticks);
}

/** The Routing page: Video | Audio | Subtitle matrix tabs + the arming panel. */
export function PatchMatrix(): JSX.Element {
  const { t } = useLingui();
  const queryClient = useQueryClient();
  const { locked } = useTakeLock();
  const [status, setStatus] = useState<RealtimeStatus>('connecting');
  const [plane, setPlane] = useState<Plane>('video');
  const [selections, setSelections] = useState<readonly ArmedSelection[]>([]);
  const [plan, setPlan] = useState<BoundPlan | undefined>();
  const [planError, setPlanError] = useState<{ readonly key: string; readonly message: string } | undefined>();
  const [planning, setPlanning] = useState(false);
  const [taking, setTaking] = useState(false);
  const [takeError, setTakeError] = useState<string | undefined>();
  /** The open Program confirm: the selection take, or a recalled salvo's take. */
  const [confirm, setConfirm] = useState<'selection' | 'salvo' | undefined>();
  const [pending, setPending] = useState<PendingTake | undefined>();
  const [outcome, setOutcome] = useState<TakeEvidence | undefined>();
  const [storeOpen, setStoreOpen] = useState(false);
  const [storeId, setStoreId] = useState('');
  const [recallId, setRecallId] = useState('');
  const [recallPlan, setRecallPlan] = useState<{ readonly id: string; readonly plan: PatchPlanResponse } | undefined>();
  const [recallError, setRecallError] = useState<string | undefined>();
  const [recalling, setRecalling] = useState(false);

  const patchQuery = useQuery<PatchView, OperationApiError>({
    queryKey: PATCH_KEY,
    queryFn: () => getPatch(),
    retry: false,
    refetchInterval: READBACK_MS,
  });
  // §8 `read-only`: the principal lacks Write (E3-03). No role read exists in
  // the API; `probePatchWrite` asks the plan route's first (role) check.
  const writeQuery = useQuery({
    queryKey: WRITE_KEY,
    queryFn: () => probePatchWrite(),
    retry: false,
    staleTime: 60_000,
  });
  const sources = useSources();
  const apiClient = useMemo(() => createApiClient(), []);
  const layouts = useLayouts(apiClient);
  const players = useMediaPlayers();
  const subtitleSources = useSubtitleSources();
  const salvos = useSalvos();
  const saveSalvo = useSaveSalvo();

  // Terminal evidence (E3-04/E3-05): one subscription for the page's life;
  // the awaited take is read through a ref, so a delivery that lands before
  // the 202 waits in the bounded early buffer instead of being dropped.
  const pendingRef = useRef<PendingTake | undefined>(undefined);
  const early = useRef(new EarlyEvidence());
  useEffect(
    () =>
      subscribeRealtime(
        '*',
        (envelope): void => {
          if (envelope.t === 'patch.take' || envelope.t === 'route.output' || envelope.t === 'switcher.operation') {
            void queryClient.invalidateQueries({ queryKey: PATCH_KEY });
          }
          const evidence = takeEvidenceOf(envelope);
          if (evidence === undefined) return;
          const awaited = pendingRef.current;
          if (awaited?.operationId === evidence.operationId) {
            if (awaited.path === evidence.path) {
              setOutcome((current) => (current?.operationId === evidence.operationId ? mergeEvidence(current, evidence) : evidence));
            }
            return;
          }
          early.current.push(evidence, Date.now());
        },
        setStatus,
      ),
    [queryClient],
  );

  const beginAwaiting = (take: PendingTake): void => {
    pendingRef.current = take;
    setPending(take);
    let settled: TakeEvidence | undefined;
    for (const evidence of early.current.drain(take.operationId, take.path, Date.now())) {
      settled = mergeEvidence(settled, evidence);
    }
    setOutcome(settled);
  };
  const clearAwaiting = (): void => {
    pendingRef.current = undefined;
    setPending(undefined);
    setOutcome(undefined);
  };

  // §8 `stale` (E3-06): the engine tick in the readback, not the fetch time.
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const timer = setInterval(() => { setNow(Date.now()); }, 500);
    return (): void => { clearInterval(timer); };
  }, []);
  const tick = readbackTick(patchQuery.data);
  const [tickSeen, setTickSeen] = useState<{ readonly tick: number; readonly at: number } | undefined>();
  if (tick !== undefined && tick !== tickSeen?.tick) {
    setTickSeen({ tick, at: patchQuery.dataUpdatedAt });
  }
  const tickFrozen = tick !== undefined && tickSeen !== undefined && now - tickSeen.at > STALE_MS;

  const readError = patchQuery.error;
  const lacksRead = readError instanceof OperationApiError && readError.status === 403;
  const canWrite = !lacksRead && writeQuery.data !== false;
  const connected = status === 'open';
  const pageState: MatrixPageState = lacksRead || writeQuery.data === false
    ? 'read-only'
    : !connected && status !== 'connecting'
      ? 'disconnected'
      : patchQuery.isPending
        ? 'loading'
        : patchQuery.data === undefined
          ? 'error'
          : tickFrozen
            ? 'stale'
            : 'live';
  /** Every take/store/recall/plan control is gated on this (§8: read-only and disconnected disable takes). */
  const canAct = canWrite && connected;
  const actDisabledReason = !canWrite
    ? t`Read-only: your role lacks Write`
    : !connected
      ? t`Disconnected: takes are disabled until the connection is back`
      : undefined;

  const playerIds = useMemo(() => (players.data ?? []).map((p) => ({ id: p.id })), [players.data]);
  const rows = useMemo(
    () =>
      buildRows(
        (sources.data ?? []).map((s) => ({ id: s.id, name: s.name, rawKind: s.rawKind })),
        (layouts.data ?? []).map((l) => ({ id: l.id, name: l.name })),
        playerIds,
      ),
    [sources.data, layouts.data, playerIds],
  );
  const audioRowList = useMemo(
    () => audioRows((sources.data ?? []).map((s) => ({ id: s.id, name: s.name }))),
    [sources.data],
  );

  const mixer = patchQuery.data?.mixer[0];
  const outputs = useMemo(() => patchQuery.data?.outputs ?? [], [patchQuery.data]);
  const mixerCols = useMemo(() => (mixer !== undefined ? mixerColumns(mixer) : []), [mixer]);
  const videoCols = useMemo(() => [...mixerCols, ...outputs.flatMap(outputVideoColumns)], [mixerCols, outputs]);
  const audioCols = useMemo(() => outputs.flatMap(outputAudioColumns), [outputs]);
  const subtitleCols = useMemo(() => outputs.flatMap(outputSubtitleColumns), [outputs]);
  const subtitleRowList = useMemo(
    () => subtitleRows((subtitleSources.data ?? []).map((s) => ({ ...s, hasCaptions: true }))),
    [subtitleSources.data],
  );
  const subtitleDataPresent = hasSubtitleData(outputs);

  /** Built-in row names translate at render (E3-11); configured names are shown as configured. */
  const rowLabel = (row: MatrixRow): string => {
    switch (row.id) {
      case 'bus:program': return t`Program`;
      case 'bus:preview': return t`Preview`;
      case 'bus:clean': return t`Clean`;
      case 'audio:follow': return t`Follow`;
      case 'audio:program': return t`Program mix`;
      case 'audio:none': return t`Silence`;
      default: return row.label;
    }
  };

  const programInput = mixer?.inputs.find((i) => i.on_program)?.number;
  const programOutputs = new Set(
    outputs
      .filter((o) => o.video.some((v) => {
        const source: unknown = v.source;
        return v.live && isRecord(source) && source.bus === 'program';
      }))
      .map((o) => o.output),
  );

  // The request the armed selection sends; its identity binds a plan (E3-08).
  const members = membersOf(selections);
  const problem = selectionProblem(selections);
  const crosspoints: readonly PatchCrosspoint[] = members.map((m) => m.crosspoint);
  const changesProgram = affectsProgram(selections, programInput, programOutputs);
  // A plan answers one selection revision under one lock state; any toggle
  // bumps the revision, so a late response for an older selection (even one
  // toggled back to the same cells) never re-enables Take.
  const [revision, setRevision] = useState(0);
  const requestKey = `${String(revision)}|${locked ? 'locked' : 'unlocked'}|${JSON.stringify(crosspoints)}`;
  const currentPlan = plan?.key === requestKey ? plan.plan : undefined;
  const currentPlanError = planError?.key === requestKey ? planError.message : undefined;

  /** Plan members keyed by destination, merged into cells (E3-09). */
  const planned = new Map<string, PlannedMember>();
  if (currentPlan !== undefined) {
    members.forEach((member, index) => {
      const answer = currentPlan.crosspoints[index];
      if (answer === undefined) return;
      planned.set(selectionKey(member.selection.plane, member.selection.column), {
        path: answer.path,
        refusal: answer.refusal ?? null,
      });
    });
  }
  const armedKeys = new Set(selections.map((sel) => `${selectionKey(sel.plane, sel.column)}|${sel.rowId}`));
  const isArmedIn = (p: Plane) => (row: MatrixRow, col: MatrixColumn): boolean =>
    armedKeys.has(`${selectionKey(p, col)}|${row.id}`);
  const withPlan = (p: Plane, row: MatrixRow, col: MatrixColumn, base: MatrixCell): MatrixCell => {
    const armed = isArmedIn(p)(row, col);
    // §8 `unknown`: no readback, or the tick stopped.
    const shown: MatrixCell = tickFrozen && base.state !== 'na' && base.state !== 'disabled' ? { state: 'unknown' } : base;
    return displayCell(shown, armed, armed ? planned.get(selectionKey(p, col)) : undefined);
  };

  const cellOfVideo = (row: MatrixRow, col: MatrixColumn): MatrixCell => {
    let base: MatrixCell;
    if (col.kind === 'mixer') {
      const input = mixer?.inputs.find((i) => i.number === col.input);
      base = input === undefined ? { state: 'unknown' } : mixerCell(row, input, {});
    } else {
      const output = outputs.find((o) => o.output === col.output);
      const slot = output?.video.find((v) => v.slot === col.slot);
      base = slot === undefined ? { state: 'unknown' } : outputSlotCell(row, slot);
    }
    return withPlan('video', row, col, base);
  };
  const cellOfAudio = (row: MatrixRow, col: MatrixColumn): MatrixCell => {
    if (col.kind === 'mixer') return { state: 'na' };
    const output = outputs.find((o) => o.output === col.output);
    const slot = output?.audio.find((a) => a.slot === col.slot);
    return withPlan('audio', row, col, slot === undefined ? { state: 'unknown' } : audioCell(row, slot));
  };
  const cellOfSubtitle = (row: MatrixRow, col: MatrixColumn): MatrixCell => {
    if (col.kind === 'mixer') return { state: 'na' };
    const output = outputs.find((o) => o.output === col.output);
    const slot = (output?.subtitle ?? []).find((s) => s.slot === col.slot);
    if (slot === undefined) return { state: 'unknown' };
    const current = isRecord(slot.source) && typeof slot.source.source === 'string'
      ? `source:${slot.source.source}` : undefined;
    // E2 refuses every subtitle crosspoint (`slot-medium-mismatch`, #345):
    // show what is on air, never arm one.
    return current === row.id ? { state: 'on-air' } : { state: 'disabled', disabledReason: 'subtitle-unsupported' };
  };

  const resetArming = (): void => {
    setSelections([]);
    setRevision((r) => r + 1);
    setPlan(undefined);
    setPlanError(undefined);
    setTakeError(undefined);
    setConfirm(undefined);
    clearAwaiting();
  };

  const toggleIn = (p: Plane) => (row: MatrixRow, column: MatrixColumn): void => {
    if (!canAct) return;
    setPlan(undefined);
    setPlanError(undefined);
    setRevision((r) => r + 1);
    setSelections((current) => {
      const key = selectionKey(p, column);
      const withoutColumn = current.filter((sel) => selectionKey(sel.plane, sel.column) !== key);
      const already = current.find((sel) => selectionKey(sel.plane, sel.column) === key && sel.rowId === row.id);
      if (already !== undefined) return withoutColumn;
      return [...withoutColumn, { column, rowId: row.id, plane: p }];
    });
  };

  /** The on_air the take sends: `allow` when unlocked, or after the explicit confirm (E3-02). */
  const requestFor = (onAir: OnAir): PatchRequest => ({ crosspoints, on_air: onAir });
  const planOnAir: OnAir = locked ? 'protect' : 'allow';

  const runPlan = async (): Promise<void> => {
    if (!canAct || crosspoints.length === 0 || problem !== undefined) return;
    const key = requestKey;
    setPlanning(true);
    setPlanError(undefined);
    try {
      // Bound to the revision it answered: shown only while that revision is current (E3-08).
      setPlan({ key, plan: await planPatch(requestFor(planOnAir)) });
    } catch (error) {
      setPlanError({ key, message: error instanceof OperationApiError ? (error.detail ?? error.message) : t`Plan failed` });
    } finally {
      setPlanning(false);
    }
  };

  const runTake = async (onAir: OnAir): Promise<void> => {
    setConfirm(undefined);
    if (!canAct || crosspoints.length === 0 || currentPlan === undefined) return;
    const body = requestFor(onAir);
    const path = takePathOf(body.crosspoints);
    setTaking(true);
    setTakeError(undefined);
    clearAwaiting();
    try {
      const accepted = await takePatch(body, { idempotencyKey: newIdempotencyKey() });
      beginAwaiting({ operationId: accepted.operation_id, path, label: 'patch' });
    } catch (error) {
      setTakeError(error instanceof OperationApiError ? (error.detail ?? error.message) : t`Take failed`);
    } finally {
      setTaking(false);
    }
  };

  const needsConfirm = locked && (changesProgram || (currentPlan !== undefined && planNeedsConfirm(currentPlan)));
  const onTakeAll = (): void => {
    if (!canAct) return;
    if (needsConfirm) {
      setConfirm('selection');
      return;
    }
    // Unlocked: `allow` (the plan used it too). Locked and off Program: `protect`.
    void runTake(planOnAir);
  };

  const outcomePhase: OperationPhase = takeError !== undefined
    ? 'failed'
    : pending === undefined
      ? 'idle'
      : outcome?.operationId === pending.operationId
        ? (outcome.outcome === 'applied' ? 'applied' : 'failed')
        : 'pending';

  const set = currentPlan?.set;
  const refusal = currentPlan !== undefined ? hardRefusal(currentPlan) : undefined;
  const confirmNote = currentPlan !== undefined && locked && planNeedsConfirm(currentPlan)
    ? t`Changes Program: needs confirmation.`
    : undefined;
  const planLine = currentPlan === undefined
    ? undefined
    : [
      set === undefined
        ? refusal === undefined ? t`Can take.` : t`Cannot take: ${refusal.detail}`
        : t`class ${currentPlan.class} · +${String(set.new_encoders)} encoder${set.new_encoders === 1 ? '' : 's'} · ${set.fits ? t`fits` : t`does not fit`}`,
      ...(set !== undefined && refusal !== undefined ? [t`Cannot take: ${refusal.detail}`] : []),
      ...(confirmNote !== undefined ? [confirmNote] : []),
    ].join(' ');

  const takeDisabledReason = actDisabledReason
    ?? (selections.length === 0
      ? t`Select crosspoints first`
      : problem === 'audio-needs-video'
        ? t`Audio Follow rides its output's video take: also arm that output's video`
        : crosspoints.length === 0
          ? t`Nothing in this selection can be taken in this release`
          : currentPlan === undefined
            ? t`Classify the selection first`
            : !planTakeable(currentPlan)
              ? t`The plan refuses this take`
              : undefined);
  const planDisabledReason = actDisabledReason
    ?? (selections.length === 0
      ? t`Select crosspoints first`
      : problem === 'audio-needs-video'
        ? t`Audio Follow rides its output's video take: also arm that output's video`
        : crosspoints.length === 0
          ? t`Nothing in this selection can be taken in this release`
          : undefined);

  const onStore = (): void => {
    if (!canAct || crosspoints.length === 0 || problem !== undefined || storeId.trim() === '') return;
    // `Salvo.patch` is typed with the generated schema's opaque
    // `Record<string, never>` source/destination; the wire JSON the model
    // builds matches it exactly (no structural overlap for the checker).
    // eslint-disable-next-line @typescript-eslint/no-unsafe-type-assertion
    const patch = crosspoints as unknown as NonNullable<Salvo['patch']>;
    const salvo: Salvo = {
      id: storeId.trim(),
      patch,
      on_air: 'protect',
    };
    saveSalvo.mutate({ salvo, create: true }, {
      onSuccess: (): void => {
        setStoreOpen(false);
        setStoreId('');
      },
    });
  };

  // Recall (E3-01): a recalled salvo's crosspoints are resolved against the
  // current readback and pass through the same Program-confirm gate.
  const recalled = (salvos.data ?? []).find((s) => s.id === recallId);
  const recalledMembers = useMemo(() => parseCrosspoints(recalled?.patch), [recalled]);
  const recalledChangesProgram = crosspointsAffectProgram(recalledMembers.crosspoints, programInput, programOutputs);
  const currentRecallPlan = recallPlan?.id === recallId ? recallPlan.plan : undefined;
  const recallProtects = (recalled?.on_air ?? 'protect') === 'protect';
  const recallOnAirRefused = currentRecallPlan?.crosspoints.some((xp) => xp.refusal?.type === ON_AIR_REFUSAL) ?? false;
  const recallNeedsConfirm = locked && (recalledChangesProgram || (currentRecallPlan !== undefined && planNeedsConfirm(currentRecallPlan)));
  const recallDisabledReason = actDisabledReason
    ?? (recallId === ''
      ? t`Choose a salvo first`
      : recalledMembers.invalid > 0
        ? t`This salvo has crosspoints this page cannot read`
        : currentRecallPlan === undefined
          ? t`Classify the salvo first`
          : recallOnAirRefused && recallProtects
            ? t`This salvo protects on-air crosspoints (stored on-air: protect); a recall cannot override it`
            : !planTakeable(currentRecallPlan)
              ? t`The plan refuses this salvo`
              : undefined);

  const onRecallPlan = async (): Promise<void> => {
    if (!canAct || recallId === '') return;
    const id = recallId;
    setRecalling(true);
    setRecallError(undefined);
    try {
      const answer = await planSalvo(id);
      setRecallPlan({ id, plan: answer });
    } catch (error) {
      setRecallError(error instanceof OperationApiError ? (error.detail ?? error.message) : t`Plan failed`);
    } finally {
      setRecalling(false);
    }
  };

  const runRecallTake = async (): Promise<void> => {
    setConfirm(undefined);
    if (!canAct || recallId === '' || recallDisabledReason !== undefined) return;
    setRecalling(true);
    setRecallError(undefined);
    clearAwaiting();
    try {
      const accepted = await takePatchSalvo(recallId);
      // A patch salvo always settles on `patch.take`, one destination too.
      beginAwaiting({ operationId: accepted.operation_id, path: 'set', label: 'salvo' });
    } catch (error) {
      setRecallError(error instanceof OperationApiError ? (error.detail ?? error.message) : t`Take failed`);
    } finally {
      setRecalling(false);
    }
  };
  const onRecallTake = (): void => {
    if (!canAct || recallDisabledReason !== undefined) return;
    if (recallNeedsConfirm) {
      setConfirm('salvo');
      return;
    }
    void runRecallTake();
  };

  const pageStateLabel = (): string => {
    switch (pageState) {
      case 'loading': return t`Loading`;
      case 'live': return t`Live`;
      case 'stale': return t`Engine not confirming (stale)`;
      case 'disconnected': return t`Disconnected`;
      case 'read-only': return t`Read-only`;
      case 'error': return t`Readback unavailable`;
    }
  };

  const recallPlanLine = currentRecallPlan === undefined
    ? undefined
    : [
      planTakeable(currentRecallPlan) ? t`Can take.` : t`Cannot take.`,
      ...(recallNeedsConfirm ? [t`Changes Program: needs confirmation.`] : []),
    ].join(' ');

  return (
    <div className="space-y-4">
      <p role="status" data-testid="matrix-page-state" aria-live="polite">
        {pageStateLabel()}
        {selections.length > 0 ? <span data-testid="matrix-arming"> · {t`Salvo arming (${String(selections.length)} selected)`}</span> : null}
      </p>
      {pageState === 'stale' ? (
        <p role="alert" data-testid="matrix-stale-warning" className="text-xs">
          <Trans>Engine not confirming: the tick has not advanced for over 2 s. Crosspoints show as unknown; takes stay enabled.</Trans>
        </p>
      ) : null}
      {pageState === 'error' ? (
        <p role="alert" data-testid="matrix-readback-error" className="text-destructive text-xs">
          {readError instanceof OperationApiError ? (readError.detail ?? readError.message) : t`The patch readback could not be read.`}
        </p>
      ) : null}
      <Tabs
        value={plane}
        onValueChange={(value): void => {
          // Arming survives a tab change: an output's audio Follow rides its
          // video take, so the two are armed together across tabs.
          if (value === 'video' || value === 'audio' || value === 'subtitle') setPlane(value);
        }}
      >

        <TabsList>
          <TabsTrigger value="video"><Trans>Video</Trans></TabsTrigger>
          <TabsTrigger value="audio"><Trans>Audio</Trans></TabsTrigger>
          <TabsTrigger value="subtitle"><Trans>Subtitle</Trans></TabsTrigger>
        </TabsList>
        <TabsContent value="video">
          <MatrixGrid
            plane="video"
            rows={rows}
            columns={videoCols}
            cellOf={cellOfVideo}
            isArmed={isArmedIn('video')}
            onToggle={toggleIn('video')}
            interactive={canAct}
            rowLabel={rowLabel}
          />
        </TabsContent>
        <TabsContent value="audio">
          <p className="rp-note text-xs" data-testid="matrix-audio-note">
            <Trans>
              In this release only an output's audio slot 0 can be patched, and
              only to Follow, which rides that output's video take: arm both.
              Other audio feeds are fixed by the output's configuration.
            </Trans>
          </p>
          <MatrixGrid
            plane="audio"
            rows={audioRowList}
            columns={audioCols}
            cellOf={cellOfAudio}
            isArmed={isArmedIn('audio')}
            onToggle={toggleIn('audio')}
            interactive={canAct}
            rowLabel={rowLabel}
          />
        </TabsContent>
        <TabsContent value="subtitle">
          {!subtitleDataPresent ? (
            <p role="status" data-testid="matrix-subtitle-empty" className="rp-note">
              <Trans>
                No subtitle slots yet: this build has not declared any
                output's subtitle renditions (#345 fills this tab).
              </Trans>
            </p>
          ) : (
            <>
              <p className="rp-note text-xs" data-testid="matrix-subtitle-note">
                <Trans>Subtitle patching is not available in this release (#345): this tab shows what is on air only.</Trans>
              </p>
              <MatrixGrid
                plane="subtitle"
                rows={subtitleRowList}
                columns={subtitleCols}
                cellOf={cellOfSubtitle}
                isArmed={isArmedIn('subtitle')}
                onToggle={toggleIn('subtitle')}
                interactive={canAct}
                rowLabel={rowLabel}
              />
            </>
          )}
        </TabsContent>
      </Tabs>

      <div className="plan-panel" data-testid="matrix-plan-panel">
        <h3>{selections.length === 0 ? <Trans>Plan — nothing selected</Trans> : <Trans>Plan: {String(selections.length)} crosspoint(s)</Trans>}</h3>
        {actDisabledReason !== undefined ? <p data-testid="matrix-act-reason" className="text-xs">{actDisabledReason}</p> : null}
        {currentPlanError !== undefined ? <p role="alert" className="text-destructive text-xs">{currentPlanError}</p> : null}
        {planLine !== undefined ? <p data-testid="matrix-plan-line" className="text-xs">{planLine}</p> : null}
        <div className="flex items-center gap-2">
          <TouchAction
            onPress={(): void => { void runPlan(); }}
            disabled={planDisabledReason !== undefined}
            disabledReason={planDisabledReason}
            pending={planning}
          >
            <Trans>Classify selection</Trans>
          </TouchAction>
          <TouchAction
            onPress={onTakeAll}
            disabled={takeDisabledReason !== undefined}
            disabledReason={takeDisabledReason}
            pending={taking}
          >
            {isSetSelection(selections) ? <Trans>Take all</Trans> : <Trans>Take this crosspoint</Trans>}
          </TouchAction>
          <Button
            type="button"
            variant="outline"
            className="min-h-12"
            disabled={planDisabledReason !== undefined}
            title={planDisabledReason}
            onClick={(): void => { if (canAct) setStoreOpen(true); }}
          >
            <Trans>Store as salvo…</Trans>
          </Button>
          <Button
            type="button"
            variant="ghost"
            className="min-h-12"
            disabled={selections.length === 0}
            onClick={resetArming}
          >
            <Trans>Clear selection</Trans>
          </Button>
        </div>
        {takeError !== undefined ? <p role="alert" className="text-destructive text-xs">{takeError}</p> : null}
        {pending !== undefined ? (
          <OperationFeedback
            phase={outcomePhase}
            operationLabel={pending.label === 'salvo' ? t`Salvo take` : t`Patch take`}
            correlationId={pending.operationId}
            failureReason={outcome?.outcome === 'refused'
              ? (outcome.reason !== undefined
                ? ((reason: string): string => t`Refused by the engine: ${reason}`)(outcome.reason)
                : t`Refused by the engine`)
              : undefined}
          />
        ) : null}
      </div>

      <div className="plan-panel" data-testid="matrix-recall-panel">
        <h3><Trans>Recall a stored salvo</Trans></h3>
        <div className="flex items-end gap-2">
          <div className="grid gap-1.5">
            <Label htmlFor="recall-salvo"><Trans>Salvo</Trans></Label>
            <Select
              value={recallId}
              onValueChange={(value): void => {
                setRecallId(value);
                setRecallPlan(undefined);
                setRecallError(undefined);
              }}
            >
              <SelectTrigger id="recall-salvo" className="w-56">
                <SelectValue placeholder={t`Choose a salvo…`} />
              </SelectTrigger>
              <SelectContent>
                {(salvos.data ?? []).map((s) => (
                  <SelectItem key={s.id} value={s.id}>{s.display_name ?? s.id}</SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
          <TouchAction
            onPress={(): void => { void onRecallPlan(); }}
            disabled={recallId === '' || !canAct}
            disabledReason={actDisabledReason ?? (recallId === '' ? t`Choose a salvo first` : undefined)}
            pending={recalling}
          >
            <Trans>Classify salvo</Trans>
          </TouchAction>
          <TouchAction
            onPress={onRecallTake}
            disabled={recallDisabledReason !== undefined}
            disabledReason={recallDisabledReason}
            pending={recalling}
          >
            <Trans>Take salvo</Trans>
          </TouchAction>
        </div>
        {recallError !== undefined ? <p role="alert" className="text-destructive text-xs">{recallError}</p> : null}
        {recallPlanLine !== undefined ? (
          <p data-testid="matrix-recall-plan-line" className="text-xs">{recallPlanLine}</p>
        ) : null}
      </div>

      <Dialog open={storeOpen && canAct} onOpenChange={setStoreOpen}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle><Trans>Store this selection as a salvo</Trans></DialogTitle>
            <DialogDescription>
              <Trans>
                The salvo stores the {String(crosspoints.length)} armed crosspoint(s) with
                on-air protection by default.
              </Trans>
            </DialogDescription>
          </DialogHeader>
          <div className="grid gap-1.5">
            <Label htmlFor="store-salvo-id"><Trans>Salvo id</Trans></Label>
            <Input id="store-salvo-id" value={storeId} onChange={(e): void => { setStoreId(e.target.value); }} />
          </div>
          <DialogFooter>
            <Button variant="outline" className="min-h-12" onClick={(): void => { setStoreOpen(false); }}>
              <Trans>Cancel</Trans>
            </Button>
            <Button className="min-h-12" disabled={storeId.trim() === '' || !canAct} onClick={onStore}>
              <Trans>Store salvo</Trans>
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <Dialog open={confirm !== undefined && canAct} onOpenChange={(open): void => { if (!open) setConfirm(undefined); }}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle><Trans>Confirm a program-affecting take?</Trans></DialogTitle>
            <DialogDescription>
              {confirm === 'salvo' ? (
                <Trans>
                  Takes are locked and this salvo changes a crosspoint that
                  currently carries Program. Confirm to take it.
                </Trans>
              ) : (
                <Trans>
                  Takes are locked and this set changes a crosspoint that
                  currently carries Program. Confirm to apply it.
                </Trans>
              )}
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" className="min-h-12" onClick={(): void => { setConfirm(undefined); }}>
              <Trans>Cancel</Trans>
            </Button>
            <Button
              className="min-h-12"
              disabled={!canAct}
              onClick={(): void => {
                // The explicit confirm is the permission: `allow` (E3-02).
                if (confirm === 'salvo') void runRecallTake();
                else void runTake('allow');
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
