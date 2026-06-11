// Read the realtime tile map that `useEngineEvents` (mounted once in the app
// shell) mirrors into the Query cache. This is a PASSIVE read — `enabled:
// false` means it never fetches; it only re-renders when the WebSocket hook
// writes the key. Best-effort by design (invariant #10): when the stream is
// down the map is simply empty.
import { useMemo } from 'react';
import { useQuery } from '@tanstack/react-query';

import { TILES_QUERY_KEY } from '../realtime/useEngineEvents';
import type { LiveTile } from '../realtime/useEngineEvents';

/** The live tiles keyed by tile id (empty when no stream/snapshot yet). */
export function useLiveTiles(): ReadonlyMap<string, LiveTile> {
  const query = useQuery<Record<string, LiveTile>>({
    queryKey: TILES_QUERY_KEY,
    queryFn: (): Record<string, LiveTile> => ({}),
    enabled: false,
    initialData: {},
  });
  return useMemo(() => new Map(Object.entries(query.data)), [query.data]);
}

/**
 * The live tile showing `sourceId`, if the running engine has one. Tiles are
 * keyed by cell id and carry the bound source in `input`, so a source matches
 * either by its own id or by a tile's input binding.
 */
export function tileForSource(
  tiles: ReadonlyMap<string, LiveTile>,
  sourceId: string,
): LiveTile | undefined {
  const direct = tiles.get(sourceId);
  if (direct !== undefined) {
    return direct;
  }
  for (const tile of tiles.values()) {
    if (tile.input === sourceId) {
      return tile;
    }
  }
  return undefined;
}
