// TaskWorkspace — the responsive task-fold pattern from the broadcast design
// reviews: a task card whose TITLE, compact STATUS and PRIMARY control stay in
// the first phone fold, with secondary detail collapsed behind a 48px Details
// disclosure on narrow viewports and in flow on wide ones (the Library
// "Details" pattern). The status line wraps (`min-w-0`, `break-words`) — the
// round-3 S2 defect was a nowrap + overflow-hidden status line clipped at
// 390px. The disclosure is fully CONTROLLED: the caller owns the open state,
// so no hidden state can disagree with the ledger.
// jsdom has no layout; the narrow/wide split is the standard Tailwind
// `hidden md:block` / `md:hidden` pair, verified in the browser notes.
import type { JSX, ReactNode } from 'react';
import { useId } from 'react';
import { useLingui } from '@lingui/react/macro';

import { Card, CardContent, CardHeader, CardTitle } from '../ui/card';
import { TouchAction } from './TouchAction';

interface TaskWorkspaceBaseProps {
  /** The task heading, already localized. */
  readonly title: ReactNode;
  /** The compact status line (e.g. a {@link ProducerStatus}); always visible, never inside the disclosure. */
  readonly status?: ReactNode | undefined;
  /** The primary task surface — the controls that must stay in the fold. */
  readonly children: ReactNode;
}

/** Props for {@link TaskWorkspace}: either no details, or a fully controlled disclosure. */
export type TaskWorkspaceProps = TaskWorkspaceBaseProps &
  (
    | {
        readonly details?: undefined;
        readonly detailsOpen?: undefined;
        readonly onDetailsToggle?: undefined;
        readonly detailsLabel?: undefined;
      }
    | {
        /** Secondary detail; collapses behind the Details key on narrow viewports. */
        readonly details: ReactNode;
        /** Whether the disclosure is open (the caller owns this state). */
        readonly detailsOpen: boolean;
        /** Called with the next open state when the Details key is pressed. */
        readonly onDetailsToggle: (open: boolean) => void;
        /** Overrides the default `Details` key label. */
        readonly detailsLabel?: string | undefined;
      }
  );

/**
 * A broadcast task card: title, always-visible compact status, primary
 * content, and an optional responsive details disclosure.
 */
export function TaskWorkspace(props: TaskWorkspaceProps): JSX.Element {
  const { title, status, children } = props;
  const { t } = useLingui();
  const panelId = useId();
  return (
    <Card>
      <CardHeader className="gap-2 p-4">
        <CardTitle>{title}</CardTitle>
        {status !== undefined ? (
          <div
            data-testid="task-workspace-status"
            className="flex min-w-0 flex-wrap break-words text-xs text-muted-foreground"
          >
            {status}
          </div>
        ) : null}
      </CardHeader>
      <CardContent className="p-4 pt-0">{children}</CardContent>
      {props.details !== undefined ? (
        <>
          <CardContent className="p-4 pt-0">
            <TouchAction
              variant="outline"
              className="md:hidden"
              aria-expanded={props.detailsOpen}
              aria-controls={panelId}
              onPress={(): void => {
                props.onDetailsToggle(!props.detailsOpen);
              }}
            >
              {props.detailsLabel ?? t`Details`}
            </TouchAction>
          </CardContent>
          <CardContent className="p-4 pt-0">
            <div
              id={panelId}
              className={props.detailsOpen ? 'block' : 'hidden md:block'}
            >
              {props.details}
            </div>
          </CardContent>
        </>
      ) : null}
    </Card>
  );
}
