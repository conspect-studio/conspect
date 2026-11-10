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
import { useEngineEvents } from '../../realtime/useEngineEvents';
import type { LiveTile } from '../../realtime/useEngineEvents';
import { switcherOperationEvidence } from '../../realtime/operationEvidence';
import { tileForSource, useLiveTiles } from '../../resources/useLiveTiles';
import { meterFillPercent, useAudioMeter } from '../../realtime/useAudioMeter';
import { usePreviewCapabilities, whepAvailable } from '../../preview/capabilities';
import { PreviewSurface } from '../../preview/PreviewSurface';
import { useJpegPreview } from '../../preview/useJpegPreview';
import { Dialog, DialogContent, DialogTitle } from '../ui/dialog';
import { useOperationFeedback, useSubmitTrackedOperation } from './useOperationFeedback';
import { OperationFeedback } from './OperationFeedback';
import { useTakeLock } from './BroadcastShell';
import { tickConfirmed, useTickAdvancedAt } from './tickObservation';

const STATE_KEY = ['switcher', 'me1'] as const;
const SENDING = '\u0000sending';
function PreviewMonitor({ program, source, fresh }: { readonly program: boolean; readonly source: string | undefined; readonly fresh: boolean }): JSX.Element {
  const { t } = useLingui();
  const caps = usePreviewCapabilities();
  const pvw = useJpegPreview('/api/v1/preview/switcher/me1/preview.jpg', 1000, !program);
  const still = program ? !whepAvailable(caps.data, 'program') : pvw !== undefined;
  return <figure className={`bd-monitor ${program ? 'bd-pgm' : 'bd-pvw'}`} data-tally={fresh && source ? (program ? 'program' : 'preview') : 'unknown'}>
    <div className="bd-picture">{program ? <PreviewSurface scope="program" whepEndpoint="/api/v1/preview/program/whep" jpegPath="/api/v1/preview/program.jpg" label={t`Program monitor`} capabilities={caps.data} className="bd-video" /> : pvw ? <div className="bd-jpeg"><img src={pvw} alt={t`Preview monitor`} /></div> : <span><Trans>Waiting for a frame…</Trans></span>}</div>
    <figcaption><b>{program ? (fresh && source ? t`● ON AIR — PROGRAM` : t`○ PROGRAM — unconfirmed`) : (fresh && source ? t`▲ PREVIEW` : t`△ PREVIEW — unconfirmed`)}</b><span>{source ?? t`Unknown source`}</span><small>{fresh ? t`Engine-confirmed bus` : t`Unconfirmed / stale`}{still ? t` · Still preview (~1 fps), live video unavailable` : ''}</small></figcaption>
    {program && <ProgramLevels />}
  </figure>;
}

/** Program L/R levels strictly from the audio.meter telemetry; otherwise explicitly unavailable. */
function ProgramLevels(): JSX.Element {
  const { current } = useAudioMeter();
  if (current === undefined || current.peak_db.length === 0) return <div className="bd-meter-note" data-testid="desk-meters-unavailable"><Trans>Audio meters unavailable — no measured level telemetry</Trans></div>;
  return <ul className="bd-meters" aria-label="Program meters" data-testid="desk-meters">{current.peak_db.map((db, i) => <li key={i}><span>{current.peak_db.length === 2 ? (i === 0 ? 'L' : 'R') : String(i + 1)}</span><i aria-hidden="true"><em style={{ width: `${String(meterFillPercent(db))}%` }} /></i><b>{db.toFixed(1)} dBFS</b></li>)}</ul>;
}

/** Truthful wall label from the live tile lifecycle; no tile means unconfirmed. */
function TileLabel({ tile }: { readonly tile: LiveTile | undefined }): JSX.Element {
  if (!tile) return <Trans>Signal unconfirmed</Trans>;
  return tile.state === 'LIVE' ? <Trans>Live</Trans> : tile.state === 'RECONNECTING' ? <Trans>Starting</Trans> : tile.state === 'STALE' ? <Trans>Stale</Trans> : <Trans>No signal</Trans>;
}

