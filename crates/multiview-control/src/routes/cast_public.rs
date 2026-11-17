//! The public cast surface (ADR-0111, design §3/§4), outside `/api/v1` and
//! merged into every listener's router beside `/iptv`, so the per-IP rate
//! limit and the concurrency cap apply and the SPA fallback never answers:
//!
//! * `GET|HEAD|OPTIONS /cast/{token}/{file}`: one cast output's HLS files
//!   behind a stream token. An unknown, expired or revoked token, a file
//!   name off the allow-list, or a file outside the output's directory all
//!   answer the SAME `404` (no oracle).
//! * `POST /cast/{token}/report`: the receiver's state report.
//! * `POST /cast/{token}/ladder`: the receiver's capability probe and the
//!   engine's transport offer and clock pair (ADR-0114 decision 3).
//! * `/cast/receiver/` (+ `index.html`, `probe.js`, `receiver.js`,
//!   `receiver.css`): the
//!   custom Web Receiver, embedded, under a strict CSP. These static routes
//!   win over the token route.
//!
//! Media is served only for a token armed at its output's CURRENT
//! authorization generation (ABUSE-1, `cast_streams::reauth`), checked
//! before the file is resolved AND again after it is open, before any byte
//! of the response is released. Every token route admits through the ONE
//! shared check, [`crate::cast_streams::reauth::admit`] (ADR-0114 5.1).
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

use crate::cast_streams::ladder::{parse_request, LadderRequest, MAX_LADDER_BYTES};
use crate::cast_streams::token::file_allowed;
use crate::cast_streams::{LadderError, ReportError};
use crate::state::AppState;

/// The receiver page's Content-Security-Policy (measured with the real CAF
/// SDK: 0 violations; `receiver-work/REPORT.md`).
pub const RECEIVER_CSP: &str = "default-src 'none'; script-src 'self' https://www.gstatic.com https://ajax.googleapis.com; style-src 'self' 'unsafe-inline'; img-src 'self' data:; media-src 'self' https: blob:; connect-src 'self' https: ws://localhost:8008; font-src 'self' https://fonts.gstatic.com; worker-src blob:";

const INDEX_HTML: &str = include_str!("../cast_receiver/index.html");
const RECEIVER_JS: &str = include_str!("../cast_receiver/receiver.js");
const PROBE_JS: &str = include_str!("../cast_receiver/probe.js");
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
        .route("/cast/receiver/probe.js", get(probe_js))
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
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<(String, String)>, PathRejection>,
) -> Response {
    // The shared token check (ADR-0114 5.1) on every token route, the
    // preflight included: a dead token gets the generic 404, never a 204.
    let admitted = path
        .as_ref()
        .is_ok_and(|Path((token, _))| crate::cast_streams::reauth::admit(&state, token).is_some());
    if !admitted {
        return with_cors(not_found(), headers.get(header::ORIGIN));
    }
    let post = path.is_ok_and(|Path((_, file))| file == "report" || file == "ladder");
    let methods = if post { REPORT_METHODS } else { MEDIA_METHODS };
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

/// The directory of the active cast output `config_id`, when `file` may be
/// served from it now: a file whose name carries a generation (segments,
/// init files, `.vtt`) only when that is the output's CURRENT generation
/// (#418 review ABUSE-HISTORY-R4: a stale file a best-effort purge left
/// behind, of a superseded activation or a failed packager, is never
/// served). Playlists carry none: they are always the current activation's.
/// A disabled output serves nothing (#418 review r7: files it could not
/// remove may be left in its directory).
fn output_dir(state: &AppState, config_id: &str, file: &str) -> Option<std::path::PathBuf> {
    let engine = state.cast_engine.as_ref()?;
    let output = engine
        .outputs()
        .into_iter()
        .find(|o| o.id == config_id && o.active && !o.disabled)?;
    current_generation_file(file, output.generation).then_some(output.dir)
}

/// Whether `file` may be served while the output's files are of
/// `generation`: a playlist always, a generation-carrying media file only
/// of that generation.
fn current_generation_file(file: &str, generation: u64) -> bool {
    let playlist = std::path::Path::new(file)
        .extension()
        .is_some_and(|e| e == "m3u8");
    playlist
        || multiview_output::hls::cast::packager_file_generation(file)
            .is_some_and(|g| g != 0 && g == generation)
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
    // ABUSE-1: the fail-closed barrier inside the shared check. A feed the
    // engine moved the output to that its tokens were not authorized for (a
    // fallback whose outcome is not relayed yet) re-authorizes them NOW;
    // then only a token armed at the output's CURRENT generation is served.
    let admitted = crate::cast_streams::reauth::admit(state, &token);
    let (Some(admitted), true) = (admitted, file_allowed(&file)) else {
        return not_found();
    };
    let crate::cast_streams::Admitted {
        stream_id,
        config_id,
        ..
    } = admitted;
    let Some(dir) = output_dir(state, &config_id, &file) else {
        return not_found();
    };
    state.cast_streams.run_before_open().await;
    // `serve_file` returns once the file is OPEN (canonicalize + confine +
    // open + fstat awaited on the blocking pool); its body streams from that
    // open handle only. Every cast file is renamed into place whole
    // (`write_renamed` / `atomic_write`), so the bytes are fixed at open: a
    // later rename cannot change what this response sends.
    let response =
        multiview_output::hls::http::serve_file(dir, &file, method, headers.clone()).await;
    let status = response.status();
    if status == StatusCode::NOT_FOUND {
        return not_found();
    }
    // ABUSE-1, the request in flight: the authorization above was taken
    // BEFORE the awaits. A take or fallback may have revoked this owner and
    // published a feed it may not read meanwhile, so re-run the barrier and
    // re-check the token (live, armed at the output's CURRENT generation,
    // same stream on the same output) AFTER the file is open and BEFORE any
    // byte is released. On failure the open handle is dropped unread and
    // the answer is the generic 404.
    if !crate::cast_streams::reauth::admit(state, &token)
        .is_some_and(|again| again.stream_id == stream_id && again.config_id == config_id)
        || output_dir(state, &config_id, &file).is_none()
    {
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
    let Some(admitted) = crate::cast_streams::reauth::admit(state, &token) else {
        return not_found();
    };
    if file == "ladder" {
        return take_ladder(state, &token, body).await;
    }
    if file != "report" {
        return not_found();
    }
    let limit = crate::cast_streams::MAX_REPORT_BYTES;
    let read = axum::body::to_bytes(body, limit).await;
    // The body was awaited: admit again before any answer that depends on it
    // (a revoke, an expiry or a fallback may have landed meanwhile).
    if !readmitted(state, &token, &admitted) {
        return not_found();
    }
    let Ok(bytes) = read else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(value @ serde_json::Value::Object(_)) => value,
        _ => return StatusCode::BAD_REQUEST.into_response(),
    };
    match state.cast_streams.report(&token, value) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(ReportError::TooSoon) => too_many(),
        Err(ReportError::Unknown) => not_found(),
    }
}

