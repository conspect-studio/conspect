// While a preview's source is starting (its consumer just took a lease and the
// first frame has not arrived), say so instead of showing a black picture.
import type { JSX } from 'react';
import { Trans } from '@lingui/react/macro';

import { useSourceStatus } from '../api/sourceStatus';

/** "Starting…" / "Failed, retrying" notice for source `id`, or nothing. */
export function SourceStartingNotice({ id }: { readonly id: string }): JSX.Element | null {
  const status = useSourceStatus().data?.get(id);
  if (status === undefined || status.phase === 'live') return null;
  if (status.phase === 'failed') {
    return (
      <p role="status" className="text-sm text-muted-foreground" data-testid="source-failed">
        <Trans>This source is not producing yet. It keeps retrying.</Trans>
        {status.detail !== undefined && status.detail !== null ? ` (${status.detail})` : null}
      </p>
    );
  }
  return (
    <p role="status" className="text-sm text-muted-foreground" data-testid="source-starting">
      <Trans>Starting the source… it was idle because nothing was consuming it.</Trans>
    </p>
  );
}
