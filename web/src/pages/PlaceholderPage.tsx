// A typed placeholder for routes whose full screens land in later milestones.
// Keeps the shell navigable and the route's single <h1> + landmark correct.
import type { JSX, ReactNode } from "react";
import { Trans } from "@lingui/react/macro";

import { PageHeader } from "../components/PageHeader";
import { Card, CardContent } from "../components/ui/card";

/** Props for {@link PlaceholderPage}. */
export interface PlaceholderPageProps {
  /** The route title. */
  readonly title: ReactNode;
  /** A short description of the planned screen. */
  readonly description: ReactNode;
}

/** A stub screen with a heading and an explanatory card. */
export function PlaceholderPage({
  title,
  description,
}: PlaceholderPageProps): JSX.Element {
  return (
    <>
      <PageHeader title={title} description={description} />
      <Card>
        <CardContent className="pt-6 text-sm text-muted-foreground">
          <Trans>This screen is part of an upcoming milestone.</Trans>
        </CardContent>
      </Card>
    </>
  );
}
