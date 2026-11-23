// Program/Preview switcher: all controls are disabled until a real dual-bus
// engine publishes state. Accepted operations are never labelled applied.
import { useCallback, useEffect, useRef, useState } from 'react';
import type { JSX } from 'react';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { Trans, useLingui } from '@lingui/react/macro';
import { PageHeader } from '../components/PageHeader';
import { useTakeLock } from '../components/broadcast/BroadcastShell';
import { Button } from '../components/ui/button';
import { Label } from '../components/ui/label';
import { Input } from '../components/ui/input';
import { useSources } from '../resources/queries';
import { commandSwitcher, getSwitcher, planSwitcher } from '../api/switcher';
import type { SwitcherAction, SwitcherRequest } from '../api/switcher';
import { subscribeRealtime } from '../realtime/shared';
import { useOperationFeedback, useSubmitTrackedOperation } from '../components/broadcast/useOperationFeedback';
import { OperationFeedback } from '../components/broadcast/OperationFeedback';
import { switcherOperationEvidence } from '../realtime/operationEvidence';
import { isBackendUnavailable, newIdempotencyKey } from '../api/operations';
import { useJpegStill } from '../preview/useJpegPreview';
import { getPatch, planPatch, takePatch } from '../api/patch';
import type { PatchRequest } from '../api/patch';
import { MixerInputBank } from '../components/switcher/MixerInputBank';
import { commandKeyer, extrapolatedTick, keyerDomKey, keyerIsUsk, keyerRampWeight, planKeyer } from '../api/keyers';
import type { ArmedView, ArmSet, KeyerFields, KeyerRequest, KeyerRow } from '../api/keyers';
import { OperationApiError } from '../api/operations';
import type { RealtimeStatus } from '../realtime/connection';
import { KeyerStrip } from '../components/switcher/KeyerStrip';
import { KeyerPanel } from '../components/switcher/KeyerPanel';
import { ArmStrip } from '../components/switcher/ArmStrip';

const STATE_KEY = ['switcher', 'me1'] as const;
const PATCH_KEY = ['patch'] as const;
/** A select or take; a select by mixer input carries `input`, not `source`. */
type Intent = SwitcherAction | { readonly action: 'select_preview' | 'select_program'; readonly input: number };
function SwitcherMonitor({ bus, path }: { readonly bus: string; readonly path: string }): JSX.Element {
  const { url: src, state, requestFailed } = useJpegStill(path, 1000);
  return (
    <section className="space-y-2 rounded-md border p-3" aria-label={bus}>
      <h2 className="font-semibold">{bus}</h2>
      {src === undefined
        ? <div role="img" aria-label={`${bus} unavailable`} className="flex aspect-video items-center justify-center bg-black text-sm text-white">{requestFailed ? <Trans>Preview request failed — retrying</Trans> : <Trans>Waiting for a real frame</Trans>}</div>
        : <img src={src} alt={`${bus} monitor`} className="aspect-video w-full bg-black object-contain" />}
      {src !== undefined && (requestFailed || state === 'stale') && <p role="status"><Trans>Last received image — live video cannot be verified</Trans></p>}
    </section>
  );
}

