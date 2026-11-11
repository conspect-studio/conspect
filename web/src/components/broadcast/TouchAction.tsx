// TouchAction — the broadcast 48px action key. Every mutation control in the
// broadcast surfaces must keep a 48×48 minimum target (design round 2: "every
// button/select ≥48px"); the pending spinner is anchored INSIDE the button's
// own box (correction-4 comfort N7: a spinner once painted over the
// neighbouring key and inflated scrollWidth), and a disabled action always
// says WHY — the reason is visible text tied by aria-describedby, so a
// blocked control is never a mystery (WCAG 1.4.1 and the round-2 gate rule).
// jsdom cannot measure hit rectangles; the physical 48px assertion lives in
// the browser regression notes (README.md).
import { useId } from 'react';
import type { JSX, ReactNode } from 'react';
import { Loader2 } from 'lucide-react';

import { cn } from '../../lib/utils';
import { Button } from '../ui/button';
import type { ButtonProps } from '../ui/button';

/** Props for {@link TouchAction}. */
export interface TouchActionProps
  extends Omit<
    ButtonProps,
    'children' | 'onClick' | 'size' | 'variant' | 'asChild'
  > {
  /** The visible action label (already localized by the caller). */
  readonly children: ReactNode;
  /** Fires on press; never while `disabled` or `pending`. */
  readonly onPress: () => void;
  /** The operation was accepted and is awaiting its applied event. */
  readonly pending?: boolean | undefined;
  /** A localized sentence explaining WHY the action is unavailable. */
  readonly disabledReason?: string | undefined;
  /** The shadcn button variant; the 48px floor is kept regardless. */
  readonly variant?: NonNullable<ButtonProps['variant']> | undefined;
}

/**
 * A minimum-48px action button with its own pending spinner and a visible
 * disabled reason.
 */
export function TouchAction({
  children,
  onPress,
  pending = false,
  disabled = false,
  disabledReason,
  variant = 'default',
  className,
  ...rest
}: TouchActionProps): JSX.Element {
  const reasonId = useId();
  const showReason = disabled && disabledReason !== undefined;
  return (
    <>
      <Button
        type="button"
        variant={variant}
        disabled={disabled || pending}
        aria-busy={pending ? true : undefined}
        aria-describedby={showReason ? reasonId : undefined}
        title={showReason ? disabledReason : undefined}
        className={cn('relative h-12 min-w-12', className)}
        onClick={onPress}
        {...rest}
      >
        <span
          className={cn('flex items-center gap-2', pending && 'opacity-50')}
        >
          {children}
        </span>
        {pending ? (
          <span
            aria-hidden="true"
            className="absolute inset-0 flex items-center justify-center"
          >
            <Loader2 className="size-4 animate-spin motion-reduce:animate-none" />
          </span>
        ) : null}
      </Button>
      {showReason ? (
        <p id={reasonId} className="text-xs text-muted-foreground">
          {disabledReason}
        </p>
      ) : null}
    </>
  );
}
