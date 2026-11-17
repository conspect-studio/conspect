// Settings — appearance + language controls, the API access token, the
// config-file watch status (ADR-W020), and a demonstration of the accessible
// toast notifications.
import { useMemo, useState } from "react";
import type { JSX } from "react";
import { Trans, useLingui } from "@lingui/react/macro";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import { createApiClient } from "../api/client";
import { getStoredToken, setStoredToken } from "../api/token";
import { ExportConfigButton } from "../resources/FormControls";
import { LocaleSwitcher } from "../components/LocaleSwitcher";
import { PageHeader } from "../components/PageHeader";
import { ThemeToggle } from "../components/ThemeToggle";
import { Badge } from "../components/ui/badge";
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
import { useActiveLocale } from "../i18n/I18nProvider";
import { formatDateTime } from "../i18n/format";
import {
  clearWindyAccount,
  getWindyAccount,
  setWindyAccount,
  verifyWindyAccount,
} from "../api/windy-account";
import type { WindyAccountView, WindyVerdict } from "../api/windy-account";
import { OperationApiError } from "../api/operations";
import { captureCapabilitiesKey } from "../api/web-captureQueries";

/**
 * The "Configuration file" card (ADR-W020): whether the boot config file is
 * watched for external edits, the watched path, the last applied/rejected
 * loads, and any sections pending a restart — read from
 * `GET /api/v1/config/watch-status`.
 */
function ConfigWatchCard(): JSX.Element {
  const { t } = useLingui();
  const locale = useActiveLocale();
  const client = useMemo(() => createApiClient(), []);
  const query = useQuery({
    queryKey: ["config", "watch-status"],
    queryFn: async () => {
      const { data } = await client.GET("/api/v1/config/watch-status");
      if (data === undefined) {
        throw new Error(t`Could not load the configuration file watch status.`);
      }
      return data;
    },
    refetchInterval: 15_000,
  });
  const status = query.data;
  const lastApplied = status?.last_applied ?? null;
  const lastRejected = status?.last_rejected ?? null;
  const path = status?.path ?? null;

  return (
    <Card className="lg:col-span-2">
      <CardHeader>
        <CardTitle>
          <Trans>Configuration file</Trans>
        </CardTitle>
        <CardDescription>
          <Trans>
            External edits to the boot configuration file hot-reload the parts
            that can apply live; an invalid file changes nothing and is
            reported here and on the health banner. Sections that cannot
            hot-apply stay listed until a restart.
          </Trans>
        </CardDescription>
      </CardHeader>
      <CardContent className="flex flex-col gap-2 text-sm">
        {status === undefined ? (
          <p className="text-muted-foreground">
            {query.isError ? (
              <Trans>The watch status is unavailable.</Trans>
            ) : (
              <Trans>Loading…</Trans>
            )}
          </p>
        ) : (
          <>
            <div className="flex flex-wrap items-center gap-2">
              <Badge variant={status.active ? "default" : "outline"}>
                {status.active ? (
                  <Trans>Watching</Trans>
                ) : (
                  <Trans>Not watched</Trans>
                )}
              </Badge>
              {path !== null && (
                <code className="rounded bg-muted px-1.5 py-0.5 font-mono text-xs">
                  {path}
                </code>
              )}
            </div>
            {lastApplied !== null && (
              <p>
                <Trans>Last applied:</Trans>{" "}
                {formatDateTime(locale, new Date(lastApplied.at_ms))} —{" "}
                {lastApplied.detail}
              </p>
            )}
            {lastRejected !== null && (
              <p className="text-destructive">
                <Trans>Last rejected:</Trans>{" "}
                {formatDateTime(locale, new Date(lastRejected.at_ms))} —{" "}
                {lastRejected.detail}
              </p>
            )}
            {status.restart_pending.length > 0 && (
              <p>
                <Trans>Restart pending:</Trans>{" "}
                <span className="font-medium">
                  {status.restart_pending.join(", ")}
                </span>
              </p>
            )}
          </>
        )}
      </CardContent>
    </Card>
  );
}

/** The settings page. */

const WINDY_ACCOUNT_KEY = ["integrations", "windy"] as const;

/**
 * Settings → Integrations → Windy (issue #470). Admin only: a non-admin gets
 * 403 and the card says so. The password is write-only — the server never
 * returns it, and the field is cleared after every save.
 */
