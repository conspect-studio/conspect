//! `/api/v1/catalogues/iptv-org/*` — the IPTV-ORG source browser backend
//! (ADR-M015, `docs/io/iptv-org.md`).
//!
//! | Route | Role |
//! |---|---|
//! | `GET status` / `index` / `places` / `logos/{k}` / `links` / `probes/{id}` | read |
//! | `POST probes` | write (operate) |
//! | `POST refresh` / `POST import` | administer |
//!
//! Link status is computed per caller over the sources the
//! caller may see (per-object authz, BOLA) — never over the global set.

use std::collections::HashSet;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::audit::AuditAction;
use crate::auth::{Action, Principal};
use crate::catalogue::links;
use crate::catalogue::probe::wire::ProbeJob;
use crate::catalogue::probe::{JobTarget, ProbeRequest};
use crate::catalogue::service::{index_body, StatusBody};
use crate::catalogue::sync::SyncOutcome;
use crate::error::{ControlError, ControlResult};
use crate::state::AppState;

const OBJECT_KIND: &str = "catalogue";
const OBJECT_ID: &str = "iptv-org";
/// Offline bundles may be large; cap the request body.
pub(crate) const IMPORT_BODY_LIMIT: usize = 192 * 1024 * 1024;

/// The iptv-org country of a linked source's channel (ISO alpha-2), for the
/// network report's geo-block hint. `None` when the source is not linked or the
/// channel is not in the current snapshot.
fn source_channel_country(state: &AppState, source: &multiview_config::Source) -> Option<String> {
    let channel = source.origin.as_ref()?.channel.clone()?;
    state
        .catalogue
        .as_ref()?
        .snapshot()?
        .rows
        .iter()
        .find(|r| r.ch.as_deref() == Some(channel.as_str()))
        .and_then(|r| r.country.clone())
}

fn catalogue(state: &AppState) -> ControlResult<&std::sync::Arc<crate::catalogue::Catalogue>> {
    state.catalogue.as_ref().ok_or_else(|| {
        ControlError::Conflict("the IPTV-ORG catalogue is not configured on this node".to_owned())
    })
}

fn enabled(state: &AppState) -> ControlResult<&std::sync::Arc<crate::catalogue::Catalogue>> {
    let c = catalogue(state)?;
    if c.enabled() {
        Ok(c)
    } else {
        Err(ControlError::Conflict(
            "the IPTV-ORG catalogue is disabled ([catalogues.iptv_org] enabled = false)".to_owned(),
        ))
    }
}

/// `GET /api/v1/catalogues/iptv-org/status`.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/catalogues/iptv-org/status",
        operation_id = "get_catalogue_status",
        tag = "catalogue",
        responses(
            (status = 200, description = "Sync state, counts, ETags, last error and the caller-scoped link-status tally.", body = serde_json::Value),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 409, description = "Catalogue not configured.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_status(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<StatusBody>> {
    principal.role.require(Action::Read)?;
    let cat = catalogue(&state)?;
    let mut body = cat.status();
    let visible: HashSet<String> = super::sources::visible_sources(&state, &principal)?
        .into_iter()
        .map(|s| s.id)
        .collect();
    body.links = cat.links.tally(|id| visible.contains(id));
    Ok(Json(body))
}

fn etag_matches(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .any(|t| t.trim().trim_start_matches("W/").trim_matches('"') == etag)
        })
}

fn with_etag(mut r: Response, etag: &str) -> Response {
    if let Ok(v) = HeaderValue::from_str(&format!("\"{etag}\"")) {
        r.headers_mut().insert(header::ETAG, v);
    }
    r.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-cache"),
    );
    r.headers_mut().insert(
        header::VARY,
        HeaderValue::from_static("Authorization, Accept-Encoding"),
    );
    r
}

fn accepts_gzip(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("gzip"))
}

