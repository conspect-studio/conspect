// WHEP signaling — the small fetch-based protocol layer (ADR-W023 §1).
//
// WHEP (RFC 9725) is a tiny HTTP protocol: POST a complete SDP offer as
// `application/sdp` with the control-plane bearer, receive `201` + the answer
// SDP in the body and the session resource in `Location`; DELETE that Location
// to tear down. We hand-roll it (no third-party WHEP lib) because the libraries
// assume trickle ICE we deliberately don't use and bury the field details that
// actually break: relative-Location resolution, the bearer on POST *and*
// DELETE, and keepalive teardown. None of this touches the DOM, so it is unit
// tested against a mocked `fetch`.
import { getStoredToken } from '../api/token';

/** A signaling failure, normalized to a message + (when known) HTTP status. */
export class WhepSignalingError extends Error {
  /** The HTTP status code, when the failure carried one. */
  readonly status: number | undefined;

  /**
   * Whether the server's problem body hinted a transport fallback (the `503`
   * capacity body carries `fallback: "jpeg"`, ADR-P006 move 6) — the ladder
   * uses this to degrade immediately rather than retry.
   */
  readonly fallbackHinted: boolean;

  constructor(message: string, status?: number, fallbackHinted = false) {
    super(message);
    this.name = 'WhepSignalingError';
    this.status = status;
    this.fallbackHinted = fallbackHinted;
  }
}

/** The result of a successful WHEP POST: the answer SDP + the session URL. */
export interface WhepAnswer {
  /** The SDP answer to feed `setRemoteDescription`. */
  readonly answerSdp: string;
  /**
   * The absolute session resource URL, resolved against the POST's
   * post-redirect URL — DELETE this to release the session.
   */
  readonly sessionUrl: string;
}

/**
 * Resolve a WHEP `Location` against the response URL of the POST (RFC 9725
 * servers commonly return a RELATIVE session URI, and the POST may have been
 * redirected). Never resolve against the page origin or the pre-redirect
 * request URL.
 */
export function resolveSessionUrl(location: string, responseUrl: string): string {
  return new URL(location, responseUrl).toString();
}

/** Whether a problem body (any shape) hints a transport fallback. */
function bodyHintsFallback(body: unknown): boolean {
  return (
    typeof body === 'object' &&
    body !== null &&
    'fallback' in body &&
    body.fallback === 'jpeg'
  );
}

/** Build the `Authorization` header value from the stored bearer, if any. */
function bearerHeader(): Record<string, string> {
  const token = getStoredToken();
  return token !== undefined && token !== '' ? { Authorization: `Bearer ${token}` } : {};
}

/**
 * POST an SDP offer to a WHEP endpoint and return the answer + session URL.
 *
 * The offer is sent as `application/sdp` with the stored control-plane bearer;
 * the response must be `201` carrying the answer SDP and a `Location`. A non-2xx
 * (incl. `503` capacity) throws a {@link WhepSignalingError}; a missing
 * `Location` is a protocol error.
 */
export async function postWhepOffer(
  endpoint: string,
  offerSdp: string,
  fetchImpl: typeof fetch = fetch,
): Promise<WhepAnswer> {
  const response = await fetchImpl(endpoint, {
    method: 'POST',
    headers: { 'Content-Type': 'application/sdp', ...bearerHeader() },
    body: offerSdp,
  });
  if (!response.ok) {
    let hinted = false;
    try {
      hinted = bodyHintsFallback(await response.clone().json());
    } catch {
      // No/!JSON body — a status-only failure, no fallback hint.
    }
    throw new WhepSignalingError(
      `WHEP offer rejected (${String(response.status)})`,
      response.status,
      hinted,
    );
  }
  const location = response.headers.get('Location');
  if (location === null || location === '') {
    throw new WhepSignalingError('WHEP response carried no Location header');
  }
  const answerSdp = await response.text();
  if (answerSdp.trim() === '') {
    throw new WhepSignalingError('WHEP response carried an empty answer SDP');
  }
  return { answerSdp, sessionUrl: resolveSessionUrl(location, response.url) };
}

/**
 * DELETE a WHEP session to release it promptly. Best-effort with `keepalive` so
 * it survives an unmount / `pagehide` (fetch keepalive CAN carry the bearer
 * header; sendBeacon cannot — which is exactly why keepalive fetch is used).
 * Never throws: failure is fine (the server-side idle GC is the authoritative
 * reaper, ADR-0048).
 */
export function deleteWhepSession(sessionUrl: string, fetchImpl: typeof fetch = fetch): void {
  try {
    void fetchImpl(sessionUrl, {
      method: 'DELETE',
      headers: bearerHeader(),
      keepalive: true,
    }).catch(() => {
      // Best-effort teardown; the idle GC reaps anything we miss.
    });
  } catch {
    // Synchronous throw (e.g. a bad URL) is equally non-fatal.
  }
}
