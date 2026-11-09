// OperationFeedback — the stable, honest feedback + Undo region for
// correlated broadcast mutations. Contracts carried from the correction-4
// reviews:
//   * accepted ≠ applied: the pending line says the request was ACCEPTED and
//     is waiting for the applied event — it never claims success early.
//   * a timeout is UNKNOWN, never "failed" — and this component owns no
//     timer, so a pending operation can never age itself into a false
//     failure. Every phase transition is the caller's (the realtime
//     ledger's) decision.
//   * the correlation id is displayed so a timed-out outcome can be
//     reconciled later; an `unknown` outcome persists (no Dismiss is offered
//     for it — only the caller's reconciliation clears it), while a failure
//     persists until explicitly dismissed.
//   * the region is RESERVED: idle, pending and applied all keep the same
//     min-height slot, with no overflow-hidden and no fixed height — the
//     prototype's 68px slot clipped 4px off its own 75px Undo toast
//     (correction-4 comfort, Audio row) and the pending ticker once widened
//     the 320px document; here the region wraps (`break-words`, `min-w-0`)
//     and the Undo key wraps inside it instead.
//   * presentational only: no localStorage authority, no fetch — the
//     component stores nothing and queries nothing.
import type { JSX } from 'react';
import { useLingui, Trans } from '@lingui/react/macro';
import { CheckCircle2, CircleAlert, Loader2, TriangleAlert } from 'lucide-react';

import { cn } from '../../lib/utils';
import { TouchAction } from './TouchAction';

/** The lifecycle of one correlated operation, as the caller's ledger reports it. */
export type OperationPhase =
  | 'idle'
  | 'pending'
  | 'applied'
  | 'failed'
  | 'cancelled'
  | 'unknown';

/** Props for {@link OperationFeedback}. */
export interface OperationFeedbackProps {
  /** The current phase of the operation. */
  readonly phase: OperationPhase;
  /** What the operator asked for, already localized (e.g. `Mute CAM 3`). */
  readonly operationLabel: string;
  /** The correlation id the outcome will be reported against. */
  readonly correlationId?: string | null | undefined;
  /** Why the operation failed, already localized; shown on `failed`. */
  readonly failureReason?: string | null | undefined;
  /** Offer an Undo key on `applied`. Its presence implies undoability. */
  readonly onUndo?: (() => void) | undefined;
  /** Overrides the default `Undo {operationLabel}` key label. */
  readonly undoLabel?: string | undefined;
  /** The undo itself was accepted and is awaiting its applied event. */
  readonly undoPending?: boolean | undefined;
  /** Offer a Dismiss key on `failed` and `cancelled` (they persist until dismissed). */
  readonly onDismiss?: (() => void) | undefined;
}

// The reserved slot: 6rem fits the 48px Undo key, its label line and the
// region padding at every font scale (all rem), with room to spare over the
// prototype's 75px toast. Same class in every phase — no shift on transition.
const REGION_CLASSES =
  'flex min-h-24 min-w-0 flex-col gap-1 break-words rounded-md border bg-muted/40 p-2';

function PhaseIcon({ phase }: { readonly phase: OperationPhase }): JSX.Element {
  if (phase === 'pending') {
    return (
      <Loader2
        className="size-4 shrink-0 animate-spin motion-reduce:animate-none"
        aria-hidden="true"
      />
    );
  }
  if (phase === 'applied') {
    return <CheckCircle2 className="size-4 shrink-0" aria-hidden="true" />;
  }
  if (phase === 'failed') {
    return <TriangleAlert className="size-4 shrink-0" aria-hidden="true" />;
  }
  return <CircleAlert className="size-4 shrink-0" aria-hidden="true" />;
}

/**
 * The reserved feedback region for one correlated operation. Render it inline
 * under the action it reports on; the slot exists (and keeps its size) even
 * while idle, so appearing feedback never moves the controls around it.
 */
export function OperationFeedback({
  phase,
  operationLabel,
  correlationId,
  failureReason,
  onUndo,
  undoLabel,
  undoPending,
  onDismiss,
}: OperationFeedbackProps): JSX.Element {
  const { t } = useLingui();
  return (
    <div
      role="status"
      aria-live="polite"
      data-phase={phase}
      className={cn(REGION_CLASSES)}
    >
      {phase === 'pending' ? (
        <p className="flex items-center gap-2 text-sm">
          <PhaseIcon phase={phase} />
          <span>
            <Trans>
              {operationLabel} accepted — waiting for the applied event.
            </Trans>
          </span>
        </p>
      ) : null}
      {phase === 'applied' ? (
        <p className="flex items-center gap-2 text-sm">
          <PhaseIcon phase={phase} />
          <span>
            <Trans>{operationLabel} applied.</Trans>
          </span>
        </p>
      ) : null}
      {phase === 'failed' ? (
        <p className="flex items-center gap-2 text-sm">
          <PhaseIcon phase={phase} />
          <span>
            <Trans>{operationLabel} failed — nothing was changed.</Trans>
          </span>
        </p>
      ) : null}
      {phase === 'cancelled' ? (
        <p className="flex items-center gap-2 text-sm">
          <PhaseIcon phase={phase} />
          <span>
            <Trans>{operationLabel} cancelled — nothing was changed.</Trans>
          </span>
        </p>
      ) : null}
      {phase === 'unknown' ? (
        <p className="flex items-center gap-2 text-sm">
          <PhaseIcon phase={phase} />
          <span>
            <Trans>{operationLabel} timed out — outcome unknown.</Trans>
          </span>
        </p>
      ) : null}
      {phase === 'failed' && failureReason !== undefined && failureReason !== null ? (
        <p className="text-sm text-muted-foreground">{failureReason}</p>
      ) : null}
      {correlationId !== undefined && correlationId !== null ? (
        <p className="text-xs text-muted-foreground">
          <Trans>Correlation</Trans>{' '}
          <code className="rounded bg-muted px-1 py-0.5 font-mono">
            {correlationId}
          </code>
        </p>
      ) : null}
      {phase === 'applied' && onUndo !== undefined ? (
        <TouchAction
          variant="outline"
          onPress={onUndo}
          pending={undoPending}
          // The undo key may carry a long label: wrap inside the reserved
          // region instead of overflowing it at 320–390px.
          className="max-w-full self-start whitespace-normal text-left"
        >
          {undoLabel ?? t`Undo ${operationLabel}`}
        </TouchAction>
      ) : null}
      {(phase === 'failed' || phase === 'cancelled') && onDismiss !== undefined ? (
        <TouchAction
          variant="outline"
          onPress={onDismiss}
          className="self-start"
        >
          <Trans>Dismiss</Trans>
        </TouchAction>
      ) : null}
    </div>
  );
}
