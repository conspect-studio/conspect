import { busKeyReason, deskControlReason, tallyForSource } from './deskSafety';
import { Info, TriangleAlert } from 'lucide-react';
import { useEffect, useMemo, useRef, useState } from 'react';
import type { CSSProperties, JSX } from 'react';
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
import { useSourceStatus } from '../../api/sourceStatus';
import type { SourceDemand } from '../../api/sourceStatus';
import { subscribeRealtime } from '../../realtime/shared';
import { useEngineEvents } from '../../realtime/useEngineEvents';
import type { LiveTile } from '../../realtime/useEngineEvents';
import { switcherOperationEvidence } from '../../realtime/operationEvidence';
import { tileForSource, useLiveTiles } from '../../resources/useLiveTiles';
import { meterFillPercent, useAudioMeter } from '../../realtime/useAudioMeter';
import { usePreviewCapabilities, whepAvailable } from '../../preview/capabilities';
import { PreviewSurface } from '../../preview/PreviewSurface';
import { useJpegStill } from '../../preview/useJpegPreview';
import { WallCrop, WallStreamProvider, useWallStatus, useWallSurface } from '../../preview/WallStream';
import { Dialog, DialogContent, DialogTitle } from '../ui/dialog';
import { useOperationFeedback, useSubmitTrackedOperation } from './useOperationFeedback';
import { OperationFeedback } from './OperationFeedback';
import { useTakeLock } from './BroadcastShell';
import { DeskCastButton } from '../../cast/DeskCastButton';
import { tickConfirmed, useTickAdvancedAt } from './tickObservation';

const STATE_KEY = ['switcher', 'me1'] as const;
const SENDING = '\u0000sending';
function PreviewMonitor({ program, source, fresh }: { readonly program: boolean; readonly source: string | undefined; readonly fresh: boolean }): JSX.Element {
  const { t } = useLingui();
  const caps = usePreviewCapabilities();
  // Shared wall: the preview cell is whatever source the wall currently shows on preview.
  const wallPreview = useWallSurface(useWallSurface(undefined).previewSource);
  const pvwLive = !program && wallPreview.live && wallPreview.cell !== undefined;
  const pvwStill = useJpegStill('/api/v1/preview/switcher/me1/preview.jpg', 1000, !program && !pvwLive);
  const pvw = pvwStill.url;
  const pvwUnavailable = !program && !pvwLive && pvwStill.state === 'unavailable';
  const [diagHost, setDiagHost] = useState<HTMLDivElement | null>(null);
  const [transportUnavailable, setTransportUnavailable] = useState(false);
  const still = program ? (!whepAvailable(caps.data, 'program') && (!caps.data?.moq || transportUnavailable)) : !pvwLive && pvw !== undefined;
  return <figure className={`bd-monitor ${program ? 'bd-pgm' : 'bd-pvw'}`} data-tally={fresh && source ? (program ? 'program' : 'preview') : 'unknown'}>
    <div className="bd-picture">{program ? <PreviewSurface scope="program" whepEndpoint="/api/v1/preview/program/whep" jpegPath="/api/v1/preview/program.jpg" label={t`Program monitor`} capabilities={caps.data} className="bd-video" diagnosticsHost={diagHost} onTransportUnavailable={() => { setTransportUnavailable(true); }} /> : pvwLive && wallPreview.previewSource ? <WallCrop source={wallPreview.previewSource} label={t`Preview monitor`} className="bd-video" testId="preview-wall" /> : pvwUnavailable ? <span role="status" data-testid="preview-unavailable"><Trans>Preview unavailable — switcher backend not deployed</Trans></span> : pvw ? <div className="bd-jpeg"><img src={pvw} alt={t`Preview monitor`} />{pvwStill.state === 'stale' ? <em className="bd-stale" data-testid="preview-stale"><Trans>Stale</Trans></em> : null}</div> : <span><Trans>Waiting for a frame…</Trans></span>}</div>
    {program ? <div className="bd-diag-slot" ref={setDiagHost} data-testid="desk-fallback-slot" /> : null}
    <figcaption><div className="bd-monitor-actions" data-monitor-actions={program ? 'program' : 'preview'}><DeskCastButton bus={program ? 'program' : 'preview'} /></div><div className="bd-capline"><b>{program ? (fresh && source ? t`● ON AIR — PROGRAM` : t`○ PROGRAM — unconfirmed`) : (fresh && source ? t`▲ PREVIEW` : t`△ PREVIEW — unconfirmed`)}</b><span>{source ?? t`Unknown source`}</span></div><small>{fresh ? t`Engine-confirmed bus` : t`Unconfirmed / stale`}{pvwUnavailable ? null : still ? <> · {t`Still preview (~1 fps) — live video is unavailable for this preview`}</> : null}</small></figcaption>
    {program && <ProgramLevels />}
  </figure>;
}

