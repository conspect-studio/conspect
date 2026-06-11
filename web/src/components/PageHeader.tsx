// A route's heading region. Renders the single <h1> per route (SC 1.3.1/2.4.3).
import type { JSX, ReactNode } from "react";

/** Props for {@link PageHeader}. */
export interface PageHeaderProps {
  /** The route title (the page's single h1). */
  readonly title: ReactNode;
  /** Optional supporting description. */
  readonly description?: ReactNode;
  /** Optional trailing actions (buttons, etc.). */
  readonly actions?: ReactNode;
}

/** Standard page heading block. */
export function PageHeader({
  title,
  description,
  actions,
}: PageHeaderProps): JSX.Element {
  return (
    <div className="mb-6 flex flex-wrap items-start justify-between gap-3">
      <div className="space-y-1">
        <h1 className="text-2xl font-semibold tracking-tight">{title}</h1>
        {description !== undefined ? (
          <p className="text-sm text-muted-foreground">{description}</p>
        ) : null}
      </div>
      {actions !== undefined ? (
        <div className="flex items-center gap-2">{actions}</div>
      ) : null}
    </div>
  );
}
