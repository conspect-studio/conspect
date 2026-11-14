// Approved "Sources & library" surface: search + All/Needs me/On air filter,
// lifecycle summary, six-column table (tablet/desktop) or cards (phone) with
// exceptions first. It only recomposes the existing page's facts and controls:
// signal (live tile), lifecycle (demand phase), configured activation and tally
// are separate; anything the page cannot establish reads Unknown, never a
// guessed health value. Program tally has no unambiguous source binding here,
// so it is always Unknown and the On air filter is shown unavailable with its
// reason.
import { useState, useSyncExternalStore } from 'react';
import type { JSX, ReactNode } from 'react';
import { Link } from 'react-router-dom';
import { Trans, useLingui } from '@lingui/react/macro';
import { CircleHelp, CirclePause, Pencil, RotateCw, Trash2 } from 'lucide-react';

import { useSourceAction } from '../../api/sourceStatus';
import type { SourceDemand } from '../../api/sourceStatus';
import type { LinkInfo } from '../../api/catalogue';
import { LinkActions, LinkCell, SourceNetworkAction } from '../../iptv/SourceLinks';
import { SourceEventsButton } from '../../events/SourceEventsButton';
import { SourcePreviewButton } from '../../preview/SourcePreviewButton';
import { SourceDemandBadge } from '../../components/SourceDemandBadge';
import { SourceLifecycleActions } from '../../components/SourceLifecycleActions';
import { TileStateBadge } from '../../components/TileStateBadge';
import { TouchAction } from '../../components/broadcast';
import { isNotLiveStreamReason, YoutubeVtClipHint } from '../../components/YoutubeVtClipHint';
import type { LiveTile } from '../../realtime/useEngineEvents';
import type { SourceView } from '../../resources/types';
import type { ProgramTally } from './useProgramTally';
import './sources-approved.css';

type Filter = 'all' | 'needs' | 'onair';

/** Tablet/desktop (>= 768px) get the table; phones get cards. */
const TABLE_QUERY = '(min-width: 768px)';

function subscribeTable(onChange: () => void): () => void {
  const mql = window.matchMedia(TABLE_QUERY);
  mql.addEventListener('change', onChange);
  return (): void => {
    mql.removeEventListener('change', onChange);
  };
}

function useTableLayout(): boolean {
  return useSyncExternalStore(
    subscribeTable,
    () => window.matchMedia(TABLE_QUERY).matches,
    () => false,
  );
}

/** A failed or starting source needs attention; unknown status is not an exception. */
function exceptionRank(demand: SourceDemand | undefined): number {
  if (demand?.phase === 'failed') return 0;
  if (demand?.phase === 'starting') return 1;
  return 2;
}

/** Props for {@link SourcesSurface}. */
export interface SourcesSurfaceProps {
  readonly rows: readonly SourceView[];
  readonly demand: ReadonlyMap<string, SourceDemand> | undefined;
  readonly tileFor: (sourceId: string) => LiveTile | undefined;
  readonly linkFor: (sourceId: string) => LinkInfo | undefined;
  readonly onEdit: (row: SourceView) => void;
  readonly onDelete: (row: SourceView) => void;
  /** Engine-confirmed program/preview bus ids; undefined = not confirmed (Unknown). */
  readonly tally?: ProgramTally | undefined;
}

function RetryButton({ id, name }: { readonly id: string; readonly name: string }): JSX.Element {
  const { t } = useLingui();
  const action = useSourceAction();
  const [refusal, setRefusal] = useState<string | null>(null);
  return (
    <span className="inline-flex flex-col gap-1">
      <button
        type="button"
        className="act-btn retry"
        disabled={action.isPending}
        aria-label={t`Retry ${name}`}
        onClick={(): void => {
          setRefusal(null);
          action.mutate(
            { id, action: 'start' },
            {
              // Start can answer 409 with a problem `detail` (e.g. "applies on
              // restart" for a stored-not-applied source) — surface that exact
              // text, never a generic failure.
              onError: (error) => { setRefusal(error.detail ?? error.message); },
            },
          );
        }}
      >
        <RotateCw aria-hidden="true" /><Trans>Retry</Trans>
      </button>
      {refusal !== null ? (
        <span role="alert" className="text-xs text-destructive" data-testid={`source-retry-refused-${id}`}>
          {refusal}
        </span>
      ) : null}
    </span>
  );
}

function Pill({ phase, children }: { readonly phase: string; readonly children: ReactNode }): JSX.Element {
  return <span className="src-pill-host" data-phase={phase}>{children}</span>;
}

