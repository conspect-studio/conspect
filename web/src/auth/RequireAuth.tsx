// The login gate: decides whether to render the app or the key-entry page,
// based on the control plane's auth mode and the stored credential (task #71).
//
// It runs BEFORE `AppLayout` (which opens the realtime WebSocket), so an
// un-authenticated browser never opens a socket that would 401 and loop
// "reconnecting" — it sees a proper login page instead. When auth is disabled
// server-side, or the stored token authenticates, it renders the app directly.
import { type JSX, type ReactNode, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Trans } from "@lingui/react/macro";

import { getStoredToken } from "../api/token";
import { fetchAuthStatus } from "./authStatus";
import { LoginPage } from "./LoginPage";

/** Props for [`RequireAuth`]. */
export interface RequireAuthProps {
  /** The application to render once auth is satisfied (or not required). */
  readonly children: ReactNode;
}

/** Gate the application behind the control plane's auth mode. */
export function RequireAuth({ children }: RequireAuthProps): JSX.Element {
  // Bumped after a successful login to re-query the status with the new token.
  const [revalidate, setRevalidate] = useState<number>(0);

  const { data, isPending, isError } = useQuery({
    queryKey: ["auth-status", revalidate],
    // Read the token fresh each run so a just-stored key is reflected.
    queryFn: () => fetchAuthStatus(getStoredToken()),
    retry: 1,
    staleTime: 60_000,
  });

  if (isPending) {
    return (
      <div className="flex min-h-screen items-center justify-center bg-background">
        <p className="text-sm text-muted-foreground">
          <Trans>Connecting…</Trans>
        </p>
      </div>
    );
  }

  // If the status endpoint is unreachable, fail toward the login page (the user
  // can still enter a key) rather than a blank screen — unless we already know
  // auth is off. `data` is the last good value on a ret[ry] error.
  const required = data?.authRequired ?? true;
  const authenticated = data?.authenticated ?? false;

  if (!isError && (!required || authenticated)) {
    return <>{children}</>;
  }

  return (
    <LoginPage
      onAuthenticated={(): void => {
        setRevalidate((n) => n + 1);
      }}
    />
  );
}
