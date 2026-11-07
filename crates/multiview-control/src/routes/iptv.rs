//! IPTV publication surface (epg-playlist).
//!
//! Management API (authenticated, under `/api/v1`):
//! * `GET`/`PUT /api/v1/iptv` — the `[iptv]` publication document with a
//!   content `ETag` (`If-Match` required on `PUT` → `428`/`412`), validation of
//!   every reference, durable commit through the desired-state transaction, and
//!   per-object authorization on every referenced output/source id.
//! * `POST /api/v1/sources/import/m3u` — bounded catalogue import (operator).
//!
//! Public delivery (outside `/api/v1`, mounted like `/hls/{output}`):
//! * `GET /iptv/playlist.m3u8`, `GET /iptv/guide.xml`, `GET /iptv/guide.xml.gz`.
//!
//! **Auth choice:** the public files use the same model as the `/hls/…`
//! delivery mounts — unauthenticated, because IPTV players and set-top boxes
//! cannot send `Bearer` tokens. They expose only what an operator explicitly
//! published: output playback paths that are already public under `/hls`, and
//! source upstream URLs only with `allow_direct_source_url` (userinfo always
//! stripped). Disabled ⇒ `404`. No upstream fetch ever happens on a request.
use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use multiview_config::IptvConfig;
use serde::{Deserialize, Serialize};

use crate::audit::AuditAction;
use crate::auth::{Action, Principal};
use crate::error::{ControlError, ControlResult};
use crate::iptv::{self, fetch, m3u, FeedStatus, ResolvedEntry};
use crate::state::AppState;

/// The audit/precondition kind of the publication singleton.
pub const IPTV_KIND: &str = "iptv";

/// The `GET`/`PUT /api/v1/iptv` response envelope.
#[derive(Debug, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct IptvState {
    /// The publication document (entries filtered to the caller's scope).
    #[cfg_attr(feature = "openapi", schema(value_type = crate::openapi_schemas::IptvConfigDoc))]
    pub config: IptvConfig,
    /// Every entry resolved against the current configuration.
    pub entries: Vec<ResolvedEntry>,
    /// Per upstream XMLTV feed refresh status.
    pub feeds: Vec<FeedStatus>,
    /// Server path of the published playlist.
    pub playlist_path: String,
    /// Server path of the published guide.
    pub guide_path: String,
    /// Unix seconds the current guide snapshot was assembled, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guide_generated_at: Option<i64>,
}

fn require_all_in_scope(principal: &Principal, config: &IptvConfig) -> ControlResult<()> {
    for entry in &config.entries {
        crate::auth::authorize_object(principal, &entry.reference)?;
    }
    Ok(())
}

fn envelope(state: &AppState, principal: &Principal) -> ControlResult<(IptvState, String)> {
    let publication = iptv::publication(state)?;
    let etag = iptv::config_etag(&publication.config);
    let mut config = publication.config.clone();
    config
        .entries
        .retain(|e| crate::auth::authorize_object(principal, &e.reference).is_ok());
    let entries = publication
        .entries
        .iter()
        .filter(|e| crate::auth::authorize_object(principal, &e.reference).is_ok())
        .cloned()
        .collect();
    let feeds = state.iptv.status(&publication);
    let guide_generated_at = state.iptv.snapshot().map(|s| s.generated_at);
    Ok((
        IptvState {
            config,
            entries,
            feeds,
            playlist_path: "/iptv/playlist.m3u8".into(),
            guide_path: "/iptv/guide.xml".into(),
            guide_generated_at,
        },
        etag,
    ))
}

fn with_etag(mut response: Response, etag: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(etag) {
        response.headers_mut().insert(header::ETAG, value);
    }
    response
}

