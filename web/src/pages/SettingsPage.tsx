// Settings — appearance + language controls, the API access token, and a
// demonstration of the accessible toast notifications.
import { useState } from "react";
import type { JSX } from "react";
import { Trans, useLingui } from "@lingui/react/macro";
import { useQueryClient } from "@tanstack/react-query";

import { getStoredToken, setStoredToken } from "../api/token";
import { ExportConfigButton } from "../resources/FormControls";
import { LocaleSwitcher } from "../components/LocaleSwitcher";
import { PageHeader } from "../components/PageHeader";
import { ThemeToggle } from "../components/ThemeToggle";
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
import { toast } from "../components/ui/use-toast";

/** The settings page. */
export function SettingsPage(): JSX.Element {
  const { t } = useLingui();
  const queryClient = useQueryClient();
  const [token, setToken] = useState<string>(() => getStoredToken() ?? "");

  const saveToken = (): void => {
    setStoredToken(token);
    // Re-run every query so the pages re-fetch with the new credential.
    void queryClient.invalidateQueries();
    toast({
      title: token === "" ? t`API token cleared` : t`API token saved`,
      description:
        token === ""
          ? t`Requests will be unauthenticated.`
          : t`The UI will authenticate with this token.`,
    });
  };

  return (
    <>
      <PageHeader
        title={<Trans>Settings</Trans>}
        description={<Trans>Appearance and language preferences.</Trans>}
      />

      <div className="grid gap-4 lg:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle>
              <Trans>Appearance</Trans>
            </CardTitle>
            <CardDescription>
              <Trans>Choose light, dark, or follow the system.</Trans>
            </CardDescription>
          </CardHeader>
          <CardContent className="flex items-center justify-between gap-4">
            <Label id="theme-label">
              <Trans>Theme</Trans>
            </Label>
            <ThemeToggle />
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>
              <Trans>Language</Trans>
            </CardTitle>
            <CardDescription>
              <Trans>Switch the interface language and text direction.</Trans>
            </CardDescription>
          </CardHeader>
          <CardContent className="flex items-center justify-between gap-4">
            <Label>
              <Trans>Locale</Trans>
            </Label>
            <LocaleSwitcher />
          </CardContent>
        </Card>

        <Card className="lg:col-span-2">
          <CardHeader>
            <CardTitle>
              <Trans>API access</Trans>
            </CardTitle>
            <CardDescription>
              <Trans>
                Paste the control-plane bearer token. Set it from the
                MULTIVIEW_CONTROL_TOKEN environment variable, or copy the
                bootstrap token the server logs once at startup. Stored in this
                browser only and sent as a Bearer token to the same-origin API.
              </Trans>
            </CardDescription>
          </CardHeader>
          <CardContent className="flex flex-col gap-3 sm:flex-row sm:items-end">
            <div className="flex flex-1 flex-col gap-1.5">
              <Label htmlFor="api-token">
                <Trans>API token</Trans>
              </Label>
              <Input
                id="api-token"
                type="password"
                autoComplete="off"
                placeholder={t`admin.xxxxxxxx-xxxx-…`}
                value={token}
                onChange={(event): void => {
                  setToken(event.target.value);
                }}
              />
            </div>
            <div className="flex gap-2">
              <Button onClick={saveToken}>
                <Trans>Save</Trans>
              </Button>
              <Button
                variant="outline"
                onClick={(): void => {
                  setToken("");
                  setStoredToken("");
                  void queryClient.invalidateQueries();
                  toast({ title: t`API token cleared` });
                }}
              >
                <Trans>Clear</Trans>
              </Button>
            </div>
          </CardContent>
        </Card>

        <Card className="lg:col-span-2">
          <CardHeader>
            <CardTitle>
              <Trans>Configuration export</Trans>
            </CardTitle>
            <CardDescription>
              <Trans>
                Download the current stores (sources, outputs, overlays,
                layouts, canvas) as multiview.toml. Stored UI edits take effect
                when Multiview restarts with this file; live actions (apply
                layout, swap, routing, salvos) act on the running engine
                immediately.
              </Trans>
            </CardDescription>
          </CardHeader>
          <CardContent>
            <ExportConfigButton />
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>
              <Trans>Notifications</Trans>
            </CardTitle>
            <CardDescription>
              <Trans>Accessible toasts announce via a live region.</Trans>
            </CardDescription>
          </CardHeader>
          <CardContent>
            <Button
              variant="outline"
              onClick={(): void => {
                toast({
                  title: t`Test notification`,
                  description: t`This toast is announced to assistive technology.`,
                });
              }}
            >
              <Trans>Show a test notification</Trans>
            </Button>
          </CardContent>
        </Card>
      </div>
    </>
  );
}