/// `GET /api/v1/catalogues/iptv-org/index` — the joined stream index.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/catalogues/iptv-org/index",
        operation_id = "get_catalogue_index",
        tag = "catalogue",
        responses(
            (status = 200, description = "`{schema, generated_at, categories[], languages[], streams[]}`; gzip when accepted; identical for every caller (`already_added` is computed by the web).", body = serde_json::Value),
            (status = 304, description = "If-None-Match matched ."),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 503, description = "No snapshot yet; refresh or import first.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_index(
    State(state): State<AppState>,
    principal: Principal,
    headers: HeaderMap,
) -> ControlResult<Response> {
    principal.role.require(Action::Read)?;
    let cat = enabled(&state)?;
    let snapshot = cat.snapshot().ok_or_else(|| {
        ControlError::Conflict(
            "no catalogue snapshot yet; POST /catalogues/iptv-org/refresh or /import".to_owned(),
        )
    })?;
    // The index is identical for every caller (`already_added` is computed by
    // the web from its own visible sources), so the shared cached gzip body and
    // the base ETag serve everyone.
    let etag = snapshot.etag.clone();
    if etag_matches(&headers, &etag) {
        return Ok(with_etag(StatusCode::NOT_MODIFIED.into_response(), &etag));
    }
    let gz = accepts_gzip(&headers);
    let body: Vec<u8> = match cat.cached_index() {
        Some((cached, _)) if gz => cached.as_ref().clone(),
        _ => {
            let raw = index_body(&snapshot, None)
                .ok_or_else(|| ControlError::Repository("index serialization failed".to_owned()))?;
            if gz {
                crate::catalogue::store::gzip(&raw)
                    .map_err(|e| ControlError::Repository(e.to_string()))?
            } else {
                raw
            }
        }
    };
    let mut r = (StatusCode::OK, body).into_response();
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if gz {
        r.headers_mut()
            .insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    }
    Ok(with_etag(r, &etag))
}

/// `GET .../places` response: `{schema, places[]}` (the web contract).
#[derive(Serialize)]
struct PlacesBody {
    schema: u32,
    places: Vec<crate::catalogue::index::Place>,
}

/// `GET /api/v1/catalogues/iptv-org/places` — the gazetteer with stream counts.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/catalogues/iptv-org/places",
        operation_id = "get_catalogue_places",
        tag = "catalogue",
        responses(
            (status = 200, description = "`{schema, places[{id, kind, name, alt, country, subdivision, lat, lon, flag, members, streams, streams_within}]}`; `id` is prefixed like a row's `area`.", body = serde_json::Value),
            (status = 304, description = "If-None-Match matched."),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 409, description = "No snapshot yet.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_places(
    State(state): State<AppState>,
    principal: Principal,
    headers: HeaderMap,
) -> ControlResult<Response> {
    principal.role.require(Action::Read)?;
    let cat = enabled(&state)?;
    let (places, etag) = cat
        .places()
        .ok_or_else(|| ControlError::Conflict("no catalogue snapshot yet".to_owned()))?;
    if etag_matches(&headers, &etag) {
        return Ok(with_etag(StatusCode::NOT_MODIFIED.into_response(), &etag));
    }
    Ok(with_etag(
        Json(PlacesBody {
            schema: crate::catalogue::index::SCHEMA_VERSION,
            places: places.flat(),
        })
        .into_response(),
        &etag,
    ))
}

