// Typed control-plane API client.
//
// The `paths` type is generated verbatim from the control-plane OpenAPI spec
// (`docs/api/openapi.json`, produced by `cargo xtask gen-openapi`) via
// openapi-typescript — see `npm run generate:api`. openapi-fetch is a thin,
// fully-typed wrapper around `fetch` whose request/response shapes are derived
// from that spec, so the SPA and the Rust API are checked to agree at compile
// time. Do NOT hand-write request/response types here.
import createClient from 'openapi-fetch';

import type { paths } from './schema';
import { getStoredToken } from './token';

/** Options for constructing a {@link MultiviewApiClient}. */
export interface ApiClientOptions {
  /**
   * Base URL the API is served from. Defaults to the document origin, which
   * the Vite dev server proxies to the control plane and which is correct when
   * the SPA is embedded in the `multiview` binary.
   */
  readonly baseUrl?: string;
  /**
   * Optional bearer token. When present it is sent as
   * `Authorization: Bearer <token>` on every request (the control plane
   * authenticates reads of the layout/realtime surface).
   */
  readonly token?: string;
}

/** A typed openapi-fetch client bound to the control-plane `paths`. */
export type MultiviewApiClient = ReturnType<typeof createClient<paths>>;

/**
 * Build a typed API client. The returned client's `GET`/`POST`/… methods only
 * accept paths and shapes that exist in the generated schema.
 */
export function createApiClient(options: ApiClientOptions = {}): MultiviewApiClient {
  const headers: Record<string, string> = {};
  // An explicit token wins; otherwise fall back to the operator's stored token
  // so every page authenticates without threading the token through each call.
  const token = options.token ?? getStoredToken();
  if (token !== undefined && token !== '') {
    headers.Authorization = `Bearer ${token}`;
  }
  return createClient<paths>({
    // Same-origin by default, as the explicit origin rather than ''. The two
    // are identical in the browser ('/api/v1/…' is root-relative), but
    // openapi-fetch builds a `new Request(url)` itself and a relative URL is
    // unparseable outside a real document (jsdom/undici in component tests).
    baseUrl: options.baseUrl ?? (typeof window === 'undefined' ? '' : window.location.origin),
    headers,
  });
}
