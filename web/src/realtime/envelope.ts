// Realtime envelope types (docs/api/realtime.md §2).
//
// Static type definitions for event payloads that have a direct equivalent in
// the AsyncAPI 3.0 spec are imported from `generated-types.ts` (produced by
// `npm run generate:events` from `docs/api/asyncapi.json`). Hand-modelled types
// are kept only where the generated schema does not cover the shape exactly:
//   - `Envelope` itself (hand-owned per ADR-RT006 — the transport layer must be
//     able to tolerate unknown `t` values and unknown majors defensively).
//   - `TileSnapshotEntry`/`TilesSnapshotData` — the AsyncAPI spec declares the
//     wire-minimal `TilesSnapshot`/`TileSnapshotEntry` (id, state, input?);
//     these hand-modelled composites extend them with the `fps`/`since_ts`/
//     `reason` fields the snapshot/delta reconciliation logic carries (parsed
//     defensively when present, silently absent otherwise, per §2).
//   - `TileStateDeltaData` — extends the generated `TileState` payload with the
//     `showing` and `since_ts` fields used internally by the snapshot/delta
//     reconciliation logic; kept hand-modelled to preserve forward-compatibility
//     as those fields are wired (they are silently dropped by parseTileStateDelta
//     when absent, per §2 defensive parsing).
// Parsing is defensive: unknown `t` values and unknown envelope majors are
// tolerated, never thrown on.

// The LifecycleState type (LIVE/STALE/RECONNECTING/NO_SIGNAL) is generated from
// the AsyncAPI spec. Re-exported as `TileState` for backward compatibility with
// callers that import the lifecycle state under the shorter name from this module.
export type { LifecycleState as TileState } from "./generated-types";

/** The envelope schema major this client speaks. Reject unknown majors. */
export const ENVELOPE_MAJOR = 1;

/** Control frames travel on this synthetic topic. */
export const CONTROL_TOPIC = "$control";

/**
 * The versioned envelope wrapping every realtime message. `data` is kept as
 * `unknown` here and narrowed per `t` by the event reducers; this keeps the
 * transport layer agnostic to the (large, evolving) payload union.
 */
export interface Envelope {
  /** Envelope schema major. Clients reject an unknown major. */
  readonly v: number;
  /** Dotted event type; the discriminator selecting the `data` schema. */
  readonly t: string;
  /** Subscription routing key (control frames use `$control`). */
  readonly topic: string;
  /** Optional resource scope (tile/input/output/job id). */
  readonly id?: string;
  /** Per-connection monotonic resume cursor; a gap means drops. */
  readonly seq: number;
  /** Engine monotonic nanoseconds. */
  readonly ts: number;
  /** Optional correlation id echoing a REST command / job. */
  readonly corr?: string;
  /** Typed payload selected by `t` (narrowed by reducers). */
  readonly data: unknown;
}

/** Narrow an unknown value to a plain record without an unsafe assertion. */
function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

/** A coarse type guard validating the structural envelope shape. */
export function isEnvelope(value: unknown): value is Envelope {
  if (!isRecord(value)) {
    return false;
  }
  return (
    typeof value.v === "number" &&
    typeof value.t === "string" &&
    typeof value.topic === "string" &&
    typeof value.seq === "number" &&
    typeof value.ts === "number" &&
    "data" in value
  );
}

/** Parse a raw WS text frame into an Envelope, or `undefined` if malformed. */
export function parseEnvelope(raw: string): Envelope | undefined {
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return undefined;
  }
  return isEnvelope(parsed) ? parsed : undefined;
}

// Internal alias: `LifecycleState` from generated-types is the same four-value
// union this module re-exports as `TileState`. Import it under the generated
// name for use in the hand-owned composite types below so the relationship is
// explicit (tsc will enforce they are structurally identical).
import type { LifecycleState } from "./generated-types";

// --- Narrowed payloads we surface today (extend as topics are wired). -------

