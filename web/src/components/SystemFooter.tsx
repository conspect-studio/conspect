// The live system-metrics footer: a sticky, desktop-only status bar that shows
// the host/GPU utilisation streaming over the `system` realtime topic. Each cell
// pairs a tiny label, a {@link Sparkline} of the recent series, and the current
// numeric value. The box is shared with co-tenant processes, so device-wide
// totals are NOT all ours: every cell shows OURS vs TOTAL, told apart WITHOUT
// colour (explicit `us X / Y` text + a solid total line with a dashed "ours"
// overlay), per WCAG 1.4.1. ALL GPUs are shown (a tight per-GPU mini-group),
// not just the first. The whole bar is a button that navigates to the System
// detail page. Status is conveyed by VALUE + SHAPE (a filled vs hollow live
// dot). All visible words are localised.
import type { JSX } from "react";
import { Trans, useLingui } from "@lingui/react/macro";
import { useNavigate } from "react-router-dom";

import { useActiveLocale } from "../i18n/I18nProvider";
import { formatFps, formatPercent } from "../i18n/format";
import { useSystemMetrics } from "../realtime/useSystemMetrics";
import type { GpuMetrics, GpuSeries } from "../realtime/useSystemMetrics";
import type { RealtimeStatus } from "../realtime/connection";
import { Sparkline } from "./Sparkline";
import { cn } from "../lib/utils";

/** A live indicator whose shape (filled vs hollow) carries meaning, not colour. */
function LiveDot({ status }: { readonly status: RealtimeStatus }): JSX.Element {
  const { t } = useLingui();
  const open = status === "open";
  const label = open ? t`Live` : t`Offline`;
  return (
    <span className="flex items-center gap-1" role="status" aria-live="polite">
      <span
        aria-hidden="true"
        className={cn(
          "size-2 rounded-full border",
          open
            ? "border-status-live bg-status-live"
            : "border-muted-foreground bg-transparent",
        )}
      />
      <span className="text-[10px] uppercase tracking-wide text-muted-foreground">
        {label}
      </span>
    </span>
  );
}

/**
 * Render an "ours / total" value. When `self` is undefined (the platform can't
 * attribute our share) only the total is shown — never `undefined` or a false 0.
 * The `us` prefix + slash make the relation legible without colour.
 */
function oursVsTotal(
  total: string | undefined,
  self: string | undefined,
): string {
  if (total === undefined) {
    return "—";
  }
  return self === undefined ? total : `${self} / ${total}`;
}

/** One footer cell: a label, a sparkline (total + optional dashed "ours"), value. */
interface CellProps {
  readonly label: JSX.Element;
  readonly series: number[];
  /** Optional "ours" series, drawn dashed over the total (colour-independent). */
  readonly overlay?: (number | undefined)[];
  readonly value: string;
  /** The sparkline normalisation ceiling (default 1, i.e. a 0..1 ratio). */
  readonly max?: number;
  /** Accessible name for the sparkline graphic. */
  readonly ariaLabel: string;
}

function Cell({
  label,
  series,
  overlay,
  value,
  max = 1,
  ariaLabel,
}: CellProps): JSX.Element {
  return (
    <div className="flex min-w-0 items-center gap-2">
      <div className="flex flex-col leading-tight">
        <span className="text-[10px] uppercase tracking-wide text-muted-foreground">
          {label}
        </span>
        <span className="text-xs font-semibold tabular-nums">{value}</span>
      </div>
      <Sparkline
        values={series}
        {...(overlay === undefined ? {} : { overlay })}
        max={max}
        ariaLabel={ariaLabel}
        width={48}
        height={18}
        className="text-foreground/70"
      />
    </div>
  );
}

/** The peak of a series (for an auto-scaled sparkline), or `1` when flat/empty. */
function seriesMax(series: number[]): number {
  const peak = series.reduce((acc, v) => Math.max(acc, v), 0);
  return peak > 0 ? peak : 1;
}

/** A short device label, `GPU{index}`, used as the per-GPU mini-group title. */
function gpuShortLabel(index: number): string {
  return `GPU${String(index)}`;
}

