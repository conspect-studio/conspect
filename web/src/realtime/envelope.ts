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

// --- devices topic (ADR-M008 / ADR-RT007) -----------------------------------

// The device payload types come from the generated AsyncAPI schema — the
// canonical source of truth for the `devices` topic. The guards below narrow
// raw `data` defensively (§2: unknown shapes are dropped, never thrown on).
import type {
  AchievedSync,
  DeviceCapabilities,
  DeviceDiscovered,
  DeviceState,
  DeviceStatus,
  DeviceStreamStatus,
  DeviceSyncSummary,
  ImpactClass,
  MediaPlayerEvent,
  MediaPlayerState,
  SyncCapability,
} from "./generated-types";

// Runtime guard over the six canonical DeviceState values from the generated
// spec, pinned here so a spec/runtime divergence is immediately visible.
function isDeviceState(value: unknown): value is DeviceState {
  return (
    value === "DISCOVERED" ||
    value === "ADOPTING" ||
    value === "ONLINE" ||
    value === "DEGRADED" ||
    value === "AUTH_FAILED" ||
    value === "UNREACHABLE"
  );
}

function isAchievedSync(value: unknown): value is AchievedSync {
  return value === "frame-accurate" || value === "bounded-skew" || value === "none";
}

function isSyncCapability(value: unknown): value is SyncCapability {
  return value === "frame-accurate" || value === "offset-only" || value === "none";
}

function parseStreamStatus(value: unknown): DeviceStreamStatus | undefined {
  const record = asRecord(value);
  if (record === undefined) {
    return undefined;
  }
  const role = record.role;
  if ((role !== "encode" && role !== "decode") || typeof record.healthy !== "boolean") {
    return undefined;
  }
  const stream: {
    role: "encode" | "decode";
    healthy: boolean;
    bitrate_bps?: number;
    fps?: number;
    output_ref?: string;
  } = { role, healthy: record.healthy };
  if (typeof record.bitrate_bps === "number") {
    stream.bitrate_bps = record.bitrate_bps;
  }
  if (typeof record.fps === "number") {
    stream.fps = record.fps;
  }
  if (typeof record.output_ref === "string") {
    stream.output_ref = record.output_ref;
  }
  return stream;
}

function parseSyncSummary(value: unknown): DeviceSyncSummary | undefined {
  const record = asRecord(value);
  if (record === undefined) {
    return undefined;
  }
  if (
    typeof record.group !== "string" ||
    !isAchievedSync(record.achieved) ||
    typeof record.offset_ms !== "number"
  ) {
    return undefined;
  }
  return { group: record.group, achieved: record.achieved, offset_ms: record.offset_ms };
}

function parseCapabilities(value: unknown): DeviceCapabilities | undefined {
  const record = asRecord(value);
  if (record === undefined) {
    return undefined;
  }
  if (
    typeof record.audio !== "boolean" ||
    typeof record.decode !== "boolean" ||
    typeof record.display !== "boolean" ||
    typeof record.encode !== "boolean" ||
    typeof record.firmware_update !== "boolean" ||
    typeof record.reboot !== "boolean" ||
    !isSyncCapability(record.sync)
  ) {
    return undefined;
  }
  return {
    audio: record.audio,
    decode: record.decode,
    display: record.display,
    encode: record.encode,
    firmware_update: record.firmware_update,
    reboot: record.reboot,
    sync: record.sync,
  };
}

/**
 * Narrow an envelope `data` to a `device.status` snapshot (the conflated
 * latest-wins lane; also the wire shape of the `/devices/{id}/status` REST
 * fallback). Optional facets parse independently: a malformed sub-shape is
 * dropped, the snapshot survives.
 */
export function parseDeviceStatus(data: unknown): DeviceStatus | undefined {
  const record = asRecord(data);
  if (record === undefined) {
    return undefined;
  }
  if (typeof record.device_id !== "string" || !isDeviceState(record.state)) {
    return undefined;
  }
  const status: {
    device_id: string;
    state: DeviceState;
    mode?: string;
    last_seen_ts?: number;
    temperature_c?: number;
    streams?: readonly DeviceStreamStatus[];
    sync?: DeviceSyncSummary;
    capabilities?: DeviceCapabilities;
  } = { device_id: record.device_id, state: record.state };
  if (typeof record.mode === "string") {
    status.mode = record.mode;
  }
  if (typeof record.last_seen_ts === "number") {
    status.last_seen_ts = record.last_seen_ts;
  }
  if (typeof record.temperature_c === "number") {
    status.temperature_c = record.temperature_c;
  }
  if (Array.isArray(record.streams)) {
    const streams: DeviceStreamStatus[] = [];
    for (const raw of record.streams) {
      const stream = parseStreamStatus(raw);
      if (stream !== undefined) {
        streams.push(stream);
      }
    }
    status.streams = streams;
  }
  const sync = parseSyncSummary(record.sync);
  if (sync !== undefined) {
    status.sync = sync;
  }
  const capabilities = parseCapabilities(record.capabilities);
  if (capabilities !== undefined) {
    status.capabilities = capabilities;
  }
  return status;
}

/**
 * A device lifecycle event from the lossless `devices` lane, normalized for
 * the session event ring (the Events tab).
 */