/** A tile row as carried in a `tiles` `$snapshot`. */
export interface TileSnapshotEntry {
  readonly id: string;
  // `state` is typed as `LifecycleState` from the generated spec — the four
  // values (LIVE/STALE/RECONNECTING/NO_SIGNAL) are the canonical tile lifecycle
  // states from the AsyncAPI schema (resilience invariant #2).
  readonly state: LifecycleState;
  readonly input?: string;
  readonly fps?: number;
  readonly since_ts?: number;
  readonly reason?: string;
}

/** The `data` of a `tiles` `$snapshot`. */
export interface TilesSnapshotData {
  readonly as_of_seq: number;
  readonly tiles: readonly TileSnapshotEntry[];
}

/** The `data` of a `tile.state` delta.
 *
 * Extends the generated `TileState` payload (which has `from`, `to`, `trigger`,
 * and optionally `input`) with the internal `showing` and `since_ts` fields
 * used by the snapshot/delta reconciliation logic in `useEngineEvents.ts`. The
 * extra fields are silently absent when not carried by the wire frame, per §2
 * defensive parsing.
 */
export interface TileStateDeltaData {
  readonly from: LifecycleState;
  readonly to: LifecycleState;
  readonly input?: string;
  readonly trigger?: string;
  readonly showing?: string;
  readonly since_ts?: number;
}

function asRecord(value: unknown): Record<string, unknown> | undefined {
  return isRecord(value) ? value : undefined;
}

// Runtime guard: validates a raw unknown value is one of the four canonical
// LifecycleState values from the generated AsyncAPI spec. The four literals are
// pinned here so a divergence between the spec and runtime is immediately
// visible — if the spec adds a fifth state, this guard needs updating too.
function isTileState(value: unknown): value is LifecycleState {
  return (
    value === "LIVE" ||
    value === "STALE" ||
    value === "RECONNECTING" ||
    value === "NO_SIGNAL"
  );
}

function parseTileEntry(value: unknown): TileSnapshotEntry | undefined {
  const record = asRecord(value);
  if (record === undefined) {
    return undefined;
  }
  const id = record.id;
  const state = record.state;
  if (typeof id !== "string" || !isTileState(state)) {
    return undefined;
  }
  const entry: {
    id: string;
    state: LifecycleState;
    input?: string;
    fps?: number;
    since_ts?: number;
    reason?: string;
  } = { id, state };
  if (typeof record.input === "string") {
    entry.input = record.input;
  }
  if (typeof record.fps === "number") {
    entry.fps = record.fps;
  }
  if (typeof record.since_ts === "number") {
    entry.since_ts = record.since_ts;
  }
  if (typeof record.reason === "string") {
    entry.reason = record.reason;
  }
  return entry;
}

/** Narrow an envelope `data` to a tiles snapshot payload. */
export function parseTilesSnapshot(
  data: unknown,
): TilesSnapshotData | undefined {
  const record = asRecord(data);
  if (record === undefined) {
    return undefined;
  }
  const rawTiles = record.tiles;
  if (!Array.isArray(rawTiles)) {
    return undefined;
  }
  const tiles: TileSnapshotEntry[] = [];
  for (const raw of rawTiles) {
    const entry = parseTileEntry(raw);
    if (entry !== undefined) {
      tiles.push(entry);
    }
  }
  const asOf = record.as_of_seq;
  return {
    as_of_seq: typeof asOf === "number" ? asOf : 0,
    tiles,
  };
}

/** Narrow an envelope `data` to a tile.state delta payload. */
export function parseTileStateDelta(
  data: unknown,
): TileStateDeltaData | undefined {
  const record = asRecord(data);
  if (record === undefined) {
    return undefined;
  }
  const from = record.from;
  const to = record.to;
  if (!isTileState(from) || !isTileState(to)) {
    return undefined;
  }
  const delta: {
    from: LifecycleState;
    to: LifecycleState;
    input?: string;
    trigger?: string;
    showing?: string;
    since_ts?: number;
  } = { from, to };
  if (typeof record.input === "string") {
    delta.input = record.input;
  }
  if (typeof record.trigger === "string") {
    delta.trigger = record.trigger;
  }
  if (typeof record.showing === "string") {
    delta.showing = record.showing;
  }
  if (typeof record.since_ts === "number") {
    delta.since_ts = record.since_ts;
  }
  return delta;
}