/// The shared check again after an `.await`: the same stream, on the same
/// output, still admitted.
fn readmitted(state: &AppState, token: &str, before: &crate::cast_streams::Admitted) -> bool {
    crate::cast_streams::reauth::admit(state, token).is_some_and(|again| {
        again.stream_id == before.stream_id && again.config_id == before.config_id
    })
}

/// Microseconds since the Unix epoch on the system wall clock.
fn unix_micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX))
}

fn too_many() -> Response {
    let mut response = StatusCode::TOO_MANY_REQUESTS.into_response();
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    response
}

/// `POST /cast/<token>/ladder` (ADR-0114 decision 3), after the shared
/// token check: rate (before the body is read, so a bad call costs the
/// same), body at most 2 KiB, strict validation, then the answer. Only
/// control-plane state is touched: the registry (one short lock) and the
/// engine's epoch cell (a nanosecond read). No data-plane thread is reached.
async fn take_ladder(state: &AppState, token: &str, body: Body) -> Response {
    // `t1`: when the request reached the engine (before the body is read).
    let t1 = unix_micros();
    let Some(admitted) = crate::cast_streams::reauth::admit(state, token) else {
        return not_found();
    };
    match state.cast_streams.admit_ladder(&admitted.stream_id) {
        Ok(()) => {}
        Err(LadderError::TooSoon) => return too_many(),
        Err(LadderError::Unknown) => return not_found(),
    }
    let read = axum::body::to_bytes(body, MAX_LADDER_BYTES).await;
    // The body was awaited: admit again (a revoke, an expiry or a take may
    // have landed meanwhile) BEFORE any answer that depends on the body, and
    // answer only for the same stream on the same output.
    if !readmitted(state, token, &admitted) {
        return not_found();
    }
    let again = admitted;
    let Ok(bytes) = read else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let request: LadderRequest = match parse_request(&bytes, state.cast_streams.now_ms()) {
        Ok(request) => request,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    let clock = state
        .cast_engine
        .as_ref()
        .and_then(|engine| engine.clock(&again.config_id, t1.saturating_mul(1000)));
    let offer = request.full.then(|| {
        crate::cast_streams::ladder::offer(
            &state.cast_streams.ladder(),
            crate::cast_streams::ladder::SERVED_RUNGS,
            request.probe.as_ref(),
        )
    });
    let (attach, then) = offer.map_or((None, Vec::new()), |(attach, then)| (Some(attach), then));
    let full = request.full;
    let t0 = request.t0_us;
    if state
        .cast_streams
        .record_ladder(&again.stream_id, request, attach)
        .is_err()
    {
        return not_found();
    }
    let clock = clock.map(|c| {
        serde_json::json!({
            "source": "epoch",
            "epoch_generation": c.epoch_generation,
            "media_us": c.media_us,
            "wall_unix_us": c.wall_unix_us,
            "t0_unix_us": t0,
            "t1_unix_us": t1,
            "t2_unix_us": unix_micros(),
        })
    });
    let answer = if full {
        serde_json::json!({
            "v": 1,
            "clock": clock,
            "attach": attach.map(|rung| serde_json::json!({ "rung": rung })),
            "then": then,
            "srt": null,
        })
    } else {
        serde_json::json!({ "v": 1, "clock": clock })
    };
    (
        [(header::CONTENT_TYPE, "application/json")],
        answer.to_string(),
    )
        .into_response()
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

async fn probe_js() -> Response {
    receiver_file("text/javascript; charset=utf-8", PROBE_JS)
}

async fn receiver_css() -> Response {
    receiver_file("text/css; charset=utf-8", RECEIVER_CSS)
}
