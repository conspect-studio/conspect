//! Config-versioning routes under `/api/v1/config/{target}`.
//!
//! A config/layout document is tracked as an append-only sequence of immutable
//! revisions per `target` key (see [`crate::versioning`]). The routes expose:
//!
//! * `GET  /config/{target}` — the revision history (newest-first, role: read).
//! * `GET  /config/{target}/rev/{revision}` — one immutable revision (role:
//!   read). A distinct `rev/` prefix avoids any routing ambiguity with the
//!   sibling literal `diff`/`rollback` segments.
//! * `PUT  /config/{target}` — commit a new revision (role: write).
//! * `GET  /config/{target}/diff?from=&to=` — structural diff (role: read).
//! * `POST /config/{target}/rollback` — append a revision restoring a prior one
//!   (role: write).
//!
//! Every successful commit/rollback is recorded in the change audit log.
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;

use crate::audit::AuditAction;
use crate::auth::{Action, Principal};
use crate::error::ControlResult;
use crate::state::AppState;
use crate::versioning::{ConfigRevision, DocumentDiff, RevisionId, CONFIG_REVISION_KIND};

/// The body of a `PUT /config/{target}` commit.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "openapi", derive(serde::Serialize, utoipa::ToSchema))]
pub struct CommitRequest {
    /// The document to commit as the next revision.
    pub document: serde_json::Value,
    /// A short commit message.
    #[serde(default)]
    pub message: String,
}

/// The body of a `POST /config/{target}/rollback`.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "openapi", derive(serde::Serialize, utoipa::ToSchema))]
pub struct RollbackRequest {
    /// The revision id to restore (a new revision is appended).
    pub to: u64,
}

/// Query parameters for the revision diff.
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::IntoParams))]
pub struct DiffQuery {
    /// The base revision.
    pub from: u64,
    /// The target revision.
    pub to: u64,
}

/// `GET /api/v1/config/{target}` — revision history, newest-first (role: read).
pub(crate) async fn list_history(
    State(state): State<AppState>,
    principal: Principal,
    Path(target): Path<String>,
) -> ControlResult<Json<Vec<ConfigRevision>>> {
    principal.role.require(Action::Read)?;
    crate::auth::authorize_object(&principal, &target)?;
    Ok(Json(state.config_versions.history(&target)?))
}

/// `GET /api/v1/config/{target}/diff?from=&to=` — structural diff (role: read).
pub(crate) async fn diff_revisions(
    State(state): State<AppState>,
    principal: Principal,
    Path(target): Path<String>,
    Query(query): Query<DiffQuery>,
) -> ControlResult<Json<DocumentDiff>> {
    principal.role.require(Action::Read)?;
    crate::auth::authorize_object(&principal, &target)?;
    let diff = state.config_versions.diff(
        &target,
        RevisionId::new(query.from),
        RevisionId::new(query.to),
    )?;
    Ok(Json(diff))
}

/// `GET /api/v1/config/{target}/{revision}` — one revision (role: read).
pub(crate) async fn get_revision(
    State(state): State<AppState>,
    principal: Principal,
    Path((target, revision)): Path<(String, u64)>,
) -> ControlResult<Json<ConfigRevision>> {
    principal.role.require(Action::Read)?;
    crate::auth::authorize_object(&principal, &target)?;
    Ok(Json(
        state
            .config_versions
            .get(&target, RevisionId::new(revision))?,
    ))
}

/// `PUT /api/v1/config/{target}` — commit a new revision (role: write).
pub(crate) async fn commit_revision(
    State(state): State<AppState>,
    principal: Principal,
    Path(target): Path<String>,
    Json(req): Json<CommitRequest>,
) -> ControlResult<(StatusCode, Json<ConfigRevision>)> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &target)?;
    let revision =
        state
            .config_versions
            .commit(&target, req.document, &principal.key_id, &req.message)?;
    state.audit(
        &principal.key_id,
        AuditAction::Update,
        CONFIG_REVISION_KIND,
        &target,
        Some(serde_json::json!({ "revision": revision.revision.get() })),
    );
    Ok((StatusCode::CREATED, Json(revision)))
}

/// `POST /api/v1/config/{target}/rollback` — restore a prior revision as a new
/// revision (role: write).
pub(crate) async fn rollback_revision(
    State(state): State<AppState>,
    principal: Principal,
    Path(target): Path<String>,
    Json(req): Json<RollbackRequest>,
) -> ControlResult<(StatusCode, Json<ConfigRevision>)> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &target)?;
    let revision =
        state
            .config_versions
            .rollback(&target, RevisionId::new(req.to), &principal.key_id)?;
    state.audit(
        &principal.key_id,
        AuditAction::Rollback,
        CONFIG_REVISION_KIND,
        &target,
        Some(serde_json::json!({ "restored_to": req.to, "new_revision": revision.revision.get() })),
    );
    Ok((StatusCode::CREATED, Json(revision)))
}

