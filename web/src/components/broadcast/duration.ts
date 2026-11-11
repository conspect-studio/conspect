// Per-asset duration math for the broadcast surfaces — the production fix for
// the prototype's two duration defects (correction-4-review-comfort §N8 and
// the safety review's rate/marks criteria):
//   * Audio assets format through their SAMPLE grid (elapsed clock units),
//     never through a frame-based timecode that needs an fps the asset does
//     not have — a valid 48 kHz asset can never read INVALID.
//   * Video cadence is an exact RATIONAL (numerator/denominator, e.g.
//     30000/1001). NDF timecode labels count ceil(n/d) frames per second and
//     the boundary keeps the integer frame index plus the exact rational pair
//     — never a divided float fps.
//   * An asset with no usable grid is UNKNOWN, never a fabricated precision
//     grid; a known grid without a measured length is UNMEASURED, distinct
//     from unknown; a derivative with no grid of its own falls back to its
//     parent's grid and says so.
// Pure functions only: no React, no i18n, no side effects.

/** The result of formatting one asset's duration. */
export interface AssetDurationFormat {
  /** `formatted` = a real label; `unmeasured` = cadence known, length absent; `unknown` = no usable cadence. */
  readonly state: 'formatted' | 'unmeasured' | 'unknown';
  /** The duration label, or null when no honest label exists. */
  readonly duration: string | null;
  /** The exact rational boundary at the asset's length, or null. */
  readonly boundary: RationalBoundary | null;
  /** The grid the duration was measured on, or null when none is known. */
  readonly grid: DurationGrid | null;
  /** Whether the grid was measured on this asset or inherited from its parent. */
  readonly gridOrigin: 'measured' | 'inherited' | 'none';
}

/** An audio cadence: the asset's sample rate in Hz. */
export interface AudioGrid {
  readonly kind: 'audio';
  readonly sampleRateHz: number;
}

/** A video cadence: an exact rational frame rate, never a divided float. */
export interface VideoGrid {
  readonly kind: 'video';
  readonly numerator: number;
  readonly denominator: number;
}

export type DurationGrid = AudioGrid | VideoGrid;

/** Where a duration's length was measured, in the grid's own integer units. */
export interface AssetDurationSource {
  /** The asset's measured cadence; null/absent when not measured. */
  readonly grid?: DurationGrid | null | undefined;
  /** Sample count (audio grids) or frame count (video grids); null/absent when not measured. */
  readonly units?: number | null | undefined;
}

/**
 * An exact rational position on an asset's grid — the boundary metadata a
 * derivative stores (correction-4 safety: `gridStartBoundary`/`gridEndBoundary`
 * kept `{index, numerator, denominator, kind}` — the rational pair, never the
 * divided rate).
 */
export type RationalBoundary =
  | {
      readonly kind: 'frames';
      readonly index: number;
      readonly numerator: number;
      readonly denominator: number;
    }
  | {
      readonly kind: 'samples';
      readonly index: number;
      readonly sampleRateHz: number;
    };

function isUsableGrid(
  grid: DurationGrid | null | undefined,
): grid is DurationGrid {
  if (grid === null || grid === undefined) {
    return false;
  }
  if (grid.kind === 'audio') {
    return Number.isSafeInteger(grid.sampleRateHz) && grid.sampleRateHz >= 1;
  }
  return (
    Number.isSafeInteger(grid.numerator) &&
    grid.numerator >= 1 &&
    Number.isSafeInteger(grid.denominator) &&
    grid.denominator >= 1
  );
}

function isUsableUnits(
  units: number | null | undefined,
): units is number {
  return (
    units !== null &&
    units !== undefined &&
    Number.isSafeInteger(units) &&
    units >= 0
  );
}

function pad(value: bigint, width: number): string {
  return String(value).padStart(width, '0');
}

/** Elapsed audio clock label, `HH:MM:SS.mmm`, floored at the millisecond. */
function elapsedClock(totalMs: bigint): string {
  const hours = totalMs / 3_600_000n;
  const minutes = totalMs / 60_000n % 60n;
  const seconds = totalMs / 1000n % 60n;
  const millis = totalMs % 1000n;
  return `${pad(hours, 2)}:${pad(minutes, 2)}:${pad(seconds, 2)}.${pad(millis, 3)}`;
}

