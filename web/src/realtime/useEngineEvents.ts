// useEngineEvents — the React binding over the realtime WebSocket.
//
// Contract (docs/api/realtime.md §5, §9): snapshot seeds caches, ordered deltas
// patch them; on `$resync` the UI REBUILDS (discards) the affected state rather
// than merging. The hook NEVER blocks render — the socket lives in a ref, all
// state lands via setState/Query-cache writes from event callbacks.
import { useEffect, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import type { QueryClient } from "@tanstack/react-query";

import { getStoredToken } from "../api/token";
import { RealtimeConnection } from "./connection";
import type { RealtimeStatus } from "./connection";
import {
  CONTROL_TOPIC,
  parseTileStateDelta,
  parseTilesSnapshot,
} from "./envelope";
import type { Envelope, TileSnapshotEntry } from "./envelope";
// `LifecycleState` (LIVE/STALE/RECONNECTING/NO_SIGNAL) comes from the generated
// AsyncAPI schema types — the canonical source of truth for tile lifecycle values
// (resilience invariant #2). `envelope.ts` re-exports it as `TileState` for
// backward compat, but we import under the generated name here to make the
// dependency on the spec explicit.
import type { LifecycleState } from "./generated-types";

/** The TanStack Query key the live tile map is mirrored into. */
export const TILES_QUERY_KEY = ["realtime", "tiles"] as const;

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

function resolveWsUrl(): string {
  // Same-origin: the dev proxy and the embedded build both serve `/api/v1/ws`.
  const { protocol, host } = window.location;
  const wsProtocol = protocol === "https:" ? "wss:" : "ws:";
  const base = `${wsProtocol}//${host}/api/v1/ws`;
  // A browser WebSocket can't send an Authorization header, so the control plane
  // also accepts the bearer token as an `access_token` query parameter; pass the
  // operator's stored token (same-origin) so the privileged stream authenticates.
  const token = getStoredToken();
  return token === undefined
    ? base
    : `${base}?access_token=${encodeURIComponent(token)}`;
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
    const connection = new RealtimeConnection(resolveWsUrl(), {
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
        }
        if (envelope.topic === CONTROL_TOPIC) {
          // $resync = rebuild: drop affected caches so stale state cannot leak.
          if (envelope.t === "$resync") {
            queryClient.removeQueries({ queryKey: TILES_QUERY_KEY });
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
          default:
            // Unknown event types are ignored (forward-compatible, §2).
            return;
        }
      },
    });
    connectionRef.current = connection;
    connection.start();
    return (): void => {
      connection.stop();
      connectionRef.current = null;
    };
  }, [queryClient]);

  return { status, lastSeq, gaps };
}
