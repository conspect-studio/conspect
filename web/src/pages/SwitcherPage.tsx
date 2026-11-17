// Program/Preview switcher: all controls are disabled until a real dual-bus
// engine publishes state. Accepted operations are never labelled applied.
import { useEffect, useState } from 'react';
import type { JSX } from 'react';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { Trans, useLingui } from '@lingui/react/macro';
import { PageHeader } from '../components/PageHeader';
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
import { useJpegPreview } from '../preview/useJpegPreview';
import { getPatch, planPatch, takePatch } from '../api/patch';
import type { PatchRequest } from '../api/patch';
import { MixerInputBank } from '../components/switcher/MixerInputBank';
import { commandKeyer, planKeyer } from '../api/keyers';
import type { KeyerRequest } from '../api/keyers';
import { KeyerStrip } from '../components/switcher/KeyerStrip';
import { KeyerPanel } from '../components/switcher/KeyerPanel';

const STATE_KEY = ['switcher', 'me1'] as const;
const PATCH_KEY = ['patch'] as const;
/** A select or take; a select by mixer input carries `input`, not `source`. */
type Intent = SwitcherAction | { readonly action: 'select_preview' | 'select_program'; readonly input: number };
function SwitcherMonitor({ bus, path }: { readonly bus: string; readonly path: string }): JSX.Element {
  const src = useJpegPreview(path, 1000, bus === 'PGM');
  return (
    <section className="space-y-2 rounded-md border p-3" aria-label={bus}>
      <h2 className="font-semibold">{bus}</h2>
      {src === undefined
        ? <div role="img" aria-label={`${bus} unavailable`} className="flex aspect-video items-center justify-center bg-black text-sm text-white"><Trans>Waiting for a real frame</Trans></div>
        : <img src={src} alt={`${bus} monitor`} className="aspect-video w-full bg-black object-contain" />}
    </section>
  );
}