/** The compact approved list with every existing control retained. */
export function SourcesSurface(props: SourcesSurfaceProps): JSX.Element {
  const { t } = useLingui();
  const asTable = useTableLayout();
  const [query, setQuery] = useState('');
  const [filter, setFilter] = useState<Filter>('all');
  const [open, setOpen] = useState<ReadonlySet<string>>(new Set());

  const demandOf = (id: string): SourceDemand | undefined => props.demand?.get(id);
  const needle = query.trim().toLowerCase();
  const visible = props.rows
    .map((row, index) => ({ row, index }))
    .filter(({ row }) => {
      if (filter === 'needs' && exceptionRank(demandOf(row.id)) > 1) return false;
      if (filter === 'onair' && row.id !== props.tally?.program) return false;
      return (
        needle === '' ||
        [row.name, row.id, row.locator ?? ''].some((v) => v.toLowerCase().includes(needle))
      );
    })
    .sort((a, b) => exceptionRank(demandOf(a.row.id)) - exceptionRank(demandOf(b.row.id)) || a.index - b.index)
    .map(({ row }) => row);

  const count = (phase: SourceDemand['phase']): number =>
    props.rows.filter((r) => demandOf(r.id)?.phase === phase).length;
  const unknown = props.rows.filter((r) => demandOf(r.id) === undefined).length;
  const alwaysOn = props.rows.filter((r) => demandOf(r.id)?.activation === 'always_on').length;

  const toggle = (id: string): void => {
    setOpen((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  const identity = (source: SourceView, heading: boolean): JSX.Element => (
    <div className="src-name">
      <span className="src-head">
        <span className="kind-chip" lang="" dir="auto">{source.rawKind}</span>
        {heading ? (
          <h3 lang="" dir="auto" className="src-title">{source.name}</h3>
        ) : (
          <b lang="" dir="auto" className="src-title">{source.name}</b>
        )}
        <code lang="" dir="auto" className="src-ep break-all">{source.id}</code>
      </span>
      <span className="src-ep">
        <code lang="" dir="auto" className="break-all">{source.locator ?? t`No locator configured`}</code>
      </span>
    </div>
  );

  const failure = (source: SourceView): JSX.Element | null => {
    const d = demandOf(source.id);
    if (d?.phase !== 'failed') return null;
    const manual = d.consumers.some((c) => c.kind === 'manual');
    const hasReason = d.detail !== undefined && d.detail !== null && d.detail !== '';
    // #419 item 4: a `youtube` source failing because the video is not live
    // (status was_live/not_live/post_live) gets the VT-clip redirect hint
    // instead of leaving the operator to guess what "not a live stream" means.
    const tileReason = props.tileFor(source.id)?.reason;
    const notLive =
      source.rawKind === 'youtube' &&
      (isNotLiveStreamReason(d.detail) || isNotLiveStreamReason(tileReason));
    return (
      <div className="src-fail" data-testid={`source-failure-box-${source.id}`}>
        {hasReason ? (
          <p className="src-fail-reason" data-testid={`source-failure-${source.id}`}>{d.detail}</p>
        ) : (
          <p className="src-fail-reason"><Trans>Failed — no reason reported.</Trans></p>
        )}
        {notLive ? <YoutubeVtClipHint /> : null}
        {manual ? null : <RetryButton id={source.id} name={source.name} />}
      </div>
    );
  };

  const crossLink = (source: SourceView): JSX.Element | null =>
    source.rawKind === 'youtube' ? (
      <Link className="cross-link" to="/routing">
        <Trans>Outbound publishing is a separate path (Routing)</Trans>
      </Link>
    ) : null;

  const signal = (source: SourceView): JSX.Element => {
    const d = demandOf(source.id);
    const tile = props.tileFor(source.id);
    const running = d !== undefined && (d.video_running || d.audio_running);
    if (running && tile !== undefined) {
      return <Pill phase="signal"><TileStateBadge state={tile.state} /></Pill>;
    }
    return (
      <Pill phase={d === undefined ? 'unknown' : 'nosignal'}>
        <span className="src-pill">
          {d === undefined ? <CircleHelp aria-hidden="true" /> : <CirclePause aria-hidden="true" />}
          {d === undefined ? <Trans>Signal unknown</Trans> : <Trans>No live signal data</Trans>}
        </span>
      </Pill>
    );
  };

  const lifecycle = (source: SourceView): JSX.Element => {
    const d = demandOf(source.id);
    return d === undefined ? (
      <Pill phase="unknown">
        <span className="src-pill"><CircleHelp aria-hidden="true" /><Trans>Status unknown</Trans></span>
      </Pill>
    ) : (
      <Pill phase={d.phase}><SourceDemandBadge demand={d} /></Pill>
    );
  };

  const activation = (source: SourceView): JSX.Element => {
    const a = demandOf(source.id)?.activation;
    return (
      <span className="src-activation" data-activation={a ?? 'unknown'}>
        {a === 'always_on' ? <Trans>Always on</Trans> : a === 'on_demand' ? <Trans>On demand</Trans> : <Trans>Activation unknown</Trans>}
      </span>
    );
  };

  const tally = (source: SourceView): JSX.Element => {
    if (props.tally === undefined) {
      return (
        <span className="src-muted" title={t`Program tally is not confirmed right now (switcher state missing or stale), so it is not guessed.`}>
          <Trans>Tally unknown</Trans>
        </span>
      );
    }
    if (source.id === props.tally.program) {
      return <Pill phase="onair"><Trans>● RED — on air</Trans></Pill>;
    }
    if (source.id === props.tally.preview) {
      return <Pill phase="preview"><Trans>● AMBER — preview</Trans></Pill>;
    }
    return <Pill phase="dark"><Trans>— dark</Trans></Pill>;
  };

  const lifecycleActions = (source: SourceView): JSX.Element | null => {
    const d = demandOf(source.id);
    return d === undefined ? null : <SourceLifecycleActions id={source.id} name={source.name} demand={d} />;
  };

  const details = (source: SourceView): JSX.Element => {
    const editReason = source.editable
      ? undefined
      : t`kind "${source.rawKind}" isn't editable in this UI; the document is preserved as authored`;
    const link = props.linkFor(source.id);
    return (
      <div className="src-detail-body">
        <div className="src-detail-actions">
          <SourcePreviewButton id={source.id} name={source.name} />
          <SourceEventsButton id={source.id} name={source.name} eventObservation={source.eventObservation} />
          <TouchAction
            variant="outline"
            aria-label={editReason === undefined ? `${t`Edit source`}: ${source.name}` : `${t`Edit source`}: ${source.name} — ${editReason}`}
            aria-disabled={!source.editable}
            disabled={!source.editable}
            disabledReason={editReason}
            className="min-h-12"
            onPress={(): void => { if (source.editable) { props.onEdit(source); } }}
          >
            <Pencil aria-hidden="true" /><Trans>Edit</Trans>
          </TouchAction>
          <TouchAction variant="ghost" className="min-h-12" aria-label={`${t`Delete source`}: ${source.name}`} onPress={(): void => { props.onDelete(source); }}>
            <Trash2 aria-hidden="true" /><Trans>Delete</Trans>
          </TouchAction>
        </div>
        <div className="src-detail-links">
          {link !== undefined ? <LinkCell link={link} /> : <span className="src-muted"><Trans>Not linked to IPTV-ORG</Trans></span>}
          <div className="src-detail-actions">
            <LinkActions sourceId={source.id} sourceName={source.name} locator={source.locator} link={link} />
            <SourceNetworkAction sourceId={source.id} sourceName={source.name} />
          </div>
        </div>
      </div>
    );
  };

  const detailToggle = (source: SourceView): JSX.Element => (
    <button
      type="button"
      className="act-btn detail"
      aria-expanded={open.has(source.id)}
      aria-controls={`src-detail-${source.id}`}
      aria-label={`${t`Details`}: ${source.name}`}
      onClick={(): void => { toggle(source.id); }}
    >
      <Trans>Details</Trans>
    </button>
  );

  const isException = (source: SourceView): boolean => exceptionRank(demandOf(source.id)) <= 1;

  return (
    <div className="src-surface">
      <div className="src-filter">
        <input
          className="inp"
          type="search"
          value={query}
          placeholder={t`Search sources…`}
          aria-label={t`Search sources by name`}
          onChange={(e): void => { setQuery(e.target.value); }}
        />
        <div className="src-filter-chips" role="group" aria-label={t`Filter sources`}>
          <button type="button" className="sf-chip" aria-pressed={filter === 'all'} onClick={(): void => { setFilter('all'); }}>
            <Trans>All</Trans>
          </button>
          <button type="button" className="sf-chip" aria-pressed={filter === 'needs'} onClick={(): void => { setFilter('needs'); }}>
            <Trans>Needs me</Trans>
          </button>
          {props.tally !== undefined ? (
            <button type="button" className="sf-chip" aria-pressed={filter === 'onair'} onClick={(): void => { setFilter('onair'); }}>
              <Trans>On air</Trans>
            </button>
          ) : (
            <button
              type="button"
              className="sf-chip"
              aria-pressed={false}
              aria-disabled="true"
              aria-describedby="src-onair-reason"
              onClick={(e): void => { e.preventDefault(); }}
            >
              <Trans>On air</Trans>
            </button>
          )}
        </div>
      </div>
      {props.tally === undefined ? (
        <p id="src-onair-reason" className="src-onair-reason">
          <Trans>On air filter unavailable: the program bus is not confirmed right now (switcher state missing or stale), so no source can be confirmed on air.</Trans>
        </p>
      ) : null}

      <div className="src-summary" role="status">
        {count('ready') > 0 ? <span className="sum-chip"><b>{count('ready')}</b> <Trans>Ready</Trans></span> : null}
        {count('starting') > 0 ? <span className="sum-chip"><b>{count('starting')}</b> <Trans>Starting</Trans></span> : null}
        {count('idle') > 0 ? <span className="sum-chip"><b>{count('idle')}</b> <Trans>Idle</Trans></span> : null}
        {count('failed') > 0 ? <span className="sum-chip warn"><b>{count('failed')}</b> <Trans>Failed</Trans></span> : null}
        {alwaysOn > 0 ? <span className="sum-chip"><b>{alwaysOn}</b> <Trans>Always on</Trans></span> : null}
        {unknown > 0 ? <span className="sum-chip"><b>{unknown}</b> <Trans>Unknown</Trans></span> : null}
        <span className="sum-chip"><Trans>Exceptions (failed / starting) sort first</Trans></span>
      </div>

      {visible.length === 0 ? (
        <p className="src-empty" role="status"><Trans>No sources match this filter.</Trans></p>
      ) : asTable ? (
        <div className="src-table-wrap">
          <table className="src-table" aria-label={t`Configured ingest sources.`}>
            <thead>
              <tr>
                <th scope="col"><Trans>Source</Trans></th>
                <th scope="col"><Trans>Signal</Trans></th>
                <th scope="col"><Trans>Lifecycle</Trans></th>
                <th scope="col"><Trans>Activation</Trans></th>
                <th scope="col"><Trans>Tally</Trans></th>
                <th scope="col" className="th-actions"><Trans>Actions</Trans></th>
              </tr>
            </thead>
            <tbody>
              {visible.map((source) => (
                <FragmentRows key={source.id}>
                  <tr data-testid={`source-row-${source.id}`} data-phase={demandOf(source.id)?.phase ?? 'unknown'} className={isException(source) ? 'tr-exception' : undefined}>
                    <td>
                      {identity(source, false)}
                      {crossLink(source)}
                    </td>
                    <td>{signal(source)}</td>
                    <td>{lifecycle(source)}</td>
                    <td>{activation(source)}</td>
                    <td>{tally(source)}</td>
                    <td>
                      <div className="src-actions-cell">
                        {lifecycleActions(source)}
                        {detailToggle(source)}
                      </div>
                    </td>
                  </tr>
                  {demandOf(source.id)?.phase === 'failed' ? (
                    <tr className="src-fail-row" data-testid={`source-fail-row-${source.id}`}>
                      <td colSpan={6}>{failure(source)}</td>
                    </tr>
                  ) : null}
                  {open.has(source.id) ? (
                    <tr className="src-detail-row">
                      <td colSpan={6} id={`src-detail-${source.id}`}>{details(source)}</td>
                    </tr>
                  ) : null}
                </FragmentRows>
              ))}
            </tbody>
          </table>
        </div>
      ) : (
        <ul className="src-cards" aria-label={t`Configured ingest sources.`}>
          {visible.map((source) => (
            <li
              key={source.id}
              data-testid={`source-card-${source.id}`}
              data-phase={demandOf(source.id)?.phase ?? 'unknown'}
              className={isException(source) ? 'src-card tr-exception' : 'src-card'}
            >
              {identity(source, true)}
              {failure(source)}
              <div className="sc-pills">
                <div className="sc-meta-item"><span className="lbl"><Trans>Signal</Trans></span><div>{signal(source)}</div></div>
                <div className="sc-meta-item"><span className="lbl"><Trans>Lifecycle</Trans></span><div>{lifecycle(source)}</div></div>
                <div className="sc-meta-item"><span className="lbl"><Trans>Activation</Trans></span><div>{activation(source)}</div></div>
                <div className="sc-meta-item"><span className="lbl"><Trans>Tally</Trans></span><div>{tally(source)}</div></div>
              </div>
              <div className="sc-actions">
                {lifecycleActions(source)}
                {detailToggle(source)}
              </div>
              {crossLink(source)}
              {/* Phones keep the primary action + Details; the rest is one tap away. */}
              {open.has(source.id) ? <div id={`src-detail-${source.id}`}>{details(source)}</div> : null}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

function FragmentRows({ children }: { readonly children: ReactNode }): JSX.Element {
  return <>{children}</>;
}
