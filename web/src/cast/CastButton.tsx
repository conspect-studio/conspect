// A Cast button for one target (program / an HLS output): shows browser
// support, idle/connecting/casting/error state, and a Stop action once
// casting. Place wherever that target is listed (desk, Outputs, the Cast
// page).
import { useMemo } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { Cast, Loader2, X } from 'lucide-react';

import { useCast, useCastBusyWithOther } from './CastProvider';
import type { CastTarget } from './castController';
import { isCastSupportedBrowser } from './support';
import { Button } from '../components/ui/button';
import { cn } from '../lib/utils';

export interface CastButtonProps {
  readonly target: CastTarget;
  /** Icon-only footprint for dense rows (table/list cells). */
  readonly compact?: boolean;
  readonly className?: string;
}

/** A Cast (browser sender) button + state for one target. */
export function CastButton({ target, compact = false, className }: CastButtonProps): JSX.Element {
  const { t } = useLingui();
  const cast = useCast(target);
  const busyWithOther = useCastBusyWithOther(target);
  const supported = useMemo(() => isCastSupportedBrowser(), []);

  if (!supported) {
    return (
      <Button
        variant="outline"
        size={compact ? 'icon' : 'sm'}
        className={cn(className)}
        disabled
        aria-label={t`Cast needs Chrome or Edge`}
        title={t`Cast needs Chrome or Edge`}
      >
        <Cast aria-hidden="true" />
        {compact ? null : <Trans>Cast needs Chrome or Edge</Trans>}
      </Button>
    );
  }

  if (cast.status === 'playing' && cast.isThisTarget) {
    return (
      <span className={cn('inline-flex items-center gap-2', className)}>
        <span role="status" aria-live="polite" className="text-sm">
          <Trans>Casting to {cast.deviceName ?? t`this device`}</Trans>
        </span>
        <Button
          variant="ghost"
          size={compact ? 'icon' : 'sm'}
          aria-label={`${t`Stop casting`}: ${target.name}`}
          onClick={cast.stop}
        >
          <X aria-hidden="true" />
          {compact ? null : <Trans>Stop</Trans>}
        </Button>
      </span>
    );
  }

  if ((cast.status === 'connecting' || cast.status === 'loading') && cast.isThisTarget) {
    return (
      <span className={cn('inline-flex items-center gap-2', className)}>
        <Button variant="outline" size={compact ? 'icon' : 'sm'} disabled aria-label={t`Connecting…`}>
          <Loader2 aria-hidden="true" className="animate-spin" />
          {compact ? null : <Trans>Connecting…</Trans>}
        </Button>
        <span role="status" aria-live="polite" className="sr-only">
          <Trans>Connecting…</Trans>
        </span>
      </span>
    );
  }

  if (cast.status === 'error' && cast.isThisTarget) {
    return (
      <span className={cn('inline-flex flex-col items-start gap-1', className)}>
        <span role="alert" aria-live="assertive" className="text-xs text-destructive">
          {cast.errorText}
        </span>
        <Button
          variant="outline"
          size={compact ? 'icon' : 'sm'}
          aria-label={`${t`Retry cast`}: ${target.name}`}
          onClick={cast.start}
        >
          <Cast aria-hidden="true" />
          {compact ? null : <Trans>Retry</Trans>}
        </Button>
      </span>
    );
  }

  return (
    <Button
      variant="outline"
      size={compact ? 'icon' : 'sm'}
      className={cn(className)}
      aria-label={busyWithOther ? `${t`Cast`}: ${target.name} — ${t`replaces the current browser cast`}` : `${t`Cast`}: ${target.name}`}
      title={busyWithOther ? t`Replaces the current browser cast` : undefined}
      onClick={cast.start}
    >
      <Cast aria-hidden="true" />
      {compact ? null : <Trans>Cast</Trans>}
    </Button>
  );
}