/// `POST /api/v1/catalogues/iptv-org/refresh` — sync now (administer).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/catalogues/iptv-org/refresh",
        operation_id = "refresh_catalogue",
        tag = "catalogue",
        responses(
            (status = 200, description = "`{outcome: unchanged|updated, changed[]}`; a failed sync keeps the previous snapshot.", body = serde_json::Value),
            (status = 403, description = "Requires administer.", body = crate::problem::Problem),
            (status = 409, description = "Disabled / not configured.", body = crate::problem::Problem),
            (status = 502, description = "Upstream sync failed.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn post_refresh(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Response> {
    principal.role.require(Action::Administer)?;
    let cat = enabled(&state)?.clone();
    let outcome = cat
        .refresh()
        .await
        .map_err(|e| ControlError::Conflict(format!("catalogue sync failed: {e}")))?;
    after_sync(&state, &cat).await;
    state.audit(
        &principal.key_id,
        AuditAction::Command,
        OBJECT_KIND,
        OBJECT_ID,
        Some(serde_json::json!({"op":"refresh"})),
    );
    Ok(Json(outcome_json(&outcome)).into_response())
}

/// `POST /api/v1/catalogues/iptv-org/import` — offline bundle import (administer).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/catalogues/iptv-org/import",
        operation_id = "import_catalogue",
        tag = "catalogue",
        request_body(content = serde_json::Value, description = "A JSON object keyed by upstream file name (`streams`, `channels`, `feeds`, `logos`, `cities`, `countries`, `subdivisions`, `regions`, `blocklist`, `categories`, `languages`), optionally gzip'd. Replaces the named files; the rest stay as persisted.", content_type = "application/json"),
        responses(
            (status = 200, description = "`{outcome, changed[]}`.", body = serde_json::Value),
            (status = 403, description = "Requires administer.", body = crate::problem::Problem),
            (status = 422, description = "Bundle is malformed.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn post_import(
    State(state): State<AppState>,
    principal: Principal,
    body: Bytes,
) -> ControlResult<Response> {
    principal.role.require(Action::Administer)?;
    let cat = enabled(&state)?.clone();
    let outcome = cat
        .import_bundle(body.to_vec())
        .await
        .map_err(|e| ControlError::Validation(format!("catalogue import failed: {e}")))?;
    after_sync(&state, &cat).await;
    state.audit(
        &principal.key_id,
        AuditAction::Command,
        OBJECT_KIND,
        OBJECT_ID,
        Some(serde_json::json!({"op":"import"})),
    );
    Ok(Json(outcome_json(&outcome)).into_response())
}

fn outcome_json(o: &SyncOutcome) -> serde_json::Value {
    match o {
        SyncOutcome::Unchanged => serde_json::json!({"outcome":"unchanged","changed":[]}),
        SyncOutcome::Updated { changed } => {
            serde_json::json!({"outcome":"updated","changed":changed})
        }
    }
}

/// Recompute link status after a sync and publish transitions (slice D).
pub(crate) async fn after_sync(
    state: &AppState,
    cat: &std::sync::Arc<crate::catalogue::Catalogue>,
) {
    crate::catalogue::link_runtime::recompute_and_publish(state, cat).await;
}

/// `GET /api/v1/catalogues/iptv-org/logos/{k}` — persistent server-side logo cache.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/catalogues/iptv-org/logos/{k}",
        operation_id = "get_catalogue_logo",
        tag = "catalogue",
        params(("k" = String, Path, description = "Logo key `l:<hex>` from an index row.")),
        responses(
            (status = 200, description = "The logo image (PNG/JPEG/WebP/GIF/SVG), size-capped, cached on disk for 30 days. `Cache-Control: private, max-age=2592000, immutable` and a strong `ETag`.", content_type = "image/*"),
            (status = 204, description = "No logo: unknown key, upstream missing/failing, oversized or not an image (`Cache-Control: private, max-age=86400`), or the upstream fetch queue was busy (`Cache-Control: no-store`; retry later). Never an error status."),
            (status = 304, description = "`If-None-Match` matched the logo's `ETag`."),
            (status = 409, description = "Catalogue not configured.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_logo(
    State(state): State<AppState>,
    principal: Principal,
    headers: axum::http::HeaderMap,
    Path(k): Path<String>,
) -> ControlResult<Response> {
    use crate::catalogue::logos::{self, Logo};
    principal.role.require(Action::Read)?;
    let cat = enabled(&state)?;
    let no_logo = |cache_control: &'static str| {
        let mut r = StatusCode::NO_CONTENT.into_response();
        r.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(cache_control),
        );
        r
    };
    let Some(url) = cat.logo_url(&k) else {
        return Ok(no_logo("private, max-age=86400"));
    };
    let image = match logos::serve(&state, &cat.logo_dir(), &url).await {
        Logo::Image(i) => i,
        Logo::Missing => return Ok(no_logo("private, max-age=86400")),
        Logo::Busy => return Ok(no_logo("no-store")),
    };
    let matches = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .map(|t| t.trim().trim_start_matches("W/"))
                .any(|t| t == "*" || t == image.etag)
        });
    let mut r = if matches {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        let mut r = (StatusCode::OK, image.bytes.clone()).into_response();
        if let Ok(v) = HeaderValue::from_str(&image.content_type) {
            r.headers_mut().insert(header::CONTENT_TYPE, v);
        }
        r
    };
    if let Ok(v) = HeaderValue::from_str(&image.etag) {
        r.headers_mut().insert(header::ETAG, v);
    }
    r.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=2592000, immutable"),
    );
    r.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    r.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'; sandbox"),
    );
    Ok(r)
}

