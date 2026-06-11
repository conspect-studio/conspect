// The login gate's key-entry screen, shown when the control plane requires
// authentication and the SPA has no working token (task #71).
//
// It validates the entered key against `GET /api/v1/auth/status` (which reports
// `authenticated` for the presented credential) BEFORE storing it, so a wrong
// key is rejected inline rather than silently failing every request afterwards.
import { type JSX, useState } from "react";
import { Trans, useLingui } from "@lingui/react/macro";

import { setStoredToken } from "../api/token";
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
import { fetchAuthStatus } from "./authStatus";

/** Props for [`LoginPage`]. */
export interface LoginPageProps {
  /** Called once an entered key has been validated and stored. */
  readonly onAuthenticated: () => void;
}

/** The full-screen API-key login page. */
export function LoginPage({ onAuthenticated }: LoginPageProps): JSX.Element {
  const { t } = useLingui();
  const [value, setValue] = useState<string>("");
  const [error, setError] = useState<string | null>(null);
  const [checking, setChecking] = useState<boolean>(false);

  async function submit(): Promise<void> {
    const key = value.trim();
    if (key === "") {
      setError(t`Enter an API key.`);
      return;
    }
    setChecking(true);
    setError(null);
    try {
      const status = await fetchAuthStatus(key);
      if (status.authenticated) {
        setStoredToken(key);
        onAuthenticated();
      } else {
        setError(t`That key was not accepted. Check it and try again.`);
      }
    } catch {
      setError(t`Could not reach the control plane. Is it running?`);
    } finally {
      setChecking(false);
    }
  }

  return (
    <div className="flex min-h-screen items-center justify-center bg-background p-4">
      <Card className="w-full max-w-sm">
        <CardHeader>
          <CardTitle>
            <Trans>Sign in</Trans>
          </CardTitle>
          <CardDescription>
            <Trans>
              This Multiview control plane requires an API key. Paste your admin
              or operator key to continue.
            </Trans>
          </CardDescription>
        </CardHeader>
        <CardContent>
          <form
            className="flex flex-col gap-3"
            onSubmit={(event): void => {
              event.preventDefault();
              void submit();
            }}
          >
            <div className="flex flex-col gap-1.5">
              <Label htmlFor="login-token">
                <Trans>API key</Trans>
              </Label>
              <Input
                id="login-token"
                type="password"
                autoComplete="off"
                placeholder={t`admin.xxxxxxxx-xxxx-…`}
                value={value}
                onChange={(event): void => {
                  setValue(event.target.value);
                }}
              />
            </div>
            {error !== null ? (
              <p className="text-sm text-destructive" role="alert">
                {error}
              </p>
            ) : null}
            <Button type="submit" disabled={checking}>
              {checking ? <Trans>Checking…</Trans> : <Trans>Sign in</Trans>}
            </Button>
          </form>
        </CardContent>
      </Card>
    </div>
  );
}