/// `GET /api/v1/iptv` — the publication document and status (role: read).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/iptv",
        tag = "iptv",
        responses(
            (status = 200, description = "The IPTV publication (ETag in the response header). Entries are filtered to the caller's object scope.", body = IptvState),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to read.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_iptv(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Response> {
    principal.role.require(Action::Read)?;
    let (body, etag) = envelope(&state, &principal)?;
    Ok(with_etag(
        (StatusCode::OK, Json(body)).into_response(),
        &etag,
    ))
}

/// `PUT /api/v1/iptv` — replace the publication (role: write; `If-Match`).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        put,
        path = "/api/v1/iptv",
        tag = "iptv",
        request_body = crate::openapi_schemas::IptvConfigDoc,
        responses(
            (status = 200, description = "The replaced publication (new ETag). Applies live: the playlist/guide are served from accepted intent.", body = IptvState),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to write, or an entry references an object outside the caller's scope.", body = crate::problem::Problem),
            (status = 412, description = "If-Match precondition failed.", body = crate::problem::Problem),
            (status = 422, description = "Invalid document: unknown output/source reference, a non-HLS output, a source entry without allow_direct_source_url, or a structural error.", body = crate::problem::Problem),
            (status = 428, description = "No If-Match precondition was sent.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn put_iptv(
    State(state): State<AppState>,
    principal: Principal,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    let _mutation = state.lock_config_mutation().await;
    let (current_config, sources, outputs) = iptv::accepted_inputs(&state)?;
    let current_config = current_config.unwrap_or_default();
    let current = iptv::config_etag(&current_config);
    let presented = headers
        .get(header::IF_MATCH)
        .and_then(|v| v.to_str().ok())
        .ok_or(ControlError::PreconditionRequired { kind: IPTV_KIND })?;
    if presented.trim() != current && presented.trim() != "*" {
        return Err(ControlError::VersionConflict {
            kind: IPTV_KIND,
            id: "singleton".into(),
            expected: presented.to_owned(),
            actual: current,
        });
    }
    // A scoped principal may neither publish nor unpublish objects it cannot see.
    require_all_in_scope(&principal, &current_config)?;
    let config: IptvConfig = serde_path_to_error::deserialize(body).map_err(|e| {
        ControlError::Validation(format!(
            "iptv document invalid at `{}`: {}",
            e.path(),
            e.inner()
        ))
    })?;
    require_all_in_scope(&principal, &config)?;
    let source_ids: Vec<&str> = sources.iter().map(|s| s.id.as_str()).collect();
    let output_refs: Vec<(&str, bool)> = outputs
        .iter()
        .map(|(id, output)| {
            (
                id.as_str(),
                matches!(
                    output,
                    multiview_config::Output::Hls { .. } | multiview_config::Output::LlHls { .. }
                ),
            )
        })
        .collect();
    config
        .validate(Some(&source_ids), Some(&output_refs))
        .map_err(|e| ControlError::Validation(e.to_string()))?;
    if state.boot_model.is_some() {
        let value = serde_json::to_value(&config)
            .map_err(|e| ControlError::Repository(format!("iptv serialization: {e}")))?;
        let prospective = crate::boot_model::prospective_config(&state, "iptv", value)?;
        crate::boot_model::commit_desired(
            &state,
            prospective,
            crate::boot_model::layout_library(&state)?,
        )
        .await?;
    } else {
        state.iptv.set_fallback(Some(config.clone()));
    }
    state.audit(
        &principal.key_id,
        AuditAction::Update,
        IPTV_KIND,
        "singleton",
        serde_json::to_value(&config).ok(),
    );
    state.iptv.wake();
    let (body, etag) = envelope(&state, &principal)?;
    Ok(crate::typed_resources::with_apply(
        crate::typed_resources::ApplyMode::Live,
        with_etag((StatusCode::OK, Json(body)).into_response(), &etag),
    ))
}

/// `POST /api/v1/sources/import/m3u` request body: exactly one of `url`/`text`.
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct M3uImportRequest {
    /// An http(s) URL of an extended-M3U catalogue to fetch (bounded, SSRF-screened).
    #[serde(default)]
    pub url: Option<String>,
    /// Pasted catalogue text (at most 2 MiB).
    #[serde(default)]
    pub text: Option<String>,
    /// Permit a catalogue on a private/LAN address (loopback, link-local and
    /// metadata addresses are always refused).
    #[serde(default)]
    pub allow_private_network: bool,
}

/// Fetch timeout for a catalogue import.
const IMPORT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// `POST /api/v1/sources/import/m3u` — parse a catalogue into source
/// candidates (role: write). Creates nothing: the client creates the selected
/// candidates through `POST /api/v1/sources/{id}`.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/sources/import/m3u",
        tag = "sources",
        request_body = M3uImportRequest,
        responses(
            (status = 200, description = "The parsed catalogue: header guide URL(s) and candidates.", body = crate::iptv::m3u::M3uCatalogue),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to write.", body = crate::problem::Problem),
            (status = 422, description = "Neither/both of url and text, a refused URL, a failed fetch, or an unparseable/oversized catalogue.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn import_m3u(
    State(state): State<AppState>,
    principal: Principal,
    Json(request): Json<M3uImportRequest>,
) -> ControlResult<Json<m3u::M3uCatalogue>> {
    principal.role.require(Action::Write)?;
    let catalogue = match (request.url, request.text) {
        (Some(url), None) => {
            fetch::check_fetch_url(&url).map_err(ControlError::Validation)?;
            let policy = if request.allow_private_network {
                std::sync::Arc::clone(&state.dial_policy)
            } else {
                std::sync::Arc::new(
                    multiview_config::device::net_guard::DialPolicy::deny_all_allowlistable(),
                )
            };
            let outcome = state
                .iptv
                .fetcher()
                .fetch(
                    fetch::FetchRequest {
                        url: url.clone(),
                        max_bytes: u64::try_from(m3u::MAX_M3U_BYTES).unwrap_or(u64::MAX),
                        timeout: IMPORT_TIMEOUT,
                        etag: None,
                        last_modified: None,
                    },
                    policy,
                )
                .await
                .map_err(|e| ControlError::Validation(format!("catalogue fetch failed: {e}")))?;
            let fetch::FetchOutcome::Body { file, .. } = outcome else {
                return Err(ControlError::Validation(
                    "catalogue fetch returned no body".into(),
                ));
            };
            tokio::task::spawn_blocking(move || {
                use std::io::Read;
                let mut bytes = Vec::new();
                file.take(u64::try_from(m3u::MAX_M3U_BYTES).unwrap_or(u64::MAX) + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|e| e.to_string())?;
                m3u::parse_m3u(&bytes, Some(&url)).map_err(|e| e.to_string())
            })
            .await
            .map_err(|e| ControlError::Repository(format!("import task failed: {e}")))?
            .map_err(ControlError::Validation)?
        }
        (None, Some(text)) => m3u::parse_m3u(text.as_bytes(), None)
            .map_err(|e| ControlError::Validation(e.to_string()))?,
        _ => {
            return Err(ControlError::Validation(
                "provide exactly one of `url` or `text`".into(),
            ));
        }
    };
    Ok(Json(catalogue))
}

/// Characters allowed in a `Host` header used to build playlist links.
fn safe_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 255
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']' | '_'))
}

/// `scheme://host[:port]` for links in the published playlist.
fn public_base(state: &AppState, config: &IptvConfig, headers: &HeaderMap) -> Option<String> {
    if let Some(base) = &config.public_base_url {
        return Some(base.trim_end_matches('/').to_owned());
    }
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .filter(|h| safe_host(h))?;
    let forwarded = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .filter(|p| matches!(*p, "http" | "https"));
    // The listener terminates TLS itself when `[control.tls]` is configured.
    let tls = state
        .base_document
        .as_deref()
        .and_then(|doc| doc.get("control"))
        .and_then(|control| control.get("tls"))
        .is_some_and(|tls| !tls.is_null());
    let scheme = forwarded.unwrap_or(if tls { "https" } else { "http" });
    Some(format!("{scheme}://{host}"))
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "IPTV publication is not enabled\n").into_response()
}

