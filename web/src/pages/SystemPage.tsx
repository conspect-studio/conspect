// System — the live host/GPU telemetry detail page. Streams the same `system`
// realtime topic the footer uses ({@link useSystemMetrics}) and renders the host
// summary plus a card per GPU (vendor, name, compute util, VRAM, encoder/decoder
// util, NVENC sessions/ceiling) with larger sparklines. All reads are
// best-effort and never block the engine (invariant #10). Status is conveyed by
// VALUE + a labelled meter, never colour alone (WCAG 1.4.1).
import type { JSX } from "react";
import { Trans, useLingui } from "@lingui/react/macro";

import { PageHeader } from "../components/PageHeader";
import { Sparkline } from "../components/Sparkline";
import { ConnectionStatus } from "../components/ConnectionStatus";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "../components/ui/card";
import { useActiveLocale } from "../i18n/I18nProvider";
import { formatBytes, formatFps, formatPercent } from "../i18n/format";
import { useSystemMetrics } from "../realtime/useSystemMetrics";
import type { GpuMetrics, GpuVendor } from "../realtime/useSystemMetrics";

/** Clamp a 0..1 ratio to a 0..100 integer percent for a meter width/value. */
function toPercent(ratio: number): number {
  return Math.min(100, Math.max(0, Math.round(ratio * 100)));
}

/**
 * A labelled horizontal meter. The numeric text carries the value; the bar is a
 * redundant visual cue, so meaning is never colour-only.
 *
 * When `selfRatio` is given, OUR-process share of the same total is drawn as a
 * distinct OUTLINED inset bar (a dashed border, not a colour) over the filled
 * device total, and the value text spells out `us X / Y` — so ours-vs-total is
 * legible without relying on colour (WCAG 1.4.1). When `selfRatio` is undefined
 * (not attributable) only the total is shown.
 */
function Meter(props: {
  readonly label: JSX.Element;
  readonly ratio: number;
  readonly valueText: string;
  readonly selfRatio?: number;
  /** Accessible description of the ours-vs-total relation for the meter. */
  readonly ariaLabel?: string;
}): JSX.Element {
  const pct = toPercent(props.ratio);
  const selfPct =
    props.selfRatio === undefined ? undefined : toPercent(props.selfRatio);
  return (
    <div>
      <div className="mb-1 flex items-center justify-between gap-2 text-sm">
        <span className="text-muted-foreground">{props.label}</span>
        <span className="font-medium tabular-nums">{props.valueText}</span>
      </div>
      <div
        role="meter"
        aria-valuenow={pct}
        aria-valuemin={0}
        aria-valuemax={100}
        {...(props.ariaLabel === undefined ? {} : { "aria-label": props.ariaLabel })}
        className="relative h-2 w-full overflow-hidden rounded-full bg-muted"
      >
        <div
          className="h-full rounded-full bg-foreground/40"
          style={{ width: `${String(pct)}%` }}
        />
        {selfPct === undefined ? null : (
          // Our-share marker: a dashed outline (shape, not colour) from the
          // origin to our fraction, overlaid on the lighter total fill.
          <div
            aria-hidden="true"
            className="absolute inset-y-0 left-0 rounded-full border border-dashed border-foreground/80"
            style={{ width: `${String(selfPct)}%` }}
          />
        )}
      </div>
    </div>
  );
}

/** Map the vendor enum to a localised display name (vendor is not colour-coded). */
function vendorLabel(vendor: GpuVendor): JSX.Element {
  switch (vendor) {
    case "nvidia":
      return <Trans>NVIDIA</Trans>;
    case "intel":
      return <Trans>Intel</Trans>;
    case "amd":
      return <Trans>AMD</Trans>;
    case "apple":
      return <Trans>Apple</Trans>;
    case "other":
      return <Trans>Other</Trans>;
  }
}

/**
 * A utilisation value text. With an attributed `self` share it reads `us X / Y`
 * (ours of the device total); otherwise just the total `Y`. Never `undefined`.
 */
function pctOursVsTotal(
  locale: string,
  total: number,
  self: number | undefined,
): string {
  const totalText = formatPercent(locale, total);
  return self === undefined
    ? totalText
    : `${formatPercent(locale, self)} / ${totalText}`;
}

