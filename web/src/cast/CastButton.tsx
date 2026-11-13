// A Cast button for one target (Program, Preview, a source): shows browser
// support, the progress of a start, casting + Stop once the TV plays, and an
// error with Retry. The detailed card (volume, report) lives on the Cast page.
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { Cast, Loader2, X } from 'lucide-react';

import { useCast, useCastBusyWithOther, useCastReceiver } from './CastProvider';
import type { CastTarget } from './castController';
import { castStatusLabel } from './castLabels';
import { Button } from '../components/ui/button';
import { cn } from '../lib/utils';

export interface CastButtonProps {
  readonly target: CastTarget;
  /** Icon-only footprint for dense places (monitor captions, table cells). */
  readonly compact?: boolean;
  readonly className?: string;
}

/** A Cast (browser sender) button + state for one target. */
export function CastButton({ target, compact = false, className }: CastButtonProps): JSX.Element {
  const { t } = useLingui();
  const cast = useCast(target);
  const busyWithOther = useCastBusyWithOther(target);
  const receiver = useCastReceiver();
  const size = compact ? 'icon' : 'sm';

  if (!receiver.supported) {
    return (
      <Button
        variant="outline"
        size={size}
        className={cn(className)}
        disabled
        aria-label={t`Cast needs Chrome or Edge on a computer or Android`}
        title={t`Cast needs Chrome or Edge on a computer or Android`}
      >
        <Cast aria-hidden="true" />
        {compact ? null : <Trans>Cast needs Chrome or Edge</Trans>}
      </Button>
    );
  }

  if (cast.isThisTarget && (cast.status === 'playing' || cast.status === 'buffering')) {
    return (
      <span className={cn('inline-flex items-center gap-2', className)}>
        <span role="status" aria-live="polite" className={compact ? 'sr-only' : 'text-sm'}>
          {cast.status === 'playing' ? (
            <Trans>Casting to {cast.deviceName ?? t`this device`}</Trans>
          ) : (
            <Trans>Buffering on {cast.deviceName ?? t`this device`}</Trans>
          )}
        </span>
        <Button
          variant="ghost"
          size={size}
          aria-label={`${t`Stop casting`}: ${target.name}`}
          title={t`Stop casting`}
          onClick={cast.stop}
        >
          <X aria-hidden="true" />
          {compact ? null : <Trans>Stop</Trans>}
        </Button>
      </span>
    );
  }

  if (
    cast.isThisTarget &&
    (cast.status === 'preparing' || cast.status === 'connecting' || cast.status === 'loading' || cast.status === 'stopping')
  ) {
    const label = castStatusLabel(cast.status);
    return (
      <span className={cn('inline-flex items-center gap-2', className)}>
        <Button variant="outline" size={size} disabled aria-label={label} title={label}>
          <Loader2 aria-hidden="true" className="animate-spin" />
          {compact ? null : label}
        </Button>
        <span role="status" aria-live="polite" className="sr-only">
          {label}
        </span>
      </span>
    );
  }

  if (cast.isThisTarget && cast.status === 'error') {
    return (
      <span className={cn('inline-flex flex-col items-start gap-1', className)}>
        <span role="alert" className={compact ? 'sr-only' : 'text-xs text-destructive'}>
          {cast.errorText}
        </span>
        <Button
          variant="outline"
          size={size}
          aria-label={`${t`Retry cast`}: ${target.name}`}
          title={cast.errorText ?? t`Retry cast`}
          onClick={cast.start}
        >
          <Cast aria-hidden="true" />
          {compact ? null : <Trans>Retry</Trans>}
        </Button>
      </span>
    );
  }

  const label = busyWithOther
    ? `${t`Cast`}: ${target.name} — ${t`replaces the current browser cast`}`
    : `${t`Cast`}: ${target.name}`;
  return (
    <Button
      variant="outline"
      size={size}
      className={cn(className)}
      disabled={!receiver.ready}
      aria-label={label}
      title={busyWithOther ? t`Replaces the current browser cast` : label}
      onClick={cast.start}
    >
      <Cast aria-hidden="true" />
      {compact ? null : <Trans>Cast</Trans>}
    </Button>
  );
}