function WindyIntegrationCard(): JSX.Element {
  const { t } = useLingui();
  const locale = useActiveLocale();
  const queryClient = useQueryClient();
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const account = useQuery({
    queryKey: WINDY_ACCOUNT_KEY,
    queryFn: () => getWindyAccount(),
    retry: false,
  });
  const apply = (view: WindyAccountView): void => {
    queryClient.setQueryData(WINDY_ACCOUNT_KEY, view);
    void queryClient.invalidateQueries({ queryKey: captureCapabilitiesKey });
  };
  const verdictText = (verdict: WindyVerdict): string => {
    switch (verdict) {
      case "premium":
        return t`Login OK — Premium active`;
      case "rejected":
        return t`Login failed: wrong email or password`;
      case "not_premium":
        return t`Logged in, but the account has no Premium subscription`;
      case "unavailable":
        return t`Login check could not complete (network or site change); try again`;
    }
  };
  const save = useMutation({
    mutationFn: () => setWindyAccount(email, password),
    onSuccess: (view) => {
      setPassword("");
      apply(view);
      toast({ title: t`Windy account saved`, description: t`Run "Test login" to enable Premium capture.` });
    },
    onError: (error: Error) => {
      toast({ title: t`Windy account not saved`, description: error.message, variant: "destructive" });
    },
  });
  const clear = useMutation({
    mutationFn: () => clearWindyAccount(),
    onSuccess: (view) => {
      apply(view);
      toast({ title: t`Windy account removed` });
    },
  });
  const verify = useMutation({
    mutationFn: () => verifyWindyAccount(),
    onSuccess: (result) => {
      apply(result.account);
      toast({
        title: verdictText(result.result),
        ...(result.result === "premium" ? {} : { variant: "destructive" as const }),
      });
    },
    onError: (error: Error) => {
      toast({ title: t`Login check failed`, description: error.message, variant: "destructive" });
    },
  });
  const forbidden = account.error instanceof OperationApiError && account.error.status === 403;
  const unconfigured = account.error instanceof OperationApiError && account.error.status === 409;
  const view = account.data;
  const busy = save.isPending || clear.isPending || verify.isPending;

  return (
    <Card className="lg:col-span-2">
      <CardHeader>
        <CardTitle>
          <Trans>Integrations — Windy</Trans>
        </CardTitle>
        <CardDescription>
          <Trans>
            The Windy account used for Premium captures. Stored encrypted on the
            server and never shown again; only administrators can change it.
          </Trans>
        </CardDescription>
      </CardHeader>
      <CardContent className="flex flex-col gap-3">
        {forbidden ? (
          <p className="text-sm text-muted-foreground">
            <Trans>Only an administrator can view or change the Windy account.</Trans>
          </p>
        ) : unconfigured ? (
          <p className="text-sm text-muted-foreground">
            <Trans>
              The Windy account store is not configured on this server
              (CONSPECT_WINDY_MASTER_KEY_FILE).
            </Trans>
          </p>
        ) : (
          <>
            <div className="flex flex-wrap items-center gap-2" data-testid="windy-account-status">
              {view?.set === true ? (
                <Badge variant="secondary">
                  <Trans>Account set</Trans>
                </Badge>
              ) : (
                <Badge variant="outline">
                  <Trans>No account</Trans>
                </Badge>
              )}
              {view?.premium_available === true ? (
                <Badge>
                  <Trans>Premium available</Trans>
                </Badge>
              ) : null}
              {view?.result != null ? (
                <span className="text-sm">{verdictText(view.result)}</span>
              ) : view?.set === true ? (
                <span className="text-sm text-muted-foreground">
                  <Trans>Not tested yet</Trans>
                </span>
              ) : null}
              {view?.last_verified_at != null ? (
                <span className="text-xs text-muted-foreground">
                  <Trans>Last tested {formatDateTime(locale, new Date(view.last_verified_at))}</Trans>
                </span>
              ) : null}
            </div>
            <form
              className="flex flex-col gap-3 sm:flex-row sm:items-end"
              onSubmit={(event): void => {
                event.preventDefault();
                save.mutate();
              }}
            >
              <div className="flex flex-1 flex-col gap-1.5">
                <Label htmlFor="windy-email">
                  <Trans>Windy email</Trans>
                </Label>
                <Input
                  id="windy-email"
                  type="email"
                  autoComplete="off"
                  value={email}
                  onChange={(event): void => {
                    setEmail(event.target.value);
                  }}
                />
              </div>
              <div className="flex flex-1 flex-col gap-1.5">
                <Label htmlFor="windy-password">
                  <Trans>Windy password</Trans>
                </Label>
                <Input
                  id="windy-password"
                  type="password"
                  autoComplete="new-password"
                  placeholder={view?.set === true ? t`(unchanged)` : ""}
                  value={password}
                  onChange={(event): void => {
                    setPassword(event.target.value);
                  }}
                />
              </div>
              <div className="flex gap-2">
                <Button type="submit" disabled={busy || email === "" || password === ""}>
                  <Trans>Save Windy account</Trans>
                </Button>
                <Button
                  type="button"
                  variant="outline"
                  disabled={busy || view?.set !== true}
                  onClick={(): void => {
                    verify.mutate();
                  }}
                >
                  {verify.isPending ? <Trans>Testing…</Trans> : <Trans>Test login</Trans>}
                </Button>
                <Button
                  type="button"
                  variant="outline"
                  disabled={busy || view?.set !== true}
                  onClick={(): void => {
                    clear.mutate();
                  }}
                >
                  <Trans>Remove</Trans>
                </Button>
              </div>
            </form>
          </>
        )}
      </CardContent>
    </Card>
  );
}

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

        <ConfigWatchCard />

        <WindyIntegrationCard />

        <Card className="lg:col-span-2">
          <CardHeader>
            <CardTitle>
              <Trans>Configuration export</Trans>
            </CardTitle>
            <CardDescription>
              <Trans>
                Download the current stores (sources, outputs, overlays,
                layouts, canvas) as multiview.toml. Stored UI edits take effect
                when Conspect restarts with this file; live actions (apply
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
