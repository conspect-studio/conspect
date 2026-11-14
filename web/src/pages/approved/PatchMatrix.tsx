// The Routing page as the patch matrix (ADR-0110 §8, §12.9, #308).
//
// Video | Audio | Subtitle tabs; rows are sources grouped Buses / Layouts /
// Players / Generators / Sources; columns are mixer inputs then output slots.
// A cell shows the §8 glyph + path; selecting cells across 2+ destinations
// arms a salvo ("Take all"). The take lock and the on-air confirm guard any
// crosspoint that would change Program. Page states: loading / live / stale
// (>2s) / disconnected / read-only / salvo-arming (§8).
import { Fragment, useEffect, useMemo, useState } from 'react';
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
import { getPatch, planPatch, takePatch } from '../../api/patch';
import type { PatchPlanResponse, PatchRequest, PatchView } from '../../api/patch';
import { planSalvo, takePatchSalvo } from '../../api/salvos';
import type { Salvo } from '../../api/salvos';
import { useSalvos, useSaveSalvo } from '../../api/salvosQueries';
import { useSources } from '../../resources/queries';
import { useLayouts } from '../../api/queries';
import { useMediaPlayers } from '../../api/media-playersQueries';
import { createApiClient } from '../../api/client';
import { useSubtitleSources } from './useSubtitleSources';
import {
  affectsProgram,
  buildRows,
  crosspointsOf,
  hasSubtitleData,
  isSetSelection,
  mixerCell,
  mixerColumns,
  outputAudioColumns,
  outputSlotCell,
  outputSubtitleColumns,
  outputVideoColumns,
  subtitleRows,
} from './patchMatrixModel';
import type { ArmedSelection, MatrixCell, MatrixColumn, MatrixRow } from './patchMatrixModel';

const PATCH_KEY = ['patch', 'matrix'] as const;
/** §8: a frozen tick for more than this many ms is `stale`, not `live`. */
const STALE_MS = 2000;

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

/** Validated `patch.take` evidence for one destination (ADR-0110 §12.8). */
function patchTakeEvidence(data: unknown): { readonly operationId: string; readonly outcome: 'applied' | 'refused' } | undefined {
  if (!isRecord(data) || typeof data.operation_id !== 'string') return undefined;
  const outcome = data.outcome;
  if (outcome !== 'applied' && outcome !== 'refused') return undefined;
  return { operationId: data.operation_id, outcome };
}

/** Coarse page state (§8); `salvo-arming` is layered on top once a selection exists. */
export type MatrixPageState = 'loading' | 'live' | 'stale' | 'disconnected' | 'read-only';

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

/** Group heading labels, in render order. */
const GROUPS: readonly (readonly [MatrixRow['group'], string])[] = [
  ['buses', 'Buses'],
  ['layouts', 'Layouts'],
  ['players', 'Players'],
  ['generators', 'Generators'],
  ['sources', 'Sources'],
];

function columnKey(column: MatrixColumn): string {
  return column.kind === 'mixer' ? `mixer:${column.me}:${String(column.input)}` : `output:${column.output}:${String(column.slot)}`;
}