/// `GET /api/v1/catalogues/iptv-org/links` — per-source link status (caller-scoped).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/catalogues/iptv-org/links",
        operation_id = "list_catalogue_links",
        tag = "catalogue",
        responses(
            (status = 200, description = "`{links: [{source_id, status, checked_at, upstream, replaced_by, alternatives}]}` — status, upstream target and ranked alternatives (with cached probe verdict), only for sources the caller may see.", body = serde_json::Value),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 409, description = "Catalogue not configured.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_links(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<serde_json::Value>> {
    principal.role.require(Action::Read)?;
    let cat = catalogue(&state)?;
    let visible: HashSet<String> = super::sources::visible_sources(&state, &principal)?
        .into_iter()
        .map(|s| s.id)
        .collect();
    let snapshot = cat.snapshot();
    let probes = state.catalogue_probe.clone();
    let verdict_of = |alt: &links::wire::LinkAlternative| {
        probes
            .as_ref()?
            .latest_for(&ProbeRequest {
                url: alt.url.clone(),
                user_agent: alt.user_agent.clone(),
                referrer: alt.referrer.clone(),
                quality: None,
                channel_country: None,
            })?
            .verdict
    };
    let infos: Vec<links::wire::WireLink> = cat
        .links
        .all()
        .into_iter()
        .filter(|i| visible.contains(&i.source_id))
        .map(|i| links::wire::to_wire(&i, snapshot.as_deref(), &verdict_of))
        .collect();
    Ok(Json(serde_json::json!({ "links": infos })))
}

/// Request body of `POST .../probes` (the web contract's `ProbeRequest`).
///
/// The browser never supplies a URL: it names index rows (`keys`) and/or linked
/// sources (`source_ids`); the server resolves URL, headers and quality.
#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub(crate) struct ProbeBody {
    /// Index row keys (`s:<hex>`).
    #[serde(default)]
    pub keys: Vec<String>,
    /// Linked source ids (per-object authorised).
    #[serde(default)]
    pub source_ids: Vec<String>,
}

/// Largest batch one `POST /probes` may carry.
const MAX_PROBE_BATCH: usize = 50;

