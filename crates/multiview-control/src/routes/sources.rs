//! The sources resource surface under `/api/v1/sources`.
//!
//! A **source** is a managed input (`multiview_config::Source`): RTSP/HLS/SRT/
//! RTMP/NDI/file/test. This module mirrors the layouts handlers
//! ([`crate::routes`]) over the [`ResourceRepository`](crate::resource_store::ResourceRepository)
//! trait, with `ETag`/`If-Match` optimistic concurrency on every mutation
//! (ADR-W006), RBAC via [`Principal`], and an audit record after each successful
//! write. The stored `body` is the config-as-code document, **validated against
//! `multiview_config::Source` at this boundary** (ADR-W015): an invalid
//! document is rejected with `422 /problems/validation` naming the field path,
//! and every accepted mutation declares its apply semantics via
//! `X-Multiview-Apply`. Errors are RFC 9457 problem documents.
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::audit::AuditAction;
use crate::auth::{Action, Principal};
use crate::command::{Command, OperationId};
use crate::concurrency::IfMatch;
use crate::error::ControlResult;
use crate::resource_store::{Resource, ResourceInput, VersionedResource, SOURCE_KIND};
use crate::state::AppState;
use crate::typed_resources::{validated_body, with_apply, ApplyMode, TypedCollection};

/// Parse a stored (already ADR-W015-validated) source body back into the
/// canonical config type, or `None` for a legacy/foreign document that does
/// not parse (then no live apply is attempted — restart semantics).
fn parse_stored_source(body: &serde_json::Value) -> Option<multiview_config::Source> {
    serde_json::from_value(body.clone()).ok()
}

/// Compare media configuration without cloning arbitrary source documents.
/// Only EPG and IPTV presentation metadata may bypass the engine command path.
fn guide_only_change(previous: &serde_json::Value, next: &serde_json::Value) -> bool {
    let (Some(previous), Some(next)) = (previous.as_object(), next.as_object()) else {
        return false;
    };
    if previous == next {
        // An unchanged body is not a guide edit; it keeps the canonical path.
        return false;
    }
    let media_field = |key: &str| key != "epg" && key != "playlist_metadata" && key != "events";
    // `events.automation` is control-plane only (ADR-0183): arming or editing a
    // rule must never restart the producer. `events.observe` stays media.
    // Effective `observe` (absent block or field => the documented default `true`).
    let observe = |body: &serde_json::Map<String, serde_json::Value>| {
        body.get("events")
            .and_then(|events| events.get("observe"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true)
    };
    observe(previous) == observe(next)
        && previous
            .iter()
            .filter(|(key, _)| media_field(key))
            .eq(next.iter().filter(|(key, _)| media_field(key)))
}

/// Persist an `events.automation` change to one source through the canonical
/// durable path (`commit_resource` → store → audit), never restarting ingest
/// (ADR-0183). `mutate` edits the stored body; returns the new body.
///
/// # Errors
/// Unknown source (404), invalid resulting document (422) or commit failure.
pub(crate) async fn update_source_automation(
    state: &AppState,
    actor: &str,
    id: &str,
    mutate: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>) -> ControlResult<()>,
) -> ControlResult<serde_json::Value> {
    let _mutation = state.lock_config_mutation().await;
    let current = accepted_source(state, id)?;
    let mut body = current.resource.body.clone();
    let object = body.as_object_mut().ok_or_else(|| {
        crate::error::ControlError::Validation("stored source body is not an object".into())
    })?;
    mutate(object)?;
    let input = ResourceInput {
        name: current.resource.name.clone(),
        body: validated_body(TypedCollection::Sources, id, &body)?,
    };
    crate::boot_model::commit_resource(
        state,
        "sources",
        id,
        Some(input.body.clone()),
        Some(input.name.clone()),
    )
    .await?;
    if let Err(error) = state.sources.update(id, input) {
        if state.boot_model.is_none() {
            return Err(error);
        }
        tracing::warn!(source = %id, %error, "accepted automation committed; store projection failed");
    }
    let versioned = accepted_source(state, id)?;
    if let (Some(model), Some(next)) = (
        state.boot_model.as_ref(),
        parse_stored_source(&versioned.resource.body),
    ) {
        if model.adopted_sources().iter().any(|source| source.id == id) {
            model.adopt_source(next);
        }
    }
    // Arming or disarming changes what keeps a source (and its replacement)
    // running under demand. Tell the running engine; on a demand-managed run
    // the hub sees the media definition is unchanged and only re-reads the
    // source's standing leases: the producer is never restarted (review
    // DMD-09). Best-effort: a shed submit applies on restart.
    if let Some(next) = parse_stored_source(&versioned.resource.body) {
        if state.live_sources.is_live(&next.kind) {
            let _ = state.commands.try_submit(Command::UpsertSource {
                op: OperationId::new(),
                source: Box::new(next),
            });
        }
    }
    state.audit(
        actor,
        AuditAction::Update,
        SOURCE_KIND,
        id,
        Some(serde_json::json!({ "events": versioned.resource.body.get("events") })),
    );
    Ok(versioned.resource.body)
}