/** One GPU's detail card. Every metric is shown as OURS vs the device TOTAL. */
function GpuCard(props: { readonly gpu: GpuMetrics }): JSX.Element {
  const { t } = useLingui();
  const locale = useActiveLocale();
  const { gpu } = props;
  const vramRatio = gpu.mem_total_bytes > 0 ? gpu.mem_used_bytes / gpu.mem_total_bytes : 0;
  const selfVramRatio =
    gpu.self_mem_used_bytes !== undefined && gpu.mem_total_bytes > 0
      ? gpu.self_mem_used_bytes / gpu.mem_total_bytes
      : undefined;
  const vramValue =
    gpu.self_mem_used_bytes === undefined
      ? `${formatBytes(locale, gpu.mem_used_bytes)} / ${formatBytes(locale, gpu.mem_total_bytes)}`
      : `${formatBytes(locale, gpu.self_mem_used_bytes)} / ${formatBytes(locale, gpu.mem_used_bytes)} / ${formatBytes(locale, gpu.mem_total_bytes)}`;

  return (
    <Card>
      <CardHeader className="pb-3">
        <CardTitle className="truncate text-base" lang="" dir="auto">
          {gpu.name ?? gpu.id}
        </CardTitle>
        <CardDescription>
          {vendorLabel(gpu.vendor)}
          <span aria-hidden="true"> · </span>
          <code className="text-xs" lang="" dir="auto">
            {gpu.id}
          </code>
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        <Meter
          label={<Trans>Compute</Trans>}
          ratio={gpu.compute_util}
          {...(gpu.self_compute_util === undefined
            ? {}
            : { selfRatio: gpu.self_compute_util })}
          valueText={pctOursVsTotal(locale, gpu.compute_util, gpu.self_compute_util)}
          ariaLabel={
            gpu.self_compute_util === undefined
              ? t`Compute utilisation (device total)`
              : t`Compute utilisation: ours over device total`
          }
        />
        <Meter
          label={<Trans>VRAM</Trans>}
          ratio={vramRatio}
          {...(selfVramRatio === undefined ? {} : { selfRatio: selfVramRatio })}
          valueText={vramValue}
          ariaLabel={
            selfVramRatio === undefined
              ? t`VRAM used of total`
              : t`VRAM: ours / device used / device total`
          }
        />
        {gpu.encoder_util !== undefined ? (
          <Meter
            label={<Trans>Encoder</Trans>}
            ratio={gpu.encoder_util}
            {...(gpu.self_encoder_util === undefined
              ? {}
              : { selfRatio: gpu.self_encoder_util })}
            valueText={pctOursVsTotal(locale, gpu.encoder_util, gpu.self_encoder_util)}
            ariaLabel={
              gpu.self_encoder_util === undefined
                ? t`Encoder utilisation (device total)`
                : t`Encoder utilisation: ours over device total`
            }
          />
        ) : null}
        {gpu.decoder_util !== undefined ? (
          <Meter
            label={<Trans>Decoder</Trans>}
            ratio={gpu.decoder_util}
            {...(gpu.self_decoder_util === undefined
              ? {}
              : { selfRatio: gpu.self_decoder_util })}
            valueText={pctOursVsTotal(locale, gpu.decoder_util, gpu.self_decoder_util)}
            ariaLabel={
              gpu.self_decoder_util === undefined
                ? t`Decoder utilisation (device total)`
                : t`Decoder utilisation: ours over device total`
            }
          />
        ) : null}
        {gpu.encoder_sessions !== undefined ? (
          <div className="flex items-center justify-between gap-2 text-sm">
            <span className="text-muted-foreground">
              <Trans>NVENC sessions</Trans>
            </span>
            <span
              className="font-medium tabular-nums"
              aria-label={
                gpu.self_encoder_sessions === undefined
                  ? t`Active NVENC sessions out of the discovered ceiling`
                  : t`Our NVENC sessions / device-wide sessions / ceiling`
              }
            >
              {gpu.self_encoder_sessions === undefined
                ? null
                : `${String(gpu.self_encoder_sessions)} / `}
              {String(gpu.encoder_sessions)}
              {" / "}
              {gpu.encoder_session_ceiling === undefined
                ? "∞"
                : String(gpu.encoder_session_ceiling)}
            </span>
          </div>
        ) : null}
      </CardContent>
    </Card>
  );
}