/** First usable M/E when the backend is wired, honest unavailable panel otherwise. */
export function SwitcherPage(): JSX.Element {
  const { t } = useLingui();
  const queryClient = useQueryClient();
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
  useEffect(() => subscribeRealtime('switcher', (envelope) => {
    if (envelope.t !== 'switcher.operation') return;
    // Display-only reason: the shared tracker, not this page, decides the phase.
    const evidence = switcherOperationEvidence(envelope);
    if (evidence !== undefined && evidence.corr === operationId && evidence.resource === 'me1' && evidence.verdict === 'failed') {
      setFailureReason(evidence.reason);
    }
    void queryClient.invalidateQueries({ queryKey: STATE_KEY });
    void queryClient.invalidateQueries({ queryKey: PATCH_KEY });
  }, undefined, () => {
    void queryClient.invalidateQueries({ queryKey: STATE_KEY });
    void queryClient.invalidateQueries({ queryKey: PATCH_KEY });
  }), [queryClient, operationId]);
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
  const ready = state.data !== undefined && (operationId === undefined || settled) && !mutate.isPending && !repatch.isPending;
  const keyerReason = ready ? undefined : !state.data ? t`Switcher unavailable` : t`Operation in flight or outcome pending`;
  const [keyerError, setKeyerError] = useState<string | undefined>();
  const keyerMutate = useMutation({
    mutationFn: async (fields: Omit<KeyerRequest, 'me' | 'program'>) => {
      const { action, ...rest } = fields;
      const req: KeyerRequest = { me: 'me1', program: 'main', action, ...rest };
      const plan = await planKeyer(req);
      if (!plan.can_take) throw new Error(plan.refusal?.detail ?? t`The keyer verb was refused`);
      return submitTracked(() => commandKeyer(action, req, { idempotencyKey: newIdempotencyKey() }),
        { topic: 'switcher', resource: 'me1', event: 'switcher.operation', terminal: { kind: 'switcher-operation', me: 'me1' } });
    },
    onSuccess: (result) => { setOperationId(result.operationId); setFailureReason(undefined); setKeyerError(undefined); },
    onError: (reason: Error) => { setKeyerError(reason.message); },
  });
  // Canvas size is not carried by the switcher API; the operator sets it once for fly presets.
  const [canvas] = useState({ w: 1920, h: 1080 });
  return (
    <main className="space-y-5 p-4">
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
        onSelect={(action, input) => { mutate.mutate({ action, input }); }}
        onRepatch={async (request) => { await repatch.mutateAsync(request).catch(() => undefined); }}
      />}
      {mixer === undefined && <div className="flex flex-wrap items-end gap-3">
        <div className="space-y-1"><Label htmlFor="switcher-source"><Trans>Source</Trans></Label>
          <select id="switcher-source" className="rounded border bg-background p-2" value={selected} disabled={!ready} onChange={(event) => { setSelected(event.target.value); }}>
            <option value="">{t`Choose a source`}</option>
            {sources.data?.map((source) => <option key={source.id} value={source.id}>{source.name}</option>)}
          </select>
        </div>
        <Button disabled={!ready || selected === ''} onClick={() => { mutate.mutate('select_preview'); }}><Trans>Select PVW</Trans></Button>
        <Button disabled={!ready || selected === ''} onClick={() => { mutate.mutate('select_program'); }}><Trans>Punch PGM</Trans></Button>
      </div>}
      <div className="flex flex-wrap items-end gap-3">
        <Button disabled={!ready} onClick={() => { mutate.mutate('cut'); }}><Trans>Cut</Trans></Button>
        <div className="space-y-1"><Label htmlFor="switcher-duration"><Trans>Auto frames</Trans></Label><Input id="switcher-duration" type="number" min="1" max="1000" value={duration} onChange={(event) => { setDuration(event.target.value); }} /></div>
        <Button disabled={!ready || !Number.isInteger(Number(duration)) || Number(duration) < 1} onClick={() => { mutate.mutate('auto'); }}><Trans>Auto</Trans></Button>
      </div>
      {/*
        TODO(#388): the BKGD/KEY 1-4 arm strip is built behind the presence of
        USK rows/an `arm` verb in the generated client. K3 (upstream keyers)
        is not merged — `components['schemas']` has no `arm` field as of this
        commit — so this renders nothing rather than inventing API types.
      */}
      {(state.data?.keyers ?? []).length > 0 && (
        <section className="space-y-3" aria-label={t`Downstream keyers`}>
          <h2 className="font-semibold"><Trans>Downstream keyers</Trans></h2>
          {(state.data?.keyers ?? []).map((row) => (
            <KeyerStrip
              key={row.id}
              row={row}
              tick={state.data?.tick ?? 0}
              disabledReason={keyerReason}
              onTie={(tie) => { keyerMutate.mutate({ action: 'keyer_tie', keyer: row.id, tie }); }}
              onCut={() => { keyerMutate.mutate({ action: 'keyer_cut', keyer: row.id, on: !row.on_air }); }}
              onAuto={(rateFrames) => { keyerMutate.mutate({ action: 'keyer_auto', keyer: row.id, on: !row.on_air, duration_frames: rateFrames }); }}
            />
          ))}
          {keyerError !== undefined && <p role="alert" className="text-destructive">{keyerError}</p>}
          {(state.data?.keyers ?? []).map((row) => (
            <KeyerPanel
              key={`panel-${row.id}`}
              row={row}
              sources={sources.data ?? []}
              disabledReason={keyerReason}
              canvas={canvas}
              onPlan={async (fields) => planKeyer({ me: 'me1', program: 'main', action: 'keyer_set', keyer: row.id, ...fields })}
              onApply={async (fields, allow) => {
                const req: KeyerRequest = { me: 'me1', program: 'main', action: 'keyer_set', keyer: row.id, ...fields, ...(allow ? { on_air: 'allow' } : {}) };
                const plan = await planKeyer(req);
                if (!plan.can_take) throw new Error(plan.refusal?.detail ?? t`The keyer_set was refused`);
                const result = await submitTracked(() => commandKeyer('keyer_set', req, { idempotencyKey: newIdempotencyKey() }),
                  { topic: 'switcher', resource: 'me1', event: 'switcher.operation', terminal: { kind: 'switcher-operation', me: 'me1' } });
                setOperationId(result.operationId);
                setFailureReason(undefined);
              }}
            />
          ))}
        </section>
      )}
      <p className="text-xs text-muted-foreground"><Trans>Aux buses, multi-M/E and compositions as sources remain future switcher stages. Upstream keyers (USK) and the BKGD/KEY arm strip arrive with #388.</Trans></p>
    </main>
  );
}
