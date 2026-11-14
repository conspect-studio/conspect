// Subtitle tab rows (patch-f note, #345): sources that carry a `captions`
// selector other than `off`/absent. `GET /patch` has no subtitle-source
// listing of its own yet, so this reads the raw source documents (the same
// `GET /api/v1/sources` the rest of the page already uses) and inspects the
// opaque body — no hand-written fetch, reusing `listResources`.
import { useQuery } from '@tanstack/react-query';
import { listResources } from '../../resources/api';
import { resourceKeys } from '../../resources/queries';

/** One source with captions, for the Subtitle-tab row list. */
export interface SubtitleSourceRow {
  readonly id: string;
  readonly name: string;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

function hasCaptions(body: Record<string, unknown>): boolean {
  const captions = body.captions;
  if (!isRecord(captions)) return false;
  const mode = captions.mode;
  return typeof mode === 'string' && mode !== 'off';
}

/** Sources whose declared `captions` selector is present and not `off`. */
export function useSubtitleSources(): { readonly data: readonly SubtitleSourceRow[] | undefined; readonly isLoading: boolean } {
  const query = useQuery({
    queryKey: [...resourceKeys.list('sources'), 'captions'],
    queryFn: async (): Promise<readonly SubtitleSourceRow[]> => {
      const records = await listResources('sources');
      return records.filter((r) => hasCaptions(r.body)).map((r) => ({ id: r.id, name: r.name }));
    },
  });
  return { data: query.data, isLoading: query.isPending };
}
