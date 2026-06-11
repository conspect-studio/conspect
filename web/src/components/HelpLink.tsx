// HelpLink — contextual deep link from a management surface into the in-app
// documentation (ADR-W016).
//
// Renders an accessible "help" affordance that routes to a docs page (and,
// via the URL fragment, a specific DocSection anchor) so an operator can jump
// from the place a question arises straight to the explanation. Two visual
// forms: the default inline icon+label link (page headers, section titles)
// and a compact icon-only form for dense rows/fields (the accessible name is
// preserved via aria-label).
import type { JSX } from "react";
import { useLingui } from "@lingui/react/macro";
import { BookOpen } from "lucide-react";
import { Link } from "react-router-dom";

interface HelpLinkProps {
  /** Docs destination, e.g. "/help/concepts/transports#choosing". */
  readonly to: string;
  /**
   * What the destination explains, e.g. "About source transports". Rendered
   * as the link text (default form) or the aria-label (compact form).
   */
  readonly label: string;
  /** Compact icon-only form for dense rows and form fields. */
  readonly compact?: boolean;
}

/** Deep link into the in-app docs; help is one click from the question. */
export function HelpLink({ to, label, compact = false }: HelpLinkProps): JSX.Element {
  const { t } = useLingui();
  if (compact) {
    return (
      <Link
        to={to}
        aria-label={label}
        title={label}
        className="inline-flex shrink-0 items-center rounded-sm p-1 text-muted-foreground transition-colors hover:text-foreground focus-visible:outline-2 focus-visible:outline-ring"
      >
        <BookOpen className="size-4" aria-hidden="true" />
      </Link>
    );
  }
  return (
    <Link
      to={to}
      className="inline-flex items-center gap-1.5 rounded-sm text-sm text-muted-foreground underline-offset-4 transition-colors hover:text-foreground hover:underline focus-visible:outline-2 focus-visible:outline-ring"
    >
      <BookOpen className="size-4 shrink-0" aria-hidden="true" />
      <span>
        {label}
        <span className="sr-only"> — {t`open documentation`}</span>
      </span>
    </Link>
  );
}
