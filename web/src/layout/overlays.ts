// The layout `overlays` sub-document (PR-A engine contract, landing separately).
//
// A stored layout body MAY carry an `overlays` object naming which multiview
// layers a routed layout (an output showing that layout) bakes:
//   { labels, meters, flags, tally, clock, safe_area } — all booleans.
// An absent object, or an absent key within it, defaults to:
//   labels = meters = flags = tally = clock = true, safe_area = false.
// The engine REJECTS unknown keys on this object (`deny_unknown_fields`), so
// this module never writes any key beyond the six above. When every value
// equals the default, the `overlays` key is omitted entirely — this keeps
// bodies authored before PR-A byte-stable.
//
// This module is deliberately framework-free (no React) so it is unit-tested
// in isolation and shared by both the absolute and grid editors' save paths.
import { isRecord } from './cellProps';

/** Which multiview layers a routed layout bakes (engine `overlays` schema). */
export interface LayoutOverlays {
  /** Source name captions. */
  readonly labels: boolean;
  /** Audio level meters. */
  readonly meters: boolean;
  /** State + fault flags (LIVE/STALE/NO SIGNAL and BLACK/FROZEN/NO AUDIO). */
  readonly flags: boolean;
  /** Tally borders (red = on Program, green = on Preview). */
  readonly tally: boolean;
  /** The on-screen clock. */
  readonly clock: boolean;
  /** Safe-area markers. */
  readonly safe_area: boolean;
}

/** The engine's default overlay set when the key/object is absent. */
export const DEFAULT_LAYOUT_OVERLAYS: LayoutOverlays = {
  labels: true,
  meters: true,
  flags: true,
  tally: true,
  clock: true,
  safe_area: false,
};

/** The six keys the `overlays` object carries — and no others. */
const OVERLAY_KEYS: readonly (keyof LayoutOverlays)[] = [
  'labels',
  'meters',
  'flags',
  'tally',
  'clock',
  'safe_area',
];

function asBoolean(value: unknown, fallback: boolean): boolean {
  return typeof value === 'boolean' ? value : fallback;
}

/**
 * Read the effective overlay set from a persisted layout `body` (tolerant: a
 * missing object, a missing key, or a non-boolean value all fall back to the
 * engine default for that key — this never throws).
 */
export function overlaysFromBody(body: unknown): LayoutOverlays {
  if (!isRecord(body)) {
    return DEFAULT_LAYOUT_OVERLAYS;
  }
  const raw = body.overlays;
  if (!isRecord(raw)) {
    return DEFAULT_LAYOUT_OVERLAYS;
  }
  return {
    labels: asBoolean(raw.labels, DEFAULT_LAYOUT_OVERLAYS.labels),
    meters: asBoolean(raw.meters, DEFAULT_LAYOUT_OVERLAYS.meters),
    flags: asBoolean(raw.flags, DEFAULT_LAYOUT_OVERLAYS.flags),
    tally: asBoolean(raw.tally, DEFAULT_LAYOUT_OVERLAYS.tally),
    clock: asBoolean(raw.clock, DEFAULT_LAYOUT_OVERLAYS.clock),
    safe_area: asBoolean(raw.safe_area, DEFAULT_LAYOUT_OVERLAYS.safe_area),
  };
}

/** Whether `o` equals the engine default (every key at its default value). */
function isDefaultOverlays(o: LayoutOverlays): boolean {
  return OVERLAY_KEYS.every((key) => o[key] === DEFAULT_LAYOUT_OVERLAYS[key]);
}

/**
 * Return a NEW body with `overlays` set to exactly the six keys in `o` — or,
 * when `o` equals the default, with the `overlays` key deleted (keeps bodies
 * that never diverged from the default byte-stable). Every other root key is
 * carried over verbatim; `body` itself is never mutated.
 */
export function withOverlays(
  body: Record<string, unknown>,
  o: LayoutOverlays,
): Record<string, unknown> {
  const next = { ...body };
  if (isDefaultOverlays(o)) {
    delete next.overlays;
    return next;
  }
  next.overlays = {
    labels: o.labels,
    meters: o.meters,
    flags: o.flags,
    tally: o.tally,
    clock: o.clock,
    safe_area: o.safe_area,
  };
  return next;
}

/**
 * A compact summary of an overlay set, for the Layouts page's "Layers"
 * column. This is a discriminated code, not a rendered string — the pure
 * model layer never hardcodes user-facing text (every string the operator
 * sees goes through Lingui at the call site).
 */
export type OverlaysSummary =
  | { readonly kind: 'all' }
  | { readonly kind: 'clean' }
  | { readonly kind: 'count'; readonly on: number; readonly total: number };

/**
 * Summarize an overlay set: `{ kind: 'all' }` when every layer is on,
 * `{ kind: 'clean' }` when every layer is off, else `{ kind: 'count', on,
 * total }` (the on-count out of the six keys) — the page renders each case
 * (e.g. "All", "Clean", "4 of 6") through its own `t` template.
 */
export function summarizeOverlays(o: LayoutOverlays): OverlaysSummary {
  const total = OVERLAY_KEYS.length;
  const on = OVERLAY_KEYS.filter((key) => o[key]).length;
  if (on === total) {
    return { kind: 'all' };
  }
  if (on === 0) {
    return { kind: 'clean' };
  }
  return { kind: 'count', on, total };
}