/// `GET /api/v1/config/export` — render the live resource stores as a complete
/// `multiview.toml` document (ADR-W015).
///
/// Composes the working layout (the id-sorted first layout whose body carries a
/// `canvas`) with every stored source/output/overlay/probe into a
/// [`multiview_config::MultiviewConfig`], validates the whole document, and
/// returns it as TOML. This closes the management loop honestly today: edit in
/// the UI → export → persist as the config file → the next start applies it.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/config/export",
        tag = "config",
        responses(
            (status = 200, description = "The composed configuration as TOML (`application/toml`).", body = String),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to read.", body = crate::problem::Problem),
            (status = 422, description = "The stores do not compose into a valid configuration (detail names the violation).", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn export_config(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<axum::response::Response> {
    use axum::response::IntoResponse;

    principal.role.require(Action::Read)?;

    let document = compose_export_document(&state)?;
    let config: multiview_config::MultiviewConfig = serde_path_to_error::deserialize(document)
        .map_err(|err| {
            let path = err.path().to_string();
            crate::error::ControlError::Validation(format!(
                "stored resources do not compose into a valid configuration at `{path}`: {}",
                err.into_inner()
            ))
        })?;
    config.validate().map_err(|err| {
        crate::error::ControlError::Validation(format!(
            "composed configuration failed validation: {err}"
        ))
    })?;
    let toml = config.to_toml().map_err(|err| {
        crate::error::ControlError::Repository(format!("TOML render failed: {err}"))
    })?;

    Ok((
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, "application/toml"),
            (
                axum::http::header::CONTENT_DISPOSITION,
                "attachment; filename=\"multiview.toml\"",
            ),
        ],
        toml,
    )
        .into_response())
}

/// Compose the export JSON document: the loaded base configuration (when one
/// was installed) overlaid with everything the live stores own — the working
/// layout's canvas/layout/cells and the source/output/overlay/probe
/// collections.
fn compose_export_document(state: &AppState) -> ControlResult<serde_json::Value> {
    // The working layout carries { canvas, layout, cells }. Prefer the
    // designated working layout (set at seed time); fall back to the id-sorted
    // first layout document that declares a canvas (store-only deployments).
    let layouts = state.repository.list_layouts()?;
    let working = state
        .working_layout_id
        .as_deref()
        .and_then(|id| {
            layouts
                .iter()
                .map(|v| &v.layout)
                .find(|layout| layout.id == id && layout.body.get("canvas").is_some())
        })
        .or_else(|| {
            layouts
                .iter()
                .map(|v| &v.layout)
                .find(|layout| layout.body.get("canvas").is_some())
        })
        .ok_or_else(|| {
            crate::error::ControlError::Validation(
                "no working layout (a layout body carrying `canvas`) to export".to_owned(),
            )
        })?;

    let collect = |repo: &std::sync::Arc<dyn crate::resource_store::ResourceRepository>|
     -> ControlResult<Vec<serde_json::Value>> {
        Ok(repo
            .list()?
            .into_iter()
            .map(|v| v.resource.body)
            .collect())
    };

    // Start from the loaded configuration document (so authored sections the
    // stores do not carry — control, placement, audio, salvos, tally
    // profiles, walls, routing — survive the round-trip verbatim), then
    // overlay everything the live stores own.
    let mut document = state
        .base_document
        .as_deref()
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let Some(doc) = document.as_object_mut() else {
        return Err(crate::error::ControlError::Repository(
            "the export base document is not a JSON object".to_owned(),
        ));
    };
    doc.entry("schema_version").or_insert(serde_json::json!(1));
    doc.insert(
        "canvas".to_owned(),
        working
            .body
            .get("canvas")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
    );
    doc.insert(
        "layout".to_owned(),
        working
            .body
            .get("layout")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
    );
    doc.insert(
        "cells".to_owned(),
        working
            .body
            .get("cells")
            .cloned()
            .unwrap_or_else(|| serde_json::json!([])),
    );
    doc.insert(
        "sources".to_owned(),
        serde_json::Value::Array(collect(&state.sources)?),
    );
    doc.insert(
        "outputs".to_owned(),
        serde_json::Value::Array(collect(&state.outputs)?),
    );
    doc.insert(
        "overlays".to_owned(),
        serde_json::Value::Array(collect(&state.overlays)?),
    );
    doc.insert(
        "probes".to_owned(),
        serde_json::Value::Array(collect(&state.probes)?),
    );
    // Managed devices + sync groups (ADR-M008): config-as-code is the durable
    // source, so a device or sync group adopted at runtime via the API must
    // round-trip through the export exactly as sources/outputs do. The runtime
    // `device_status` registry is a separate `Arc` that is never collected
    // here, so live status never leaks into the desired-state document.
    doc.insert(
        "devices".to_owned(),
        serde_json::Value::Array(collect(&state.devices)?),
    );
    doc.insert(
        "sync_groups".to_owned(),
        serde_json::Value::Array(collect(&state.sync_groups)?),
    );
    // The audio-routing singleton overlays the `audio` key when an operator
    // (or the seeded config) configured it; otherwise the base document's
    // authored block — if any — is left untouched. The whole-document
    // validation below this composition is where routes are cross-checked
    // against the declared sources (the check `PUT /api/v1/audio-routing`
    // intentionally defers).
    let (audio, _) = state.audio_routing.snapshot();
    if let Some(routing) = audio {
        doc.insert(
            "audio".to_owned(),
            serde_json::to_value(&routing).map_err(|e| {
                crate::error::ControlError::Repository(format!(
                    "serializing the audio-routing document: {e}"
                ))
            })?,
        );
    }
    Ok(document)
}