/**
 * NDF video timecode label, `HH:MM:SS:FF`, where the frame field counts
 * ceil(numerator/denominator) frames per second — the integer NDF labels the
 * broadcast reviews require (a 30000/1001 asset labels 30 frames a second;
 * the exact position stays the integer index).
 */
function ndfTimecode(frames: bigint, framesPerSecond: bigint): string {
  const secondsTotal = frames / framesPerSecond;
  const hours = secondsTotal / 3600n;
  const minutes = secondsTotal / 60n % 60n;
  const seconds = secondsTotal % 60n;
  const frameField = frames % framesPerSecond;
  return `${pad(hours, 2)}:${pad(minutes, 2)}:${pad(seconds, 2)}:${pad(frameField, 2)}`;
}

/**
 * Format one asset's duration, optionally falling back to a parent asset's
 * grid when the asset (typically a derivative clip) has no measured cadence of
 * its own. Invalid or missing data degrades to honest `unknown`/`unmeasured`
 * states — never INVALID, NaN, or an invented precision grid.
 */
export function formatAssetDuration(
  asset: AssetDurationSource,
  parent?: AssetDurationSource,
): AssetDurationFormat {
  const ownGrid = isUsableGrid(asset.grid) ? asset.grid : null;
  const parentGrid =
    parent !== undefined && isUsableGrid(parent.grid) ? parent.grid : null;
  const grid = ownGrid ?? parentGrid;
  if (grid === null) {
    return {
      state: 'unknown',
      duration: null,
      boundary: null,
      grid: null,
      gridOrigin: 'none',
    };
  }
  const gridOrigin = ownGrid !== null ? 'measured' : 'inherited';
  if (!isUsableUnits(asset.units)) {
    return {
      state: 'unmeasured',
      duration: null,
      boundary: null,
      grid,
      gridOrigin,
    };
  }
  const units = asset.units;
  if (grid.kind === 'audio') {
    return {
      state: 'formatted',
      duration: elapsedClock(BigInt(units) * 1000n / BigInt(grid.sampleRateHz)),
      boundary: {
        kind: 'samples',
        index: units,
        sampleRateHz: grid.sampleRateHz,
      },
      grid,
      gridOrigin,
    };
  }
  return {
    state: 'formatted',
    duration: ndfTimecode(BigInt(units), (BigInt(grid.numerator) + BigInt(grid.denominator) - 1n) / BigInt(grid.denominator)),
    boundary: {
      kind: 'frames',
      index: units,
      numerator: grid.numerator,
      denominator: grid.denominator,
    },
    grid,
    gridOrigin,
  };
}

/**
 * Operator-facing cadence label: `48 kHz` for audio; `25 fps` for integer
 * video cadence; the exact rational pair with an NDF note for fractional
 * cadence — `29.97 (30000/1001) fps NDF`, never a raw repeating float.
 */
export function describeGrid(grid: DurationGrid): string {
  if (grid.kind === 'audio') {
    const kilohertz =
      grid.sampleRateHz % 1000 === 0
        ? String(grid.sampleRateHz / 1000)
        : (grid.sampleRateHz / 1000).toFixed(1);
    return `${kilohertz} kHz`;
  }
  if (grid.denominator === 1) {
    return `${String(grid.numerator)} fps`;
  }
  const approx = String(
    Number.parseFloat((grid.numerator / grid.denominator).toFixed(3)),
  );
  return `${approx} (${String(grid.numerator)}/${String(grid.denominator)}) fps NDF`;
}

/** Locale-independent thousands grouping for sample counts (`96,000`). */
export function groupThousands(value: number): string {
  return String(value).replace(/\B(?=(\d{3})+(?!\d))/g, ',');
}

/**
 * The precision line for a formatted duration: the cadence label, plus the
 * sample count (`SMP 96,000`) for audio, where the elapsed clock alone cannot
 * distinguish 48,000 from 48,001 samples. Null when the cadence itself is
 * unknown.
 */
export function describeDurationPrecision(
  format: AssetDurationFormat,
): string | null {
  if (format.grid === null) {
    return null;
  }
  const base = describeGrid(format.grid);
  if (format.boundary !== null && format.boundary.kind === 'samples') {
    return `${base} · SMP ${groupThousands(format.boundary.index)}`;
  }
  return base;
}
