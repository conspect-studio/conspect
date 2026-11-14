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
import { useEffect, useMemo, useReducer, useRef } from 'react';
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
import { EMPTY_SUBTITLE_ATTEMPTS, subtitleAttemptsReducer } from './subtitlePatchAttempts';
import type { SubtitleAttemptState } from './subtitlePatchAttempts';

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
  const [attempts, dispatch] = useReducer(subtitleAttemptsReducer, EMPTY_SUBTITLE_ATTEMPTS);
  // Per-destination monotonic attempt counter (review UI2-1): a plain ref
  // is enough here — it is read and incremented synchronously inside the
  // click handler, before any `await`, so there is no race in generating
  // the next attempt number itself; attempt OWNERSHIP (which attempt's
  // continuations are allowed to still write) lives entirely in the
  // reducer state above, compared via `isCurrentAttempt`.
  const attemptSeq = useRef(new Map<string, number>());

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
        dispatch({ type: 'outcome', operationId, outcome });
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
    dispatch({ type: 'start', key, attempt });
    try {
      const crosspoint = subtitleCrosspoint(output, slot, source);
      const plan = await planPatch({ crosspoints: [crosspoint] });
      const planned = plan.crosspoints[0];
      if (planned?.refusal) {
        dispatch({ type: 'planRefused', key, attempt, message: planned.refusal.detail });
        return;
      }
      const accepted = await takePatch({ crosspoints: [crosspoint] });
      const note = planned?.note;
      // The outcome may already have arrived (buffered above) while this
      // take's own HTTP response was still in flight.
      const already = recentOutcomes.current.get(accepted.operation_id);
      dispatch({
        type: 'accepted',
        key,
        attempt,
        operationId: accepted.operation_id,
        ...(note !== undefined && note !== null ? { note } : {}),
        ...(already !== undefined ? { outcome: already } : {}),
      });
    } catch (error) {
      dispatch({
        type: 'takeFailed',
        key,
        attempt,
        message: error instanceof OperationApiError ? (error.detail ?? error.message) : t`Take failed`,
      });
    }
  };

  /**
   * Destination-level status/error text for one column slot, if any:
   * an error (plan refusal or a thrown take) wins over everything else;
   * then a settled outcome; then the `source-has-no-captions` plan note;
   * then "pending" while busy (dispatched but not yet settled).
   * `undefined` ⇒ no status to show (never attempted).
   */
  function statusOf(dest: SubtitleAttemptState | undefined): JSX.Element | undefined {
    if (dest === undefined) return undefined;
    if (dest.error !== undefined) {
      return <span className="text-destructive">{dest.error}</span>;
    }
    if (dest.outcome !== undefined) {
      return <span>{dest.outcome}</span>;
    }
    if (dest.note === 'source-has-no-captions') {
      return (
        <span>
          <Trans>source-has-no-captions</Trans>
        </span>
      );
    }
    if (dest.busy) {
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
                    const busy = attempts[key]?.busy ?? false;
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
                const busy = attempts[key]?.busy ?? false;
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
                const content = statusOf(attempts[key]);
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
