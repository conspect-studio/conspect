// React Query bindings for the IPTV-ORG catalogue (see ./catalogue.ts).
import { useMutation, useQuery } from '@tanstack/react-query';
import type { UseMutationResult, UseQueryResult } from '@tanstack/react-query';

import {
  fetchIndex,
  fetchLinks,
  fetchPlaces,
  fetchProbe,
  fetchStatus,
  startProbe,
} from './catalogue';
import type {
  CatalogueIndex,
  CatalogueStatus,
  LinksResponse,
  PlacesResponse,
  ProbeJob,
  ProbeRequest,
  ProbeResult,
} from './catalogue';

/** Catalogue status (sync state, counts, optional server location). */
export function useCatalogueStatus(): UseQueryResult<CatalogueStatus, Error> {
  return useQuery<CatalogueStatus, Error>({
    queryKey: ['catalogue', 'iptv-org', 'status'],
    queryFn: () => fetchStatus(),
    retry: false,
  });
}

/** The joined index (large; cached for the session — the server ETags it). */
export function useCatalogueIndex(): UseQueryResult<CatalogueIndex, Error> {
  return useQuery<CatalogueIndex, Error>({
    queryKey: ['catalogue', 'iptv-org', 'index'],
    queryFn: () => fetchIndex(),
    staleTime: 5 * 60_000,
  });
}

/** The place gazetteer slice. */
export function useCataloguePlaces(): UseQueryResult<PlacesResponse, Error> {
  return useQuery<PlacesResponse, Error>({
    queryKey: ['catalogue', 'iptv-org', 'places'],
    queryFn: () => fetchPlaces(),
    staleTime: 5 * 60_000,
  });
}

/** Per-source link statuses (Sources page). Degrades quietly if unavailable. */
export function useCatalogueLinks(enabled = true): UseQueryResult<LinksResponse, Error> {
  return useQuery<LinksResponse, Error>({
    queryKey: ['catalogue', 'iptv-org', 'links'],
    queryFn: () => fetchLinks(),
    enabled,
    retry: false,
    staleTime: 30_000,
  });
}

const POLL_MS = 1000;
const MAX_POLLS = 40;

/** Run a probe job to completion (start, then poll until `done`/`failed`). */
export async function runProbe(
  request: ProbeRequest,
  pollMs: number = POLL_MS,
): Promise<readonly ProbeResult[]> {
  let job: ProbeJob = await startProbe(request);
  for (let i = 0; i < MAX_POLLS && (job.state === 'queued' || job.state === 'running'); i += 1) {
    await new Promise((resolve) => setTimeout(resolve, pollMs));
    job = await fetchProbe(job.id);
  }
  if (job.state === 'failed') {
    throw new Error(job.error ?? 'Probe failed');
  }
  if (job.state !== 'done') {
    throw new Error('Probe timed out');
  }
  return job.results;
}

/** Probe mutation: results in, caller merges them into its own view state. */
export function useProbeMutation(
  pollMs?: number,
): UseMutationResult<readonly ProbeResult[], Error, ProbeRequest> {
  return useMutation<readonly ProbeResult[], Error, ProbeRequest>({
    mutationFn: (request) => runProbe(request, pollMs),
  });
}
