// 404 route.
import type { JSX } from "react";
import { Trans } from "@lingui/react/macro";
import { Link } from "react-router-dom";

import { PageHeader } from "../components/PageHeader";
import { Button } from "../components/ui/button";

/** Rendered for unmatched routes. */
export function NotFoundPage(): JSX.Element {
  return (
    <>
      <PageHeader
        title={<Trans>Page not found</Trans>}
        description={<Trans>The requested screen does not exist.</Trans>}
      />
      <Button asChild variant="outline">
        <Link to="/">
          <Trans>Back to dashboard</Trans>
        </Link>
      </Button>
    </>
  );
}
