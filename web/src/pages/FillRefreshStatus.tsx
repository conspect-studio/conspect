// Scheduled fill-refresh status (issue #472): shown in the Windy fill dialog
// and on each media-player card. Reads `GET /api/v1/media/fills/{id}/refresh`
// (per-object authorized server-side); renders nothing when the fill has no
// schedule or the status cannot be read.
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { Loader2, RefreshCw } from 'lucide-react';

import { useFillRefresh } from '../api/web-captureQueries';
import type { FillRefreshStatus } from '../api/web-captureQueries';

function when(value: string | null | undefined): string {
  if (value === null || value === undefined) return '—';
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? value : date.toLocaleTimeString();
}

/** Pure presentation of one fill's refresh status. */
export function FillRefreshSummary({ status }: { readonly status: FillRefreshStatus }): JSX.Element {
  const { t } = useLingui();
  const phase = status.phase === 'capturing'
    ? t`capturing`
    : status.phase === 'loading'
      ? t`loading new clip`
      : t`idle`;
  return (
    <div role="status" aria-label={t`Scheduled refresh`} className="rounded-md border p-2 text-xs">
      <p className="flex items-center gap-1 font-medium">
        {status.in_flight
          ? <Loader2 className="size-3 animate-spin" aria-hidden="true" />
          : <RefreshCw className="size-3" aria-hidden="true" />}
        <Trans>Refreshes every {status.minutes} min</Trans> · {phase}
      </p>
      <p className="text-muted-foreground">
        <Trans>Next</Trans>: {status.in_flight ? '—' : when(status.next_due)} ·{' '}
        <Trans>Last success</Trans>: {when(status.last_success_at)}
        {status.last_artifact_id !== null && status.last_artifact_id !== undefined
          ? <> · <code className="break-all">{status.last_artifact_id}</code></>
          : null}
      </p>
      {status.update_pending ? (
        <p className="text-muted-foreground">
          <Trans>Updated schedule applies after the current refresh finishes.</Trans>
        </p>
      ) : null}
      {!status.persistent ? (
        <p className="text-amber-700 dark:text-amber-400">
          <Trans>Not kept across a server restart — declare it in [[media_fill]] to make it permanent.</Trans>
        </p>
      ) : null}
      {status.last_error !== null && status.last_error !== undefined ? (
        <p className="text-destructive">
          <Trans>Last refresh failed (previous clip kept)</Trans>: {status.last_error}
          {status.consecutive_failures > 1 ? ` ×${String(status.consecutive_failures)}` : ''}
        </p>
      ) : null}
    </div>
  );
}

/** Self-fetching status for `fillId`; nothing when unscheduled or unreadable. */
export function FillRefreshStatusBadge({ fillId }: { readonly fillId: string }): JSX.Element | null {
  const status = useFillRefresh(fillId);
  if (status.data === null || status.data === undefined) return null;
  return <FillRefreshSummary status={status.data} />;
}
