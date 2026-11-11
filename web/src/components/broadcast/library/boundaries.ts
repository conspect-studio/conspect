// Exact grid-boundary helpers for the library surfaces — the transcript/
// trim endpoint math that the evidence audit pins (library-evidence.md §5,
// §9.7): word times and trim points are INTEGER indices on the asset's own
// rational frame grid (e.g. 30000/1001) or integer sample grid, and a float
// seconds value must never become stored authority ("a float seconds value
// will not round-trip across 60000/1001"). Formatting mirrors the rules in
// `../duration.ts`: NDF timecode `HH:MM:SS:FF` counting ceil(n/d) frames per
// second for video, elapsed clock `HH:MM:SS.mmm` floored at the millisecond
// for audio. Invalid or absent data degrades to honest nulls — never NaN,
// never an invented precision.
// Pure functions only: no React, no i18n, no side effects.
import type { DurationGrid, RationalBoundary } from '../duration';

/** One signed step of a boundary stepper. */
export type BoundaryStepDirection = -1 | 1;

function isUsableIndex(index: number): boolean {
  return Number.isInteger(index) && index >= 0;
}

function pad(value: number, width: number): string {
  return String(value).padStart(width, '0');
}

/** NDF video timecode label `HH:MM:SS:FF` (same rule as `../duration.ts`). */
function ndfTimecode(frames: number, framesPerSecond: number): string {
  const hours = Math.floor(frames / (framesPerSecond * 3600));
  const minutes = Math.floor(frames / (framesPerSecond * 60)) % 60;
  const seconds = Math.floor(frames / framesPerSecond) % 60;
  const frameField = frames % framesPerSecond;
  return `${pad(hours, 2)}:${pad(minutes, 2)}:${pad(seconds, 2)}:${pad(frameField, 2)}`;
}

/** Elapsed audio clock label `HH:MM:SS.mmm`, floored at the millisecond. */
function elapsedClock(totalMs: number): string {
  const hours = Math.floor(totalMs / 3_600_000);
  const minutes = Math.floor(totalMs / 60_000) % 60;
  const seconds = Math.floor(totalMs / 1000) % 60;
  const millis = totalMs % 1000;
  return `${pad(hours, 2)}:${pad(minutes, 2)}:${pad(seconds, 2)}.${pad(millis, 3)}`;
}

/** Whether two boundaries sit on the SAME grid (index comparison is only then meaningful). */
export function isSameGrid(a: RationalBoundary, b: RationalBoundary): boolean {
  if (a.kind !== b.kind) {
    return false;
  }
  if (a.kind === 'samples' && b.kind === 'samples') {
    return a.sampleRateHz === b.sampleRateHz;
  }
  if (a.kind === 'frames' && b.kind === 'frames') {
    return a.numerator === b.numerator && a.denominator === b.denominator;
  }
  return false;
}

/**
 * The exact operator-facing label for one grid position: `HH:MM:SS:FF` NDF
 * timecode on a rational frame grid, `HH:MM:SS.mmm` elapsed clock on a
 * sample grid. Null when the boundary is unusable — never NaN.
 */
export function formatBoundaryLabel(boundary: RationalBoundary | null | undefined): string | null {
  if (boundary === null || boundary === undefined) {
    return null;
  }
  if (!isUsableIndex(boundary.index)) {
    return null;
  }
  if (boundary.kind === 'samples') {
    if (!Number.isInteger(boundary.sampleRateHz) || boundary.sampleRateHz < 1) {
      return null;
    }
    return elapsedClock(
      Math.floor((boundary.index * 1000) / boundary.sampleRateHz),
    );
  }
  if (
    !Number.isInteger(boundary.numerator) ||
    boundary.numerator < 1 ||
    !Number.isInteger(boundary.denominator) ||
    boundary.denominator < 1
  ) {
    return null;
  }
  return ndfTimecode(boundary.index, Math.ceil(boundary.numerator / boundary.denominator));
}

/** Locale-independent thousands grouping (`96,000`) — same as `../duration.ts`. */
function groupThousands(value: number): string {
  return String(value).replace(/\B(?=(\d{3})+(?!\d))/g, ',');
}

