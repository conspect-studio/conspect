// The viewer playback-URL probe (ADR-0101 — "a link to copy the actual URL
// to paste into a tool like VLC"): `GET /api/v1/outputs/{id}/playback` tells
// the SPA, per output, the URL(s) an operator pastes into VLC/ffplay (or a
// browser for WHEP), with operator-facing notes for a push destination or a
// missing public host. Never fetched for an unsaved (no id) output.
import { useQuery } from '@tanstack/react-query';
import type { UseQueryResult } from '@tanstack/react-query';

import { createApiClient } from '../api/client';
import { isUnavailableStatus } from '../api/operations';
import type { components } from '../api/types';

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
export interface OutputPlayback {
  readonly urls: readonly PlaybackUrl[];
  /** True when the server does not serve the playback route (404/405/501). */
  readonly unavailable: boolean;
}

export function useOutputPlayback(
  outputId: string | undefined,
): UseQueryResult<OutputPlayback> {
  return useQuery<OutputPlayback>({
    queryKey: ['outputs', outputId, 'playback'],
    queryFn: async (): Promise<OutputPlayback> => {
      if (outputId === undefined || outputId === '') {
        return { urls: [], unavailable: false };
      }
      const client = createApiClient();
      const { data, response } = await client.GET('/api/v1/outputs/{id}/playback', {
        params: { path: { id: outputId } },
      });
      // 404/405/501: main does not serve this route yet — an explicit
      // "unavailable (backend not deployed)" state, not an empty success.
      if (isUnavailableStatus(response.status)) {
        return { urls: [], unavailable: true };
      }
      return { urls: data?.urls ?? [], unavailable: false };
    },
    enabled: outputId !== undefined && outputId !== '',
    retry: false,
  });
}