/// `POST /api/v1/catalogues/iptv-org/probes` — start a probe job (operate).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/catalogues/iptv-org/probes",
        operation_id = "create_catalogue_probe",
        tag = "catalogue",
        request_body = ProbeBody,
        responses(
            (status = 202, description = "The probe job `{id, state: queued|running|done|failed, results[], error}`; poll `GET probes/{id}`. A 15-minute cache hit completes immediately.", body = serde_json::Value),
            (status = 403, description = "Requires operate (write), or a `source_ids` entry the caller may not use.", body = crate::problem::Problem),
            (status = 404, description = "Unknown key or source id.", body = crate::problem::Problem),
            (status = 422, description = "`keys` + `source_ids` must hold 1..=50 entries.", body = crate::problem::Problem),
            (status = 429, description = "Per-principal rate (60/min) or the global queue (64) is exhausted.", body = crate::problem::Problem),
            (status = 503, description = "No probe service on this node.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn post_probe(
    State(state): State<AppState>,
    principal: Principal,
    Json(body): Json<ProbeBody>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    let cat = enabled(&state)?;
    let svc = state
        .catalogue_probe
        .as_ref()
        .ok_or_else(|| ControlError::Conflict("no probe service on this node".to_owned()))?;
    let total = body.keys.len() + body.source_ids.len();
    if total == 0 || total > MAX_PROBE_BATCH {
        return Err(ControlError::Validation(format!(
            "`keys` + `source_ids` must hold 1..={MAX_PROBE_BATCH} entries (got {total})"
        )));
    }
    let mut targets = Vec::with_capacity(total);
    if !body.keys.is_empty() {
        let snap = cat
            .snapshot()
            .ok_or_else(|| ControlError::Conflict("no catalogue snapshot yet".to_owned()))?;
        for k in &body.keys {
            let row = snap.row(k).ok_or_else(|| ControlError::NotFound {
                kind: "index-row",
                id: k.clone(),
            })?;
            targets.push(JobTarget {
                k: Some(k.clone()),
                source_id: None,
                request: ProbeRequest {
                    url: row.url.clone(),
                    user_agent: row.ua.clone(),
                    referrer: row.referrer.clone(),
                    quality: row.q.clone(),
                    channel_country: row.country.clone(),
                },
            });
        }
    }
    for id in &body.source_ids {
        // BOLA: authorise the object before revealing whether it exists.
        crate::auth::authorize_object(&principal, id)?;
        let source = super::sources::visible_sources(&state, &principal)?
            .into_iter()
            .find(|s| &s.id == id)
            .ok_or_else(|| ControlError::NotFound {
                kind: "source",
                id: id.clone(),
            })?;
        let url = links::source_url(&source).ok_or_else(|| {
            ControlError::Validation(format!("source {id:?} has no probeable URL"))
        })?;
        let (user_agent, referrer) = links::source_headers(&source);
        let channel_country = source_channel_country(&state, &source);
        targets.push(JobTarget {
            k: None,
            source_id: Some(id.clone()),
            request: ProbeRequest {
                url,
                user_agent,
                referrer,
                quality: None,
                channel_country,
            },
        });
    }
    let job_id = svc.submit_job(&principal.key_id, targets)?;
    let job = svc
        .job(&job_id)
        .ok_or_else(|| ControlError::Conflict("probe job vanished".to_owned()))?;
    let mut r = (StatusCode::ACCEPTED, Json(job)).into_response();
    if let Ok(v) = HeaderValue::from_str(&format!("/api/v1/catalogues/iptv-org/probes/{job_id}")) {
        r.headers_mut().insert(header::LOCATION, v);
    }
    Ok(r)
}

/// `GET /api/v1/catalogues/iptv-org/probes/{id}` — a probe job.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/catalogues/iptv-org/probes/{id}",
        operation_id = "get_catalogue_probe",
        tag = "catalogue",
        params(("id" = String, Path, description = "Probe job id.")),
        responses(
            (status = 200, description = "The probe job and its finished results (verdict, facts, events).", body = serde_json::Value),
            (status = 404, description = "Unknown job.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_probe(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Json<ProbeJob>> {
    principal.role.require(Action::Read)?;
    let svc = state
        .catalogue_probe
        .as_ref()
        .ok_or_else(|| ControlError::Conflict("no probe service on this node".to_owned()))?;
    let not_found = |id: String| ControlError::NotFound { kind: "probe", id };
    // BOLA: a job is visible only to its submitter or an administer-role
    // principal, and only while every linked source is still authorised and
    // visible; every other case is indistinguishable from an unknown id.
    let (owner, source_ids) = svc.job_access(&id).ok_or_else(|| not_found(id.clone()))?;
    if owner != principal.key_id && !principal.role.can(Action::Administer) {
        return Err(not_found(id));
    }
    if !source_ids.is_empty() {
        let visible = super::sources::visible_sources(&state, &principal)?;
        for sid in &source_ids {
            if crate::auth::authorize_object(&principal, sid).is_err()
                || !visible.iter().any(|s| &s.id == sid)
            {
                return Err(not_found(id));
            }
        }
    }
    svc.job(&id).map(Json).ok_or_else(|| not_found(id))
}

/// `POST /api/v1/sources/{id}/network` — the §3a network-path report for any
/// existing source (operate; per-object authorization).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/sources/{id}/network",
        operation_id = "create_source_network_report",
        tag = "catalogue",
        params(("id" = String, Path, description = "Source id.")),
        responses(
            (status = 200, description = "`NetworkReport {egress, hosts, hops, request_headers, assessment, curl}`; userinfo and query strings are redacted.", body = serde_json::Value),
            (status = 403, description = "Requires operate (write), or the source is outside the caller's scope.", body = crate::problem::Problem),
            (status = 404, description = "Unknown source.", body = crate::problem::Problem),
            (status = 409, description = "Diagnostics are not available on this node.", body = crate::problem::Problem),
            (status = 422, description = "The source has no http(s) URL.", body = crate::problem::Problem),
            (status = 429, description = "The probe queue is full.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn post_source_network(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Json<crate::catalogue::netdiag::NetworkReport>> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    let svc = state
        .catalogue_probe
        .as_ref()
        .ok_or_else(|| ControlError::Conflict("no diagnostics service on this node".to_owned()))?;
    let collector = svc.collector().ok_or_else(|| {
        ControlError::Conflict("network diagnostics are not available on this node".to_owned())
    })?;
    let source = super::sources::visible_sources(&state, &principal)?
        .into_iter()
        .find(|s| s.id == id)
        .ok_or(ControlError::NotFound {
            kind: "source",
            id: id.clone(),
        })?;
    let url = links::source_url(&source)
        .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
        .ok_or_else(|| ControlError::Validation("the source has no http(s) URL".to_owned()))?;
    let (user_agent, referrer) = links::source_headers(&source);
    let country = source_channel_country(&state, &source);
    // The same permit pool as the probes: the diagnostics obey the probe bounds.
    let _permit = svc.acquire_for_diagnostics(&principal.key_id).await?;
    let report = collector
        .collect(
            ProbeRequest {
                url,
                user_agent,
                referrer,
                quality: None,
                channel_country: None,
            },
            country,
        )
        .await;
    Ok(Json(report))
}
