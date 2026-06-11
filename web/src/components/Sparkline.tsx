// A dependency-free inline-SVG sparkline (no chart library).
//
// Renders the recent `values` as a single polyline normalised into the [0, max]
// band (default max = 1, i.e. a 0..1 utilisation ratio). The SVG origin is
// top-left, so we invert the y-axis: a higher value sits higher on the chart.
// The element is exposed as `role="img"` with the caller's `aria-label` so the
// trend is announced as a single accessible graphic (WCAG 1.1.1) — the numeric
// "current value" is rendered as visible text alongside it by callers, so the
// chart is decorative-but-labelled, never the sole carrier of meaning.
//
// An optional `overlay` series is drawn on the SAME band as a DASHED line, so a
// second quantity (e.g. OUR-process share against the device total) is told
// apart by line STYLE, not colour alone (WCAG 1.4.1). Overlay entries are
// `number | undefined`; an `undefined` tick (unattributed) breaks the dashed
// line into a gap rather than reading as 0.
import type { JSX } from "react";

import { cn } from "../lib/utils";

/** Props for {@link Sparkline}. */
export interface SparklineProps {
  /** The series to plot, oldest first. Values are clamped to `[0, max]`. */
  readonly values: readonly number[];
  /**
   * An optional secondary series on the SAME band, drawn DASHED so it is
   * distinguished from {@link values} by line style (not colour). Index-aligned
   * to `values`; an `undefined` entry is a gap (no point plotted there).
   */
  readonly overlay?: readonly (number | undefined)[];
  /** The top of the normalisation band (default `1`, i.e. a 0..1 ratio). */
  readonly max?: number;
  /** Viewbox width in user units (default `64`). */
  readonly width?: number;
  /** Viewbox height in user units (default `20`). */
  readonly height?: number;
  /** The accessible name announced for the trend (required). */
  readonly ariaLabel: string;
  /** Optional extra classes for the `<svg>`. */
  readonly className?: string;
}

/** Round to at most 2 decimals and drop a trailing `.0` so points stay compact. */
function trim(n: number): string {
  return String(Math.round(n * 100) / 100);
}

/** Project a value into the inverted [0, height] band, clamped to `[0, top]`. */
function projectY(value: number, top: number, height: number): number {
  const clamped = Math.min(top, Math.max(0, value));
  // Invert: value `top` -> y 0 (top edge); value 0 -> y `height` (bottom).
  return height - (clamped / top) * height;
}

/** Split an aligned overlay into contiguous runs (gaps at `undefined`). */
function overlayRuns(
  overlay: readonly (number | undefined)[],
  step: number,
  top: number,
  height: number,
): string[] {
  const runs: string[] = [];
  let run: string[] = [];
  const flush = (): void => {
    if (run.length > 0) {
      runs.push(run.join(" "));
      run = [];
    }
  };
  overlay.forEach((value, index) => {
    if (value === undefined) {
      flush();
      return;
    }
    run.push(`${trim(index * step)},${trim(projectY(value, top, height))}`);
  });
  flush();
  return runs;
}

/**
 * A tiny inline-SVG trend line. Stateless and theme-aware (the stroke uses
 * `currentColor`, so the caller's text colour drives it).
 */
export function Sparkline({
  values,
  overlay,
  max = 1,
  width = 64,
  height = 20,
  ariaLabel,
  className,
}: SparklineProps): JSX.Element {
  // A non-positive `max` would divide by zero; fall back to a unit band.
  const top = max > 0 ? max : 1;
  // One x-step per gap between samples; a single sample sits at x=0.
  const step = values.length > 1 ? width / (values.length - 1) : 0;

  const points = values
    .map((value, index): string => {
      const x = index * step;
      return `${trim(x)},${trim(projectY(value, top, height))}`;
    })
    .join(" ");

  const overlaySegments =
    overlay === undefined ? [] : overlayRuns(overlay, step, top, height);

  return (
    <svg
      role="img"
      aria-label={ariaLabel}
      viewBox={`0 0 ${trim(width)} ${trim(height)}`}
      width={width}
      height={height}
      preserveAspectRatio="none"
      className={cn("overflow-visible", className)}
    >
      {points.length > 0 ? (
        <polyline
          points={points}
          fill="none"
          stroke="currentColor"
          strokeWidth={1.5}
          strokeLinejoin="round"
          strokeLinecap="round"
          vectorEffect="non-scaling-stroke"
        />
      ) : null}
      {overlaySegments.map((segment) => (
        // The first "x,y" of a run is a stable, unique key within this render:
        // runs are non-overlapping by construction, so their start point differs.
        <polyline
          key={segment.slice(0, segment.indexOf(" "))}
          points={segment}
          fill="none"
          stroke="currentColor"
          strokeOpacity={0.85}
          strokeWidth={1.25}
          strokeDasharray="2 2"
          strokeLinejoin="round"
          strokeLinecap="round"
          vectorEffect="non-scaling-stroke"
        />
      ))}
    </svg>
  );
}