/**
 * The exact-unit line for one grid position — `FR 250 · 30000/1001` for
 * frames, `SMP 96,000 · 48 kHz` for samples — so a precision endpoint is
 * always accompanied by its integer index, never by a divided float.
 * Null when the boundary is unusable.
 */
export function describeBoundaryUnit(
  boundary: RationalBoundary | null | undefined,
): string | null {
  if (boundary === null || boundary === undefined) {
    return null;
  }
  if (!isUsableIndex(boundary.index)) {
    return null;
  }
  if (boundary.kind === 'samples') {
    if (!Number.isInteger(boundary.sampleRateHz) || boundary.sampleRateHz < 1) {
      return null;
    }
    const kilohertz =
      boundary.sampleRateHz % 1000 === 0
        ? String(boundary.sampleRateHz / 1000)
        : (boundary.sampleRateHz / 1000).toFixed(1);
    return `SMP ${groupThousands(boundary.index)} · ${kilohertz} kHz`;
  }
  if (
    !Number.isInteger(boundary.numerator) ||
    boundary.numerator < 1 ||
    !Number.isInteger(boundary.denominator) ||
    boundary.denominator < 1
  ) {
    return null;
  }
  return `FR ${groupThousands(boundary.index)} · ${String(boundary.numerator)}/${String(boundary.denominator)}`;
}

/**
 * Build the boundary at `index` on a grid, or null when the grid/index is
 * unusable. The exact rational pair (or sample rate) is carried in the
 * boundary — never the divided float rate.
 */
export function boundaryAtGridIndex(
  grid: DurationGrid | null | undefined,
  index: number,
): RationalBoundary | null {
  if (grid === null || grid === undefined || !isUsableIndex(index)) {
    return null;
  }
  if (grid.kind === 'audio') {
    if (!Number.isInteger(grid.sampleRateHz) || grid.sampleRateHz < 1) {
      return null;
    }
    return { kind: 'samples', index, sampleRateHz: grid.sampleRateHz };
  }
  if (
    !Number.isInteger(grid.numerator) ||
    grid.numerator < 1 ||
    !Number.isInteger(grid.denominator) ||
    grid.denominator < 1
  ) {
    return null;
  }
  return { kind: 'frames', index, numerator: grid.numerator, denominator: grid.denominator };
}

/**
 * Propose the next boundary one grid unit away, clamped to
 * `[0, maxIndex]` when the asset's measured length is known. Null when the
 * boundary or clamp leaves no valid next index (e.g. stepping below 0).
 * The caller stays the authority: this is a PROPOSAL emitted in the intent,
 * never an applied edit.
 */
export function stepBoundary(
  boundary: RationalBoundary,
  direction: BoundaryStepDirection,
  maxIndex: number | null | undefined,
): RationalBoundary | null {
  // Widen to a plain number for the runtime guard: callers may pass
  // anything, and the step must degrade to null, never throw.
  const step: number = direction;
  if (!isUsableIndex(boundary.index) || (step !== -1 && step !== 1)) {
    return null;
  }
  const next = boundary.index + step;
  if (next < 0) {
    return null;
  }
  if (maxIndex !== null && maxIndex !== undefined && Number.isInteger(maxIndex) && next > maxIndex) {
    return null;
  }
  if (boundary.kind === 'samples') {
    if (!Number.isInteger(boundary.sampleRateHz) || boundary.sampleRateHz < 1) {
      return null;
    }
    return { kind: 'samples', index: next, sampleRateHz: boundary.sampleRateHz };
  }
  if (
    !Number.isInteger(boundary.numerator) ||
    boundary.numerator < 1 ||
    !Number.isInteger(boundary.denominator) ||
    boundary.denominator < 1
  ) {
    return null;
  }
  return { kind: 'frames', index: next, numerator: boundary.numerator, denominator: boundary.denominator };
}

/**
 * Order two boundaries on the SAME grid (`isSameGrid`): -1 when `a` is
 * strictly before `b`, 1 after, 0 when equal. On differing grids returns
 * `null` — cross-grid ordering is meaningless and must not be invented.
 */
export function compareBoundaries(
  a: RationalBoundary,
  b: RationalBoundary,
): -1 | 0 | 1 | null {
  if (!isSameGrid(a, b)) {
    return null;
  }
  if (a.index < b.index) {
    return -1;
  }
  return a.index === b.index ? 0 : 1;
}
