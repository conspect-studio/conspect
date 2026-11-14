// The Routing page's Subtitle patch view (ADR-0110 §12.3/§12.8, #345 part F3):
// rows = sources that declare captions, columns = output subtitle slots
// (`GET /patch` outputs[].subtitle). Clicking a cell plans then takes a
// `subtitle` crosspoint via `web/src/api/patch.ts`, with a fresh
// Idempotency-Key per attempt; the outcome rides the realtime `route.output`
// event (`subtitle:applied` / `subtitle:refused:<reason>`).
//
// Self-contained: mounted once on the Routing page, does not touch
// RoutingPage's own structure — lane patch-e is building a full patch-matrix
// page in parallel on other branches, so this stays a drop-in component a
// later merge can relocate trivially.
import { useEffect, useMemo, useRef, useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { useQuery } from '@tanstack/react-query';
import {
  getPatch,
  planPatch,
  subtitleCrosspoint,
  subtitleSlotRedacted,
  subtitleSlotSource,
  takePatch,
} from '../../api/patch';
import type { PatchOutputView, PatchSubtitleSlot } from '../../api/patch';
import { OperationApiError } from '../../api/operations';
import { subscribeRealtime } from '../../realtime/shared';
import { useOutputs, useSources } from '../../resources/queries';
import {
  Table,
  TableBody,
  TableCaption,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '../ui/table';
import { Badge } from '../ui/badge';

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

/** One output column: its id, name and declared subtitle slots. */
interface Column {
  readonly output: string;
  readonly name: string;
  readonly slots: readonly PatchSubtitleSlot[];
}

function columnsOf(outputs: readonly PatchOutputView[], names: ReadonlyMap<string, string>): readonly Column[] {
  return outputs
    .filter((o) => o.subtitle.length > 0)
    .map((o) => ({ output: o.output, name: names.get(o.output) ?? o.output, slots: o.subtitle }));
}

/**
 * Per-destination (output + slot) pending/outcome state, keyed by
 * `${output}:${slot}`. One state per destination, not per source row: the
 * destination is what a take applies to, so its status is shown once per
 * column (review UI-5), covering both the captioned-source rows and the
 * None (unpatch) row.
 */
interface CellState {
  readonly operationId: string;
  readonly outcome?: string;
  readonly note?: string;
  readonly error?: string;
}

function cellKey(output: string, slot: number): string {
  return `${output}:${String(slot)}`;
}

/** Bounded ring of the most recent realtime outcomes, keyed by operation id. */
const RECENT_OUTCOMES = 32;

/** The Routing page's Subtitle patch view (ADR-0110 §12.8, #345 part F3). */
export function SubtitlePatchPanel(): JSX.Element | null {
  const { t } = useLingui();
  const sources = useSources();
  const outputs = useOutputs();
  const patch = useQuery({
    queryKey: ['patch', 'subtitle'],
    queryFn: () => getPatch(),
    refetchInterval: 4000,
  });
  const [cells, setCells] = useState<Readonly<Record<string, CellState>>>({});
  // One destination (output+slot) accepts only its LATEST attempt's result:
  // review UI2-1 found that two attempts on the same destination (e.g. a
  // second click while the first is still awaiting its take response) each
  // wrote `cells[key]` unconditionally, so an older attempt's late-arriving
  // 202/outcome could overwrite a newer attempt's already-displayed
  // refusal. `attemptSeq` is the latest dispatched attempt number per
  // destination key; every continuation (plan result, take acceptance, the
  // catch block, and settlement from the buffered/live outcome) re-checks
  // it against the attempt's own captured number before writing anything,
  // and ignores itself — never overwrites, never "wins" late — when it is
  // not the latest.
  const attemptSeq = useRef(new Map<string, number>());
  const isLatestAttempt = (key: string, attempt: number): boolean => attemptSeq.current.get(key) === attempt;
  // Destinations with at least one attempt still in flight (dispatched but
  // not yet settled by the LATEST attempt): their buttons are disabled
  // (aria-disabled + a reason in the status region) so a second click
  // cannot start a second concurrent attempt on the same destination while
  // one is outstanding.
  const [pendingKeys, setPendingKeys] = useState<ReadonlySet<string>>(new Set());
  const setPending = (key: string, pending: boolean): void => {
    setPendingKeys((current) => {
      const has = current.has(key);
      if (has === pending) return current;
      const next = new Set(current);
      if (pending) next.add(key);
      else next.delete(key);
      return next;
    });
  };

  // Outcomes are buffered by operation id from mount (not from the 202 on):
  // the realtime `route.output` event can arrive before `takePatch`'s HTTP
  // response resolves (e.g. the engine answers on the wire before this
  // client's own fetch promise settles), in which case a naive "only match
  // a cell already carrying this operation id" listener drops it forever —
  // a refused take would then show no refusal at all (review UI-1). Buffer
  // every subtitle outcome here (bounded, oldest evicted first) and
  // reconcile once the operation id is known, from either direction.
  const recentOutcomes = useRef(new Map<string, string>());

  // Destination labels must come from the OUTPUT resource list, never from
  // sources: an output and a source can share an id, and resolving the
  // name from sources would then label the destination as that unrelated
  // source — an operator could patch the wrong output under a plausible-
  // looking label (review UI-4).
  const outputNames = useMemo(
    () => new Map((outputs.data ?? []).map((o) => [o.id, o.name] as const)),
    [outputs.data],
  );
  const columns = useMemo(() => columnsOf(patch.data?.outputs ?? [], outputNames), [patch.data, outputNames]);
  const captionSources = useMemo(
    () => (sources.data ?? []).filter((s) => s.hasCaptions === true),
    [sources.data],
  );

  // One subscription for the panel's lifetime, independent of any take in
  // flight.
  useEffect(
    () =>
      subscribeRealtime('outputs', (envelope) => {
        if (envelope.t !== 'route.output') return;
        const data = envelope.data;
        if (!isRecord(data) || typeof data.operation_id !== 'string' || typeof data.outcome !== 'string') {
          return;
        }
        const operationId = data.operation_id;
        const outcome = data.outcome;
        if (!outcome.startsWith('subtitle:')) return;
        recentOutcomes.current.set(operationId, outcome);
        while (recentOutcomes.current.size > RECENT_OUTCOMES) {
          const oldest = recentOutcomes.current.keys().next().value;
          if (oldest === undefined) break;
          recentOutcomes.current.delete(oldest);
        }
        // A cell's `operationId` is only ever written by the destination's
        // latest attempt (every writer below is gated by
        // `isLatestAttempt`), so matching on it here already excludes a
        // superseded attempt's operation id without any extra bookkeeping.
        // The settled keys are collected here and applied to `pendingKeys`
        // in a separate call, outside the `cells` updater (an updater may
        // run more than once, e.g. under StrictMode, and must stay a pure
        // function of its previous state).
        const settledKeys: string[] = [];
        setCells((current) => {
          let changed: Record<string, CellState> | undefined;
          for (const [key, state] of Object.entries(current)) {
            if (state.operationId === operationId && state.outcome === undefined) {
              changed = { ...(changed ?? current), [key]: { ...state, outcome } };
              settledKeys.push(key);
            }
          }
          return changed ?? current;
        });
        for (const key of settledKeys) {
          setPending(key, false);
        }
      }),
    [],
  );

  if (columns.length === 0) {
    return null;
  }

  const runTake = async (output: string, slot: number, source: string | undefined): Promise<void> => {
    const key = cellKey(output, slot);
    const attempt = (attemptSeq.current.get(key) ?? 0) + 1;
    attemptSeq.current.set(key, attempt);
    const isLatest = (): boolean => isLatestAttempt(key, attempt);
    setPending(key, true);
    setCells((current) => ({ ...current, [key]: { operationId: '' } }));
    try {
      const crosspoint = subtitleCrosspoint(output, slot, source);
      const plan = await planPatch({ crosspoints: [crosspoint] });
      if (!isLatest()) return; // a newer attempt on this destination has started; ignore.
      const planned = plan.crosspoints[0];
      if (planned?.refusal) {
        setCells((current) => ({
          ...current,
          [key]: { operationId: '', error: planned.refusal?.detail ?? t`Refused` },
        }));
        setPending(key, false);
        return;
      }
      const accepted = await takePatch({ crosspoints: [crosspoint] });
      if (!isLatest()) return; // superseded while the take was in flight.
      const note = planned?.note;
      // The outcome may already have arrived (buffered above) while this
      // take's own HTTP response was still in flight.
      const already = recentOutcomes.current.get(accepted.operation_id);
      setCells((current) => ({
        ...current,
        [key]:
          note !== undefined && note !== null
            ? { operationId: accepted.operation_id, note, ...(already !== undefined ? { outcome: already } : {}) }
            : { operationId: accepted.operation_id, ...(already !== undefined ? { outcome: already } : {}) },
      }));
      // Settled already (the outcome had already been buffered) ⇒ no
      // longer pending; otherwise stay pending until the realtime listener
      // above settles it (or a newer attempt supersedes it, in which case
      // that newer attempt owns the pending flag from here on).
      if (already !== undefined) {
        setPending(key, false);
      }
    } catch (error) {
      if (!isLatest()) return; // a newer attempt owns this destination now.
      setCells((current) => ({
        ...current,
        [key]: {
          operationId: '',
          error: error instanceof OperationApiError ? (error.detail ?? error.message) : t`Take failed`,
        },
      }));
      setPending(key, false);
    }
  };

  /**
   * Destination-level status/error text for one column slot, if any:
   * an error (plan refusal or a thrown take) wins over everything else;
   * then a settled outcome; then the `source-has-no-captions` plan note;
   * then "pending" while a take is accepted but not yet settled (a
   * non-empty operation id with no outcome), or while any attempt on this
   * destination is still in flight. `undefined` ⇒ no status to show
   * (never attempted).
   */
  function statusOf(cellState: CellState | undefined, pending: boolean): JSX.Element | undefined {
    if (cellState === undefined) {
      return pending ? <span>{t`pending`}</span> : undefined;
    }
    if (cellState.error !== undefined) {
      return <span className="text-destructive">{cellState.error}</span>;
    }
    if (cellState.outcome !== undefined) {
      return <span>{cellState.outcome}</span>;
    }
    if (cellState.note === 'source-has-no-captions') {
      return (
        <span>
          <Trans>source-has-no-captions</Trans>
        </span>
      );
    }
    if (pending || cellState.operationId !== '') {
      return (
        <span>
          <Trans>pending</Trans>
        </span>
      );
    }
    return undefined;
  }

  return (
    <section className="grid gap-2" data-testid="subtitle-patch-panel" aria-label={t`Subtitle patch`}>
      <h3 className="text-sm font-medium">
        <Trans>Subtitle patch</Trans>
      </h3>
      <Table>
        <TableCaption>
          <Trans>Rows are sources that declare captions; columns are output subtitle slots.</Trans>
        </TableCaption>
        <TableHeader>
          <TableRow>
            <TableHead scope="col">
              <Trans>Source</Trans>
            </TableHead>
            {columns.flatMap((column) =>
              column.slots.map((slot) => (
                <TableHead key={cellKey(column.output, slot.slot)} scope="col">
                  {column.name} — {t`slot`} {slot.slot}
                  <div className="text-xs font-normal text-muted-foreground">{slot.format}</div>
                </TableHead>
              )),
            )}
          </TableRow>
        </TableHeader>
        <TableBody>
          {captionSources.length === 0 ? (
            <TableRow>
              <TableCell colSpan={1 + columns.reduce((n, c) => n + c.slots.length, 0)}>
                <Trans>No source declares captions.</Trans>
              </TableCell>
            </TableRow>
          ) : (
            captionSources.map((source) => (
              <TableRow key={source.id}>
                <TableCell>{source.name}</TableCell>
                {columns.flatMap((column) =>
                  column.slots.map((slot) => {
                    const key = cellKey(column.output, slot.slot);
                    const current = subtitleSlotSource(slot);
                    const redacted = subtitleSlotRedacted(slot);
                    const cellId = `subtitle-cell-${source.id}-${key}`;
                    const patched = current === source.id;
                    const busy = pendingKeys.has(key);
                    return (
                      <TableCell key={key} data-testid={cellId}>
                        {redacted ? (
                          <Badge variant="outline">
                            <Trans>redacted</Trans>
                          </Badge>
                        ) : (
                          <button
                            type="button"
                            className="h-9 rounded-md border px-2 text-sm"
                            aria-pressed={patched}
                            disabled={busy}
                            aria-disabled={busy}
                            aria-label={t`Patch ${source.name} to ${column.name} slot ${slot.slot}`}
                            onClick={(): void => {
                              void runTake(column.output, slot.slot, source.id);
                            }}
                          >
                            {patched ? <Trans>patched</Trans> : <Trans>patch</Trans>}
                          </button>
                        )}
                        <div className="mt-1 text-xs text-muted-foreground">
                          {slot.live ? <Trans>live</Trans> : <Trans>config</Trans>}
                        </div>
                      </TableCell>
                    );
                  }),
                )}
              </TableRow>
            ))
          )}
          <TableRow>
            <TableCell>
              <Trans>None (unpatch)</Trans>
            </TableCell>
            {columns.flatMap((column) =>
              column.slots.map((slot) => {
                const key = cellKey(column.output, slot.slot);
                const current = subtitleSlotSource(slot);
                const redacted = subtitleSlotRedacted(slot);
                const cellId = `subtitle-cell-none-${key}`;
                const busy = pendingKeys.has(key);
                return (
                  <TableCell key={key} data-testid={cellId}>
                    {redacted ? null : (
                      <button
                        type="button"
                        className="h-9 rounded-md border px-2 text-sm"
                        aria-pressed={current === undefined}
                        disabled={busy}
                        aria-disabled={busy}
                        aria-label={t`Unpatch ${column.name} slot ${slot.slot}`}
                        onClick={(): void => {
                          void runTake(column.output, slot.slot, undefined);
                        }}
                      >
                        <Trans>none</Trans>
                      </button>
                    )}
                  </TableCell>
                );
              }),
            )}
          </TableRow>
          {/*
            Destination-level status, rendered once per column (never
            duplicated per source row) and always present — including when
            zero sources declare captions, so an unpatch failure (no
            captioned row to carry it) is never silently dropped (review
            UI-5). `aria-live="polite"` + `role="status"` announce a change
            to assistive tech without the cell needing focus. While a
            column is disabled for a pending attempt (review UI2-1), this
            region also carries the reason ("pending").
          */}
          <TableRow data-testid="subtitle-status-row">
            <TableCell>
              <Trans>Status</Trans>
            </TableCell>
            {columns.flatMap((column) =>
              column.slots.map((slot) => {
                const key = cellKey(column.output, slot.slot);
                const cellState = cells[key];
                const busy = pendingKeys.has(key);
                const content = statusOf(cellState, busy);
                return (
                  <TableCell key={key} data-testid={`subtitle-status-${key}`}>
                    <span role="status" aria-live="polite" className="text-xs">
                      {content ?? <span className="sr-only">{t`No pending change`}</span>}
                    </span>
                  </TableCell>
                );
              }),
            )}
          </TableRow>
        </TableBody>
      </Table>
    </section>
  );
}
