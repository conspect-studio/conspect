//! The public cast surface (ADR-0111, design §3/§4), outside `/api/v1` and
//! merged into every listener's router beside `/iptv`, so the per-IP rate
//! limit and the concurrency cap apply and the SPA fallback never answers:
//!
//! * `GET|HEAD|OPTIONS /cast/{token}/{file}`: one cast output's HLS files
//!   behind a stream token. An unknown, expired or revoked token, a file
//!   name off the allow-list, or a file outside the output's directory all
//!   answer the SAME `404` (no oracle).
//! * `POST /cast/{token}/report`: the receiver's state report.
//! * `/cast/receiver/` (+ `index.html`, `receiver.js`, `receiver.css`): the
//!   custom Web Receiver, embedded, under a strict CSP. These static routes
//!   win over the token route.
//!
//! Media is served only for a token armed at its output's CURRENT
//! authorization generation (ABUSE-1, `cast_streams::reauth`).
//!
//! The token is a path segment: it is never logged by this crate (no layer
//! here records request paths), never audited and never published.

use axum::body::{Body, Bytes};
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use crate::cast_streams::token::file_allowed;
use crate::cast_streams::ReportError;
use crate::state::AppState;

/// The receiver page's Content-Security-Policy (measured with the real CAF
/// SDK: 0 violations; `receiver-work/REPORT.md`).
pub const RECEIVER_CSP: &str = "default-src 'none'; script-src 'self' https://www.gstatic.com https://ajax.googleapis.com; style-src 'self' 'unsafe-inline'; img-src 'self' data:; media-src 'self' https: blob:; connect-src 'self' https: ws://localhost:8008; font-src 'self' https://fonts.gstatic.com; worker-src blob:";

const INDEX_HTML: &str = include_str!("../cast_receiver/index.html");
const RECEIVER_JS: &str = include_str!("../cast_receiver/receiver.js");
const RECEIVER_CSS: &str = include_str!("../cast_receiver/receiver.css");

const MEDIA_METHODS: HeaderValue = HeaderValue::from_static("GET, HEAD, OPTIONS");
const REPORT_METHODS: HeaderValue = HeaderValue::from_static("POST, OPTIONS");
const ALLOW_HEADERS: HeaderValue = HeaderValue::from_static("Range, Content-Type, Accept-Encoding");
const EXPOSE_HEADERS: HeaderValue = HeaderValue::from_static("Content-Length, Content-Range");
const MAX_AGE: HeaderValue = HeaderValue::from_static("86400");
/// The one body of every cast `404`.
pub const NOT_FOUND_BODY: &str = "not found\n";

/// The public cast routes.
pub fn public_router() -> Router<AppState> {
    Router::new()
        .route("/cast/receiver", get(receiver_redirect))
        .route("/cast/receiver/", get(receiver_index))
        .route("/cast/receiver/index.html", get(receiver_index))
        .route("/cast/receiver/receiver.js", get(receiver_js))
        .route("/cast/receiver/receiver.css", get(receiver_css))
        .route(
            "/cast/{token}/{file}",
            get(media).options(preflight).post(report),
        )
        // Anything else under `/cast/` is the same 404 (never the SPA
        // fallback's index page, never a different body).
        .route("/cast/", axum::routing::any(cast_other))
        .route("/cast/{token}", axum::routing::any(cast_other))
        .route("/cast/{token}/", axum::routing::any(cast_other))
        .route("/cast/{token}/{file}/{*rest}", axum::routing::any(cast_other))
}

async fn cast_other(headers: HeaderMap) -> Response {
    with_cors(not_found(), headers.get(header::ORIGIN))
}

fn not_found() -> Response {
    let mut response = (StatusCode::NOT_FOUND, NOT_FOUND_BODY).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn user_agent(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
}

/// The cache contract of every token route (ABUSE-2): `no-store`, so no
/// shared or private cache can answer for a token after it expired or was
/// revoked. Overrides the public-HLS tiers `serve_file` sets (playlists
/// `max-age=1`, media a year `immutable`); ordinary `/hls/` mounts keep them.
fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// The cast CORS contract on any response: reflect the Origin (any), always
/// `Vary: Origin`, expose the range headers.
fn with_cors(mut response: Response, origin: Option<&HeaderValue>) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::VARY, HeaderValue::from_static("Origin"));
    if let Some(origin) = origin {
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
        headers.insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, EXPOSE_HEADERS);
    }
    response
}

async fn preflight(
    headers: HeaderMap,
    path: Result<Path<(String, String)>, PathRejection>,
) -> Response {
    let report = path.is_ok_and(|Path((_, file))| file == "report");
    let methods = if report {
        REPORT_METHODS
    } else {
        MEDIA_METHODS
    };
    let mut response = StatusCode::NO_CONTENT.into_response();
    let out = response.headers_mut();
    out.insert(header::ALLOW, methods.clone());
    if headers.contains_key(header::ORIGIN) {
        out.insert(header::ACCESS_CONTROL_ALLOW_METHODS, methods);
        out.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, ALLOW_HEADERS);
        out.insert(header::ACCESS_CONTROL_MAX_AGE, MAX_AGE);
    }
    with_cors(no_store(response), headers.get(header::ORIGIN))
}

