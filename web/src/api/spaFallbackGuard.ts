// Guard against the SPA-index fallback masquerading as an API success.
//
// A backend that lacks a route still gets `200 text/html` (the SPA index) from
// the front proxy's `try_files` fallback. Every API consumer would otherwise
// parse that as success (or fail with an opaque JSON parse error). This wraps
// `fetch` once at start-up: a 2xx `text/html` answer to an `/api/` request is
// re-expressed as `501 Not Implemented`, which the typed pending-route
// machinery (`isUnavailableStatus` → `BackendUnavailableError`) already
// understands. No `/api/` route legitimately answers HTML.

/** True when `res` is a successful answer that is really the SPA index page. */
export function isSpaFallback(res: Response): boolean {
  if (!res.ok || res.status === 204) return false;
  return (res.headers.get('content-type') ?? '').toLowerCase().includes('text/html');
}

function requestPath(input: RequestInfo | URL): string {
  try {
    const raw = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url;
    return new URL(raw, 'http://localhost').pathname;
  } catch {
    return '';
  }
}

/** Wrap a fetch so an HTML 200 under `/api/` becomes a typed 501. */
export function guardFetch(base: typeof fetch): typeof fetch {
  return async (input, init) => {
    const res = await base(input, init);
    if (requestPath(input).startsWith('/api/') && isSpaFallback(res)) {
      return new Response(null, { status: 501, statusText: 'Not Implemented' });
    }
    return res;
  };
}

let installed = false;

/** Install the guard on the global fetch (idempotent). */
export function installSpaFallbackGuard(): void {
  if (installed || typeof globalThis.fetch !== 'function') return;
  installed = true;
  globalThis.fetch = guardFetch(globalThis.fetch.bind(globalThis));
}
