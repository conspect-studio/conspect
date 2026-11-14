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
import { useSources } from '../../resources/queries';
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

/** Per-cell pending/outcome state, keyed by `${output}:${slot}`. */
interface CellState {
  readonly operationId: string;
  readonly outcome?: string;
  readonly note?: string;
  readonly error?: string;
}

function cellKey(output: string, slot: number): string {
  return `${output}:${String(slot)}`;
}

/** The Routing page's Subtitle patch view (ADR-0110 §12.8, #345 part F3). */
export function SubtitlePatchPanel(): JSX.Element | null {
  const { t } = useLingui();
  const sources = useSources();
  const patch = useQuery({
    queryKey: ['patch', 'subtitle'],
    queryFn: () => getPatch(),
    refetchInterval: 4000,
  });
  const [cells, setCells] = useState<Readonly<Record<string, CellState>>>({});
  const cellsRef = useRef(cells);
  useEffect(() => {
    cellsRef.current = cells;
  }, [cells]);

  const outputNames = useMemo(
    () => new Map((sources.data ?? []).map((s) => [s.id, s.name] as const)),
    [sources.data],
  );
  const columns = useMemo(() => columnsOf(patch.data?.outputs ?? [], outputNames), [patch.data, outputNames]);
  const captionSources = useMemo(
    () => (sources.data ?? []).filter((s) => s.hasCaptions === true),
    [sources.data],
  );

  // One subscription for the panel's lifetime: an outcome may arrive before
  // the take's 202 response (or before a per-cell effect runs).
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
        setCells((current) => {
          let changed: Record<string, CellState> | undefined;
          for (const [key, state] of Object.entries(current)) {
            if (state.operationId === operationId && state.outcome === undefined) {
              changed = { ...(changed ?? current), [key]: { ...state, outcome } };
            }
          }
          return changed ?? current;
        });
      }),
    [],
  );

  if (columns.length === 0) {
    return null;
  }

  const runTake = async (output: string, slot: number, source: string | undefined): Promise<void> => {
    const key = cellKey(output, slot);
    setCells((current) => ({ ...current, [key]: { operationId: '' } }));
    try {
      const crosspoint = subtitleCrosspoint(output, slot, source);
      const plan = await planPatch({ crosspoints: [crosspoint] });
      const planned = plan.crosspoints[0];
      if (planned?.refusal) {
        setCells((current) => ({
          ...current,
          [key]: { operationId: '', error: planned.refusal?.detail ?? t`Refused` },
        }));
        return;
      }
      const accepted = await takePatch({ crosspoints: [crosspoint] });
      const note = planned?.note;
      setCells((current) => ({
        ...current,
        [key]: note !== undefined && note !== null
          ? { operationId: accepted.operation_id, note }
          : { operationId: accepted.operation_id },
      }));
    } catch (error) {
      setCells((current) => ({
        ...current,
        [key]: {
          operationId: '',
          error: error instanceof OperationApiError ? (error.detail ?? error.message) : t`Take failed`,
        },
      }));
    }
  };

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
                    const cellState = cells[key];
                    const patched = current === source.id;
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
                            aria-label={t`Patch ${source.name} to ${column.name} slot ${slot.slot}`}
                            onClick={(): void => {
                              void runTake(column.output, slot.slot, source.id);
                            }}
                          >
                            {patched ? <Trans>patched</Trans> : <Trans>patch</Trans>}
                          </button>
                        )}
                        <div className="mt-1 flex flex-wrap items-center gap-1 text-xs text-muted-foreground">
                          <span>{slot.live ? <Trans>live</Trans> : <Trans>config</Trans>}</span>
                          {cellState?.note === 'source-has-no-captions' ? (
                            <span data-testid={`${cellId}-note`}>
                              <Trans>source-has-no-captions</Trans>
                            </span>
                          ) : null}
                          {cellState?.error !== undefined ? (
                            <span role="alert" data-testid={`${cellId}-error`} className="text-destructive">
                              {cellState.error}
                            </span>
                          ) : null}
                          {cellState?.outcome !== undefined ? (
                            <span data-testid={`${cellId}-outcome`}>{cellState.outcome}</span>
                          ) : null}
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
                return (
                  <TableCell key={key} data-testid={cellId}>
                    {redacted ? null : (
                      <button
                        type="button"
                        className="h-9 rounded-md border px-2 text-sm"
                        aria-pressed={current === undefined}
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
        </TableBody>
      </Table>
    </section>
  );
}