export type DeviceLifecycleEvent =
  | {
      readonly kind: "adopted";
      readonly deviceId: string;
      readonly driver: string;
      readonly name?: string;
    }
  | { readonly kind: "removed"; readonly deviceId: string }
  | {
      readonly kind: "mode";
      readonly deviceId: string;
      readonly mode: string;
      readonly phase: "started" | "finished" | "failed";
      readonly impact: ImpactClass;
      readonly detail?: string;
    }
  | {
      readonly kind: "error";
      readonly deviceId: string;
      readonly message: string;
      readonly code?: string;
    };

function isImpactClass(value: unknown): value is ImpactClass {
  return value === "cp" || value === "c1" || value === "c2" || value === "dev";
}

/**
 * Narrow a lossless devices-lane event (`device.adopted` / `device.removed` /
 * `device.mode` / `device.error`) to its normalized form, or `undefined` for
 * any other `t` or a malformed payload.
 */
export function parseDeviceEvent(
  t: string,
  data: unknown,
): DeviceLifecycleEvent | undefined {
  const record = asRecord(data);
  if (record === undefined || typeof record.device_id !== "string") {
    return undefined;
  }
  const deviceId = record.device_id;
  switch (t) {
    case "device.adopted": {
      if (typeof record.driver !== "string") {
        return undefined;
      }
      return typeof record.name === "string"
        ? { kind: "adopted", deviceId, driver: record.driver, name: record.name }
        : { kind: "adopted", deviceId, driver: record.driver };
    }
    case "device.removed":
      return { kind: "removed", deviceId };
    case "device.mode": {
      const phase = record.phase;
      if (
        typeof record.mode !== "string" ||
        (phase !== "started" && phase !== "finished" && phase !== "failed") ||
        !isImpactClass(record.impact)
      ) {
        return undefined;
      }
      return typeof record.detail === "string"
        ? {
            kind: "mode",
            deviceId,
            mode: record.mode,
            phase,
            impact: record.impact,
            detail: record.detail,
          }
        : { kind: "mode", deviceId, mode: record.mode, phase, impact: record.impact };
    }
    case "device.error": {
      if (typeof record.message !== "string") {
        return undefined;
      }
      return typeof record.code === "string"
        ? { kind: "error", deviceId, message: record.message, code: record.code }
        : { kind: "error", deviceId, message: record.message };
    }
    default:
      return undefined;
  }
}

/**
 * Narrow an envelope `data` to a `device.discovered` row (an UNTRUSTED hint
 * streamed while a scan runs, correlated via the envelope `corr`; ADR-0041).
 */
export function parseDeviceDiscovered(data: unknown): DeviceDiscovered | undefined {
  const record = asRecord(data);
  if (record === undefined) {
    return undefined;
  }
  const family = record.family;
  if (
    typeof record.address !== "string" ||
    typeof record.driver !== "string" ||
    (family !== "ipv6" && family !== "ipv4-legacy")
  ) {
    return undefined;
  }
  const row: {
    address: string;
    driver: string;
    family: "ipv6" | "ipv4-legacy";
    name?: string;
  } = { address: record.address, driver: record.driver, family };
  if (typeof record.name === "string") {
    row.name = record.name;
  }
  return row;
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

// --- switcher topic: media players (ADR-0057 / ADR-0097 / ADR-RT008) ---------

// Narrow an unknown value to a `MediaPlayerState` (the discriminated transport
// state, tagged by `kind`). The seven known kinds from the generated AsyncAPI
// schema are pinned here so a spec/runtime divergence is immediately visible;
// `vamping` additionally requires a boolean `exit_armed`. An unrecognized or
// malformed shape returns `undefined` and the whole frame is dropped (§2:
// unknown shapes are dropped, never thrown on) — forward-compatible.
function parseMediaPlayerState(value: unknown): MediaPlayerState | undefined {
  const record = asRecord(value);
  if (record === undefined || typeof record.kind !== "string") {
    return undefined;
  }
  switch (record.kind) {
    case "loading":
    case "cued":
    case "playing":
    case "paused":
    case "stopped":
    case "eof":
      return { kind: record.kind };
    case "vamping":
      if (typeof record.exit_armed !== "boolean") {
        return undefined;
      }
      return { kind: "vamping", exit_armed: record.exit_armed };
    default:
      return undefined;
  }
}

/**
 * Narrow an envelope `data` to a `media.player_state` event (topic `switcher`,
 * envelope `id` = player id; ADR-RT008). A LOSSLESS transport-state transition:
 * the player's new {@link MediaPlayerState} plus the integer `position_frames`
 * playhead and the optional loaded `asset`. A malformed payload is dropped.
 */
export function parseMediaPlayerEvent(
  data: unknown,
): MediaPlayerEvent | undefined {
  const record = asRecord(data);
  if (record === undefined) {
    return undefined;
  }
  if (
    typeof record.player !== "string" ||
    typeof record.position_frames !== "number"
  ) {
    return undefined;
  }
  const state = parseMediaPlayerState(record.state);
  if (state === undefined) {
    return undefined;
  }
  const event: {
    player: string;
    position_frames: number;
    state: MediaPlayerState;
    asset?: string;
  } = {
    player: record.player,
    position_frames: record.position_frames,
    state,
  };
  if (typeof record.asset === "string") {
    event.asset = record.asset;
  }
  return event;
}
