// useEngineEvents — the React binding over the realtime WebSocket.
//
// Contract (docs/api/realtime.md §5, §9): snapshot seeds caches, ordered deltas
// patch them; on `$resync` the UI REBUILDS (discards) the affected state rather
// than merging. The hook NEVER blocks render — the socket lives in a ref, all
// state lands via setState/Query-cache writes from event callbacks.
import { useEffect, useRef, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import type { QueryClient } from "@tanstack/react-query";

import { RealtimeConnection } from "./connection";
import type { RealtimeStatus } from "./connection";
import { mintWsTicket, realtimeWsBaseUrl } from "./ticket";
import {
  CONTROL_TOPIC,
  parseDeviceDiscovered,
  parseDeviceEvent,
  parseDeviceStatus,
  parseMediaPlayerEvent,
  parseTileStateDelta,
  parseTilesSnapshot,
} from "./envelope";
import type {
  DeviceLifecycleEvent,
  Envelope,
  TileSnapshotEntry,
} from "./envelope";
import type {
  DeviceDiscovered,
  DeviceStatus,
  MediaPlayerEvent,
} from "./generated-types";
import { resourceKeys } from "../resources/queries";
// `LifecycleState` (LIVE/STALE/RECONNECTING/NO_SIGNAL) comes from the generated
// AsyncAPI schema types — the canonical source of truth for tile lifecycle values
// (resilience invariant #2). `envelope.ts` re-exports it as `TileState` for
// backward compat, but we import under the generated name here to make the
// dependency on the spec explicit.
import type { LifecycleState } from "./generated-types";

/** The TanStack Query key the live tile map is mirrored into. */
export const TILES_QUERY_KEY = ["realtime", "tiles"] as const;

/**
 * The Query key for the latest-wins per-device status map (`device.status` on
 * the `devices` topic, keyed by the envelope `id` = device id; ADR-M008 §2.1).
 */
export const DEVICE_STATUS_QUERY_KEY = ["realtime", "devices", "status"] as const;

/**
 * The Query key for the bounded, newest-first session ring of lossless device
 * lifecycle events (adopted / removed / mode / error) — the Events tab feed.
 */
export const DEVICE_EVENTS_QUERY_KEY = ["realtime", "devices", "events"] as const;

/**
 * The Query key for live `device.discovered` rows grouped by the envelope
 * `corr` (the scan's operation id, ADR-RT007) — UNTRUSTED hints, never devices.
 */
export const DISCOVERED_LIVE_QUERY_KEY = [
  "realtime",
  "devices",
  "discovered",
] as const;

/**
 * The Query key for the engine-monotonic clock reference: the newest envelope
 * `ts` paired with the wall time it was observed at. Engine timestamps (e.g. a
 * device's `last_seen_ts`) can only be aged against this reference — without
 * one the UI shows no age rather than fabricating it.
 */
export const ENGINE_CLOCK_QUERY_KEY = ["realtime", "engineClock"] as const;

/**
 * The Query key for the latest-wins per-player transport-state map
 * (`media.player_state` on the `switcher` topic, keyed by the player id =
 * envelope `id`; ADR-RT008). A LOSSLESS lifecycle event, but the UI only needs
 * the newest state per player, so it conflates to latest-wins here.
 */
export const MEDIA_PLAYER_STATE_QUERY_KEY = [
  "realtime",
  "switcher",
  "players",
] as const;

/** The engine-monotonic ↔ wall-clock pairing (see ENGINE_CLOCK_QUERY_KEY). */
export interface EngineClockRef {
  /** Engine monotonic nanoseconds of the newest envelope seen. */
  readonly engineTs: number;
  /** `Date.now()` at the moment that envelope was observed. */
  readonly wallMs: number;
}

/** One entry in the bounded device-events session ring (newest first). */
export interface DeviceEventEntry {
  /** The envelope sequence number (session-unique ordering). */
  readonly seq: number;
  /** Engine monotonic nanoseconds. */
  readonly ts: number;
  /** The normalized lifecycle event. */
  readonly event: DeviceLifecycleEvent;
}

/** The device-events ring bound (drop-oldest beyond this). */
const DEVICE_EVENTS_RING_MAX = 200;

/**
 * The minimum engine-clock advance (ns) between {@link ENGINE_CLOCK_QUERY_KEY}
 * writes. Envelopes arrive at event rate; rewriting the clock reference on
 * every one re-renders every page holding the ref at that same rate. 500 ms is
 * far finer than anything aged against the reference ("last seen Ns ago").
 */
const ENGINE_CLOCK_MIN_ADVANCE_NS = 500_000_000;

/** Per-corr cap on live discovery rows (drop-newest beyond this). */
const DISCOVERED_ROWS_MAX = 100;

/** A resolved tile as held in the cache (snapshot ⊕ deltas). */
export interface LiveTile {
  readonly id: string;
  // Typed using the generated `LifecycleState` from the AsyncAPI spec.
  readonly state: LifecycleState;
  readonly input?: string;
  readonly fps?: number;
  readonly since_ts?: number;
  readonly reason?: string;
}

/** What {@link useEngineEvents} returns. */
export interface EngineEvents {
  /** Coarse connection status (for the header indicator). */
  readonly status: RealtimeStatus;
  /** The last per-connection sequence cursor observed. */
  readonly lastSeq: number;
  /** A monotonically-increasing count of sequence gaps seen this session. */
  readonly gaps: number;
}

function tileFromEntry(entry: TileSnapshotEntry): LiveTile {
  const tile: {
    id: string;
    state: LifecycleState;
    input?: string;
    fps?: number;
    since_ts?: number;
    reason?: string;
  } = { id: entry.id, state: entry.state };
  if (entry.input !== undefined) {
    tile.input = entry.input;
  }
  if (entry.fps !== undefined) {
    tile.fps = entry.fps;
  }
  if (entry.since_ts !== undefined) {
    tile.since_ts = entry.since_ts;
  }
  if (entry.reason !== undefined) {
    tile.reason = entry.reason;
  }
  return tile;
}

function applyTilesSnapshot(client: QueryClient, envelope: Envelope): void {
  const snapshot = parseTilesSnapshot(envelope.data);
  if (snapshot === undefined) {
    return;
  }
  // Snapshot REPLACES (rebuilds) the cached map — never merges (§9 risk note).
  const next: Record<string, LiveTile> = {};
  for (const entry of snapshot.tiles) {
    next[entry.id] = tileFromEntry(entry);
  }
  client.setQueryData<Record<string, LiveTile>>(TILES_QUERY_KEY, next);
}

function applyTileStateDelta(client: QueryClient, envelope: Envelope): void {
  const id = envelope.id;
  if (id === undefined) {
    return;
  }
  const delta = parseTileStateDelta(envelope.data);
  if (delta === undefined) {
    return;
  }
  client.setQueryData<Record<string, LiveTile>>(
    TILES_QUERY_KEY,
    (current): Record<string, LiveTile> => {
      const base = current ?? {};
      const existing = base[id];
      const merged: {
        id: string;
        state: LifecycleState;
        input?: string;
        fps?: number;
        since_ts?: number;
        reason?: string;
      } = {
        id,
        state: delta.to,
      };
      const input = delta.input ?? existing?.input;
      if (input !== undefined) {
        merged.input = input;
      }
      if (existing?.fps !== undefined) {
        merged.fps = existing.fps;
      }
      const since = delta.since_ts ?? existing?.since_ts;
      if (since !== undefined) {
        merged.since_ts = since;
      }
      const reason = delta.showing ?? existing?.reason;
      if (reason !== undefined) {
        merged.reason = reason;
      }
      return { ...base, [id]: merged };
    },
  );
}

function applyDeviceStatus(client: QueryClient, envelope: Envelope): void {
  const status = parseDeviceStatus(envelope.data);
  if (status === undefined) {
    return;
  }
  // Latest wins, scoped by device id (the envelope `id` and the payload's
  // `device_id` agree; the payload is authoritative for the key).
  client.setQueryData<Record<string, DeviceStatus>>(
    DEVICE_STATUS_QUERY_KEY,
    (current): Record<string, DeviceStatus> => ({
      ...(current ?? {}),
      [status.device_id]: status,
    }),
  );
}

function applyMediaPlayerState(client: QueryClient, envelope: Envelope): void {
  const event = parseMediaPlayerEvent(envelope.data);
  if (event === undefined) {
    return;
  }
  // Latest wins, scoped by player id (the envelope `id` and the payload's
  // `player` agree; the payload is authoritative for the key).
  client.setQueryData<Record<string, MediaPlayerEvent>>(
    MEDIA_PLAYER_STATE_QUERY_KEY,
    (current): Record<string, MediaPlayerEvent> => ({
      ...(current ?? {}),
      [event.player]: event,
    }),
  );
}

function pushDeviceEvent(client: QueryClient, envelope: Envelope): void {
  const event = parseDeviceEvent(envelope.t, envelope.data);
  if (event === undefined) {
    return;
  }
  if (event.kind === "removed") {
    // The lifecycle removal also retires the latest-wins snapshot.
    client.setQueryData<Record<string, DeviceStatus>>(
      DEVICE_STATUS_QUERY_KEY,
      (current): Record<string, DeviceStatus> =>
        Object.fromEntries(
          Object.entries(current ?? {}).filter(([id]) => id !== event.deviceId),
        ),
    );
  }
  client.setQueryData<readonly DeviceEventEntry[]>(
    DEVICE_EVENTS_QUERY_KEY,
    (current): readonly DeviceEventEntry[] => {
      const entry: DeviceEventEntry = { seq: envelope.seq, ts: envelope.ts, event };
      // Newest first, bounded (drop-oldest): a long session never grows.
      return [entry, ...(current ?? [])].slice(0, DEVICE_EVENTS_RING_MAX);
    },
  );
  if (event.kind === "adopted" || event.kind === "removed") {
    // Registry membership changed (possibly by another operator): re-read the
    // stored devices list rather than waiting for an unrelated refetch. Mode
    // and error churn deliberately does NOT refetch — it is status, not
    // membership.
    void client.invalidateQueries({ queryKey: resourceKeys.list("devices") });
  }
}

function pushDiscoveredRow(client: QueryClient, envelope: Envelope): void {
  const corr = envelope.corr;
  if (corr === undefined) {
    return;
  }
  const row = parseDeviceDiscovered(envelope.data);
  if (row === undefined) {
    return;
  }
  client.setQueryData<Record<string, readonly DeviceDiscovered[]>>(
    DISCOVERED_LIVE_QUERY_KEY,
    (current): Record<string, readonly DeviceDiscovered[]> => {
      const base = current ?? {};
      const rows = base[corr];
      if (rows === undefined) {
        // A new scan's rows begin: keep ONLY the active scan. Finished scans'
        // rows otherwise accumulate one capped array per corr for the whole
        // session (the REST inventory snapshot covers them between scans).
        return { [corr]: [row] };
      }
      if (rows.length >= DISCOVERED_ROWS_MAX) {
        return base;
      }
      return { ...base, [corr]: [...rows, row] };
    },
  );
}

function clearDeviceCaches(client: QueryClient): void {
  client.removeQueries({ queryKey: DEVICE_STATUS_QUERY_KEY });
  client.removeQueries({ queryKey: DEVICE_EVENTS_QUERY_KEY });
  client.removeQueries({ queryKey: DISCOVERED_LIVE_QUERY_KEY });
}

/**
 * Connect to the engine realtime stream and reconcile snapshots/deltas into the
 * Query cache. Returns the live connection status for the UI shell.
 */
export function useEngineEvents(): EngineEvents {
  const queryClient = useQueryClient();
  const [status, setStatus] = useState<RealtimeStatus>("connecting");
  const [lastSeq, setLastSeq] = useState(0);
  const [gaps, setGaps] = useState(0);
  const connectionRef = useRef<RealtimeConnection | null>(null);

  useEffect(() => {
    const connection = new RealtimeConnection(realtimeWsBaseUrl(), {
      onStatus: (next): void => {
        setStatus(next);
      },
      onGap: (): void => {
        setGaps((count) => count + 1);
      },
      onEnvelope: (envelope): void => {
        // Control frames may carry seq 0; only advance the displayed cursor for
        // real, sequenced frames so the indicator never regresses.
        if (envelope.seq > 0) {
          setLastSeq(envelope.seq);
          // Pair the newest engine-monotonic ts with the wall clock so engine
          // timestamps (device last-seen) can be aged honestly. Throttled:
          // rewriting the ref on every envelope would re-render every page
          // holding it at envelope rate (the stored ref + elapsed wall time
          // already ages accurately between writes).
          if (envelope.ts > 0) {
            const stored = queryClient.getQueryData<EngineClockRef>(
              ENGINE_CLOCK_QUERY_KEY,
            );
            if (
              stored === undefined ||
              envelope.ts - stored.engineTs >= ENGINE_CLOCK_MIN_ADVANCE_NS
            ) {
              queryClient.setQueryData<EngineClockRef>(ENGINE_CLOCK_QUERY_KEY, {
                engineTs: envelope.ts,
                wallMs: Date.now(),
              });
            }
          }
        }
        if (envelope.topic === CONTROL_TOPIC) {
          // $resync = rebuild: drop affected caches so stale state cannot leak.
          if (envelope.t === "$resync") {
            queryClient.removeQueries({ queryKey: TILES_QUERY_KEY });
            clearDeviceCaches(queryClient);
          }
          // $snapshot frames carry their topic in `topic`, not `$control`, per
          // §5; nothing else on $control mutates cached domain state here.
          return;
        }
        switch (envelope.t) {
          case "$snapshot": {
            if (envelope.topic === "tiles") {
              applyTilesSnapshot(queryClient, envelope);
            }
            return;
          }
          case "tile.state": {
            applyTileStateDelta(queryClient, envelope);
            return;
          }
          case "device.status": {
            applyDeviceStatus(queryClient, envelope);
            return;
          }
          case "media.player_state": {
            applyMediaPlayerState(queryClient, envelope);
            return;
          }
          case "device.adopted":
          case "device.removed":
          case "device.mode":
          case "device.error": {
            pushDeviceEvent(queryClient, envelope);
            return;
          }
          case "device.discovered": {
            pushDiscoveredRow(queryClient, envelope);
            return;
          }
          default:
            // Unknown event types are ignored (forward-compatible, §2).
            return;
        }
      },
    }, mintWsTicket);
    connectionRef.current = connection;
    connection.start();
    return (): void => {
      connection.stop();
      connectionRef.current = null;
    };
  }, [queryClient]);

  return { status, lastSeq, gaps };
}

/**
 * Read the latest-wins per-player transport-state map that {@link useEngineEvents}
 * populates from `media.player_state` events (keyed by player id). A component
 * must also mount {@link useEngineEvents} somewhere in the tree for the socket to
 * run; this selector only reads the cache (it never opens a connection), so a
 * player with no event yet is simply absent from the map.
 */
export function useMediaPlayerStates(): Record<string, MediaPlayerEvent> {
  const query = useQuery<Record<string, MediaPlayerEvent>>({
    queryKey: MEDIA_PLAYER_STATE_QUERY_KEY,
    // The cache is written only by the event reducer; never fetched.
    queryFn: (): Record<string, MediaPlayerEvent> => ({}),
    enabled: false,
    initialData: {},
  });
  // `initialData` makes `data` always defined.
  return query.data;
}