/** First usable M/E when the backend is wired, honest unavailable panel otherwise. */
export function SwitcherPage(): JSX.Element {
  const { t } = useLingui();
  const queryClient = useQueryClient();
  const { locked } = useTakeLock();
  const submitTracked = useSubmitTrackedOperation();
  const state = useQuery({ queryKey: STATE_KEY, queryFn: () => getSwitcher(), retry: false });
  const sources = useSources();
  // No mixer block (no inputs declared, or not readable): today's page unchanged.
  const patch = useQuery({ queryKey: PATCH_KEY, queryFn: () => getPatch(), retry: false });
  const mixer = patch.data?.mixer.find((m) => m.me === 'me1');
  const [selected, setSelected] = useState('');
  const [duration, setDuration] = useState('25');
  const [operationId, setOperationId] = useState<string | undefined>();
  const operation = useOperationFeedback(operationId);
  const [failureReason, setFailureReason] = useState<string | undefined>();
  const [error, setError] = useState<string | undefined>();
  const [stream, setStream] = useState<RealtimeStatus>('connecting');
  useEffect(() => {
    const refresh = (): void => {
      void queryClient.invalidateQueries({ queryKey: STATE_KEY });
      void queryClient.invalidateQueries({ queryKey: PATCH_KEY });
    };
    return subscribeRealtime('switcher', (envelope) => {
      // A keyer state change (operator, tie, source lost/recovered, a new
      // definition) carries no operation outcome: refresh the snapshot.
      if (envelope.t === 'switcher.keyer') {
        void queryClient.invalidateQueries({ queryKey: STATE_KEY });
        return;
      }
      if (envelope.t !== 'switcher.operation') return;
      // Display-only reason: the shared tracker, not this page, decides the phase.
      const evidence = switcherOperationEvidence(envelope);
      if (evidence !== undefined && evidence.corr === operationId && evidence.resource === 'me1' && evidence.verdict === 'failed') {
        setFailureReason(evidence.reason);
      }
      refresh();
    }, (next) => {
      setStream(next);
      // Back from a disconnect: events may have been missed.
      if (next === 'open') refresh();
    }, refresh);
  }, [queryClient, operationId]);
  // One submission gate for every switcher and keyer control, held from the
  // plan through the 202 (or the refusal): a second click while a request is
  // in flight is refused here, never enqueued with another Idempotency-Key.
  const gateRef = useRef(false);
  const [submitting, setSubmitting] = useState(false);
  const gated = useCallback(async <T,>(run: () => Promise<T>): Promise<T | undefined> => {
    if (gateRef.current) return undefined;
    gateRef.current = true;
    setSubmitting(true);
    try {
      return await run();
    } finally {
      gateRef.current = false;
      setSubmitting(false);
    }
  }, []);
  const mutate = useMutation({
    mutationFn: async (intent: Intent) => {
      const action = typeof intent === 'string' ? intent : intent.action;
      const input = typeof intent === 'string' ? undefined : intent.input;
      const source = input === undefined && (action === 'select_program' || action === 'select_preview') ? selected : undefined;
      const durationFrames = action === 'auto' ? Number(duration) : undefined;
      const req: SwitcherRequest = {
        me: 'me1', program: 'main', action,
        ...(source === undefined ? {} : { source }),
        ...(input === undefined ? {} : { input }),
        ...(durationFrames === undefined ? {} : { duration_frames: durationFrames }),
      };
      await planSwitcher(req);
      return submitTracked(() => commandSwitcher(action, req, { idempotencyKey: newIdempotencyKey() }),
        { topic: 'switcher', resource: 'me1', event: 'switcher.operation', terminal: { kind: 'switcher-operation', me: 'me1' } });
    },
    onSuccess: (result) => {
      setOperationId(result.operationId);
      setFailureReason(undefined);
      setError(undefined);
    },
    onError: (reason: Error) => { setError(reason.message); },
  });
  const repatch = useMutation({
    mutationFn: async (request: PatchRequest) => {
      const plan = await planPatch(request);
      const refusal = plan.crosspoints.find((xp) => xp.refusal !== null && xp.refusal !== undefined)?.refusal;
      if (!plan.can_take) throw new Error(refusal?.detail ?? t`The patch cannot be taken`);
      // The tracker keys on the operation id; `kind` names the verb.
      return submitTracked(async () => ({ ...(await takePatch(request, { idempotencyKey: newIdempotencyKey() })), kind: 'patch.take' }),
        { topic: 'switcher', resource: 'me1', event: 'switcher.operation', terminal: { kind: 'switcher-operation', me: 'me1' } });
    },
    onSuccess: (result) => {
      setOperationId(result.operationId);
      setFailureReason(undefined);
      setError(undefined);
    },
    onError: (reason: Error) => { setError(reason.message); },
  });
  // Unknown (including unobserved/replay) is not permission to take again.
  const settled = operation?.phase === 'applied' || operation?.phase === 'failed';
  const ready = state.data !== undefined && (operationId === undefined || settled) && !submitting;
  // Review K5-R2-02: the arm set and the keyer drafts are built from the
  // snapshot, so after an outcome they stay gated until a snapshot read
  // AFTER the outcome arrives. The engine publishes the snapshot before the
  // tick's outcomes, so a GET started once the outcome is known reads a state
  // at or after it; `invalidateQueries` cancels any older in-flight GET and
  // starts that one. A failed refresh keeps them gated (fail closed) until a
  // later GET succeeds.
  const [refreshed, setRefreshed] = useState<{ readonly op: string; readonly ok: boolean; readonly at: number } | undefined>();
  // Review K5-R2-04: a failed refresh offers an explicit retry (a read, never
  // a mutation), so recovery needs no other switcher command or event.
  const [refreshAttempt, setRefreshAttempt] = useState(0);
  useEffect(() => {
    if (operationId === undefined || !settled) return undefined;
    let live = true;
    const started = Date.now();
    void queryClient.invalidateQueries({ queryKey: STATE_KEY }, { cancelRefetch: true }).then(() => {
      if (!live) return;
      const query = queryClient.getQueryState(STATE_KEY);
      const ok = query?.status === 'success' && query.dataUpdatedAt >= started && query.errorUpdatedAt < started;
      setRefreshed({ op: operationId, ok, at: Date.now() });
    });
    return () => { live = false; };
  }, [operationId, settled, queryClient, refreshAttempt]);
  const refreshReason = operationId === undefined || !settled ? undefined
    : refreshed?.op !== operationId ? t`Refreshing the switcher state after the last operation`
    : refreshed.ok || state.dataUpdatedAt > refreshed.at ? undefined
    : t`The switcher state could not be refreshed after the last operation; keyer and arm controls stay disabled until it is`;
  const refreshFailed = operationId !== undefined && settled && refreshed?.op === operationId
    && !refreshed.ok && state.dataUpdatedAt <= refreshed.at;
  const keyerReason = locked ? t`Takes locked — unlock in the header` : ready ? refreshReason : !state.data ? t`Switcher unavailable` : submitting ? t`A switcher request is in flight` : t`Operation in flight or outcome pending`;
  const [keyerError, setKeyerError] = useState<string | undefined>();
  const keyerMutate = useMutation({
    mutationFn: async (fields: Omit<KeyerRequest, 'me' | 'program'>) => {
      const req: KeyerRequest = { me: 'me1', program: 'main', ...fields };
      const plan = await planKeyer(req);
      if (!plan.can_take) throw new Error(plan.refusal?.detail ?? t`The keyer verb was refused`);
      return submitTracked(() => commandKeyer(fields.action, req, { idempotencyKey: newIdempotencyKey() }),
        { topic: 'switcher', resource: 'me1', event: 'switcher.operation', terminal: { kind: 'switcher-operation', me: 'me1' } });
    },
    onSuccess: (result) => { setOperationId(result.operationId); setFailureReason(undefined); setKeyerError(undefined); },
    onError: (reason: Error) => { setKeyerError(reason.message); },
  });
  const keyerVerb = (fields: Omit<KeyerRequest, 'me' | 'program'>): void => {
    if (locked) return;
    void gated(() => keyerMutate.mutateAsync(fields)).catch(() => undefined);
  };
  // `arm` (K3): the full next-transition set, planned then commanded behind
  // the same gate; the toggles light only from the engine's `armed` view.
  const [armError, setArmError] = useState<string | undefined>();
  const armMutate = useMutation({
    mutationFn: async (next: ArmSet) => {
      const req: KeyerRequest = { me: 'me1', program: 'main', action: 'arm', background: next.background, keys: [...next.keys] };
      const plan = await planKeyer(req);
      if (!plan.can_take) throw new Error(plan.refusal?.detail ?? t`The arm set was refused`);
      return submitTracked(() => commandKeyer('arm', req, { idempotencyKey: newIdempotencyKey() }),
        { topic: 'switcher', resource: 'me1', event: 'switcher.operation', terminal: { kind: 'switcher-operation', me: 'me1' } });
    },
    onSuccess: (result) => { setOperationId(result.operationId); setFailureReason(undefined); setArmError(undefined); },
    onError: (reason: Error) => {
      setArmError(reason instanceof OperationApiError && reason.detail !== undefined ? `${reason.message}: ${reason.detail}` : reason.message);
    },
  });
  const arm = (next: ArmSet): void => { if (locked) return; void gated(() => armMutate.mutateAsync(next)).catch(() => undefined); };
  const take = (intent: Intent): void => { if (locked && (typeof intent === 'string' ? intent : intent.action) !== 'select_preview') return; void gated(() => mutate.mutateAsync(intent)).catch(() => undefined); };
  // The output tick now, extrapolated from the snapshot at the exact cadence
  // so a ramp's progress advances between (change-only) snapshots.
  const ramping = (state.data?.keyers ?? []).some((row) => row.ramp !== null && row.ramp !== undefined);
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!ramping) return undefined;
    const timer = setInterval(() => { setNow(Date.now()); }, 100);
    return () => { clearInterval(timer); };
  }, [ramping]);
  const tickNow = state.data === undefined ? 0
    : extrapolatedTick(state.data.tick, state.dataUpdatedAt, Math.max(now, state.dataUpdatedAt), state.data.fps_num, state.data.fps_den);
  // A ramp that has run to its end ends without a guaranteed event: refresh once.
  const rampDone = (state.data?.keyers ?? []).some((row) => {
    const weight = keyerRampWeight(row, tickNow);
    return weight !== undefined && row.ramp !== null && row.ramp !== undefined && weight === (row.ramp.to_on ? 1 : 0);
  });
  useEffect(() => {
    if (rampDone) void queryClient.invalidateQueries({ queryKey: STATE_KEY });
  }, [rampDone, queryClient]);
  // Defensive against older backends: `keyers`/`armed` may be absent at runtime.
  const keyerRows: readonly KeyerRow[] = state.data?.keyers ?? [];
  const uskRows = keyerRows.filter(keyerIsUsk);
  const armedView: ArmedView | undefined = state.data?.armed;
  const canvas = state.data === undefined ? undefined : { w: state.data.canvas_width, h: state.data.canvas_height };
  const keyerSet = (keyer: string, fields: KeyerFields, allow: boolean): KeyerRequest => ({
    ...fields, me: 'me1', program: 'main', action: 'keyer_set', keyer, ...(allow ? { on_air: 'allow' } : {}),
  });
  return (
    <section className="space-y-5 p-4">
      <PageHeader title={t`Program / Preview switcher`} />
      {state.isError && <p role="status" className="rounded border border-amber-600 p-3">{isBackendUnavailable(state.error) ? <Trans>Switcher unavailable: this server does not provide it yet (backend not deployed). Controls are disabled.</Trans> : <Trans>Switcher unavailable: no real Program/Preview bus is attached. Controls are disabled.</Trans>}</p>}
      {state.isLoading && <p role="status"><Trans>Checking switcher engine…</Trans></p>}
      {error !== undefined && <p role="alert" className="text-destructive">{error}</p>}
      {operationId !== undefined && <OperationFeedback
        phase={operation?.phase === 'accepted' ? 'pending' : operation?.phase ?? 'unknown'}
        operationLabel={t`Switcher`}
        correlationId={operationId}
        failureReason={failureReason ?? t`Switcher operation rejected`}
        unknownReason={t`Outcome unknown; refresh the engine state before taking again`}
      />}
      {state.data !== undefined && <p role="status">PGM: {state.data.program_source} · PVW: {state.data.preview_source} · {t`Tick`} {state.data.tick}{state.data.in_flight ? ` · ${t`Transition running`}` : ''}</p>}
      <div className="grid gap-4 md:grid-cols-2">
        <SwitcherMonitor bus="PGM" path="/api/v1/preview/program.jpg" />
        <SwitcherMonitor bus="PVW" path="/api/v1/preview/switcher/me1/preview.jpg" />
      </div>
      {mixer !== undefined && <MixerInputBank
        mixer={mixer}
        sources={sources.data ?? []}
        disabled={!ready}
        takesLocked={locked}
        onSelect={(action, input) => { take({ action, input }); }}
        onRepatch={async (request) => { if (locked) throw new Error(t`Takes locked — unlock in the header`); await gated(() => repatch.mutateAsync(request)).catch(() => undefined); }}
      />}
      {mixer === undefined && <div className="flex flex-wrap items-end gap-3">
        <div className="space-y-1"><Label htmlFor="switcher-source"><Trans>Source</Trans></Label>
          <select id="switcher-source" className="rounded border bg-background p-2" value={selected} disabled={!ready} onChange={(event) => { setSelected(event.target.value); }}>
            <option value="">{t`Choose a source`}</option>
            {sources.data?.map((source) => <option key={source.id} value={source.id}>{source.name}</option>)}
          </select>
        </div>
        <Button disabled={!ready || selected === ''} onClick={() => { take('select_preview'); }}><Trans>Preview selected source</Trans></Button>
        <Button disabled={!ready || locked || selected === ''} onClick={() => { take('select_program'); }}><Trans>Send selected source to air</Trans></Button>
      </div>}
      {locked && <p role="status"><Trans>Takes are locked. Use the Takes locked control in the header to unlock. A confirmed engine state is also required.</Trans></p>}
      <div className="flex flex-wrap items-end gap-3 rounded-lg border p-3">
        <Button disabled={!ready || locked} onClick={() => { take('cut'); }}><Trans>Cut preview to air</Trans></Button>
        <div className="space-y-1"><Label htmlFor="switcher-duration"><Trans>Auto frames</Trans></Label><Input id="switcher-duration" type="number" min="1" max="1000" value={duration} onChange={(event) => { setDuration(event.target.value); }} /></div>
        <Button disabled={!ready || locked || !Number.isInteger(Number(duration)) || Number(duration) < 1} onClick={() => { take('auto'); }}><Trans>Transition preview to air</Trans></Button>
      </div>
      {refreshFailed && (
        <div role="alert" className="flex items-center gap-2" data-testid="switcher-refresh-failed">
          <span>{refreshReason}</span>
          <Button variant="outline" onClick={() => { setRefreshed(undefined); setRefreshAttempt((n) => n + 1); }}>
            <Trans>Retry reading the switcher state</Trans>
          </Button>
        </div>
      )}
      {/* A backend without upstream keyers (pre-K3) sends no `armed` view and no USK rows: no strip. */}
      {armedView !== undefined && uskRows.length > 0 && (
        <>
          <ArmStrip
            armed={armedView}
            keys={uskRows}
            disabledReason={keyerReason ?? (state.data?.in_flight === true ? t`The arm set cannot change during a transition` : undefined)}
            onArm={arm}
          />
          {armError !== undefined && <p role="alert" className="text-destructive" data-testid="arm-error">{armError}</p>}
        </>
      )}
      {(state.data?.keyers ?? []).length > 0 && canvas !== undefined && (
        <section className="space-y-3" aria-label={t`Keyers`}>
          <h2 className="font-semibold"><Trans>Keyers</Trans></h2>
          {stream !== 'open' && (
            <p role="status" className="rounded border border-amber-600 p-2" data-testid="keyer-stream-stale">
              <Trans>Realtime updates are not connected: keyer lamps and tally may be stale.</Trans>
            </p>
          )}
          {(state.data?.keyers ?? []).map((row) => (
            <KeyerStrip
              key={keyerDomKey(row)}
              row={row}
              tick={tickNow}
              disabledReason={keyerReason}
              transitionInFlight={state.data?.in_flight === true}
              onTie={(tie) => { keyerVerb({ action: 'keyer_tie', keyer: row.id, tie }); }}
              onCut={() => { keyerVerb({ action: 'keyer_cut', keyer: row.id, on: !row.on_air, ...(keyerIsUsk(row) ? { family: 'usk' as const } : {}) }); }}
              onAuto={(rateFrames) => { keyerVerb({ action: 'keyer_auto', keyer: row.id, on: !row.on_air, duration_frames: rateFrames }); }}
            />
          ))}
          {keyerError !== undefined && <p role="alert" className="text-destructive">{keyerError}</p>}
          {(state.data?.keyers ?? []).map((row) => (
            <KeyerPanel
              key={`panel-${keyerDomKey(row)}`}
              row={row}
              sources={sources.data ?? []}
              disabledReason={keyerReason}
              canvas={canvas}
              onPlan={async (fields) => gated(() => planKeyer(keyerSet(row.id, fields, false)))}
              onApply={async (fields, allow) => {
                if (locked) throw new Error(t`Takes locked — unlock in the header`);
                // The reviewed draft, submitted as reviewed; refusals reject.
                const result = await gated(() => submitTracked(() => commandKeyer('keyer_set', keyerSet(row.id, fields, allow), { idempotencyKey: newIdempotencyKey() }),
                  { topic: 'switcher', resource: 'me1', event: 'switcher.operation', terminal: { kind: 'switcher-operation', me: 'me1' } }));
                if (result === undefined) throw new Error(t`Another switcher request is in flight`);
                setOperationId(result.operationId);
                setFailureReason(undefined);
              }}
            />
          ))}
        </section>
      )}
    </section>
  );
}
