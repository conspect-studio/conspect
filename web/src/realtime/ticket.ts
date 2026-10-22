// Realtime browser auth: a single-use ticket for the WS/SSE upgrade (ADR-RT011).
//
// A browser `WebSocket` / `EventSource` cannot set an `Authorization` header, so
// the SPA used to smuggle the durable bearer through the URL as `?access_token=`.
// That leaks the credential into reverse-proxy/access logs and browser history in
// cleartext (SEC-01, CWE-598). Instead the SPA POSTs its stored bearer (as a
// header, never a URL) to `POST /api/v1/ws/ticket` and connects with `?ticket=`.
// The ticket is single-use and expires in seconds, so even if it lands in a log it
// is inert — unlike the durable admin token the query used to carry.
import { getStoredToken } from "../api/token";

/**
 * The same-origin realtime WebSocket base URL (`ws(s)://<host>/api/v1/ws`), with
 * NO credential in it. The dev proxy and the embedded build both serve
 * `/api/v1/ws` on the document origin.
 */
export function realtimeWsBaseUrl(): string {
  const { protocol, host } = window.location;
  const wsProtocol = protocol === "https:" ? "wss:" : "ws:";
  return `${wsProtocol}//${host}/api/v1/ws`;
}

/**
 * Mint a short-lived, single-use realtime ticket, or `undefined` when none could
 * be minted (no/invalid credential, or a transient failure).
 *
 * The caller connects with `?ticket=<t>`; on `undefined` it attempts a bare
 * connect, which the control plane accepts only when auth is disabled (else it
 * refuses with `401` and the transport reconnects). Never throws — a mint failure
 * degrades to a reconnect, never an unhandled rejection.
 */
export async function mintWsTicket(): Promise<string | undefined> {
  const headers: Record<string, string> = {};
  const token = getStoredToken();
  if (token !== undefined && token !== "") {
    // Same-origin, header-only — the bearer never appears in a URL (SEC-01).
    headers.Authorization = `Bearer ${token}`;
  }
  try {
    const response = await fetch("/api/v1/ws/ticket", {
      method: "POST",
      headers,
    });
    if (!response.ok) {
      return undefined;
    }
    // Parse defensively without an unsafe assertion (mirrors realtime/envelope
    // and auth/authStatus): only a non-empty string `ticket` is accepted.
    const body: unknown = await response.json();
    const ticket = isRecord(body) ? body.ticket : undefined;
    return typeof ticket === "string" && ticket !== "" ? ticket : undefined;
  } catch {
    // Network/transport failure: no ticket this attempt; the transport reconnects.
    return undefined;
  }
}

/** Narrow an unknown value to a plain record without an unsafe assertion. */
function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}
