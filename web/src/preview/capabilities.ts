// The preview-transport capabilities probe (ADR-P006 move 6 / ADR-W023).
//
// `GET /api/v1/preview/capabilities` tells the SPA — BEFORE it issues any WHEP
// offer — whether this build can serve WebRTC at all and, per scope, whether a
// WHEP focus can be opened (plus the program scope's fidelity label). The
// `<PreviewSurface>` ladder consults this to pick WHEP vs the JPEG poll: on a
// build with `webrtc: false` JPEG is the honest primary, not a degradation.
import { useQuery } from '@tanstack/react-query';
import type { UseQueryResult } from '@tanstack/react-query';

import { createApiClient } from '../api/client';
import type { components } from '../api/schema';

/** The capabilities document (`PreviewCapabilities`, the generated schema type). */
export type PreviewCapabilities = components['schemas']['PreviewCapabilities'];

/** Per-scope WHEP availability + the program fidelity label. */
export type ScopeCapability = components['schemas']['ScopeCapability'];

/** The program WHEP fidelity label (ADR-P006). */
export type ProgramFidelity = components['schemas']['ProgramFidelity'];

/** The preview scopes a surface can target. */
export type PreviewScope = 'program' | 'input' | 'output';

/**
 * Whether a WHEP focus can be opened on `scope` given the capabilities document.
 * A `false`/absent document, `webrtc: false`, or a scope whose `whep` is false
 * all mean "no WHEP" — the surface then uses the JPEG rung as its primary.
 */
export function whepAvailable(
  capabilities: PreviewCapabilities | undefined,
  scope: PreviewScope,
): boolean {
  if (!capabilities?.webrtc) {
    return false;
  }
  switch (scope) {
    case 'program':
      return capabilities.scopes.program.whep;
    case 'input':
      return capabilities.scopes.inputs.whep;
    case 'output':
      return capabilities.scopes.outputs.whep;
  }
}

/** The program fidelity label the capabilities document advertises, if any. */
export function programFidelity(
  capabilities: PreviewCapabilities | undefined,
): components['schemas']['ProgramFidelity'] | undefined {
  return capabilities?.scopes.program.fidelity ?? undefined;
}

/**
 * Fetch the preview capabilities once (cached). Best-effort: a failed probe
 * resolves to `undefined` so the surface falls back to JPEG rather than erroring
 * — the probe never blocks the UI (engine isolation, inv #10).
 */
export function usePreviewCapabilities(): UseQueryResult<PreviewCapabilities | undefined> {
  return useQuery<PreviewCapabilities | undefined>({
    queryKey: ['preview', 'capabilities'],
    queryFn: async (): Promise<PreviewCapabilities | undefined> => {
      const client = createApiClient();
      const { data } = await client.GET('/api/v1/preview/capabilities');
      return data;
    },
    // Capabilities are a build property — they do not change while the page is
    // open; cache for the session and never auto-refetch.
    staleTime: Infinity,
    retry: false,
  });
}