fn not_modified(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag || t.trim() == "*"))
}

fn cached(status: StatusCode, etag: &str, content_type: &str, body: Body) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    let headers = response.headers_mut();
    if let Ok(v) = HeaderValue::from_str(etag) {
        headers.insert(header::ETAG, v);
    }
    if let Ok(v) = HeaderValue::from_str(content_type) {
        headers.insert(header::CONTENT_TYPE, v);
    }
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=60"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    response
}

/// `GET /iptv/playlist.m3u8` — the published extended-M3U (public; 404 when disabled).
pub(crate) async fn public_playlist(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let publication = match iptv::publication(&state) {
        Ok(p) => p,
        Err(error) => {
            tracing::warn!(%error, "IPTV playlist: cannot read accepted intent");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    if !publication.config.enabled {
        return not_found();
    }
    let Some(base) = public_base(&state, &publication.config, &headers) else {
        return (StatusCode::BAD_REQUEST, "missing or invalid Host header\n").into_response();
    };
    let text = iptv::render_playlist(&publication, &base);
    let etag = format!("\"pl-{}\"", {
        use sha2::Digest;
        let digest = sha2::Sha256::digest(text.as_bytes());
        digest.iter().take(12).fold(String::new(), |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        })
    });
    if not_modified(&headers, &etag) {
        return cached(
            StatusCode::NOT_MODIFIED,
            &etag,
            "audio/x-mpegurl; charset=utf-8",
            Body::empty(),
        );
    }
    cached(
        StatusCode::OK,
        &etag,
        "audio/x-mpegurl; charset=utf-8",
        Body::from(text),
    )
}

async fn guide(state: AppState, headers: HeaderMap, force_gzip_file: bool) -> Response {
    let publication = match iptv::publication(&state) {
        Ok(p) => p,
        Err(error) => {
            tracing::warn!(%error, "IPTV guide: cannot read accepted intent");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    if !publication.config.enabled {
        return not_found();
    }
    // This request is what makes the guide worth fetching. The wait is bounded,
    // so a slow feed yields the last good guide or channels with no programmes.
    state
        .iptv
        .request_guide(&publication, iptv::GUIDE_COLD_WAIT)
        .await;
    let service = std::sync::Arc::clone(&state.iptv);
    let Ok(Ok(snapshot)) =
        tokio::task::spawn_blocking(move || service.current_snapshot(&publication)).await
    else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let etag = if force_gzip_file {
        snapshot.etag.replacen("\"guide-", "\"guidegz-", 1)
    } else {
        snapshot.etag.clone()
    };
    let accepts_gzip = headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim().starts_with("gzip")));
    let content_type = if force_gzip_file {
        "application/gzip"
    } else {
        "application/xml; charset=utf-8"
    };
    if not_modified(&headers, &etag) {
        let mut response = cached(StatusCode::NOT_MODIFIED, &etag, content_type, Body::empty());
        response
            .headers_mut()
            .insert(header::VARY, HeaderValue::from_static("accept-encoding"));
        return response;
    }
    if force_gzip_file {
        return cached(
            StatusCode::OK,
            &etag,
            content_type,
            Body::from(snapshot.gzip.clone()),
        );
    }
    let mut response = if accepts_gzip {
        let mut r = cached(
            StatusCode::OK,
            &etag,
            content_type,
            Body::from(snapshot.gzip.clone()),
        );
        r.headers_mut()
            .insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        r
    } else {
        cached(
            StatusCode::OK,
            &etag,
            content_type,
            Body::from(snapshot.plain.clone()),
        )
    };
    response
        .headers_mut()
        .insert(header::VARY, HeaderValue::from_static("accept-encoding"));
    response
}

/// `GET /iptv/guide.xml` — the filtered XMLTV guide (public; gzip on request).
pub(crate) async fn public_guide(State(state): State<AppState>, headers: HeaderMap) -> Response {
    guide(state, headers, false).await
}

/// `GET /iptv/guide.xml.gz` — the gzip guide file (public).
pub(crate) async fn public_guide_gz(State(state): State<AppState>, headers: HeaderMap) -> Response {
    guide(state, headers, true).await
}

/// The public IPTV delivery router (mounted outside `/api/v1`).
pub fn public_router() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/iptv/playlist.m3u8", axum::routing::get(public_playlist))
        .route("/iptv/guide.xml", axum::routing::get(public_guide))
        .route("/iptv/guide.xml.gz", axum::routing::get(public_guide_gz))
}