/// The directory of the active cast output `config_id`.
fn output_dir(state: &AppState, config_id: &str) -> Option<std::path::PathBuf> {
    let engine = state.cast_engine.as_ref()?;
    engine
        .outputs()
        .into_iter()
        .find(|o| o.id == config_id && o.active)
        .map(|o| o.dir)
}

async fn media(
    State(state): State<AppState>,
    method: Method,
    headers: HeaderMap,
    path: Result<Path<(String, String)>, PathRejection>,
) -> Response {
    let origin = headers.get(header::ORIGIN).cloned();
    let response = no_store(serve_media(&state, method, &headers, path).await);
    with_cors(response, origin.as_ref())
}

async fn serve_media(
    state: &AppState,
    method: Method,
    headers: &HeaderMap,
    path: Result<Path<(String, String)>, PathRejection>,
) -> Response {
    let Ok(Path((token, file))) = path else {
        return not_found();
    };
    // Token first (constant-time over every digest), then the file name, so
    // the work done does not depend on which one is wrong.
    let stream = state.cast_streams.lookup(&token);
    let (Some((_, config_id)), true) = (stream, file_allowed(&file)) else {
        return not_found();
    };
    // ABUSE-1: the fail-closed barrier. A feed the engine moved the output
    // to that its tokens were not authorized for (a fallback whose outcome
    // is not relayed yet) re-authorizes them NOW; then only a token armed at
    // the output's CURRENT generation is served.
    crate::cast_streams::reauth::on_media(state, &config_id);
    let Some((stream_id, config_id)) = state.cast_streams.serve(&token) else {
        return not_found();
    };
    let Some(dir) = output_dir(state, &config_id) else {
        return not_found();
    };
    let response =
        multiview_output::hls::http::serve_file(dir, &file, method, headers.clone()).await;
    let status = response.status();
    if status == StatusCode::NOT_FOUND {
        return not_found();
    }
    if status.is_success() || status == StatusCode::NOT_MODIFIED {
        let playlist = std::path::Path::new(&file)
            .extension()
            .is_some_and(|e| e == "m3u8");
        state
            .cast_streams
            .record_fetch(&stream_id, playlist, user_agent(headers));
    }
    response
}

async fn report(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<(String, String)>, PathRejection>,
    body: Body,
) -> Response {
    let origin = headers.get(header::ORIGIN).cloned();
    let response = no_store(take_report(&state, path, body).await);
    with_cors(response, origin.as_ref())
}

async fn take_report(
    state: &AppState,
    path: Result<Path<(String, String)>, PathRejection>,
    body: Body,
) -> Response {
    let Ok(Path((token, file))) = path else {
        return not_found();
    };
    let Some((_, config_id)) = state.cast_streams.lookup(&token) else {
        return not_found();
    };
    crate::cast_streams::reauth::on_media(state, &config_id);
    if file != "report" || state.cast_streams.serve(&token).is_none() {
        return not_found();
    }
    let limit = crate::cast_streams::MAX_REPORT_BYTES;
    let bytes: Bytes = match axum::body::to_bytes(body, limit).await {
        Ok(bytes) => bytes,
        Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
    };
    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(value @ serde_json::Value::Object(_)) => value,
        _ => return StatusCode::BAD_REQUEST.into_response(),
    };
    match state.cast_streams.report(&token, value) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(ReportError::TooSoon) => {
            let mut response = StatusCode::TOO_MANY_REQUESTS.into_response();
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
            response
        }
        Err(ReportError::Unknown) => not_found(),
    }
}

async fn receiver_redirect() -> Response {
    (
        StatusCode::PERMANENT_REDIRECT,
        [(
            header::LOCATION,
            HeaderValue::from_static("/cast/receiver/"),
        )],
    )
        .into_response()
}

fn receiver_file(content_type: &'static str, body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-cache"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::CONTENT_SECURITY_POLICY, RECEIVER_CSP),
        ],
        body,
    )
        .into_response()
}

async fn receiver_index(State(state): State<AppState>, headers: HeaderMap) -> Response {
    state
        .cast_streams
        .record_receiver_fetch(user_agent(&headers));
    receiver_file("text/html; charset=utf-8", INDEX_HTML)
}

async fn receiver_js() -> Response {
    receiver_file("text/javascript; charset=utf-8", RECEIVER_JS)
}

async fn receiver_css() -> Response {
    receiver_file("text/css; charset=utf-8", RECEIVER_CSS)
}
