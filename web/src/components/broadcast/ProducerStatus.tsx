// ProducerStatus — the always-visible output-producer line for the broadcast
// surfaces (correction-4 safety criteria 7): the PRODUCER state, derived from
// observed output ticks only, is kept distinct from the MANAGEMENT
// connection, which renders as its own chip and never feeds the verdict.
// State is conveyed by text + glyph, never colour alone (WCAG 1.4.1); the
// line wraps (`min-w-0`, `break-words`) — the round-3 S2 defect was a
// nowrap + overflow-hidden producer line clipped at 390px. The component
// owns no clock of its own: age is computed against the caller's `now`, so a
// fresh tick is a fact, not a repaint side effect.
import type { JSX } from 'react';
import { useLingui, Trans } from '@lingui/react/macro';
import { CheckCircle2, CircleAlert, CirclePause, TriangleAlert, XCircle } from 'lucide-react';

import { Badge } from '../ui/badge';
import type { BadgeProps } from '../ui/badge';
import { deriveProducerDisplay } from './producerDisplay';
import type { ProducerState } from './producerDisplay';
import { useProducerNowMs } from './producerNow';

/** Props for {@link ProducerStatus}. */
export interface ProducerStatusProps {
  /** The producer state the control plane reports. */
  readonly state: ProducerState;
  /** Epoch ms of the last OBSERVED output tick; null when none was observed. */
  readonly lastTickAt: number | null;
  /** What emitted the observed tick, already localized (e.g. `output engine`). */
  readonly tickSource?: string | null | undefined;
  /** The management-plane connection label, rendered as its own separate chip. */
  readonly managementLabel?: string | null | undefined;
  /** The clock age is measured against; defaults to the shared ticking clock. Inject for tests. */
  readonly now?: (() => number) | undefined;
  /** A tick at least this old is frozen/stale. Default 3000 ms. */
  readonly staleAfterMs?: number | undefined;
}

interface ProducerPresentation {
  readonly variant: NonNullable<BadgeProps['variant']>;
  readonly icon: JSX.Element;
  readonly label: JSX.Element;
}

function present(display: ReturnType<typeof deriveProducerDisplay>['display']): ProducerPresentation {
  switch (display) {
    case 'producing':
      return {
        variant: 'live',
        icon: <CheckCircle2 className="size-3.5" aria-hidden="true" />,
        label: <Trans>Producing</Trans>,
      };
    case 'stale':
      return {
        variant: 'stale',
        icon: <TriangleAlert className="size-3.5" aria-hidden="true" />,
        label: <Trans>Frozen / stale</Trans>,
      };
    case 'failed':
      return {
        variant: 'destructive',
        icon: <XCircle className="size-3.5" aria-hidden="true" />,
        label: <Trans>Failed</Trans>,
      };
    case 'stopped':
      return {
        variant: 'secondary',
        icon: <CirclePause className="size-3.5" aria-hidden="true" />,
        label: <Trans>Stopped</Trans>,
      };
    case 'unknown':
      return {
        variant: 'offline',
        icon: <CircleAlert className="size-3.5" aria-hidden="true" />,
        label: <Trans>Unknown</Trans>,
      };
  }
}

/**
 * The compact producer line: state badge, tick age (or the honest reason no
 * age can be shown), the tick's source, and — separately — the management
 * connection chip. A healthy management plane never makes a stopped, failed
 * or unknown producer read as producing.
 */
export function ProducerStatus({
  state,
  lastTickAt,
  tickSource,
  managementLabel,
  now,
  staleAfterMs = 3000,
}: ProducerStatusProps): JSX.Element {
  const { t } = useLingui();
  // The shared clock keeps renders pure; an injected `now` wins so tests (and
  // any ledger-driven integration) stay deterministic.
  const storeNowMs = useProducerNowMs();
  const nowMs = now !== undefined ? now() : storeNowMs;
  const derived = deriveProducerDisplay(state, lastTickAt, nowMs, staleAfterMs);
  const { variant, icon, label } = present(derived.display);

  const ageText =
    derived.reason === 'no-tick'
      ? t`No output tick observed`
      : derived.reason === 'invalid-observation'
        ? t`Output tick observation is invalid — age cannot be verified`
      : derived.reason === 'future-tick'
        ? t`Tick timestamp is ahead of the clock — age cannot be verified`
        : derived.ageMs !== null
          ? t`last tick ${(derived.ageMs / 1000).toFixed(1)}s ago`
          : null;

  return (
    <div
      role="status"
      aria-live="polite"
      className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 break-words"
    >
      <Badge variant={variant} data-producer-badge="">
        {icon}
        <span>{label}</span>
      </Badge>
      {managementLabel !== undefined && managementLabel !== null ? (
        <Badge variant="outline">
          <Trans>Management</Trans> · {managementLabel}
        </Badge>
      ) : null}
      {ageText !== null ? (
        <span className="text-xs text-muted-foreground">{ageText}</span>
      ) : null}
      {derived.reason === 'state-unknown' ? (
        <span className="text-xs text-muted-foreground">
          <Trans>Producer state not reported</Trans>
        </span>
      ) : null}
      {tickSource !== undefined && tickSource !== null ? (
        <span className="text-xs text-muted-foreground">{tickSource}</span>
      ) : null}
    </div>
  );
}