/** Program L/R levels strictly from the audio.meter telemetry; otherwise explicitly unavailable. */
function ProgramLevels(): JSX.Element {
  const { current, status } = useAudioMeter();
  // A dropped meter socket must not leave a frozen level on screen.
  if (status !== 'open' || current === undefined || current.peak_db.length === 0) return <div className="bd-meter-note" data-testid="desk-meters-unavailable"><Trans>Audio meters unavailable — no measured level telemetry</Trans></div>;
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

/** Longest engine `detail` shown on a tile, in characters. */
const TILE_DETAIL_MAX_CHARS = 160;

/**
 * The engine's reason as one bounded, plain-text line. React renders it as a
 * text node, so a detail containing markup shows literally — never as HTML.
 */
function tileDetail(detail: string | null | undefined): string | undefined {
  if (!detail) return undefined;
  // Drop Unicode format characters (bidi overrides, zero-width marks), turn any
  // other control character into a space, then collapse: one bounded, plain line.
  const flat = detail.replace(/\p{Cf}/gu, '').replace(/\p{Cc}/gu, ' ').replace(/\s+/gu, ' ').trim();
  if (!flat) return undefined;
  // Count code points, not UTF-16 units, so a truncation never splits a pair.
  const chars = Array.from(flat);
  return chars.length > TILE_DETAIL_MAX_CHARS ? `${chars.slice(0, TILE_DETAIL_MAX_CHARS).join('')}…` : flat;
}

/**
 * A wall tile's picture, or the reason it has none. Every state resolves to a
 * picture or stated text: a failed source shows a no-signal slate with the
 * engine's reason, a starting source says "Starting" over its last good frame,
 * and a stale still keeps its "Stale frame" mark.
 */
function SourcePicture({ id, demand }: { readonly id: string; readonly demand: SourceDemand | undefined }): JSX.Element {
  const { t } = useLingui();
  // Keeps the last good frame across failed polls; stale only after a bounded age (max(5 polls,10 s)).
  const wall = useWallSurface(id);
  const phase = demand?.phase;
  // Keep polling while the cell is not live — including a failed source: the poll
  // holds its 5 s preview demand, and dropping it would lapse the lease and flip
  // the tile to "Idle" (a scoped viewer, or a wall without that cell, has no other
  // demand keeper). The route answers the typed problem instead of a frame.
  const { url, state } = useJpegStill(`/api/v1/preview/inputs/${encodeURIComponent(id)}.jpg`, 2000, !wall.live);
  if (wall.live) return <WallCrop source={id} label={t`Source live preview`} className="bd-source-picture" />;
  const reason = tileDetail(demand?.detail);
  if (phase === 'failed') return <span role="status" className="bd-slate" data-testid="tile-failed"><span className="bd-slate-title"><Trans>No signal</Trans></span>{reason !== undefined ? <span className="bd-slate-detail" data-testid="tile-failed-detail">{reason}</span> : null}</span>;
  if (phase === 'starting' || (phase === 'idle' && demand?.video_wanted === true)) return url !== undefined
    ? <><img className="bd-source-picture" src={url} alt={t`Source still preview`} data-still-state={state} /><em className="bd-tile-overlay" data-testid="tile-starting"><Trans>Starting</Trans></em></>
    : <span role="status" className="bd-slate" data-testid="tile-starting"><span className="bd-slate-title"><Trans>Starting</Trans></span></span>;
  if (url !== undefined) return <><img className="bd-source-picture" src={url} alt={t`Source still preview`} data-still-state={state} />{state === 'stale' ? <em className="bd-stale" data-testid="tile-stale"><Trans>Stale frame</Trans></em> : null}</>;
  if (phase === 'idle') return <span role="status" className="bd-slate" data-testid="tile-idle"><span className="bd-slate-title"><Trans>Idle — not in use</Trans></span></span>;
  return <span data-testid="tile-no-frame"><Trans>No confirmed frame</Trans></span>;
}

/** Real bus state and correlated commands inside the approved desk composition. */
export function ProductionDesk(): JSX.Element {
  return <WallStreamProvider><ProductionDeskBody /></WallStreamProvider>;
}

function WallNote() {
  const wall = useWallStatus();
  if (wall.live) return <p className="bd-still-note" data-testid="wall-live-note" data-transport={wall.transport} data-ice-pair={wall.icePair ?? 'none'}>{wall.transport === 'moq' ? <Trans>Wall tiles are live video from one shared MoQ stream; a tile without a live cell shows its still.</Trans> : <Trans>Wall tiles are live video from one shared WebRTC session; a tile without a live cell shows its still.</Trans>}</p>;
  return <p className="bd-still-note" data-testid="wall-still-note"><Trans>Wall tiles are still previews refreshed every ~2 s — not live video.</Trans>{wall.reason ? <> <span data-testid="wall-still-reason">({wall.reason})</span></> : null}</p>;
}

function ProductionDeskBody(): JSX.Element {
  const { t } = useLingui();
  const qc = useQueryClient();
  const { status } = useEngineEvents();
  const { locked } = useTakeLock();
  const state = useQuery({ queryKey: STATE_KEY, queryFn: () => getSwitcher(), retry: false, refetchInterval: 2000 });
  const sources = useSources();
  const demand = useSourceStatus();
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
  const [frames, setFrames] = useState('30');
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
  const sameBuses = state.data !== undefined && state.data.preview_source === state.data.program_source;
  const framesValid = /^[0-9]+$/.test(frames) && Number(frames) >= 1 && Number(frames) <= 1000;
  const autoReason = sameBuses ? t`Preview and Program carry the same source` : !framesValid ? t`Enter 1–1000 frames` : undefined;
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
  const deskStyle: CSSProperties & Record<`--desk-${string}`, number> = { '--desk-inputs': Math.max(1, sources.data?.length ?? 1), '--desk-columns': (sources.data?.length ?? 0) > 12 ? 8 : 6, '--desk-rows': Math.max(1, Math.ceil((sources.data?.length ?? 0) / ((sources.data?.length ?? 0) > 12 ? 8 : 6))) };
  return <section className="production-desk" aria-label={t`Production desk`} data-dense={(sources.data?.length ?? 0) > 12} style={deskStyle}>
    <div className="bd-heading"><div><h1><Trans>Production desk</Trans></h1><div className="bd-help-panel" id="bd-help-desk" hidden={!help}><p><Trans>Tally vs signal: the wall border and lamp are tally (red = on program, amber = cue); the chip is signal state. A source can be live and red, or live and dark — never one merged state.</Trans></p><p><Trans>Takes: when locked, CUT confirms once, naming both sources. PGM bus taps always confirm.</Trans></p></div></div><button type="button" className="bd-help-btn" aria-label={t`Help`} title={t`Help`} aria-expanded={help} aria-controls="bd-help-desk" onClick={() => { setHelp((v) => !v); }}><Info aria-hidden="true" size={14} className="mv-help-ico" /> <Trans>Help</Trans></button><div className="bd-heading-side"><Link className="bd-alarm-btn" to="/alarms" aria-label={alarms.isSuccess ? t`Alarms — ${alarms.data.length} active system` : t`Alarms`}><TriangleAlert aria-hidden="true" size={16} />{alarms.isSuccess ? <b>{alarms.data.length}</b> : null}</Link><div className="bd-desk-clock" role="group" aria-label={t`Program timecode`}><span className="bd-cell bd-cell-na" title={t`No timecode source in the engine API`} data-testid="desk-tc-unavailable"><span className="bd-lbl">PGM TC</span><span className="bd-clock-na">— <Trans>unavailable</Trans></span></span><span className="bd-cell bd-cell-na" title={t`No show clock in the engine API`} data-testid="desk-elapsed-unavailable"><span className="bd-lbl">SHOW ELAPSED</span><span className="bd-clock-na">— <Trans>unavailable</Trans></span></span><span className="bd-cell bd-tick"><span className="bd-lbl"><Trans>ENGINE TICK</Trans></span><b>{state.data?.tick ?? '—'}</b></span></div></div></div>
    <div className="bd-operation" role="status" hidden={!reason && !error && !operationId}>{reason && <span>{t`Controls disabled`}: {reason}</span>}{error && <p role="alert">{error}</p>}{operationId && <OperationFeedback phase={operation?.phase === 'accepted' ? 'pending' : operation?.phase ?? 'unknown'} operationLabel={t`Switcher`} correlationId={operationId} failureReason={failure ?? t`Switcher operation rejected`} unknownReason={t`Outcome unknown; controls remain blocked until a correlated terminal event`} />}</div>
    <div className="bd-desk-grid">
      <PreviewMonitor program source={name(state.data?.program_source)} fresh={fresh && status === 'open'} />
      <PreviewMonitor program={false} source={name(state.data?.preview_source)} fresh={fresh && status === 'open'} />
      <section className="bd-wall"><div className="bd-wall-heading"><h2><Trans>Multiview wall</Trans></h2><span><Trans>Select a source to Preview</Trans></span><Link className="bd-route-link bd-add-source-link" to="/sources?new=1" aria-label={t`Add a source — opens the New source dialog`}><Trans>Add source</Trans></Link><Link className="bd-route-link" to="/routing"><Trans>Route / patch</Trans></Link></div><div className="bd-wall-grid">{sources.data?.map((source) => { const keyReason = reason ?? busKeyReason('preview', source.id, state.data, demand.data?.get(source.id)?.phase); return <button type="button" className="bd-wall-tile" key={source.id} aria-disabled={disabled || keyReason !== undefined} aria-describedby={`desk-tally-${source.id} desk-signal-${source.id} desk-reason-${source.id}`} aria-label={t`Preview ${source.name}`} aria-pressed={tallyForSource(source.id, state.data, fresh && status === 'open') === 'preview'} title={keyReason} onClick={() => { if (!disabled && keyReason === undefined) request('select_preview', source.id); }} data-tile-state={tileForSource(tiles, source.id)?.state ?? 'unknown'} data-tally={tallyForSource(source.id, state.data, fresh && status === 'open')}><SourcePicture id={source.id} demand={demand.data?.get(source.id)} /><small id={`desk-tally-${source.id}`}>{tallyGlyph(tallyForSource(source.id, state.data, fresh && status === 'open'))}</small><b>{source.name}</b><span className="sr-only" id={`desk-reason-${source.id}`}>{keyReason}</span><span id={`desk-signal-${source.id}`}><TileLabel tile={tileForSource(tiles, source.id)} /></span></button>; })}</div><WallNote />{sources.isError || !sources.data?.length ? <p>{sources.isError ? t`Source catalog unavailable` : t`No source catalog available`}</p> : null}</section>
      <section className="bd-switcher"><h2><Trans>M/E 1 — Switcher</Trans></h2>
        <div className="bd-buses">{(['preview', 'program'] as const).map((bus) => <div className={`bd-bus bd-bus-${bus}`} key={bus} role="group" aria-label={bus === 'program' ? t`PGM bus — direct taps always confirm` : t`PVW bus`}><b>{bus === 'program' ? 'PGM' : 'PVW'}</b><div>{sources.data?.map((source, index) => { const keyReason = reason ?? busKeyReason(bus, source.id, state.data, demand.data?.get(source.id)?.phase); return <button key={source.id} aria-label={source.name} disabled={disabled || keyReason !== undefined} data-key-reason={keyReason} title={keyReason ? `${source.name} — ${keyReason}` : source.name} aria-pressed={tallyForSource(source.id, state.data, fresh && status === 'open') === bus} onClick={() => { request(bus === 'program' ? 'select_program' : 'select_preview', source.id); }}><span className="bd-key-name">{sources.data.length > 12 ? Array.from(source.name).slice(0, 10).join('') : source.name}</span><i className="bd-idx" aria-hidden="true">{index + 1}</i></button>; })}{!sources.data?.length && <span><Trans>No sources available</Trans></span>}</div></div>)}</div>
        <div className="bd-takes"><div className="bd-transitions" role="group" aria-label={t`Transition type`}><button disabled aria-pressed="true" title={t`MIX is the only supported transition`}>MIX</button><button disabled title={t`WIPE and DIP have no supported control contract`}>WIPE</button><button disabled title={t`WIPE and DIP have no supported control contract`}>DIP</button></div><button className="bd-auto" disabled={disabled || autoReason !== undefined} title={autoReason ? `AUTO — ${autoReason}` : t`AUTO mix over ${frames} frames`} onClick={() => { request('auto'); }}>AUTO</button><div className="bd-cutrow"><button className="bd-cut" disabled={disabled || sameBuses} title={sameBuses ? t`CUT — Preview and Program carry the same source` : undefined} aria-describedby="bd-cut-src" onClick={() => { request('cut'); }}>CUT<span aria-hidden="true" className="bd-cut-src"> · {name(state.data?.preview_source) ?? t`source unknown`}</span></button><span id="bd-cut-src" className="sr-only">{t`Cuts to`} {name(state.data?.preview_source) ?? t`unknown source`}</span></div></div>
        <div className="bd-more"><label><Trans>AUTO frames</Trans><input aria-label={t`Auto frames`} value={frames} type="number" min="1" max="1000" disabled={disabled} onChange={(e) => { setFrames(e.target.value); }} /></label></div>
      </section>
      <details className="bd-desk-status"><summary><Trans>Tally & engine status</Trans></summary><aside className="bd-rail"><section><h2><Trans>Tally legend</Trans></h2><p className="bd-red"><Trans>● Red — PROGRAM</Trans></p><p className="bd-amber"><Trans>● Amber — PREVIEW</Trans></p><p><Trans>○ Dark — neither bus</Trans></p><small><Trans>Tally is separate from signal and output transmission.</Trans></small></section><section><h2><Trans>Alarms & engine health</Trans></h2><div className="bd-alarm-summary" role="region" aria-label={t`Active system alarm summary`}><p><Trans>Active system alarms only</Trans></p>{alarms.isError ? <p><Trans>Alarm summary unavailable</Trans></p> : alarms.isPending ? <p><Trans>Loading alarm summary</Trans></p> : alarms.data.length === 0 ? <p><Trans>No active system alarms reported</Trans></p> : <ul>{alarms.data.slice(0, 3).map((alarm) => <li key={alarm.id} title={`${alarm.severity} · ${alarm.kind}`}><span>{alarm.severity}</span> · {alarm.kind}</li>)}</ul>}</div><Link className="bd-btn" to="/alarms"><Trans>Open alarms</Trans></Link></section><section><p><Trans>Layouts</Trans>: {layouts.isError ? t`Unavailable` : layouts.data?.length ?? '—'} · <Trans>Observed tiles</Trans>: {tilesKnown ? tiles.size : '—'}</p><p><Trans>Tally follows the engine bus IDs; source presence does not prove signal.</Trans></p><p><Trans>WIPE, DIP, USK, DSK and AUX have no supported control contract.</Trans></p><Link className="bd-link48" to="/monitoring"><Trans>Open live monitoring</Trans></Link></section></aside></details>
    </div>
    <Dialog open={confirm !== undefined} onOpenChange={(open) => { if (!open) setConfirm(undefined); }}><DialogContent touchScroll><DialogTitle><Trans>Confirm program change</Trans></DialogTitle><p>{confirm?.action.toUpperCase()} · {name(state.data?.program_source) ?? t`Unknown`} → {name(confirm?.source ?? state.data?.preview_source) ?? t`Unknown`}</p><p><Trans>The program bus changes what viewers see. Acceptance is not completion.</Trans></p><button disabled={disabled} onClick={() => { if (confirm) send(confirm); setConfirm(undefined); }}><Trans>Confirm take</Trans></button><button onClick={() => { setConfirm(undefined); }}><Trans>Cancel</Trans></button></DialogContent></Dialog>
  </section>;
}
