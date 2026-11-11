// Engine-tick advancement evidence. A poll that merely succeeds (or returns the
// same tick again), an open socket, or a cached remount never proves the program
// is live: only an OBSERVED INCREASE of the engine tick between two observations
// does. State lives in a store keyed by QueryClient so it survives remounts of
// the consuming component without mutating anything during render.
import { useEffect, useSyncExternalStore } from 'react';
import type { QueryClient } from '@tanstack/react-query';

export const TICK_FRESH_MS = 6000;

interface Observation { lastTick: number | undefined; advancedAt: number; observations: number; readonly listeners: Set<() => void> }
const stores = new WeakMap<QueryClient, Map<string, Observation>>();

function observationFor(client: QueryClient, key: string): Observation {
  let byKey = stores.get(client);
  if (!byKey) { byKey = new Map(); stores.set(client, byKey); }
  let entry = byKey.get(key);
  if (!entry) { entry = { lastTick: undefined, advancedAt: 0, observations: 0, listeners: new Set() }; byKey.set(key, entry); }
  return entry;
}

/** Record one polled tick. Returns true when it changed the confirmation evidence. */
export function observeTick(entry: { lastTick: number | undefined; advancedAt: number; observations: number }, tick: number, at: number): boolean {
  entry.observations += 1;
  if (entry.lastTick === undefined) { entry.lastTick = tick; return false; }
  if (tick > entry.lastTick) { entry.lastTick = tick; entry.advancedAt = at; return true; }
  if (tick < entry.lastTick) { entry.lastTick = tick; entry.advancedAt = 0; return true; } // engine restart / counter reset
  return false; // same tick: never refreshes confirmation
}

/**
 * Time (ms epoch) of the last observed tick advancement, 0 when none. `updatedAt`
 * keys the effect so each completed fetch is observed exactly once.
 */
export function useTickAdvancedAt(client: QueryClient, key: string, tick: number | undefined, updatedAt: number): number {
  const entry = observationFor(client, key);
  useEffect(() => {
    if (tick === undefined || updatedAt <= 0) return;
    const observation = observationFor(client, key);
    if (observeTick(observation, tick, updatedAt)) for (const listener of observation.listeners) listener();
  }, [client, key, tick, updatedAt]);
  return useSyncExternalStore(
    (listener) => { entry.listeners.add(listener); return () => { entry.listeners.delete(listener); }; },
    () => entry.advancedAt,
  );
}

/** Tick-advancement freshness: observed advancement AND the poll itself recent. */
export function tickConfirmed(advancedAt: number, updatedAt: number, now: number, isError: boolean): boolean {
  return !isError && advancedAt > 0 && updatedAt > 0 && now - advancedAt < TICK_FRESH_MS && now - updatedAt < TICK_FRESH_MS;
}
