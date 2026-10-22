// Welcome (/welcome) — the first-run onboarding screen.
//
// Greets a new machine and routes it to claim/license itself: either with a
// pairing code on the Account screen, or by hand via the offline challenge
// export on the Licence screen. Once the machine is licensed, it affirms the
// state and points at the rest of the app. The framing follows the licence
// resource (the same data every account surface reads).
import type { JSX } from "react";
import { Trans } from "@lingui/react/macro";
import { Link } from "react-router-dom";

import { useLicence } from "../api/conspectQueries";
import { PageHeader } from "../components/PageHeader";
import { Button } from "../components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "../components/ui/card";

/** The first-run Welcome screen. */
export function WelcomePage(): JSX.Element {
  const licence = useLicence();
  const claimed = (licence.data?.licensed ?? false) && licence.data?.status != null;

  return (
    <>
      <PageHeader
        title={<Trans>Welcome to Multiview</Trans>}
        description={
          <Trans>
            A quick first step: license this machine so it is ready to run.
          </Trans>
        }
      />

      {claimed ? (
        <Card>
          <CardHeader>
            <CardTitle>
              <Trans>You're set</Trans>
            </CardTitle>
            <CardDescription>
              <Trans>This machine is licensed and ready.</Trans>
            </CardDescription>
          </CardHeader>
          <CardContent className="flex flex-wrap gap-2">
            <Button asChild>
              <Link to="/">
                <Trans>Go to the dashboard</Trans>
              </Link>
            </Button>
            <Button variant="outline" asChild>
              <Link to="/settings/licence">
                <Trans>View licence details</Trans>
              </Link>
            </Button>
          </CardContent>
        </Card>
      ) : (
        <div className="grid gap-4 lg:grid-cols-2">
          <Card>
            <CardHeader>
              <CardTitle>
                <Trans>Claim with a code</Trans>
              </CardTitle>
              <CardDescription>
                <Trans>
                  If you have a 6-character pairing code, claim this machine to your
                  account.
                </Trans>
              </CardDescription>
            </CardHeader>
            <CardContent>
              <Button asChild>
                <Link to="/settings/account">
                  <Trans>Claim this machine</Trans>
                </Link>
              </Button>
            </CardContent>
          </Card>

          <Card>
            <CardHeader>
              <CardTitle>
                <Trans>No internet here?</Trans>
              </CardTitle>
              <CardDescription>
                <Trans>
                  License this machine offline: export a signed challenge and
                  install the lease you receive in return.
                </Trans>
              </CardDescription>
            </CardHeader>
            <CardContent>
              <Button variant="outline" asChild>
                <Link to="/settings/licence#challenge">
                  <Trans>Start the offline exchange</Trans>
                </Link>
              </Button>
            </CardContent>
          </Card>
        </div>
      )}
    </>
  );
}