#[cfg(test)]
mod guide_change_tests {
    use super::guide_only_change;
    use serde_json::json;

    #[test]
    fn guide_changes_do_not_change_media_identity() {
        let source = json!({"id":"news", "kind":"hls", "url":"https://example.com/live"});
        let mut next = source.clone();
        next["epg"] =
            json!({"mode":"manual", "url":"https://example.com/guide", "channel_id":"42"});
        next["playlist_metadata"] = json!({"name":"News"});
        assert!(guide_only_change(&source, &next));
        assert!(guide_only_change(&next, &source));
        next["url"] = json!("https://example.com/other");
        assert!(!guide_only_change(&source, &next));
        let mut armed = source.clone();
        armed["events"] = json!({"automation": {"armed": true,
            "on_break_start": {"action": "swap_cell", "target": "bars"}}});
        assert!(
            guide_only_change(&source, &armed),
            "automation is control-plane only"
        );
        let mut observe = source.clone();
        observe["events"] = json!({"observe": false});
        assert!(
            !guide_only_change(&source, &observe),
            "observe restarts ingest"
        );
    }

    #[test]
    fn other_source_changes_still_require_canonical_apply() {
        let source = json!({"id":"news", "kind":"hls"});
        for key in ["captions", "auth", "display_name", "wallclock"] {
            let mut next = source.clone();
            next[key] = json!("changed");
            assert!(!guide_only_change(&source, &next));
        }
        assert!(!guide_only_change(&json!(null), &json!(null)));
        assert!(!guide_only_change(&source, &source));
    }
}

/// Apply a stored source mutation to the **running** engine where the engine
/// can take it live (ADR-W018, invariant #11), returning the apply semantics
/// the response must declare.
///
/// * A kind the run declared **live-appliable**
///   ([`AppState::live_sources`](crate::LiveSourceCapability) — synthetic kinds
///   on every run; network/file kinds when the run wired a real ingest spawner
///   into its live-source hub) is enqueued as [`Command::UpsertSource`]; the
///   engine drain registers it at a frame boundary → `live`.
/// * A kind change from a live-appliable kind to one the run **cannot** apply
///   live (e.g. `rtsp` → `ndi`) enqueues [`Command::RemoveSource`] — the
///   running producer stops (a stale picture pretending to be the new feed
///   would be dishonest) — but the new document itself applies on `restart`.
/// * Non-live kinds (`ndi`/`youtube`/`aes67`, or network kinds on a run with
///   no decoder), a full/closed bus (no engine draining), or an unparseable
///   stored doc → `restart`, honestly.
///
/// Submission is `try_submit` (bounded, non-blocking, ADR-W008): control can
/// never block on the engine (invariant #10); a shed submit degrades the
/// header to `restart` (the stored doc remains the durable truth).
fn live_apply_upsert(
    state: &AppState,
    source: Option<multiview_config::Source>,
    previous: Option<&multiview_config::Source>,
) -> ApplyMode {
    let Some(source) = source else {
        return ApplyMode::Restart;
    };
    if state.live_sources.is_live(&source.kind) {
        // The running registry, not merely the durable source collection,
        // defines live admission. A stored restart-only source may already be
        // outside the adopted set. Never report `live` for an over-cap add.
        if let Some(model) = state.boot_model.as_ref() {
            let adopted = model.adopted_sources();
            if !adopted.iter().any(|entry| entry.id == source.id)
                && adopted.len() >= multiview_config::MAX_ADMITTED_SOURCES
            {
                return ApplyMode::Restart;
            }
        }
        let submitted = state
            .commands
            .try_submit(Command::UpsertSource {
                op: OperationId::new(),
                source: Box::new(source.clone()),
            })
            .is_ok();
        // MAJOR-B round 4: only a LANDED command is adopted, so only then does
        // the adopted snapshot (what `active.toml` is rendered from) gain this
        // source. A shed keeps the store (ADR-W018 restart) but is NOT adopted,
        // so it stays out of `active.toml` until a restart. The caller holds
        // the config-mutation lock, so this is ordered against the persister.
        if submitted {
            if let Some(model) = state.boot_model.as_ref() {
                model.adopt_source(source);
            }
            return ApplyMode::Live;
        }
        return ApplyMode::Restart;
    }
    if previous.is_some_and(|prev| state.live_sources.is_live(&prev.kind)) {
        // Synthetic -> decoded kind change: stop the running generator now;
        // the stored decoded source applies on restart. A shed submit is
        // surfaced (never silent): the stale generator keeps rendering until
        // restart, and the operator should know why.
        let removed = state.commands.try_submit(Command::RemoveSource {
            op: OperationId::new(),
            id: source.id.clone(),
        });
        if let Err(err) = &removed {
            tracing::warn!(
                source = %source.id,
                error = %err,
                "kind-change RemoveSource shed: the running generator keeps \
                 rendering the old synthetic picture until restart"
            );
        }
        // MAJOR-B round 4: when the stop LANDS the engine no longer runs the
        // old synthetic source, so drop it from the adopted snapshot. The new
        // decoded source is restart-only — NOT adopted — so it never enters the
        // snapshot here.
        if removed.is_ok() {
            if let Some(model) = state.boot_model.as_ref() {
                model.unadopt_source(&source.id);
            }
        }
    }
    ApplyMode::Restart
}

