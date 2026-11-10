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
import { newIdempotencyKey } from '../api/operations';
import { useJpegPreview } from '../preview/useJpegPreview';

const STATE_KEY = ['switcher', 'me1'] as const;
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
  }, undefined, () => {
    void queryClient.invalidateQueries({ queryKey: STATE_KEY });
  }), [queryClient, operationId]);
  const mutate = useMutation({
    mutationFn: async (action: SwitcherAction) => {
      const source = action === 'select_program' || action === 'select_preview' ? selected : undefined;
      const durationFrames = action === 'auto' ? Number(duration) : undefined;
      const req: SwitcherRequest = {
        me: 'me1', program: 'main', action,
        ...(source === undefined ? {} : { source }),
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
  // Unknown (including unobserved/replay) is not permission to take again.
  const settled = operation?.phase === 'applied' || operation?.phase === 'failed';
  const ready = state.data !== undefined && (operationId === undefined || settled) && !mutate.isPending;
  return (
    <main className="space-y-5 p-4">
      <PageHeader title={t`Program / Preview switcher`} />
      {state.isError && <p role="status" className="rounded border border-amber-600 p-3"><Trans>Switcher unavailable: no real Program/Preview bus is attached. Controls are disabled.</Trans></p>}
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
      <div className="flex flex-wrap items-end gap-3">
        <div className="space-y-1"><Label htmlFor="switcher-source"><Trans>Source</Trans></Label>
          <select id="switcher-source" className="rounded border bg-background p-2" value={selected} disabled={!ready} onChange={(event) => { setSelected(event.target.value); }}>
            <option value="">{t`Choose a source`}</option>
            {sources.data?.map((source) => <option key={source.id} value={source.id}>{source.name}</option>)}
          </select>
        </div>
        <Button disabled={!ready || selected === ''} onClick={() => { mutate.mutate('select_preview'); }}><Trans>Select PVW</Trans></Button>
        <Button disabled={!ready || selected === ''} onClick={() => { mutate.mutate('select_program'); }}><Trans>Punch PGM</Trans></Button>
        <Button disabled={!ready} onClick={() => { mutate.mutate('cut'); }}><Trans>Cut</Trans></Button>
        <div className="space-y-1"><Label htmlFor="switcher-duration"><Trans>Auto frames</Trans></Label><Input id="switcher-duration" type="number" min="1" max="1000" value={duration} onChange={(event) => { setDuration(event.target.value); }} /></div>
        <Button disabled={!ready || !Number.isInteger(Number(duration)) || Number(duration) < 1} onClick={() => { mutate.mutate('auto'); }}><Trans>Auto</Trans></Button>
      </div>
      <p className="text-xs text-muted-foreground"><Trans>Keyers, aux buses, multi-M/E and compositions as sources remain future switcher stages.</Trans></p>
    </main>
  );
}