/** The System telemetry page. */
export function SystemPage(): JSX.Element {
  const { t } = useLingui();
  const locale = useActiveLocale();
  const { status, current, series } = useSystemMetrics();

  return (
    <>
      <PageHeader
        title={<Trans>System</Trans>}
        description={
          <Trans>
            Live host and accelerator utilisation streaming from the engine.
          </Trans>
        }
        actions={<ConnectionStatus status={status} />}
      />

      <section aria-labelledby="host-heading">
        <h2 id="host-heading" className="mb-3 text-lg font-semibold">
          <Trans>Host</Trans>
        </h2>
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
          <Card>
            <CardHeader className="pb-2">
              <CardTitle className="text-base">
                <Trans>CPU</Trans>
              </CardTitle>
              <CardDescription>
                <Trans>Ours of host total</Trans>
              </CardDescription>
            </CardHeader>
            <CardContent className="space-y-2">
              <p className="text-3xl font-semibold tabular-nums">
                {current === undefined
                  ? "—"
                  : pctOursVsTotal(locale, current.cpu_util, current.self_cpu_util)}
              </p>
              <Sparkline
                values={series.cpu}
                overlay={series.selfCpu}
                ariaLabel={t`CPU utilisation trend: ours over host total`}
                width={220}
                height={40}
                className="w-full text-foreground/70"
              />
            </CardContent>
          </Card>

          <Card>
            <CardHeader className="pb-2">
              <CardTitle className="text-base">
                <Trans>Memory</Trans>
              </CardTitle>
            </CardHeader>
            <CardContent>
              {current?.mem_used_bytes !== undefined &&
              current.mem_total_bytes !== undefined ? (
                <Meter
                  label={<Trans>Used</Trans>}
                  ratio={
                    current.mem_total_bytes > 0
                      ? current.mem_used_bytes / current.mem_total_bytes
                      : 0
                  }
                  {...(current.self_mem_used_bytes !== undefined &&
                  current.mem_total_bytes > 0
                    ? { selfRatio: current.self_mem_used_bytes / current.mem_total_bytes }
                    : {})}
                  valueText={
                    current.self_mem_used_bytes === undefined
                      ? `${formatBytes(locale, current.mem_used_bytes)} / ${formatBytes(locale, current.mem_total_bytes)}`
                      : `${formatBytes(locale, current.self_mem_used_bytes)} / ${formatBytes(locale, current.mem_used_bytes)} / ${formatBytes(locale, current.mem_total_bytes)}`
                  }
                  ariaLabel={
                    current.self_mem_used_bytes === undefined
                      ? t`Host memory used of total`
                      : t`Host memory: ours / used / total`
                  }
                />
              ) : (
                <p className="text-sm text-muted-foreground">
                  <Trans>No host-memory sample.</Trans>
                </p>
              )}
            </CardContent>
          </Card>

          <Card>
            <CardHeader className="pb-2">
              <CardTitle className="text-base">
                <Trans>Program output</Trans>
              </CardTitle>
            </CardHeader>
            <CardContent className="space-y-2">
              <p className="text-3xl font-semibold tabular-nums">
                {current?.program_fps === undefined
                  ? "—"
                  : formatFps(locale, current.program_fps)}
              </p>
              <Sparkline
                values={series.fps}
                max={Math.max(1, ...series.fps)}
                ariaLabel={t`Program output frame rate trend`}
                width={220}
                height={40}
                className="w-full text-foreground/70"
              />
            </CardContent>
          </Card>
        </div>
      </section>

      <section aria-labelledby="gpus-heading" className="mt-8">
        <h2 id="gpus-heading" className="mb-3 text-lg font-semibold">
          <Trans>GPUs</Trans>
        </h2>
        {current === undefined ? (
          <p role="status" aria-live="polite" className="text-sm text-muted-foreground">
            <Trans>Waiting for the first metrics sample…</Trans>
          </p>
        ) : current.gpus.length === 0 ? (
          <p className="text-sm text-muted-foreground">
            <Trans>No GPUs are reported on this host.</Trans>
          </p>
        ) : (
          <ul className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
            {current.gpus.map((gpu) => (
              <li key={gpu.id}>
                <GpuCard gpu={gpu} />
              </li>
            ))}
          </ul>
        )}
      </section>
    </>
  );
}