/// Enqueue a live removal of `id` for the running engine, returning the apply
/// semantics the DELETE response must declare (`live` iff enqueued).
fn live_apply_remove(state: &AppState, id: &str) -> ApplyMode {
    let submitted = state
        .commands
        .try_submit(Command::RemoveSource {
            op: OperationId::new(),
            id: id.to_owned(),
        })
        .is_ok();
    // MAJOR-B round 4: only when the removal LANDS does the engine stop running
    // the source, so only then is it dropped from the adopted snapshot. A shed
    // removal keeps the source running, so the snapshot keeps it (active.toml is
    // never told the source is gone while the engine still runs it).
    if submitted {
        if let Some(model) = state.boot_model.as_ref() {
            model.unadopt_source(id);
        }
        ApplyMode::Live
    } else {
        ApplyMode::Restart
    }
}

/// Read accepted intent rather than a lagging, fallible SQLite projection.
pub(crate) fn accepted_sources(state: &AppState) -> ControlResult<Vec<VersionedResource>> {
    crate::boot_model::accepted_resources(state, "sources")?
        .map_or_else(|| state.sources.list(), Ok)
}

/// Whether `id` names an accepted source.
pub(crate) fn accepted_source_exists(state: &AppState, id: &str) -> bool {
    accepted_source(state, id).is_ok()
}

fn accepted_source(state: &AppState, id: &str) -> ControlResult<VersionedResource> {
    accepted_sources(state)?
        .into_iter()
        .find(|entry| entry.resource.id == id)
        .ok_or_else(|| crate::error::ControlError::NotFound {
            kind: SOURCE_KIND,
            id: id.into(),
        })
}

/// Attach the resource's `ETag` to a successful response carrying a source.
fn source_response(status: StatusCode, versioned: &VersionedResource) -> Response {
    let etag = versioned.version.to_etag();
    let mut response = (status, Json(versioned.resource.clone())).into_response();
    if let Ok(value) = header::HeaderValue::from_str(&etag) {
        response.headers_mut().insert(header::ETAG, value);
    }
    response
}

