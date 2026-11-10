// The viewer playback-URL probe (ADR-0101 — "a link to copy the actual URL
// to paste into a tool like VLC"): `GET /api/v1/outputs/{id}/playback` tells
// the SPA, per output, the URL(s) an operator pastes into VLC/ffplay (or a
// browser for WHEP), with operator-facing notes for a push destination or a
// missing public host. Never fetched for an unsaved (no id) output.
import { useQuery } from '@tanstack/react-query';
import type { UseQueryResult } from '@tanstack/react-query';

import { createApiClient } from '../api/client';
import type { components } from '../api/schema';

/** One playback URL (or no-URL note) — the generated schema type. */
export type PlaybackUrl = components['schemas']['PlaybackUrlDoc'];

/**
 * Fetch an output's playback URL(s). Best-effort: a failed probe resolves to
 * an empty list so the UI simply omits the playback section rather than
 * erroring — this is a convenience lookup, never load-bearing for the output
 * CRUD surface itself. Disabled (never fetches) while `outputId` is absent —
 * React Query forbids a query function returning `undefined`, so an empty
 * array is the "nothing to show yet" value, not `undefined`.
 */
export function useOutputPlayback(
  outputId: string | undefined,
): UseQueryResult<readonly PlaybackUrl[]> {
  return useQuery<readonly PlaybackUrl[]>({
    queryKey: ['outputs', outputId, 'playback'],
    queryFn: async (): Promise<readonly PlaybackUrl[]> => {
      if (outputId === undefined || outputId === '') {
        return [];
      }
      const client = createApiClient();
      const { data } = await client.GET('/api/v1/outputs/{id}/playback', {
        params: { path: { id: outputId } },
      });
      return data?.urls ?? [];
    },
    enabled: outputId !== undefined && outputId !== '',
    retry: false,
  });
}
