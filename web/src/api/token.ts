// The browser-stored control-plane API token.
//
// The control plane authenticates every `/api/v1` data route with a bearer
// token (ADR-W014: the admin token from `MULTIVIEW_CONTROL_TOKEN`, or a
// generated bootstrap token logged once at startup). The operator pastes that
// token into the UI (Settings → API access); it is persisted here so it
// survives reloads and is sent as `Authorization: Bearer <token>` on every
// request (see `createApiClient`). It is kept in `localStorage` only — never
// sent anywhere but the same-origin control plane.

/** The `localStorage` key the bearer token is persisted under. */
const TOKEN_KEY = "multiview.apiToken";

/**
 * The stored bearer token, or `undefined` when none is set (or storage is
 * unavailable, e.g. private-browsing). An empty string is treated as unset.
 */
export function getStoredToken(): string | undefined {
  try {
    const value = window.localStorage.getItem(TOKEN_KEY);
    return value === null || value === "" ? undefined : value;
  } catch {
    // localStorage can throw (disabled / private mode); treat as no token.
    return undefined;
  }
}

/** Persist `token` as the bearer token, or clear it when given an empty value. */
export function setStoredToken(token: string): void {
  try {
    if (token === "") {
      window.localStorage.removeItem(TOKEN_KEY);
    } else {
      window.localStorage.setItem(TOKEN_KEY, token);
    }
  } catch {
    // localStorage unavailable; the token simply will not persist this session.
  }
}

/** Remove any stored bearer token. */
export function clearStoredToken(): void {
  try {
    window.localStorage.removeItem(TOKEN_KEY);
  } catch {
    // localStorage unavailable; nothing to clear.
  }
}