/// `GET /api/v1/sources` — list all sources (role: read).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/sources",
        tag = "sources",
        responses(
            (status = 200, description = "All sources, id-sorted.", body = [crate::resource_store::Resource]),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to read.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn list_sources(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<Vec<Resource>>> {
    principal.role.require(Action::Read)?;
    // Per-object ROW visibility (BOLA, ADR-W005/ADR-W025): a source is gated by
    // its OWN id (`get_source` 403s an out-of-scope id), so the list must drop
    // rows outside the allowlist — exactly as `list_devices` does. THEN redact an
    // embedded out-of-scope `device_ref` on a surviving in-scope row (its
    // device-projection link may point to a device the principal cannot `GET`).
    // Both are no-ops for an unscoped principal.
    let sources = crate::boot_model::accepted_resources(&state, "sources")?
        .map_or_else(|| state.sources.list(), Ok)?
        .into_iter()
        .filter(|v| crate::auth::authorize_object(&principal, &v.resource.id).is_ok())
        .map(|v| {
            let mut resource = v.resource;
            crate::routes::redact_out_of_scope_device_refs(&principal, &mut resource);
            crate::support_bundle::redact_inline_secrets_for_read(&principal, &mut resource.body);
            resource
        })
        .collect();
    Ok(Json(sources))
}

/// `GET /api/v1/sources/{id}` — fetch one source (role: read; per-object authz).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/sources/{id}",
        tag = "sources",
        params(("id" = String, Path, description = "Source id.")),
        responses(
            (status = 200, description = "The source (ETag in the response header).", body = crate::resource_store::Resource),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to read this source.", body = crate::problem::Problem),
            (status = 404, description = "No source with that id.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_source(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Read)?;
    crate::auth::authorize_object(&principal, &id)?;
    let mut versioned = accepted_source(&state, &id)?;
    // Redact an embedded out-of-scope `device_ref` (BOLA visibility,
    // ADR-W005/ADR-W025): the source itself is in scope (authorized above), but
    // its device-projection link must not leak a device id the principal could
    // not `GET`. No-op when unscoped.
    crate::routes::redact_out_of_scope_device_refs(&principal, &mut versioned.resource);
    crate::support_bundle::redact_inline_secrets_for_read(&principal, &mut versioned.resource.body);
    Ok(source_response(StatusCode::OK, &versioned))
}

/// `POST /api/v1/sources/{id}` — create a source (role: write; per-object authz).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/sources/{id}",
        tag = "sources",
        params(("id" = String, Path, description = "Source id.")),
        request_body = crate::openapi_schemas::SourceResourceInputDoc,
        responses(
            (status = 201, description = "The created source (ETag in the response header). X-Multiview-Apply declares how it takes effect: `live` when the running engine applies it at a frame boundary — synthetic kinds (bars/solid/clock) on every run, network/file kinds (rtsp/hls/ts/srt/rtmp/rist/file) on a full-engine run — `restart` otherwise (ndi/youtube/aes67, or a run without the decoder; ADR-W018).", body = crate::resource_store::Resource),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to write.", body = crate::problem::Problem),
            (status = 422, description = "The body is not a valid source document (detail names the field path).", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn create_source(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
    Json(input): Json<ResourceInput>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    let input = ResourceInput {
        name: input.name,
        body: validated_body(TypedCollection::Sources, &id, &input.body)?,
    };
    // ADR-W024 MAJOR-B round 4: hold the config-mutation lock across the whole
    // store-write → submit → adopt-snapshot sequence, so the persister (which
    // takes the same lock) can never compose mid-mutation and the adopted
    // snapshot advances atomically with the engine submit.
    let _mutation = state.lock_config_mutation().await;
    if accepted_sources(&state)?.len() >= multiview_config::MAX_ADMITTED_SOURCES {
        return Err(crate::error::ControlError::Validation(format!(
            "sources exceeds the live source capacity of {}",
            multiview_config::MAX_ADMITTED_SOURCES
        )));
    }
    if accepted_sources(&state)?
        .iter()
        .any(|entry| entry.resource.id == id)
    {
        return Err(crate::error::ControlError::Validation(format!(
            "source {id:?} already exists"
        )));
    }
    crate::boot_model::commit_resource(
        &state,
        "sources",
        &id,
        Some(input.body.clone()),
        Some(input.name.clone()),
    )
    .await?;
    if let Err(error) = state.sources.create(&id, input) {
        if state.boot_model.is_none() {
            return Err(error);
        }
        tracing::warn!(source = %id, %error, "accepted source committed; store projection failed");
    }
    let versioned = accepted_source(&state, &id).map_err(|error| {
        crate::error::ControlError::CommitIndeterminate(format!(
            "source {id:?} was committed, but its accepted representation could not be reread; GET before retrying: {error}"
        ))
    })?;
    state.audit(
        &principal.key_id,
        AuditAction::Create,
        SOURCE_KIND,
        &id,
        Some(versioned.resource.body.clone()),
    );
    let mode = live_apply_upsert(&state, parse_stored_source(&versioned.resource.body), None);
    Ok(with_apply(
        mode,
        source_response(StatusCode::CREATED, &versioned),
    ))
}

/// `PUT /api/v1/sources/{id}` — replace a source (role: write; If-Match → 412).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        put,
        path = "/api/v1/sources/{id}",
        tag = "sources",
        params(("id" = String, Path, description = "Source id.")),
        request_body = crate::openapi_schemas::SourceResourceInputDoc,
        responses(
            (status = 200, description = "The replaced source (new ETag in the response header). X-Multiview-Apply declares how it takes effect: `live` when the running engine applies it at a frame boundary (synthetic kinds on every run; network/file kinds on a full-engine run — an edit swaps the producer behind the same tile store), `restart` otherwise (ADR-W018).", body = crate::resource_store::Resource),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to write.", body = crate::problem::Problem),
            (status = 404, description = "No source with that id.", body = crate::problem::Problem),
            (status = 412, description = "If-Match precondition failed.", body = crate::problem::Problem),
            (status = 422, description = "The body is not a valid source document (detail names the field path).", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn update_source(
    State(state): State<AppState>,
    principal: Principal,
    if_match: IfMatch,
    Path(id): Path<String>,
    Json(input): Json<ResourceInput>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    // ADR-W024 MAJOR-B round 4: hold the config-mutation lock across the whole
    // precondition → store-write → submit → adopt-snapshot sequence.
    let _mutation = state.lock_config_mutation().await;
    // Preconditions are evaluated before request content (RFC 9110 §13.2.2):
    // a stale `If-Match` (or a missing resource) is reported even when the
    // submitted body is itself invalid.
    let current = accepted_source(&state, &id)?;
    if_match.require(SOURCE_KIND, &id, current.version)?;
    let input = ResourceInput {
        name: input.name,
        body: validated_body(TypedCollection::Sources, &id, &input.body)?,
    };
    let previous = parse_stored_source(&current.resource.body);
    crate::boot_model::commit_resource(
        &state,
        "sources",
        &id,
        Some(input.body.clone()),
        Some(input.name.clone()),
    )
    .await?;
    if let Err(error) = state.sources.update(&id, input) {
        if state.boot_model.is_none() {
            return Err(error);
        }
        tracing::warn!(source = %id, %error, "accepted source committed; store projection failed");
    }
    let versioned = accepted_source(&state, &id).map_err(|error| {
        crate::error::ControlError::CommitIndeterminate(format!(
            "source {id:?} was committed, but its accepted representation could not be reread; GET before retrying: {error}"
        ))
    })?;
    state.audit(
        &principal.key_id,
        AuditAction::Update,
        SOURCE_KIND,
        &id,
        Some(versioned.resource.body.clone()),
    );
    let next = parse_stored_source(&versioned.resource.body);
    let mode = if guide_only_change(&current.resource.body, &versioned.resource.body) {
        // Guide/presentation edits must not restart a decoder. Preserve the
        // adopted media identity, but carry its new metadata into resume state.
        if let (Some(model), Some(next)) = (state.boot_model.as_ref(), next.as_ref()) {
            if model.adopted_sources().iter().any(|source| source.id == id) {
                model.adopt_source(next.clone());
            }
        }
        ApplyMode::Live
    } else {
        live_apply_upsert(&state, next, previous.as_ref())
    };
    Ok(with_apply(
        mode,
        source_response(StatusCode::OK, &versioned),
    ))
}

/// `DELETE /api/v1/sources/{id}` — delete a source (role: administer; If-Match).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        delete,
        path = "/api/v1/sources/{id}",
        tag = "sources",
        params(("id" = String, Path, description = "Source id.")),
        responses(
            (status = 204, description = "The source was deleted. X-Multiview-Apply: `live` when the running engine unregisters it at a frame boundary (bound tiles ride their failover slate), `restart` when no engine is draining (ADR-W018)."),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to administer.", body = crate::problem::Problem),
            (status = 404, description = "No source with that id.", body = crate::problem::Problem),
            (status = 412, description = "If-Match precondition failed.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn delete_source(
    State(state): State<AppState>,
    principal: Principal,
    if_match: IfMatch,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Administer)?;
    crate::auth::authorize_object(&principal, &id)?;
    // ADR-W024 MAJOR-B round 4: hold the config-mutation lock across the whole
    // precondition → store-delete → submit → adopt-snapshot sequence.
    let _mutation = state.lock_config_mutation().await;
    let current = accepted_source(&state, &id)?;
    if_match.require(SOURCE_KIND, &id, current.version)?;
    crate::boot_model::commit_resource(&state, "sources", &id, None, None).await?;
    if let Err(error) = state.sources.delete(&id) {
        if state.boot_model.is_none() {
            return Err(error);
        }
        tracing::warn!(source = %id, %error, "accepted source deletion committed; store projection failed");
    }
    state.audit(
        &principal.key_id,
        AuditAction::Delete,
        SOURCE_KIND,
        &id,
        None,
    );
    let mode = live_apply_remove(&state, &id);
    Ok(with_apply(mode, StatusCode::NO_CONTENT.into_response()))
}

/// Every source's lifecycle state (role: read; per-object row visibility).
///
/// `idle` means the source is not started, so it costs nothing; it is not a
/// fault. An `always_on` source (the default) is started at boot; an
/// `on_demand` source starts when something consumes it. It reads `starting`,
/// then `ready`, or `failed` while it retries with backoff.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/sources/status",
        tag = "sources",
        responses(
            (status = 200, description = "Every configured source's demand state, id-sorted.", body = [crate::source_demand::SourceDemandView]),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to read.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn list_source_status(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<Vec<crate::source_demand::SourceDemandView>>> {
    principal.role.require(Action::Read)?;
    let sources: Vec<(String, crate::source_demand::SourceActivationView)> =
        accepted_sources(&state)?
            .into_iter()
            .filter(|v| crate::auth::authorize_object(&principal, &v.resource.id).is_ok())
            .map(|v| (v.resource.id.clone(), activation_of(&v.resource)))
            .collect();
    Ok(Json(source_status_views(&state, &principal, sources)))
}

/// One source's demand state (role: read; per-object authz).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/sources/{id}/status",
        tag = "sources",
        params(("id" = String, Path, description = "Source id.")),
        responses(
            (status = 200, description = "The source's demand state.", body = crate::source_demand::SourceDemandView),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to read this source.", body = crate::problem::Problem),
            (status = 404, description = "No such source.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_source_status(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Json<crate::source_demand::SourceDemandView>> {
    principal.role.require(Action::Read)?;
    crate::auth::authorize_object(&principal, &id)?;
    let source = accepted_source(&state, &id)?;
    let activation = activation_of(&source.resource);
    Ok(Json(
        source_status_views(&state, &principal, vec![(id, activation)])
            .pop()
            .unwrap_or_else(|| crate::source_demand::SourceDemandView::idle("")),
    ))
}

/// The activation a stored source document declares. Reads only the two
/// keys, so a document that fails full parsing still reports its setting.
fn activation_of(
    resource: &crate::resource_store::Resource,
) -> crate::source_demand::SourceActivationView {
    #[derive(serde::Deserialize)]
    struct Keys {
        #[serde(default)]
        activation: Option<multiview_config::SourceActivation>,
        #[serde(default)]
        demand: Option<LegacyDemand>,
    }
    #[derive(serde::Deserialize)]
    struct LegacyDemand {
        #[serde(default)]
        always_on: Option<bool>,
    }
    let keys: Option<Keys> = serde_json::from_value(resource.body.clone()).ok();
    match keys {
        Some(Keys {
            activation: Some(activation),
            ..
        }) => activation.into(),
        Some(Keys {
            demand: Some(LegacyDemand {
                always_on: Some(false),
            }),
            ..
        }) => crate::source_demand::SourceActivationView::OnDemand,
        _ => crate::source_demand::SourceActivationView::AlwaysOn,
    }
}

/// Start a source now (role: write; per-object authz; audited).
///
/// Holds the source `ready` until Stop, whatever its activation. Never waits
/// for the source: the response is the state right after the request
/// (normally `starting`).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/sources/{id}/start",
        tag = "sources",
        params(("id" = String, Path, description = "Source id.")),
        responses(
            (status = 202, description = "Start requested; the body is the source's state.", body = crate::source_demand::SourceDemandView),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to operate this source.", body = crate::problem::Problem),
            (status = 404, description = "No such source.", body = crate::problem::Problem),
            (status = 409, description = "This run cannot start sources.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn start_source(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    let source = accepted_source(&state, &id)?;
    let started = state
        .source_demand
        .as_ref()
        .is_some_and(|demand| demand.start_source(&id));
    if !started {
        return Err(crate::error::ControlError::Conflict(
            "this run cannot start sources on request".to_owned(),
        ));
    }
    state.audit(
        &principal.key_id,
        AuditAction::Command,
        SOURCE_KIND,
        &id,
        Some(serde_json::json!({"command": "start"})),
    );
    let activation = activation_of(&source.resource);
    let view = source_status_views(&state, &principal, vec![(id, activation)])
        .pop()
        .unwrap_or_else(|| crate::source_demand::SourceDemandView::idle(""));
    Ok((StatusCode::ACCEPTED, Json(view)).into_response())
}

/// Stop a source now (role: write; per-object authz; audited).
///
/// Refused with 409 while anything else uses the source; the problem detail
/// names every consumer (an `always_on` setting counts). Set the source
/// `on_demand` to let it stop.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/sources/{id}/stop",
        tag = "sources",
        params(("id" = String, Path, description = "Source id.")),
        responses(
            (status = 202, description = "Stop requested; the body is the source's state.", body = crate::source_demand::SourceDemandView),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to operate this source.", body = crate::problem::Problem),
            (status = 404, description = "No such source.", body = crate::problem::Problem),
            (status = 409, description = "Still in use; the detail names every consumer.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn stop_source(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    let source = accepted_source(&state, &id)?;
    let outcome = state.source_demand.as_ref().map_or(
        Err(crate::source_demand::StopRefusal::Unsupported),
        |demand| demand.stop_source(&id),
    );
    match outcome {
        Ok(()) => {}
        Err(crate::source_demand::StopRefusal::InUse(mut consumers)) => {
            redact_holders(&principal, &mut consumers);
            let names: Vec<String> = consumers
                .iter()
                .map(|c| format!("{} {}", c.kind, c.label))
                .collect();
            return Err(crate::error::ControlError::Conflict(format!(
                "source {id:?} is in use by: {}",
                names.join(", ")
            )));
        }
        Err(crate::source_demand::StopRefusal::Unsupported) => {
            return Err(crate::error::ControlError::Conflict(
                "this run cannot stop sources on request".to_owned(),
            ));
        }
    }
    state.audit(
        &principal.key_id,
        AuditAction::Command,
        SOURCE_KIND,
        &id,
        Some(serde_json::json!({"command": "stop"})),
    );
    let activation = activation_of(&source.resource);
    let view = source_status_views(&state, &principal, vec![(id, activation)])
        .pop()
        .unwrap_or_else(|| crate::source_demand::SourceDemandView::idle(""));
    Ok((StatusCode::ACCEPTED, Json(view)).into_response())
}

/// The demand views of `ids`, in id order. Without a demand provider (a run
/// path with no manager) the status is unknown, reported idle with no consumers.
/// Hide every holder label that names an object `principal` may not read
/// (review DMD-04): the source row is authorized, but an automation holder
/// names its owning source, which is authorized on its own.
fn redact_holders(
    principal: &Principal,
    consumers: &mut [crate::source_demand::SourceConsumerView],
) {
    for consumer in consumers {
        consumer.redact_unless(|object| crate::auth::authorize_object(principal, object).is_ok());
    }
}

fn source_status_views(
    state: &AppState,
    principal: &Principal,
    mut sources: Vec<(String, crate::source_demand::SourceActivationView)>,
) -> Vec<crate::source_demand::SourceDemandView> {
    sources.sort_by(|a, b| a.0.cmp(&b.0));
    sources.dedup_by(|a, b| a.0 == b.0);
    let active: std::collections::HashMap<String, crate::source_demand::SourceDemandView> = state
        .source_demand
        .as_ref()
        .map(|demand| {
            demand
                .active()
                .into_iter()
                .map(|view| (view.id.clone(), view))
                .collect()
        })
        .unwrap_or_default();
    sources
        .into_iter()
        .map(|(id, activation)| {
            let mut view = active
                .get(&id)
                .cloned()
                .unwrap_or_else(|| crate::source_demand::SourceDemandView::idle(&id));
            view.activation = activation;
            redact_holders(principal, &mut view.consumers);
            view
        })
        .collect()
}