/** Tally text + glyph (never colour alone); unknown stays unknown. */
function tallyGlyph(tally: string): string {
  return tally === 'program' ? '● PGM' : tally === 'preview' ? '▲ PVW' : tally === 'dark' ? '○ OFF' : '? UNKNOWN';
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
  const tiles = useLiveTiles();
  // The tile cache is seeded {} before any snapshot; an empty map means no snapshot yet, not zero tiles.
  const tilesKnown = tiles.size > 0;
  const [operationId, setOperationId] = useState<string>();
  const operation = useOperationFeedback(operationId);
  const submit = useSubmitTrackedOperation();
  const [error, setError] = useState<string>();
  const [failure, setFailure] = useState<string>();
  const [confirm, setConfirm] = useState<{ action: SwitcherAction; source?: string }>();
  const [frames, setFrames] = useState('25');
  const [help, setHelp] = useState(false);
  const [now, setNow] = useState(Date.now);
  // 'sending' from click until the tracker record is terminal; then the operation id.
  const [guard, setGuard] = useState<string>();
  const advancedAt = useTickAdvancedAt(qc, STATE_KEY.join('/'), state.data?.tick, state.dataUpdatedAt);
  useEffect(() => { const timer = setInterval(() => { setNow(Date.now()); }, 1000); return () => { clearInterval(timer); }; }, []);
  useEffect(() => subscribeRealtime('switcher', (envelope) => {
    if (envelope.t !== 'switcher.operation') return;
    const evidence = switcherOperationEvidence(envelope);
    if (evidence !== undefined && evidence.corr === operationId && evidence.resource === 'me1' && evidence.verdict === 'failed') setFailure(evidence.reason);
    void qc.invalidateQueries({ queryKey: STATE_KEY });
  }, undefined, () => { void qc.invalidateQueries({ queryKey: STATE_KEY }); }), [qc, operationId]);
  const settled = operation?.phase === 'applied' || operation?.phase === 'failed';
  // Derived: in flight from click until the record for THIS command is terminal.
  const inFlight = guard !== undefined && !(guard !== SENDING && guard === operationId && settled);
  // Fresh = the engine tick was observed ADVANCING and the poll is recent. Socket-open and
  // poll/refetch time alone never prove the program is on air.
  const fresh = tickConfirmed(advancedAt, state.dataUpdatedAt, now, state.isError);
  const baseReason = deskControlReason(!!state.data, status === 'open', fresh, state.data?.in_flight ?? false, operationId !== undefined && !settled);
  const reason = baseReason ?? (inFlight ? 'Command in flight' : undefined);
  const mutate = useMutation({ mutationFn: async ({ action, source }: { action: SwitcherAction; source?: string }) => {
    if (!lock.current) throw new Error('Command guard not held');
    if (baseReason) throw new Error(baseReason);
    const req: SwitcherRequest = { me: 'me1', program: 'main', action, ...(source ? { source } : {}), ...(action === 'auto' ? { duration_frames: Number(frames) } : {}) };
    const plan = await planSwitcher(req);
    if (!plan.can_take) throw new Error(t`The engine refused this plan`);
    return submit(() => commandSwitcher(action, req, { idempotencyKey: newIdempotencyKey() }), { topic: 'switcher', resource: 'me1', event: 'switcher.operation', terminal: { kind: 'switcher-operation', me: 'me1' } });
  }, onSuccess: (result) => { setOperationId(result.operationId); setGuard(result.operationId); setError(undefined); setFailure(undefined); }, onError: (err: Error) => { setError(err.message); setGuard(undefined); lock.current = false; } });
  const disabled = !!reason || mutate.isPending || inFlight;
  // Synchronous guard: state is not committed between two same-turn clicks, so a ref gates the second.
  // Held from before mutate until the command is terminal; released by the effect below or on error.
  const lock = useRef(false);
  const send = (command: { action: SwitcherAction; source?: string }): void => {
    if (lock.current || disabled) return;
    lock.current = true;
    setGuard(SENDING);
    mutate.mutate(command);
  };
  const request = (action: SwitcherAction, source?: string): void => {
    if (lock.current || disabled) return;
    const command = { action, ...(source ? { source } : {}) };
    if (action === 'select_program' || (locked && (action === 'cut' || action === 'auto'))) setConfirm(command);
    else send(command);
  };
  useEffect(() => { if (!inFlight && !mutate.isPending) lock.current = false; }, [inFlight, mutate.isPending]);
  const name = (id: string | undefined): string | undefined => sources.data?.find((s) => s.id === id)?.name ?? id;
  return <section className="production-desk" aria-label={t`Production desk`}>
    <div className="bd-heading"><div><h1><Trans>Production desk</Trans></h1><p><Trans>Switch PGM/PVW, watch the wall, feel the tally.</Trans> <button type="button" className="bd-help-btn" aria-expanded={help} aria-controls="bd-help-desk" onClick={() => { setHelp((v) => !v); }}>ⓘ <Trans>Help</Trans></button></p><div className="bd-help-panel" id="bd-help-desk" hidden={!help}><p><Trans>Tally vs signal: the wall border and lamp are tally (red = on program, amber = cue); the chip is signal state. A source can be live and red, or live and dark — never one merged state.</Trans></p><p><Trans>Takes: when locked, CUT confirms once, naming both sources. PGM bus taps always confirm.</Trans></p></div></div><div className="bd-heading-side"><Link className="bd-alarm-btn" to="/alarms" aria-label={alarms.isSuccess ? t`Alarms — ${alarms.data.length} active system` : t`Alarms`}>⚠{alarms.isSuccess ? <b>{alarms.data.length}</b> : null}</Link><div className="bd-desk-clock" role="group" aria-label={t`Program timecode`}><span className="bd-cell bd-cell-na" title={t`No timecode source in the engine API`} data-testid="desk-tc-unavailable"><span className="bd-lbl">PGM TC</span><span className="bd-clock-na">— <Trans>unavailable</Trans></span></span><span className="bd-cell bd-cell-na" title={t`No show clock in the engine API`} data-testid="desk-elapsed-unavailable"><span className="bd-lbl">SHOW ELAPSED</span><span className="bd-clock-na">— <Trans>unavailable</Trans></span></span><span className="bd-cell bd-tick"><span className="bd-lbl"><Trans>ENGINE TICK</Trans></span><b>{state.data?.tick ?? '—'}</b></span></div></div></div>
    <div className="bd-operation" role="status" hidden={!reason && !error && !operationId}>{reason && <span>{t`Controls disabled`}: {reason}</span>}{error && <p role="alert">{error}</p>}{operationId && <OperationFeedback phase={operation?.phase === 'accepted' ? 'pending' : operation?.phase ?? 'unknown'} operationLabel={t`Switcher`} correlationId={operationId} failureReason={failure ?? t`Switcher operation rejected`} unknownReason={t`Outcome unknown; controls remain blocked until a correlated terminal event`} />}</div>
    <div className="bd-desk-grid">
      <PreviewMonitor program source={name(state.data?.program_source)} fresh={fresh && status === 'open'} />
      <PreviewMonitor program={false} source={name(state.data?.preview_source)} fresh={fresh && status === 'open'} />
      <section className="bd-wall"><h2><Trans>Multiview wall</Trans></h2><div className="bd-wall-grid">{sources.data?.map((source) => <div className="bd-wall-tile" key={source.id} data-tile-state={tileForSource(tiles, source.id)?.state ?? 'unknown'} data-tally={tallyForSource(source.id, state.data, fresh && status === 'open')}><SourcePicture id={source.id} /><small>{tallyGlyph(tallyForSource(source.id, state.data, fresh && status === 'open'))}</small><b>{source.name}</b><span><TileLabel tile={tileForSource(tiles, source.id)} /></span></div>)}</div><p>{sources.isError ? t`Source catalog unavailable` : !sources.data?.length ? t`No source catalog available` : t`Tally follows the engine bus IDs; source presence does not prove signal.`}</p><p><Trans>Layouts</Trans>: {layouts.isError ? t`Unavailable` : layouts.data?.length ?? '—'} · <Trans>Observed tiles</Trans>: {tilesKnown ? tiles.size : '—'} · <Link className="bd-link48" to="/monitoring"><Trans>Open live monitoring</Trans></Link></p></section>
      <section className="bd-switcher"><h2><Trans>M/E 1 — Switcher</Trans></h2>
        <div className="bd-buses">{(['preview', 'program'] as const).map((bus) => <div className={`bd-bus bd-bus-${bus}`} key={bus} role="group" aria-label={bus === 'program' ? t`PGM bus — direct taps always confirm` : t`PVW bus`}><b>{bus === 'program' ? 'PGM' : 'PVW'}</b><div>{sources.data?.map((source, index) => <button key={source.id} disabled={disabled} title={reason ? `${source.name} — ${reason}` : source.name} aria-pressed={tallyForSource(source.id, state.data, fresh && status === 'open') === bus} onClick={() => { request(bus === 'program' ? 'select_program' : 'select_preview', source.id); }}><i className="bd-idx" aria-hidden="true">{index + 1}</i>{source.name}</button>)}{!sources.data?.length && <span><Trans>No sources available</Trans></span>}</div></div>)}</div>
        <div className="bd-takes"><div className="bd-transitions" role="group" aria-label={t`Transition type — unavailable`}><button disabled aria-pressed="true" title={t`AUTO unavailable: the current engine rejects AUTO with HTTP 422`}>MIX</button><button disabled title={t`WIPE and DIP have no supported control contract`}>WIPE</button><button disabled title={t`WIPE and DIP have no supported control contract`}>DIP</button></div><button className="bd-auto" disabled title={t`AUTO unavailable: the current engine rejects AUTO with HTTP 422`} onClick={() => { request('auto'); }}>AUTO</button><div className="bd-cutrow"><button className="bd-cut" disabled={disabled} aria-describedby="bd-cut-src" onClick={() => { request('cut'); }}>CUT<span aria-hidden="true" className="bd-cut-src"> · {name(state.data?.preview_source) ?? t`source unknown`}</span></button><span id="bd-cut-src" className="sr-only">{t`Cuts to`} {name(state.data?.preview_source) ?? t`unknown source`}</span></div></div>
        <details className="bd-more"><summary><Trans>Why AUTO is unavailable · transition options</Trans></summary><p className="bd-unsupported"><Trans>AUTO unavailable: the current engine rejects AUTO with HTTP 422.</Trans></p><label><Trans>AUTO frames</Trans><input aria-label={t`Auto frames`} value={frames} type="number" min="1" max="1000" disabled onChange={(e) => { setFrames(e.target.value); }} /></label></details>
        <details><summary><Trans>More keys — unavailable</Trans></summary><p><Trans>WIPE, DIP, USK, DSK and AUX have no supported control contract.</Trans></p></details>
      </section>
      <aside className="bd-rail"><section><h2><Trans>Tally legend</Trans></h2><p className="bd-red"><Trans>● Red — PROGRAM</Trans></p><p className="bd-amber"><Trans>● Amber — PREVIEW</Trans></p><p><Trans>○ Dark — neither bus</Trans></p><small><Trans>Tally is separate from signal and output transmission.</Trans></small></section><section><h2><Trans>Alarms & engine health</Trans></h2><div className="bd-alarm-summary" role="region" aria-label={t`Active system alarm summary`}><p><Trans>Active system alarms only</Trans></p>{alarms.isError ? <p><Trans>Alarm summary unavailable</Trans></p> : alarms.isPending ? <p><Trans>Loading alarm summary</Trans></p> : alarms.data.length === 0 ? <p><Trans>No active system alarms reported</Trans></p> : <ul>{alarms.data.slice(0, 3).map((alarm) => <li key={alarm.id} title={`${alarm.severity} · ${alarm.kind}`}><span>{alarm.severity}</span> · {alarm.kind}</li>)}</ul>}</div><Link className="bd-btn" to="/alarms"><Trans>Open alarms</Trans></Link></section></aside>
    </div>
    <Dialog open={confirm !== undefined} onOpenChange={(open) => { if (!open) setConfirm(undefined); }}><DialogContent touchScroll><DialogTitle><Trans>Confirm program change</Trans></DialogTitle><p>{confirm?.action.toUpperCase()} · {name(state.data?.program_source) ?? t`Unknown`} → {name(confirm?.source ?? state.data?.preview_source) ?? t`Unknown`}</p><p><Trans>The program bus changes what viewers see. Acceptance is not completion.</Trans></p><button disabled={disabled} onClick={() => { if (confirm) send(confirm); setConfirm(undefined); }}><Trans>Confirm take</Trans></button><button onClick={() => { setConfirm(undefined); }}><Trans>Cancel</Trans></button></DialogContent></Dialog>
  </section>;
}
