//! CORS for the WebRTC media-signalling routes (ADR-0048 §9 / ADR-T014 §2).
//!
//! `webrtc.cors_allow_origins` (default `"*"`) applies **only** to the
//! media-signalling surface — WHIP ingest (`POST /api/v1/whip/{source}`),
//! WHEP-serve output (`POST /api/v1/whep/{output}`), the preview-WHEP focus
//! routes (`POST /api/v1/preview/.../whep`), and preview capabilities — so a
//! browser served from a web origin can publish and play cross-origin.
//!
//! Contract (ADR-0048 §9):
//! - a cross-origin request whose `Origin` is in the allow-list (or any origin
//!   when the list is `"*"`) gets `Access-Control-Allow-Origin` reflected and
//!   `Vary: Origin`; the actual (non-preflight) response also carries
//!   `Access-Control-Expose-Headers: location, link` so the browser can read the
//!   WHEP/WHIP `Location` (and any `Link`) header;
//! - a preflight `OPTIONS` is answered `204 No Content` with
//!   `Access-Control-Allow-Methods` and `Access-Control-Allow-Headers:
//!   authorization, content-type` plus a `Access-Control-Max-Age` — **before**
//!   routing and **without** authentication (a browser cannot send credentials
//!   on a preflight);
//! - a request with **no** `Origin` (a non-browser publisher/player) gets **no**
//!   CORS headers; and every response carries `Vary: Origin` so a shared cache
//!   never serves a header-less variant to a cross-origin caller.
//!
//! This is a small, purpose-built middleware (the same posture as the HLS CORS
//! middleware in `multiview-output`): the contract above is exactly the
//! reflected-origin rule the ADR specifies, and `OPTIONS` here returns `204`
//! (not `tower-http`'s `200`), matching the WHIP/WHEP signalling tests.

use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Router;

use crate::state::AppState;

/// The methods a media-signalling route accepts (preflight `Allow*` set). WHIP
/// and WHEP-serve carry `POST` (offer→answer) and `DELETE` (teardown);
/// capabilities is `GET`. The superset is advertised on every signalling path.
const ALLOW_METHODS: HeaderValue = HeaderValue::from_static("GET, POST, DELETE, OPTIONS");

/// The request headers a browser may send on a media-signalling request:
/// `Authorization` (the per-source/output bearer or API key) and `Content-Type`
/// (`application/sdp`). ADR-0048 §9.
const ALLOW_HEADERS: HeaderValue = HeaderValue::from_static("authorization, content-type");

/// The response headers a browser may read: the WHEP/WHIP `Location` (the
/// session resource to `DELETE`) and any `Link`. ADR-0048 §9.
const EXPOSE_HEADERS: HeaderValue = HeaderValue::from_static("location, link");

/// Preflight cache lifetime (10 minutes) — bounds preflight chatter without
/// pinning a stale policy across a reconfiguration.
const MAX_AGE: HeaderValue = HeaderValue::from_static("600");

/// Wrap `router` with the media-signalling CORS middleware, driven by the
/// `AppState`'s `cors_allow_origins`. Apply this **only** to the signalling
/// subtree (WHIP / WHEP-serve / preview-WHEP / capabilities).
pub(crate) fn with_signalling_cors(router: Router<AppState>, state: AppState) -> Router<AppState> {
    router.layer(axum::middleware::from_fn_with_state(state, signalling_cors))
}

/// Whether `origin` is permitted by the allow-list. `"*"` matches any origin;
/// otherwise an exact (case-sensitive, per the Origin grammar) match is required.
fn origin_allowed(allow: &[String], origin: &str) -> bool {
    allow.iter().any(|a| a == "*" || a == origin)
}

/// The CORS middleware. One implementation for every signalling route — never a
/// per-handler copy-paste.
async fn signalling_cors(State(state): State<AppState>, request: Request, next: Next) -> Response {
    // The request's `Origin`, if it is a browser request with an allowed origin.
    let allowed_origin: Option<HeaderValue> = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .filter(|o| origin_allowed(&state.cors_allow_origins, o))
        .and_then(|o| HeaderValue::from_str(o).ok());

    let is_preflight = request.method() == Method::OPTIONS;

    // Run the inner route. For OPTIONS this picks up any per-route handler (e.g.
    // the WHIP/WHEP `whip_options`/`whep_options` that advertise RFC 9725
    // `Accept-Post: application/sdp`); a route with no OPTIONS handler answers
    // `405`, which we normalize to a `204` preflight below.
    let mut response = next.run(request).await;

    if is_preflight {
        // Normalize a missing-OPTIONS-handler `405` into a `204` preflight so
        // every signalling path's preflight succeeds (unauthenticated — a browser
        // cannot send credentials on a preflight). A route that DID answer (e.g.
        // `204` + `Accept-Post`) keeps its body/headers; we only add the CORS
        // allow set on top.
        if response.status() == StatusCode::METHOD_NOT_ALLOWED {
            let preserved = response.headers().get("accept-post").cloned();
            response = StatusCode::NO_CONTENT.into_response();
            if let Some(accept_post) = preserved {
                response.headers_mut().insert("accept-post", accept_post);
            }
        }
        let h = response.headers_mut();
        h.insert(header::ALLOW, ALLOW_METHODS);
        if allowed_origin.is_some() {
            h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, ALLOW_METHODS);
            h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, ALLOW_HEADERS);
            h.insert(header::ACCESS_CONTROL_MAX_AGE, MAX_AGE);
        }
    } else if allowed_origin.is_some() {
        response
            .headers_mut()
            .insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, EXPOSE_HEADERS);
    }

    let h = response.headers_mut();
    // Always vary by Origin (cache correctness even for no-Origin responses);
    // reflect the Origin only when it was sent and allowed.
    h.insert(header::VARY, HeaderValue::from_static("Origin"));
    if let Some(origin) = allowed_origin {
        h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::origin_allowed;

    #[test]
    fn wildcard_allows_any_origin() {
        let allow = vec!["*".to_owned()];
        assert!(origin_allowed(&allow, "https://anything.example"));
        assert!(origin_allowed(&allow, "http://localhost:5173"));
    }

    #[test]
    fn concrete_list_is_exact_match_only() {
        let allow = vec!["https://app.example.org".to_owned()];
        assert!(origin_allowed(&allow, "https://app.example.org"));
        assert!(!origin_allowed(&allow, "https://evil.example.com"));
        // No scheme/subdomain fuzzy matching.
        assert!(!origin_allowed(&allow, "http://app.example.org"));
        assert!(!origin_allowed(&allow, "https://app.example.org.evil.com"));
    }

    #[test]
    fn empty_list_allows_nothing() {
        assert!(!origin_allowed(&[], "https://app.example.org"));
    }
}
