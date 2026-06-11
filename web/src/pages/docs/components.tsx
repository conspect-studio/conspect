// Presentational primitives shared by the documentation pages.
//
// They wrap shadcn Card styling and a small set of prose helpers so each topic
// page stays focused on content. Headings nest correctly under the route's
// single <h1> (the page renders an <h2> per section; these helpers emit <h3>).
import type { JSX, ReactNode } from "react";
import { Trans, useLingui } from "@lingui/react/macro";
import { Link as LinkIcon } from "lucide-react";

import { Badge } from "../../components/ui/badge";
import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from "../../components/ui/card";

/**
 * A titled documentation section rendered as a Card with an `<h3>` heading.
 *
 * `id` is the section's public anchor (ADR-W016): it must match the page's
 * entry in the docs registry, is kebab-case, and is append-only — renames
 * require a registry redirect. The heading carries a copyable anchor link
 * that is visible on hover and keyboard focus.
 */
export function DocSection({
  id,
  title,
  children,
}: {
  readonly id: string;
  readonly title: ReactNode;
  readonly children: ReactNode;
}): JSX.Element {
  const { t } = useLingui();
  return (
    <section id={id} className="group/docsection scroll-mt-20 rounded-xl">
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <span>{title}</span>
            <a
              href={`#${id}`}
              aria-label={t`Link to section`}
              className="rounded-sm text-muted-foreground opacity-0 transition-opacity hover:text-foreground focus-visible:opacity-100 focus-visible:outline-2 focus-visible:outline-ring group-hover/docsection:opacity-100"
            >
              <LinkIcon className="size-3.5" aria-hidden="true" />
            </a>
          </CardTitle>
        </CardHeader>
        <CardContent className="space-y-4 text-sm leading-relaxed text-muted-foreground">
          {children}
        </CardContent>
      </Card>
    </section>
  );
}

/** A body paragraph with comfortable measure and contrast. */
export function Prose({ children }: { readonly children: ReactNode }): JSX.Element {
  return <p className="max-w-prose">{children}</p>;
}

/** A short inline code span. */
export function Code({ children }: { readonly children: ReactNode }): JSX.Element {
  return (
    <code
      dir="ltr"
      className="rounded bg-muted px-1.5 py-0.5 font-mono text-xs text-foreground"
    >
      {children}
    </code>
  );
}

/**
 * A multi-line code block. Rendered as a labelled region so screen-reader users
 * can identify it; `label` describes what the snippet is (e.g. "Shell command").
 */
export function CodeBlock({
  label,
  children,
}: {
  readonly label: string;
  readonly children: ReactNode;
}): JSX.Element {
  return (
    <figure className="my-2" aria-label={label}>
      <pre
        dir="ltr"
        className="overflow-x-auto rounded-md border bg-muted p-3 font-mono text-xs leading-relaxed text-foreground"
      >
        <code>{children}</code>
      </pre>
    </figure>
  );
}

/** An unordered list of body items. */
export function DocList({ children }: { readonly children: ReactNode }): JSX.Element {
  return (
    <ul className="ms-5 list-disc space-y-1.5 max-w-prose">{children}</ul>
  );
}

/** A definition-list of term / description pairs. */
export function DocDefinitions({
  children,
}: {
  readonly children: ReactNode;
}): JSX.Element {
  return <dl className="grid gap-3 sm:grid-cols-[10rem_1fr]">{children}</dl>;
}

/** A single term / description pair inside {@link DocDefinitions}. */
export function DocTerm({
  term,
  children,
}: {
  readonly term: ReactNode;
  readonly children: ReactNode;
}): JSX.Element {
  return (
    <>
      <dt className="font-medium text-foreground">{term}</dt>
      <dd className="max-w-prose">{children}</dd>
    </>
  );
}

/**
 * An availability badge. Roadmap features are flagged honestly so a reader never
 * assumes something ships before it does. Status is carried by text, not colour
 * alone (WCAG SC 1.4.1).
 */
export function StatusBadge({
  status,
}: {
  readonly status: "available" | "roadmap";
}): JSX.Element {
  return status === "available" ? (
    <Badge variant="outline">
      <Trans>Available</Trans>
    </Badge>
  ) : (
    <Badge variant="secondary">
      <Trans>Roadmap</Trans>
    </Badge>
  );
}