/** One matrix tab's grid: rows × columns, with click-to-arm cells. */
function MatrixGrid({
  plane,
  rows,
  columns,
  cellOf,
  selections,
  onToggle,
  readOnly,
}: {
  readonly plane: Plane;
  readonly rows: readonly MatrixRow[];
  readonly columns: readonly MatrixColumn[];
  readonly cellOf: (row: MatrixRow, column: MatrixColumn) => MatrixCell;
  readonly selections: readonly ArmedSelection[];
  readonly onToggle: (row: MatrixRow, column: MatrixColumn) => void;
  readonly readOnly: boolean;
}): JSX.Element {
  const { t } = useLingui();
  /** The accessible-name state fragment (§8); needs `t` in this component's scope. */
  const stateLabel = (cell: MatrixCell): string => {
    switch (cell.state) {
      case 'on-air': return t`on air`;
      case 'planned': return t`planned`;
      case 'pending': return cell.heldTicks !== undefined
        ? t`pending (${cell.heldTicks} of 50 ticks)`
        : t`pending`;
      case 'available': return t`available`;
      case 'disabled': return cell.disabledReason !== undefined
        ? t`disabled: ${cell.disabledReason}`
        : t`disabled`;
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
            return (
              <Fragment key={row.id}>
                {groupHeader ? (
                  <tr key={`group-${row.group}`}>
                    <th className="grp-h" colSpan={columns.length + 1} scope="colgroup">
                      {GROUPS.find(([g]) => g === row.group)?.[1] ?? row.group}
                    </th>
                  </tr>
                ) : null}
                <tr key={row.id}>
                  <th className="patch-src" scope="row" title={row.label}>
                    {row.label}
                  </th>
                  {columns.map((col) => {
                    const cell = cellOf(row, col);
                    const armed = selections.some(
                      (sel) => columnKey(sel.column) === columnKey(col) && sel.rowId === row.id,
                    );
                    const disabled = readOnly || cell.state === 'na' || cell.state === 'disabled';
                    return (
                      <td key={columnKey(col)}>
                        <button
                          type="button"
                          className={armed ? 'xpt plan' : 'xpt'}
                          aria-pressed={armed}
                          disabled={disabled}
                          aria-label={t`${row.label} to ${col.label}: ${stateLabel(cell)}`}
                          title={cell.disabledReason ?? stateLabel(cell)}
                          data-testid={`cell-${plane}-${row.id}-${columnKey(col)}`}
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

/** The Routing page: Video | Audio | Subtitle matrix tabs + the arming panel. */
export function PatchMatrix(): JSX.Element {
  const { t } = useLingui();
  const queryClient = useQueryClient();
  const { locked } = useTakeLock();
  const [status, setStatus] = useState<RealtimeStatus>('connecting');
  const [plane, setPlane] = useState<Plane>('video');
  const [selections, setSelections] = useState<readonly ArmedSelection[]>([]);
  const [plan, setPlan] = useState<PatchPlanResponse | undefined>();
  const [planError, setPlanError] = useState<string | undefined>();
  const [planning, setPlanning] = useState(false);
  const [taking, setTaking] = useState(false);
  const [takeError, setTakeError] = useState<string | undefined>();
  const [confirm, setConfirm] = useState(false);
  const [operationId, setOperationId] = useState<string | undefined>();
  const [outcome, setOutcome] = useState<{ readonly operationId: string; readonly outcome: 'applied' | 'refused' } | undefined>();
  const [storeOpen, setStoreOpen] = useState(false);
  const [storeId, setStoreId] = useState('');
  const [recallId, setRecallId] = useState('');
  const [recallPlan, setRecallPlan] = useState<PatchPlanResponse | undefined>();
  const [recallError, setRecallError] = useState<string | undefined>();
  const [recalling, setRecalling] = useState(false);

  const patchQuery = useQuery<PatchView, OperationApiError>({
    queryKey: PATCH_KEY,
    queryFn: () => getPatch(),
    retry: false,
    refetchInterval: 2000,
  });
  const sources = useSources();
  const apiClient = useMemo(() => createApiClient(), []);
  const layouts = useLayouts(apiClient);
  const players = useMediaPlayers();
  const subtitleSources = useSubtitleSources();
  const salvos = useSalvos();
  const saveSalvo = useSaveSalvo();

  useEffect(
    () =>
      subscribeRealtime(
        '*',
        (envelope): void => {
          if (envelope.t === 'patch.take' || envelope.t === 'route.output' || envelope.t === 'switcher.operation') {
            void queryClient.invalidateQueries({ queryKey: PATCH_KEY });
          }
          if (envelope.t === 'patch.take') {
            const evidence = patchTakeEvidence(envelope.data);
            if (evidence !== undefined && evidence.operationId === operationId) {
              setOutcome({ operationId, outcome: evidence.outcome });
            }
          }
        },
        setStatus,
      ),
    [queryClient, operationId],
  );

  // §8: stale once the tick-bearing readback hasn't refreshed in >2s.
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const timer = setInterval(() => { setNow(Date.now()); }, 500);
    return (): void => { clearInterval(timer); };
  }, []);
  const readOnly = patchQuery.error instanceof OperationApiError && patchQuery.error.status === 403;
  const pageState: MatrixPageState = readOnly
    ? 'read-only'
    : status === 'closed' || status === 'reconnecting'
      ? 'disconnected'
      : patchQuery.isPending
        ? 'loading'
        : patchQuery.data === undefined
          ? 'disconnected'
          : now - patchQuery.dataUpdatedAt > STALE_MS
            ? 'stale'
            : 'live';

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

  const cellOfVideo = (row: MatrixRow, col: MatrixColumn): MatrixCell => {
    if (col.kind === 'mixer') {
      const input = mixer?.inputs.find((i) => i.number === col.input);
      return input === undefined ? { state: 'unknown' } : mixerCell(row, input, {});
    }
    const output = outputs.find((o) => o.output === col.output);
    const slot = output?.video.find((v) => v.slot === col.slot);
    return slot === undefined ? { state: 'unknown' } : outputSlotCell(row, slot);
  };
  const cellOfAudio = (row: MatrixRow, col: MatrixColumn): MatrixCell => {
    if (col.kind === 'mixer') return { state: 'na' };
    const output = outputs.find((o) => o.output === col.output);
    const slot = output?.audio.find((a) => a.slot === col.slot);
    return slot === undefined ? { state: 'unknown' } : outputSlotCell(row, slot);
  };
  const cellOfSubtitle = (row: MatrixRow, col: MatrixColumn): MatrixCell => {
    if (col.kind === 'mixer') return { state: 'na' };
    const output = outputs.find((o) => o.output === col.output);
    const slot = (output?.subtitle ?? []).find((s) => s.slot === col.slot);
    if (slot === undefined) return { state: 'unknown' };
    const current = isRecord(slot.source) && typeof slot.source.source === 'string'
      ? `source:${slot.source.source}` : undefined;
    return current === row.id ? { state: 'on-air' } : { state: 'available' };
  };

  const resetArming = (): void => {
    setSelections([]);
    setPlan(undefined);
    setPlanError(undefined);
    setOperationId(undefined);
    setOutcome(undefined);
    setTakeError(undefined);
  };

  const toggle = (row: MatrixRow, column: MatrixColumn): void => {
    setPlan(undefined);
    setPlanError(undefined);
    setSelections((current) => {
      const key = columnKey(column);
      const withoutColumn = current.filter((sel) => columnKey(sel.column) !== key);
      const already = current.find((sel) => columnKey(sel.column) === key && sel.rowId === row.id);
      if (already !== undefined) return withoutColumn;
      return [...withoutColumn, { column, rowId: row.id, plane }];
    });
  };

  const request: PatchRequest | undefined = selections.length === 0
    ? undefined
    : { crosspoints: crosspointsOf(selections), on_air: locked ? 'protect' : 'allow' };

  const runPlan = async (): Promise<void> => {
    if (request === undefined) return;
    setPlanning(true);
    setPlanError(undefined);
    try {
      setPlan(await planPatch(request));
    } catch (error) {
      setPlanError(error instanceof OperationApiError ? (error.detail ?? error.message) : t`Plan failed`);
    } finally {
      setPlanning(false);
    }
  };

  const runTake = async (): Promise<void> => {
    if (request === undefined) return;
    setConfirm(false);
    setTaking(true);
    setTakeError(undefined);
    setOutcome(undefined);
    try {
      const accepted = await takePatch(request, { idempotencyKey: newIdempotencyKey() });
      setOperationId(accepted.operation_id);
    } catch (error) {
      setTakeError(error instanceof OperationApiError ? (error.detail ?? error.message) : t`Take failed`);
    } finally {
      setTaking(false);
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
  const changesProgram = affectsProgram(selections, programInput, programOutputs);

  const onTakeAll = (): void => {
    if (locked && changesProgram) {
      setConfirm(true);
      return;
    }
    void runTake();
  };

  const phase: OperationPhase = takeError !== undefined
    ? 'failed'
    : operationId === undefined
      ? 'idle'
      : outcome?.operationId === operationId
        ? (outcome.outcome === 'applied' ? 'applied' : 'failed')
        : 'pending';

  const set = plan?.set;
  const planLine = plan === undefined
    ? undefined
    : set === undefined
      ? plan.can_take ? t`Can take.` : t`Cannot take: ${plan.crosspoints.find((xp) => xp.refusal !== null && xp.refusal !== undefined)?.refusal?.detail ?? t`refused`}`
      : t`class ${plan.class} · +${String(set.new_encoders)} encoder${set.new_encoders === 1 ? '' : 's'} · ${set.fits ? t`fits` : t`does not fit`}`;

  const onStore = (): void => {
    if (selections.length === 0 || storeId.trim() === '') return;
    // `Salvo.patch` is typed with the generated schema's opaque
    // `Record<string, never>` source/destination (the same shape
    // `PatchCrosspoint` structurally satisfies); the wire JSON is identical.
    // `Salvo.patch`'s generated element type is the same opaque-source shape
    // as `PatchCrosspoint`; the wire JSON `crosspointsOf` builds matches it
    // exactly (no `Record<string, never>` structural overlap for the checker
    // to narrow through).
    // eslint-disable-next-line @typescript-eslint/no-unsafe-type-assertion
    const patch = crosspointsOf(selections) as unknown as NonNullable<Salvo['patch']>;
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

  const onRecallPlan = async (): Promise<void> => {
    if (recallId === '') return;
    setRecalling(true);
    setRecallError(undefined);
    try {
      setRecallPlan(await planSalvo(recallId));
    } catch (error) {
      setRecallError(error instanceof OperationApiError ? (error.detail ?? error.message) : t`Plan failed`);
    } finally {
      setRecalling(false);
    }
  };

  const onRecallTake = async (): Promise<void> => {
    if (recallId === '') return;
    setRecalling(true);
    setRecallError(undefined);
    try {
      const accepted = await takePatchSalvo(recallId);
      setOperationId(accepted.operation_id);
    } catch (error) {
      setRecallError(error instanceof OperationApiError ? (error.detail ?? error.message) : t`Take failed`);
    } finally {
      setRecalling(false);
    }
  };

  const pageStateLabel = (): string => {
    switch (pageState) {
      case 'loading': return t`Loading`;
      case 'live': return t`Live`;
      case 'stale': return t`Engine not confirming (stale)`;
      case 'disconnected': return t`Disconnected`;
      case 'read-only': return t`Read-only`;
    }
  };

  return (
    <div className="space-y-4">
      <p role="status" data-testid="matrix-page-state" aria-live="polite">
        {pageStateLabel()}
        {selections.length > 0 ? <span data-testid="matrix-arming"> · {t`Salvo arming (${String(selections.length)} selected)`}</span> : null}
      </p>
      <Tabs
        value={plane}
        onValueChange={(value): void => {
          if (value === 'video' || value === 'audio' || value === 'subtitle') setPlane(value);
          resetArming();
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
            selections={selections}
            onToggle={toggle}
            readOnly={readOnly}
          />
        </TabsContent>
        <TabsContent value="audio">
          <MatrixGrid
            plane="audio"
            rows={rows}
            columns={audioCols}
            cellOf={cellOfAudio}
            selections={selections}
            onToggle={toggle}
            readOnly={readOnly}
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
            <MatrixGrid
              plane="subtitle"
              rows={subtitleRowList}
              columns={subtitleCols}
              cellOf={cellOfSubtitle}
              selections={selections}
              onToggle={toggle}
              readOnly={readOnly}
            />
          )}
        </TabsContent>
      </Tabs>

      <div className="plan-panel" data-testid="matrix-plan-panel">
        <h3>{selections.length === 0 ? <Trans>Plan — nothing selected</Trans> : <Trans>Plan: {String(selections.length)} crosspoint(s)</Trans>}</h3>
        {planError !== undefined ? <p role="alert" className="text-destructive text-xs">{planError}</p> : null}
        {planLine !== undefined ? <p data-testid="matrix-plan-line" className="text-xs">{planLine}</p> : null}
        <div className="flex items-center gap-2">
          <TouchAction
            onPress={(): void => { void runPlan(); }}
            disabled={selections.length === 0 || readOnly}
            pending={planning}
          >
            <Trans>Classify selection</Trans>
          </TouchAction>
          <TouchAction
            onPress={onTakeAll}
            disabled={selections.length === 0 || plan === undefined || !plan.can_take || readOnly}
            disabledReason={
              selections.length === 0
                ? t`Select crosspoints first`
                : plan === undefined
                  ? t`Classify the selection first`
                  : !plan.can_take
                    ? t`The plan refuses this take`
                    : undefined
            }
            pending={taking}
          >
            {isSetSelection(selections) ? <Trans>Take all</Trans> : <Trans>Take this crosspoint</Trans>}
          </TouchAction>
          <Button
            type="button"
            variant="outline"
            className="min-h-12"
            disabled={selections.length === 0 || readOnly}
            onClick={(): void => { setStoreOpen(true); }}
          >
            <Trans>Store as salvo…</Trans>
          </Button>
        </div>
        {takeError !== undefined ? <p role="alert" className="text-destructive text-xs">{takeError}</p> : null}
        {operationId !== undefined ? (
          <OperationFeedback
            phase={phase}
            operationLabel={t`Patch take`}
            correlationId={operationId}
            failureReason={outcome?.outcome === 'refused' ? t`Refused by the engine` : undefined}
          />
        ) : null}
      </div>

      <div className="plan-panel" data-testid="matrix-recall-panel">
        <h3><Trans>Recall a stored salvo</Trans></h3>
        <div className="flex items-end gap-2">
          <div className="grid gap-1.5">
            <Label htmlFor="recall-salvo"><Trans>Salvo</Trans></Label>
            <Select value={recallId} onValueChange={setRecallId}>
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
          <TouchAction onPress={(): void => { void onRecallPlan(); }} disabled={recallId === '' || readOnly} pending={recalling}>
            <Trans>Classify salvo</Trans>
          </TouchAction>
          <TouchAction onPress={(): void => { void onRecallTake(); }} disabled={recallId === '' || readOnly} pending={recalling}>
            <Trans>Take salvo</Trans>
          </TouchAction>
        </div>
        {recallError !== undefined ? <p role="alert" className="text-destructive text-xs">{recallError}</p> : null}
        {recallPlan !== undefined ? (
          <p data-testid="matrix-recall-plan-line" className="text-xs">
            {recallPlan.can_take ? t`Can take.` : t`Cannot take.`}
          </p>
        ) : null}
      </div>

      <Dialog open={storeOpen} onOpenChange={setStoreOpen}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle><Trans>Store this selection as a salvo</Trans></DialogTitle>
            <DialogDescription>
              <Trans>
                The salvo stores the {String(selections.length)} armed crosspoint(s) with
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
            <Button className="min-h-12" disabled={storeId.trim() === ''} onClick={onStore}>
              <Trans>Store salvo</Trans>
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <Dialog open={confirm} onOpenChange={setConfirm}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle><Trans>Confirm a program-affecting take?</Trans></DialogTitle>
            <DialogDescription>
              <Trans>
                Takes are locked and this set changes a crosspoint that
                currently carries Program. Confirm to apply it.
              </Trans>
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" className="min-h-12" onClick={(): void => { setConfirm(false); }}>
              <Trans>Cancel</Trans>
            </Button>
            <Button className="min-h-12" onClick={(): void => { void runTake(); }}>
              <Trans>Confirm take</Trans>
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
