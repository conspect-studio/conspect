// Auth-mode discovery against the control plane's unauthenticated
// `GET /api/v1/auth/status` endpoint (backend ADR/ task #71).
//
// The SPA cannot know, before it has a token, whether the deployment even
// requires one — so it asks. The response drives the login gate
// (`RequireAuth`): show a key-entry page only when auth is REQUIRED and the
// current credential does not authenticate; otherwise render the app.

/** The two booleans the control plane reports about the current request. */
export interface AuthStatus {
  /** Whether a verified credential is required to reach privileged routes. */
  readonly authRequired: boolean;
  /** Whether the credential presented on THIS request authenticates. */
  readonly authenticated: boolean;
}

/**
 * Query `GET /api/v1/auth/status`, optionally presenting `token` as a bearer
 * (so an entered key can be validated before it is stored). Same-origin.
 *
 * Throws on a network/transport failure (not on a normal `200`); the caller
 * treats a throw as "control plane unreachable".
 */
export async function fetchAuthStatus(token?: string): Promise<AuthStatus> {
  const headers: Record<string, string> = {};
  if (token !== undefined && token !== "") {
    headers.Authorization = `Bearer ${token}`;
  }
  const response = await fetch("/api/v1/auth/status", { headers });
  if (!response.ok) {
    throw new Error(`auth status request failed: ${String(response.status)}`);
  }
  // Parse defensively without an unsafe assertion (mirrors realtime/envelope).
  const body: unknown = await response.json();
  const record: Record<string, unknown> = isRecord(body) ? body : {};
  // Secure defaults if a field is somehow absent: assume auth is required and
  // the request is not authenticated.
  return {
    authRequired:
      typeof record.auth_required === "boolean" ? record.auth_required : true,
    authenticated:
      typeof record.authenticated === "boolean" ? record.authenticated : false,
  };
}

/** Narrow an unknown value to a plain record without an unsafe assertion. */
function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}
