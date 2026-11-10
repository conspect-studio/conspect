import { deskControlReason, tallyForSource } from './deskSafety';
import { useEffect, useMemo, useRef, useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { Link } from 'react-router-dom';
import { commandSwitcher, getSwitcher, planSwitcher } from '../../api/switcher';
import type { SwitcherAction, SwitcherRequest } from '../../api/switcher';
import { newIdempotencyKey } from '../../api/operations';
import { createApiClient } from '../../api/client';
import { useLayouts } from '../../api/queries';
import { useAlarms } from '../../api/alarmsQueries';
import { useSources } from '../../resources/queries';
import { subscribeRealtime } from '../../realtime/shared';
import { useEngineEvents, TILES_QUERY_KEY } from '../../realtime/useEngineEvents';
import type { LiveTile } from '../../realtime/useEngineEvents';
import { switcherOperationEvidence } from '../../realtime/operationEvidence';
import { usePreviewCapabilities } from '../../preview/capabilities';
import { PreviewSurface } from '../../preview/PreviewSurface';
import { useJpegPreview } from '../../preview/useJpegPreview';
import { Dialog, DialogContent, DialogTitle } from '../ui/dialog';
import { useOperationFeedback, useSubmitTrackedOperation } from './useOperationFeedback';
import { OperationFeedback } from './OperationFeedback';
import { useTakeLock } from './BroadcastShell';

const STATE_KEY = ['switcher', 'me1'] as const;
function PreviewMonitor({ program, source, fresh }: { readonly program: boolean; readonly source: string | undefined; readonly fresh: boolean }): JSX.Element {
  const { t } = useLingui();
  const caps = usePreviewCapabilities();
  const pvw = useJpegPreview('/api/v1/preview/switcher/me1/preview.jpg', 1000, !program);
  return <figure className={`bd-monitor ${program ? 'bd-pgm' : 'bd-pvw'}`} data-tally={fresh && source ? (program ? 'program' : 'preview') : 'unknown'}>
    <div className="bd-picture">{program ? <PreviewSurface scope="program" whepEndpoint="/api/v1/preview/program/whep" jpegPath="/api/v1/preview/program.jpg" label={t`Program monitor`} capabilities={caps.data} className="bd-video" /> : pvw ? <img src={pvw} alt={t`Preview monitor`} /> : <span><Trans>Waiting for a real frame</Trans></span>}</div>
    <figcaption><b>{program ? t`● PROGRAM` : t`● PREVIEW`}</b><span>{source ?? t`Unknown source`}</span><small>{fresh ? t`Engine-confirmed bus` : t`Unconfirmed / stale`}</small></figcaption>
    {program && <div className="bd-meter-note"><Trans>Audio meters unavailable — no measured level contract</Trans></div>}
  </figure>;
}

function SourcePicture({ id }: { readonly id: string }): JSX.Element {
  const { t } = useLingui();
  const frame = useJpegPreview(`/api/v1/preview/inputs/${encodeURIComponent(id)}.jpg`, 2000);
  return frame ? <img className="bd-source-picture" src={frame} alt={t`Source still preview`} /> : <span><Trans>No confirmed frame</Trans></span>;
}

/** Real bus state and correlated commands inside the approved desk composition. */
export function ProductionDesk(): JSX.Element {
  const { t } = useLingui();
  const qc = useQueryClient();
  const { status } = useEngineEvents();
  const { locked } = useTakeLock();
  const state = useQuery({ queryKey: STATE_KEY, queryFn: () => getSwitcher(), retry: false, refetchInterval: 2000 });
  const sources = useSources();
  // Existing alarm contract: active system alarms only, not a whole-engine health verdict.
  const alarms = useAlarms({ active: true, scope: 'system' });
  const client = useMemo(() => createApiClient(), []);
  const layouts = useLayouts(client);
  const tiles = useQuery<Record<string, LiveTile>>({ queryKey: TILES_QUERY_KEY, queryFn: (): Record<string, LiveTile> => ({}), enabled: false, initialData: {} });
  const [operationId, setOperationId] = useState<string>();
  const operation = useOperationFeedback(operationId);
  const submit = useSubmitTrackedOperation();
  const [error, setError] = useState<string>();
  const [failure, setFailure] = useState<string>();
  const [confirm, setConfirm] = useState<{ action: SwitcherAction; source?: string }>();
  const [frames, setFrames] = useState('25');
  const [now, setNow] = useState(Date.now);
  const [lastTickAt, setLastTickAt] = useState(0);
  const submitting = useRef(false);
  useEffect(() => {
    const timer = setTimeout(() => { if (state.data?.tick !== undefined) setLastTickAt(Date.now()); }, 0);
    return () => { clearTimeout(timer); };
  }, [state.data?.tick]);
  useEffect(() => { const timer = setInterval(() => { setNow(Date.now()); }, 1000); return () => { clearInterval(timer); }; }, []);
  useEffect(() => subscribeRealtime('switcher', (envelope) => {
    if (envelope.t !== 'switcher.operation') return;
    const evidence = switcherOperationEvidence(envelope);
    if (evidence !== undefined && evidence.corr === operationId && evidence.resource === 'me1' && evidence.verdict === 'failed') setFailure(evidence.reason);
    void qc.invalidateQueries({ queryKey: STATE_KEY });
  }, undefined, () => { void qc.invalidateQueries({ queryKey: STATE_KEY }); }), [qc, operationId]);
  const settled = operation?.phase === 'applied' || operation?.phase === 'failed';
  const fresh = !state.isError && state.dataUpdatedAt > 0 && lastTickAt > 0 && now - state.dataUpdatedAt < 6000 && now - lastTickAt < 6000;
  const reason = deskControlReason(!!state.data, status === 'open', fresh, state.data?.in_flight ?? false, (operationId !== undefined && !settled));
  const mutate = useMutation({ mutationFn: async ({ action, source }: { action: SwitcherAction; source?: string }) => {
    if (reason) throw new Error(reason);
    const req: SwitcherRequest = { me: 'me1', program: 'main', action, ...(source ? { source } : {}), ...(action === 'auto' ? { duration_frames: Number(frames) } : {}) };
    const plan = await planSwitcher(req);
    if (!plan.can_take) throw new Error(t`The engine refused this plan`);
    return submit(() => commandSwitcher(action, req, { idempotencyKey: newIdempotencyKey() }), { topic: 'switcher', resource: 'me1', event: 'switcher.operation', terminal: { kind: 'switcher-operation', me: 'me1' } });
  }, onSuccess: (result) => { setOperationId(result.operationId); setError(undefined); setFailure(undefined); }, onError: (err: Error) => { setError(err.message); } });
  const disabled = !!reason || mutate.isPending;
  const send = (command: { action: SwitcherAction; source?: string }): void => {
    if (disabled || submitting.current) return;
    submitting.current = true;
    mutate.mutate(command, { onSettled: () => { submitting.current = false; } });
  };
  const request = (action: SwitcherAction, source?: string): void => {
    if (disabled || submitting.current) return;
    const command = { action, ...(source ? { source } : {}) };
    if (action === 'select_program' || (locked && (action === 'cut' || action === 'auto'))) setConfirm(command);
    else send(command);
  };
  const name = (id: string | undefined): string | undefined => sources.data?.find((s) => s.id === id)?.name ?? id;
  return <section className="production-desk" aria-label={t`Production desk`}>
    <div className="bd-heading"><div><h1><Trans>Production desk</Trans></h1><p><Trans>Switch PGM/PVW, watch the wall, feel the tally.</Trans></p></div><div className="bd-tick"><span><Trans>ENGINE TICK</Trans></span><b>{state.data?.tick ?? '—'}</b></div></div>
    <div className="bd-operation" role="status">{reason ? <span>{t`Controls disabled`}: {reason}</span> : <span><Trans>Engine state confirmed · takes follow the lock</Trans></span>}{error && <p role="alert">{error}</p>}{operationId && <OperationFeedback phase={operation?.phase === 'accepted' ? 'pending' : operation?.phase ?? 'unknown'} operationLabel={t`Switcher`} correlationId={operationId} failureReason={failure ?? t`Switcher operation rejected`} unknownReason={t`Outcome unknown; controls remain blocked until a correlated terminal event`} />}</div>
    <div className="bd-desk-grid">
      <PreviewMonitor program source={name(state.data?.program_source)} fresh={fresh && status === 'open'} />
      <PreviewMonitor program={false} source={name(state.data?.preview_source)} fresh={fresh && status === 'open'} />
      <section className="bd-switcher"><h2><Trans>M/E 1 — Switcher</Trans></h2>
        <div className="bd-buses">{(['preview', 'program'] as const).map((bus) => <div className={`bd-bus bd-bus-${bus}`} key={bus} role="group" aria-label={bus === 'program' ? t`PGM bus — direct taps always confirm` : t`PVW bus`}><b>{bus === 'program' ? 'PGM' : 'PVW'}</b><div>{sources.data?.map((source) => <button key={source.id} disabled={disabled} title={reason ? `${source.name} — ${reason}` : source.name} aria-pressed={tallyForSource(source.id, state.data, fresh && status === 'open') === bus} onClick={() => { request(bus === 'program' ? 'select_program' : 'select_preview', source.id); }}>{source.name}</button>)}{!sources.data?.length && <span><Trans>No sources available</Trans></span>}</div></div>)}</div>
        <div className="bd-takes"><div className="bd-transitions" role="group" aria-label={t`Transition type — unavailable`}><button disabled aria-pressed="true" title={t`AUTO unavailable: the current engine rejects AUTO with HTTP 422`}>MIX</button><button disabled title={t`WIPE and DIP have no supported control contract`}>WIPE</button><button disabled title={t`WIPE and DIP have no supported control contract`}>DIP</button></div><button className="bd-auto" disabled title={t`AUTO unavailable: the current engine rejects AUTO with HTTP 422`} onClick={() => { request('auto'); }}>AUTO</button><button className="bd-cut" disabled={disabled} onClick={() => { request('cut'); }}>CUT</button><label><Trans>AUTO frames</Trans><input aria-label={t`Auto frames`} value={frames} type="number" min="1" max="1000" disabled onChange={(e) => { setFrames(e.target.value); }} /></label></div>
        <p className="bd-unsupported"><Trans>AUTO unavailable: the current engine rejects AUTO with HTTP 422.</Trans></p><details><summary><Trans>More keys — unavailable</Trans></summary><p><Trans>WIPE, DIP, USK, DSK and AUX have no supported control contract.</Trans></p></details>
      </section>
      <section className="bd-wall"><h2><Trans>Multiview wall</Trans></h2><div className="bd-wall-grid">{sources.data?.map((source) => <div className="bd-wall-tile" key={source.id} data-tally={tallyForSource(source.id, state.data, fresh && status === 'open')}><SourcePicture id={source.id} /><small>{tallyForSource(source.id, state.data, fresh && status === 'open').toUpperCase()}</small><b>{source.name}</b><span><Trans>Signal unconfirmed</Trans></span></div>)}</div><p>{sources.isError ? t`Source catalog unavailable` : !sources.data?.length ? t`No source catalog available` : t`Tally follows the engine bus IDs; source presence does not prove signal.`}</p><p><Trans>Layouts</Trans>: {layouts.isError ? t`Unavailable` : layouts.data?.length ?? '—'} · <Trans>Observed tiles</Trans>: {Object.keys(tiles.data).length} · <Link to="/monitoring"><Trans>Open live monitoring</Trans></Link></p></section>
      <aside className="bd-rail"><section><h2><Trans>Tally legend</Trans></h2><p className="bd-red"><Trans>● Red — PROGRAM</Trans></p><p className="bd-amber"><Trans>● Amber — PREVIEW</Trans></p><p><Trans>○ Dark — neither bus</Trans></p><small><Trans>Tally is separate from signal and output transmission.</Trans></small></section><section><h2><Trans>Alarms & engine health</Trans></h2><div className="bd-alarm-summary" role="region" aria-label={t`Active system alarm summary`}><p><Trans>Active system alarms only</Trans></p>{alarms.isError ? <p><Trans>Alarm summary unavailable</Trans></p> : alarms.isPending ? <p><Trans>Loading alarm summary</Trans></p> : alarms.data.length === 0 ? <p><Trans>No active system alarms reported</Trans></p> : <ul>{alarms.data.slice(0, 3).map((alarm) => <li key={alarm.id} title={`${alarm.severity} · ${alarm.kind}`}><span>{alarm.severity}</span> · {alarm.kind}</li>)}</ul>}</div><Link to="/alarms"><Trans>Open alarms</Trans></Link></section></aside>
    </div>
    <Dialog open={confirm !== undefined} onOpenChange={(open) => { if (!open) setConfirm(undefined); }}><DialogContent><DialogTitle><Trans>Confirm program change</Trans></DialogTitle><p>{confirm?.action.toUpperCase()} · {name(state.data?.program_source) ?? t`Unknown`} → {name(confirm?.source ?? state.data?.preview_source) ?? t`Unknown`}</p><p><Trans>The program bus changes what viewers see. Acceptance is not completion.</Trans></p><button disabled={disabled} onClick={() => { if (confirm) send(confirm); setConfirm(undefined); }}><Trans>Confirm take</Trans></button><button onClick={() => { setConfirm(undefined); }}><Trans>Cancel</Trans></button></DialogContent></Dialog>
  </section>;
}
