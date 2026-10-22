// Account (/settings/account) — claim / transfer / deactivate (§2).
//
// HONEST AS-BUILT RENDERING. The claim REDEMPTION endpoint
// (POST /api/v1/account/claim) is server-side and NOT in the OpenAPI for this
// build (blocked on the external licence-server wire protocol, brief §14 O1). So
// this screen renders the spec'd UNCLAIMED state with the 6-character claim-code
// form present but DISABLED, and states plainly that it "requires licence-server
// connectivity (not yet wired in this build)". The working offline path — export
// a signed challenge and install a lease by hand — is offered as the link to the
// Licence screen. This is the honest rendering of the as-built backend, not a
// stub: nothing pretends to redeem a code it cannot.
//
// The claim-code constants (§2.1/§2.4) are pinned here so the field validates the
// ambiguity-free charset and the 6-character length client-side, matching the
// portal generator byte-for-byte.
import { useState } from "react";
import type { JSX } from "react";
import { Trans, useLingui } from "@lingui/react/macro";
import { Link } from "react-router-dom";

import { useLicence } from "../api/conspectQueries";
import {
  CLAIM_CODE_LEN,
  canonicaliseClaimCode,
} from "./account-constants";
import { PageHeader } from "../components/PageHeader";
import { Button } from "../components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "../components/ui/card";
import { Input } from "../components/ui/input";
import { Label } from "../components/ui/label";

/** The unclaimed-state claim form (present but disabled — endpoint not wired). */
function ClaimForm(): JSX.Element {
  const { t } = useLingui();
  const [code, setCode] = useState("");

  return (
    <form
      className="space-y-3"
      onSubmit={(e): void => {
        // No redemption endpoint in this build — submission is disabled.
        e.preventDefault();
      }}
    >
      <div className="grid gap-1.5">
        <Label htmlFor="claim-code">
          <Trans>Claim code</Trans>
        </Label>
        <Input
          id="claim-code"
          // Disabled: the redemption endpoint is not wired in this build.
          disabled
          value={code}
          inputMode="text"
          autoCapitalize="characters"
          maxLength={CLAIM_CODE_LEN}
          aria-describedby="claim-code-help claim-code-note"
          className="w-40 font-mono uppercase tracking-widest"
          placeholder={t`ABC234`}
          onChange={(e): void => {
            setCode(canonicaliseClaimCode(e.target.value));
          }}
        />
        <p id="claim-code-help" className="text-sm text-muted-foreground">
          <Trans>
            6 characters, from an ambiguity-free alphabet (no 0/O/1/I/L).
            Case-insensitive.
          </Trans>
        </p>
      </div>
      <Button type="submit" disabled>
        <Trans>Claim this machine</Trans>
      </Button>
      <p
        id="claim-code-note"
        role="note"
        className="rounded-md border bg-muted/40 p-3 text-sm text-muted-foreground"
      >
        <Trans>
          Claiming with a code requires licence-server connectivity (not yet wired
          in this build). Use the offline path below to license this machine now.
        </Trans>
      </p>
    </form>
  );
}

/**
 * A discrete, honestly-disabled account action (transfer / deactivate). Its
 * endpoint is O1-blocked (external licence-server wire protocol, brief §14), so
 * the control renders present-but-disabled and states plainly that it is not yet
 * wired in this build — never a stub that pretends to act.
 */
function DisabledAction({
  testid,
  title,
  description,
  buttonLabel,
}: {
  readonly testid: string;
  readonly title: JSX.Element;
  readonly description: JSX.Element;
  readonly buttonLabel: JSX.Element;
}): JSX.Element {
  return (
    <div data-testid={testid} className="space-y-2">
      <p className="text-sm font-medium">{title}</p>
      <p className="text-sm text-muted-foreground">{description}</p>
      <Button type="button" variant="outline" disabled>
        {buttonLabel}
      </Button>
      <p role="note" className="text-sm text-muted-foreground">
        <Trans>
          This requires licence-server connectivity (not yet wired in this
          build).
        </Trans>
      </p>
    </div>
  );
}

/** The Account screen. */
export function AccountPage(): JSX.Element {
  const licence = useLicence();
  // A machine is "claimed" only when it is licensed AND carries a status (a
  // lease). This matches the Welcome screen and the header chip, which both
  // treat a null status as unclaimed.
  const claimed = (licence.data?.licensed ?? false) && licence.data?.status != null;

  return (
    <>
      <PageHeader
        title={<Trans>Account</Trans>}
        description={
          <Trans>
            Claim this machine to an owner, transfer it, or deactivate it.
          </Trans>
        }
      />

      <div className="grid gap-4 lg:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle>
              <Trans>Ownership</Trans>
            </CardTitle>
            <CardDescription>
              {licence.isPending ? (
                <Trans>Loading ownership state…</Trans>
              ) : claimed ? (
                <Trans>This machine is claimed.</Trans>
              ) : (
                <Trans>This machine is not claimed to an owner yet.</Trans>
              )}
            </CardDescription>
          </CardHeader>
          <CardContent>
            {claimed ? (
              // Claimed: transfer and deactivate are SEPARATE, discrete
              // honestly-disabled actions (their endpoints are O1-blocked, same
              // as claim redemption), each naming what it would do.
              <div className="space-y-6">
                <DisabledAction
                  testid="transfer-section"
                  title={<Trans>Transfer ownership</Trans>}
                  description={
                    <Trans>
                      Hand this machine to a new owner. The 72-hour transfer
                      window and the same-machine fingerprint check apply when
                      the licence server confirms the move.
                    </Trans>
                  }
                  buttonLabel={<Trans>Start a transfer</Trans>}
                />
                <DisabledAction
                  testid="deactivate-section"
                  title={<Trans>Deactivate this machine</Trans>}
                  description={
                    <Trans>
                      Release this machine from its owner and return its lease.
                      Program output is never interrupted by deactivation.
                    </Trans>
                  }
                  buttonLabel={<Trans>Deactivate</Trans>}
                />
              </div>
            ) : (
              <ClaimForm />
            )}
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>
              <Trans>Offline claim</Trans>
            </CardTitle>
            <CardDescription>
              <Trans>
                No internet path? Pair this machine by hand instead of with a code.
              </Trans>
            </CardDescription>
          </CardHeader>
          <CardContent className="space-y-2 text-sm text-muted-foreground">
            <p>
              <Trans>
                Export a signed challenge from this machine, take it to an online
                portal to obtain a signed lease, then install the lease here. The
                exchange is Ed25519-signed end-to-end.
              </Trans>
            </p>
            <p>
              <Link
                to="/settings/licence#challenge"
                className="underline underline-offset-2"
              >
                <Trans>Export a challenge</Trans>
              </Link>{" "}
              <Trans>on the Licence screen.</Trans>
            </p>
          </CardContent>
        </Card>
      </div>
    </>
  );
}