/** A compact per-GPU mini-group: compute (ours/total) + NVENC (ours/total). */
function GpuGroup(props: {
  readonly index: number;
  readonly gpu: GpuMetrics;
  readonly series: GpuSeries | undefined;
}): JSX.Element {
  const { t } = useLingui();
  const locale = useActiveLocale();
  const { index, gpu, series } = props;
  const short = gpuShortLabel(index);

  const computeTotal = formatPercent(locale, gpu.compute_util);
  const computeSelf =
    gpu.self_compute_util === undefined
      ? undefined
      : formatPercent(locale, gpu.self_compute_util);
  const nvencTotal =
    gpu.encoder_sessions === undefined
      ? undefined
      : String(gpu.encoder_sessions);
  const nvencSelf =
    gpu.self_encoder_sessions === undefined
      ? undefined
      : String(gpu.self_encoder_sessions);

  const computeSeries = series?.compute ?? [];
  const computeOverlay = series?.selfCompute;
  const nvencSeries = series?.nvenc ?? [];

  // An aria description spelling out ours-vs-total so screen-reader users get the
  // same relation the dashed overlay conveys visually.
  const computeAria =
    computeSelf === undefined
      ? t`${short} compute utilisation ${computeTotal} (device total)`
      : t`${short} compute utilisation: ours ${computeSelf} of ${computeTotal} device total`;

  return (
    <div className="flex min-w-0 items-center gap-2">
      <div className="flex flex-col leading-tight">
        <span className="text-[10px] uppercase tracking-wide text-muted-foreground">
          {short}
        </span>
        <span className="text-xs font-semibold tabular-nums">
          {oursVsTotal(computeTotal, computeSelf)}
        </span>
        <span className="text-[10px] tabular-nums text-muted-foreground">
          <Trans>NVENC</Trans> {oursVsTotal(nvencTotal, nvencSelf)}
        </span>
      </div>
      <Sparkline
        values={computeSeries}
        {...(computeOverlay === undefined ? {} : { overlay: computeOverlay })}
        ariaLabel={computeAria}
        width={48}
        height={18}
        className="text-foreground/70"
      />
      <Sparkline
        values={nvencSeries}
        max={seriesMax(nvencSeries)}
        ariaLabel={t`${short} NVENC encode sessions trend`}
        width={32}
        height={18}
        className="text-foreground/50"
      />
    </div>
  );
}

/** Match a GPU's series by stable id; falls back to undefined if absent. */
function seriesForGpu(
  gpu: GpuMetrics,
  gpuSeries: readonly GpuSeries[],
): GpuSeries | undefined {
  return gpuSeries.find((s) => s.id === gpu.id);
}

/**
 * The sticky system-metrics footer. Desktop-only; on smaller viewports the
 * System page carries the same data. Rendering is best-effort: an empty state
 * shows dashes until the first sample arrives, and the bar never blocks (engine
 * isolation, inv #10).
 */
export function SystemFooter(): JSX.Element {
  const { t } = useLingui();
  const locale = useActiveLocale();
  const navigate = useNavigate();
  const { status, current, series, gpuSeries } = useSystemMetrics();

  const cpuTotal =
    current === undefined ? undefined : formatPercent(locale, current.cpu_util);
  const cpuSelf =
    current?.self_cpu_util === undefined
      ? undefined
      : formatPercent(locale, current.self_cpu_util);
  const cpuValue = oursVsTotal(cpuTotal, cpuSelf);

  const fpsValue =
    current?.program_fps === undefined
      ? "—"
      : formatFps(locale, current.program_fps);

  const gpus: readonly GpuMetrics[] = current?.gpus ?? [];

  return (
    <footer className="sticky bottom-0 z-30 hidden border-t bg-card md:block">
      <button
        type="button"
        onClick={(): void => {
          void navigate("/system");
        }}
        aria-label={t`Open the system metrics page`}
        className="flex w-full items-center gap-5 overflow-x-auto px-4 py-1.5 text-start hover:bg-accent/40 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
      >
        <LiveDot status={status} />
        <Cell
          label={<Trans>CPU</Trans>}
          series={series.cpu}
          overlay={series.selfCpu}
          value={cpuValue}
          ariaLabel={
            cpuSelf === undefined
              ? t`CPU utilisation trend (host total)`
              : t`CPU utilisation trend: ours over host total`
          }
        />
        {gpus.length === 0 ? (
          <Cell
            label={<Trans>GPU</Trans>}
            series={[]}
            value="—"
            ariaLabel={t`No GPU reported`}
          />
        ) : (
          gpus.map((gpu, index) => (
            <GpuGroup
              key={gpu.id}
              index={index}
              gpu={gpu}
              series={seriesForGpu(gpu, gpuSeries)}
            />
          ))
        )}
        <Cell
          label={<Trans>PROG</Trans>}
          series={series.fps}
          value={fpsValue}
          max={seriesMax(series.fps)}
          ariaLabel={t`Program output frame rate trend`}
        />
      </button>
    </footer>
  );
}
