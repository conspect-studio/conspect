// React Query bindings for the managed-devices domain.
//
// Device/sync-group CRUD rides the shared resources hooks (ETag/If-Match,
// list invalidation); this module adds the read hooks projected onto the
// devices view-models, the merged runtime-status map (the conflated
// `device.status` WS lane first, REST fallback otherwise — the UI degrades
// gracefully when the stream is down, invariant #10), the projection
// endpoints, and the untrusted discovery inventory.
import { useQueries, useQuery } from '@tanstack/react-query';
import type { UseQueryResult } from '@tanstack/react-query';

import {
  fetchDeviceStatus,
  listDiscovered,
  listOutputTargets,
  listSourceCandidates,
  toDeviceView,
  toSyncGroupView,
} from './api';
import type {
  DiscoveredServiceView,
  OutputTargetView,
  SourceCandidateView,
} from './api';
import type { DeviceView, SyncGroupView } from './types';
import { listResources } from '../resources/api';
import { resourceKeys } from '../resources/queries';
import type { ResourceRecord } from '../resources/types';
import {
  DEVICE_STATUS_QUERY_KEY,
  ENGINE_CLOCK_QUERY_KEY,
} from '../realtime/useEngineEvents';
import type { EngineClockRef } from '../realtime/useEngineEvents';
import type { DeviceStatus } from '../realtime/generated-types';

/** List the managed devices, projected to {@link DeviceView}. */
export function useDevices(): UseQueryResult<readonly DeviceView[]> {
  return useQuery<readonly DeviceView[]>({
    queryKey: resourceKeys.list('devices'),
    queryFn: async (): Promise<readonly DeviceView[]> => {
      const records: ResourceRecord[] = await listResources('devices');
      return records.map(toDeviceView);
    },
  });
}

/** List the sync groups, projected to {@link SyncGroupView}. */
export function useSyncGroups(): UseQueryResult<readonly SyncGroupView[]> {
  return useQuery<readonly SyncGroupView[]>({
    queryKey: resourceKeys.list('sync-groups'),
    queryFn: async (): Promise<readonly SyncGroupView[]> => {
      const records: ResourceRecord[] = await listResources('sync-groups');
      return records.map(toSyncGroupView);
    },
  });
}

/**
 * The live per-device status map the realtime stream mirrors into the cache.
 * Passive (`enabled: false`): it never fetches, it only re-renders when the
 * WebSocket hook writes the key.
 */
export function useLiveDeviceStatuses(): Readonly<Record<string, DeviceStatus>> {
  const query = useQuery<Record<string, DeviceStatus>>({
    queryKey: DEVICE_STATUS_QUERY_KEY,
    queryFn: (): Record<string, DeviceStatus> => ({}),
    enabled: false,
    initialData: {},
  });
  return query.data;
}

/** The engine-monotonic clock reference (see ENGINE_CLOCK_QUERY_KEY). */
export function useEngineClockRef(): EngineClockRef | undefined {
  const query = useQuery<EngineClockRef | undefined>({
    queryKey: ENGINE_CLOCK_QUERY_KEY,
    queryFn: (): EngineClockRef | undefined => undefined,
    enabled: false,
    initialData: undefined,
  });
  return query.data;
}

/** How often the REST status fallback re-polls (the WS lane is primary). */
const STATUS_FALLBACK_REFETCH_MS = 15_000;

/**
 * The merged per-device runtime status for `ids`: the conflated `device.status`
 * WS lane wins where it has delivered; the REST snapshot fills the rest (and
 * the whole map when the stream is down). Devices with no status anywhere are
 * simply absent — state is never fabricated.
 */
export function useDeviceStatuses(
  ids: readonly string[],
): Readonly<Record<string, DeviceStatus>> {
  const live = useLiveDeviceStatuses();
  const fallback = useQueries({
    queries: ids.map((id) => ({
      queryKey: ['devices', 'status-fallback', id],
      queryFn: async (): Promise<DeviceStatus | null> =>
        (await fetchDeviceStatus(id)) ?? null,
      refetchInterval: STATUS_FALLBACK_REFETCH_MS,
      retry: false,
    })),
  });
  // Plain per-render merge (no memo): the map is tiny and consumers read
  // fields, so a fresh identity per render is cheaper than chasing the
  // unstable useQueries array identity through a dependency list.
  const merged: Record<string, DeviceStatus> = {};
  ids.forEach((id, index) => {
    const rest = fallback[index]?.data;
    if (rest !== null && rest !== undefined) {
      merged[id] = rest;
    }
    const fresh = live[id];
    if (fresh !== undefined) {
      merged[id] = fresh;
    }
  });
  return merged;
}

/** The device's enumerated source candidates (ADR-M009 facet (a)). */
export function useSourceCandidates(
  deviceId: string | undefined,
): UseQueryResult<readonly SourceCandidateView[]> {
  return useQuery<readonly SourceCandidateView[]>({
    queryKey: ['devices', 'source-candidates', deviceId ?? ''],
    queryFn: async (): Promise<readonly SourceCandidateView[]> =>
      deviceId === undefined ? [] : listSourceCandidates(deviceId),
    enabled: deviceId !== undefined,
  });
}

/** The device's enumerated decode targets (ADR-M009 facet (b)). */
export function useOutputTargets(
  deviceId: string | undefined,
): UseQueryResult<readonly OutputTargetView[]> {
  return useQuery<readonly OutputTargetView[]>({
    queryKey: ['devices', 'output-targets', deviceId ?? ''],
    queryFn: async (): Promise<readonly OutputTargetView[]> =>
      deviceId === undefined ? [] : listOutputTargets(deviceId),
    enabled: deviceId !== undefined,
  });
}

/** The Query key for the untrusted discovery inventory snapshot. */
export const DISCOVERY_INVENTORY_QUERY_KEY = ['discovery', 'inventory'] as const;

/** The untrusted discovery inventory (REST snapshot; hints, never devices). */
export function useDiscoveredInventory(): UseQueryResult<
  readonly DiscoveredServiceView[]
> {
  return useQuery<readonly DiscoveredServiceView[]>({
    queryKey: DISCOVERY_INVENTORY_QUERY_KEY,
    queryFn: async (): Promise<readonly DiscoveredServiceView[]> => listDiscovered(),
  });
}
