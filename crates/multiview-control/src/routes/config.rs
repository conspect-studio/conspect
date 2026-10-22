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
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::audit::AuditAction;
use crate::auth::{Action, Principal};
use crate::concurrency::{IdempotencyKey, Reservation};
use crate::error::{ControlError, ControlResult};
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

/// Mask inline cleartext secrets in a config-revision response for a non-admin.
/// The version store has already returned an owned clone, so this mutates only
/// the response view and never the immutable stored revision.
fn redact_revision_for_read(principal: &Principal, revision: &mut ConfigRevision) {
    crate::support_bundle::redact_inline_secrets_for_read(principal, &mut revision.document);
}

/// `GET /api/v1/config/{target}` — revision history, newest-first (role: read).
pub(crate) async fn list_history(
    State(state): State<AppState>,
    principal: Principal,
    Path(target): Path<String>,
) -> ControlResult<Json<Vec<ConfigRevision>>> {
    principal.role.require(Action::Read)?;
    crate::auth::authorize_object(&principal, &target)?;
    let mut history = state.config_versions.history(&target)?;
    for revision in &mut history {
        redact_revision_for_read(&principal, revision);
    }
    Ok(Json(history))
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
    let mut revision = state
        .config_versions
        .get(&target, RevisionId::new(revision))?;
    redact_revision_for_read(&principal, &mut revision);
    Ok(Json(revision))
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
    let mut response_revision = revision;
    redact_revision_for_read(&principal, &mut response_revision);
    Ok((StatusCode::CREATED, Json(response_revision)))
}

/// `GET /api/v1/config/export` — render the live resource stores as a complete
/// `multiview.toml` document (ADR-W015).
///
/// Composes the working layout (the id-sorted first layout whose body carries a
/// `canvas`) with every stored source/output/overlay/probe into a
/// [`multiview_config::MultiviewConfig`], validates the whole document, and
/// returns it as TOML. This closes the management loop honestly today: edit in
/// the UI → export → persist as the config file → the next start applies it.
///
/// **Secrets are redacted.** The export is outward-facing, so every inline
/// cleartext secret (WebRTC ICE `password`/`static_auth_secret`, WHIP/WHEP/
/// `whip_push` bearer `token`s, and any other secret-class field — see
/// [`crate::support_bundle::redact_config_for_export`]) is replaced with the
/// `<redacted>` placeholder. A `secret_ref` pointer (`op://…`) is preserved (it is
/// not a secret). The placeholder keeps the document re-importable, but it is a
/// **clearly-marked placeholder**: the operator restores the real credential (or
/// confirms the `secret_ref`) before the exported file is used to run — the
/// placeholder never authenticates.
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
    // Per-object visibility (BOLA, ADR-W005/ADR-W025): the export is a
    // whole-system desired-state document — every device, every `device_ref`,
    // every sync-group member id. An object-scoped principal must not obtain it
    // (it would disclose every object outside its allowlist wholesale); the same
    // gate the support bundle uses. Unscoped admin/operator/viewer are
    // unaffected.
    crate::routes::require_unscoped_for_whole_system(&principal)?;

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

/// Compose the running JSON document **with secrets intact** and the validated
/// [`MultiviewConfig`] it deserializes to (ADR-W024 §3).
///
/// This is the durable Running snapshot's source: unlike
/// [`compose_export_document`], it does **not** redact — the persisted
/// `active.toml` must carry the real credentials so a `start = "resume"` reload
/// brings the process back to a working state, and the file's mode is tightened
/// by the atomic writer (ADR-W024 §6). The document is deserialized + validated
/// here (the same whole-document validation the export route applies), so an
/// `active.toml` that exists always round-trips `MultiviewConfig::validate`.
///
/// # Errors
///
/// [`crate::error::ControlError::Validation`] when the composed stores do not
/// deserialize/validate into a configuration; [`crate::error::ControlError::Repository`]
/// for a composition fault.
pub(crate) fn compose_running_config(
    state: &AppState,
) -> ControlResult<(serde_json::Value, multiview_config::MultiviewConfig)> {
    let document = compose_document_unredacted(state)?;
    let config: multiview_config::MultiviewConfig = serde_path_to_error::deserialize(&document)
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
    Ok((document, config))
}

/// Compose the export JSON document: the loaded base configuration (when one
/// was installed) overlaid with everything the live stores own — the working
/// layout's canvas/layout/cells and the source/output/overlay/probe
/// collections. **Secrets are redacted** (the export is outward-facing); for
/// the secrets-intact Running snapshot use [`compose_running_config`].
fn compose_export_document(state: &AppState) -> ControlResult<serde_json::Value> {
    Ok(crate::support_bundle::redact_config_for_export(
        &compose_document_unredacted(state)?,
    ))
}

/// Compose the running JSON document from `base_document` + the live stores,
/// **without** redaction and **without** validation. The shared composition
/// behind the redacted export ([`compose_export_document`]), the secrets-intact
/// Running snapshot ([`compose_running_config`]), and the adopted-state persist
/// (`boot_model::persist_running_now`, which overrides sources/overlays with the
/// adopted snapshot BEFORE validating — the live store can transiently fail
/// whole-document validation, e.g. a cell referencing a just-deleted but still
/// running source).
pub(crate) fn compose_document_unredacted(state: &AppState) -> ControlResult<serde_json::Value> {
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
    // stores do not carry — control, placement, walls, routing,
    // schema_version — survive the round-trip verbatim), then overlay
    // everything the live stores own (including salvos + tally_profiles, which
    // are runtime-mutable through their definition routes — ADR-W024 round 6).
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
    // The store-backed singletons/collections that overlay the base document —
    // `audio`, `salvos`, `tally_profiles` — are composed FROM their stores
    // ([`insert_store_backed_sections`]; extracted to keep this function under
    // the line limit). The whole-document validation below cross-checks routes
    // against the declared sources (the `PUT /api/v1/audio-routing` check
    // intentionally defers).
    insert_store_backed_sections(doc, state)?;
    // The composition is returned with secrets INTACT. The redaction for the
    // outward-facing export happens in `compose_export_document`; the Running
    // snapshot (`compose_running_config`) keeps the real credentials so a
    // resume reload works (the atomic writer tightens the file's mode).
    Ok(document)
}

/// Overlay the runtime-mutable store-backed sections — `audio`, `salvos`,
/// `tally_profiles` — onto the composed document FROM their stores.
///
/// * **`audio`** overlays the `audio` key when an operator (or the seeded
///   config) configured it; otherwise the base document's authored block (if
///   any) is left untouched.
/// * **`salvos` + `tally_profiles`** (ADR-W024 round 6 / F1) are runtime-mutable
///   through their definition routes (`PUT/DELETE /salvos/{id}`, `PUT/DELETE
///   /tally/profiles/{id}`) — a pure control-plane store edit with NO engine
///   command, so the store IS the adopted state. Composing them from the store
///   (not verbatim from the boot base) means a runtime salvo/tally edit
///   round-trips into `active.toml` and survives a `resume` (the stores are
///   seeded from the boot document, so a boot-authored recall is carried too).
///
/// `to_value` on these derived config types has no failing path in practice;
/// the fault is surfaced, never panicked (guardrails forbid `unwrap`).
fn insert_store_backed_sections(
    doc: &mut serde_json::Map<String, serde_json::Value>,
    state: &AppState,
) -> ControlResult<()> {
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
    doc.insert(
        "salvos".to_owned(),
        serde_json::to_value(
            state
                .salvos
                .list()?
                .into_iter()
                .map(|v| v.salvo)
                .collect::<Vec<_>>(),
        )
        .map_err(|e| {
            crate::error::ControlError::Repository(format!("serializing the salvos: {e}"))
        })?,
    );
    doc.insert(
        "tally_profiles".to_owned(),
        serde_json::to_value(
            state
                .tally_profiles
                .list()?
                .into_iter()
                .map(|v| v.profile)
                .collect::<Vec<_>>(),
        )
        .map_err(|e| {
            crate::error::ControlError::Repository(format!("serializing the tally profiles: {e}"))
        })?,
    );
    Ok(())
}

/// `GET /api/v1/config/watch-status` — the config-file watch status
/// (ADR-W020; role: read).
///
/// Reports whether this process watches its boot config file for external
/// edits, the watched path, the last applied/rejected loads, and the
/// restart-pending section names. A store-only deployment (no watcher)
/// honestly reports `active: false`.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/config/watch-status",
        tag = "config",
        responses(
            (status = 200, description = "The config-file watch status: active flag, watched path, last applied/rejected loads, restart-pending sections.", body = crate::watch_status::WatchStatusBody),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to read.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn watch_status(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<crate::watch_status::WatchStatusBody>> {
    principal.role.require(Action::Read)?;
    Ok(Json(state.config_watch.snapshot()))
}

// ===========================================================================
// ADR-W024 — Boot/Loaded/Running config model: revert-to-start, promote,
// boot-model. These three routes wire the EXISTING seams (the moved
// `config_watch::apply_document_diff`, the ADR-W020 `expect_write` suppression
// seam, and `compose_running_config`); see ADR-W024 §5/§6/§7.
// ===========================================================================

/// The `GET /api/v1/config/boot-model` body (ADR-W024 §7): the run's
/// Boot/Loaded/Running model and the per-section divergence the UI indicator
/// shows.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct BootModelBody {
    /// Whether this run carries a boot model (started from a config file with
    /// a control plane). `false` ⇒ every other field is its honest empty
    /// default and the revert/promote actions refuse with `409`.
    pub modeled: bool,
    /// The boot config file path (the watch + promote target).
    pub boot_path: Option<String>,
    /// The `[control] start` cold-start policy (`boot` | `resume`).
    pub start: Option<String>,
    /// Whether this run started from a valid persisted `active.toml`.
    pub resumed: bool,
    /// Why a `start = "resume"` run fell back to boot, if it did.
    pub resume_fallback: Option<String>,
    /// The section names where Running diverges from the **Loaded** startup
    /// snapshot (exact, computed by the pure `ConfigDiff`). Empty ⇒ in sync.
    pub diverged_from_loaded: Vec<String>,
    /// The section names where Running diverges from the **current boot
    /// file** on disk; `null` when the file is unreadable/invalid (see
    /// [`boot_file_error`](Self::boot_file_error)).
    pub diverged_from_boot_file: Option<Vec<String>>,
    /// Why the boot file could not be compared (unreadable / parse /
    /// validation failure), when it could not.
    pub boot_file_error: Option<String>,
    /// The persisted Running state path (`<config-dir>/.multiview/active.toml`).
    pub active_path: Option<String>,
    /// Unix milliseconds of the last successful `active.toml` write this run.
    pub active_written_at_ms: Option<i64>,
}

impl BootModelBody {
    /// The honest body for a run without a boot model.
    fn unmodeled() -> Self {
        Self {
            modeled: false,
            boot_path: None,
            start: None,
            resumed: false,
            resume_fallback: None,
            diverged_from_loaded: Vec::new(),
            diverged_from_boot_file: None,
            boot_file_error: None,
            active_path: None,
            active_written_at_ms: None,
        }
    }
}

/// The section names a [`multiview_config::ConfigDiff`] touches, sorted —
/// the per-section divergence surface (exact and actionable, chosen over an
/// unreliable change count — ADR-W024 §7).
fn diverged_sections(diff: &multiview_config::ConfigDiff) -> Vec<String> {
    let mut sections = std::collections::BTreeSet::new();
    // A per-id content delta OR a pure reorder both diverge `sources` — a reorder
    // is a real (restart-class) delta, not a silent no-op (task #130).
    if !diff.sources.is_empty() || diff.sources_reordered {
        sections.insert("sources");
    }
    // Likewise overlays: a per-id delta lands in `changed_sections` below, but a
    // pure overlay reorder must also surface (it re-blends live via
    // `ReorderOverlays`, yet stays diverged from Loaded until reverted/promoted).
    if diff.overlays_reordered {
        sections.insert("overlays");
    }
    if diff.canvas_signal_changed || diff.canvas_cosmetic_changed {
        sections.insert("canvas");
    }
    if diff.layout_changed {
        sections.insert("layout");
    }
    for section in &diff.changed_sections {
        sections.insert(section);
    }
    sections.into_iter().map(str::to_owned).collect()
}

/// The `[control] start` policy as its wire token.
fn start_token(start: multiview_config::StartMode) -> &'static str {
    match start {
        multiview_config::StartMode::Boot => "boot",
        multiview_config::StartMode::Resume => "resume",
    }
}

/// `GET /api/v1/config/boot-model` — the Boot/Loaded/Running model status
/// (ADR-W024 §7; role: read).
///
/// Reports whether this run carries a boot model, the boot path and start
/// policy, whether the run resumed (+ the fallback reason), the per-section
/// divergence of Running from the Loaded snapshot and from the current boot
/// file, and the last `active.toml` write time.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/config/boot-model",
        tag = "config",
        responses(
            (status = 200, description = "The Boot/Loaded/Running model status with per-section divergence.", body = BootModelBody),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to read.", body = crate::problem::Problem),
            (status = 422, description = "The live stores do not compose into a valid Running document.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn boot_model_status(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<BootModelBody>> {
    principal.role.require(Action::Read)?;
    let Some(model) = state.boot_model.clone() else {
        return Ok(Json(BootModelBody::unmodeled()));
    };
    let (_document, running) = compose_running_config(&state)?;
    let loaded_diff = multiview_config::ConfigDiff::between(&running, model.loaded());
    // The boot FILE may have been hand-edited (or broken) since start; an
    // unreadable/invalid file is reported, never an error — the indicator
    // stays available.
    let (file_divergence, boot_file_error) = match read_validated_config(model.boot_path()).await {
        Ok(boot_file) => {
            let diff = multiview_config::ConfigDiff::between(&running, &boot_file);
            (Some(diverged_sections(&diff)), None)
        }
        Err(reason) => (None, Some(reason)),
    };
    Ok(Json(BootModelBody {
        modeled: true,
        boot_path: Some(model.boot_path().display().to_string()),
        start: Some(start_token(model.start()).to_owned()),
        resumed: model.resumed(),
        resume_fallback: model.resume_fallback().map(str::to_owned),
        diverged_from_loaded: diverged_sections(&loaded_diff),
        diverged_from_boot_file: file_divergence,
        boot_file_error,
        active_path: Some(model.active_path().display().to_string()),
        active_written_at_ms: model.active_written_ms(),
    }))
}

/// Read + parse + validate a config file, with a human-readable reason on any
/// failure (the boot-model status comparison path). The read rides
/// `tokio::fs` so the route handler never parks the control-plane reactor on
/// file I/O (review m1); parse + validate are CPU-cheap and run in place.
async fn read_validated_config(
    path: &std::path::Path,
) -> Result<multiview_config::MultiviewConfig, String> {
    let text = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| format!("the file cannot be read: {e}"))?;
    let config = multiview_config::MultiviewConfig::load_from_toml(&text)
        .map_err(|e| format!("the document does not parse: {e}"))?;
    config
        .validate()
        .map_err(|e| format!("the document does not validate: {e}"))?;
    Ok(config)
}

/// The `POST /api/v1/config/revert-to-start` response (ADR-W024 §5).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct RevertToStartBody {
    /// The operation id this revert is correlated under.
    pub operation_id: String,
    /// `true` when a replayed `Idempotency-Key` answered from the original
    /// reservation (nothing was re-applied; the other fields are this
    /// replay's empty defaults, not a record of the original outcome).
    pub replayed: bool,
    /// Whether the revert FULLY applied (`false` ⇒ either Running already
    /// equalled Loaded and nothing was enqueued, or — when
    /// [`shed`](Self::shed) is non-zero — the revert applied only partially).
    pub reverted: bool,
    /// How many engine commands were shed on a full command bus (review M4):
    /// `0` ⇒ every command landed; non-zero ⇒ the stores were reverted but
    /// the engine may not reflect every change — retry the revert once the
    /// bus drains (the `config-file-apply-incomplete` warning is raised).
    pub shed: u32,
    /// Per-section applied/warned summary parts from the one apply machinery
    /// (e.g. `sources: in_a changed`).
    pub summary: Vec<String>,
    /// The sections that could not hot-revert and re-converge on restart.
    pub restart_only: Vec<String>,
}

/// `POST /api/v1/config/revert-to-start` — Running := Loaded, live
/// (ADR-W024 §5; role: write; `Idempotency-Key`; audited).
///
/// Composes the current Running document, diffs it against the immutable
/// Loaded snapshot, and applies the diff through the ONE ADR-W020
/// diff→apply machinery ([`crate::config_watch::apply_document_diff`]) under
/// the requesting principal: synthetic source changes ride
/// `UpsertSource`/`RemoveSource` on the bounded bus, layout/cells ride the
/// shared resolve+solve+Class-1 gate, stores resync to the Loaded values, and
/// restart-only sections are reported honestly. The ADR-W020 watcher's file
/// baseline is deliberately untouched (it tracks the last applied FILE
/// content). An empty diff applies nothing and reports `reverted: false`.
///
/// This is a distinct operator capability from the config-versioning
/// `POST /config/{target}/rollback` (which restores a prior committed
/// revision of one tracked document): revert-to-start returns the whole
/// Running state to the deliberate cold-start baseline, live (ADR-W024 §1).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/config/revert-to-start",
        tag = "config",
        responses(
            (status = 202, description = "The revert was applied (or was an honest no-op) with a per-section summary.", body = RevertToStartBody),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to write.", body = crate::problem::Problem),
            (status = 409, description = "This run has no boot model (no config file); there is no start state to revert to.", body = crate::problem::Problem),
            (status = 422, description = "The live stores do not compose into a valid Running document.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn revert_to_start(
    State(state): State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
) -> ControlResult<(StatusCode, Json<RevertToStartBody>)> {
    principal.role.require(Action::Write)?;
    // Whole-system authorization runs FIRST — before the boot-model lookup,
    // idempotency reservation, config composition, or any command submit:
    // revert-to-start rewrites the ENTIRE running document across all objects,
    // so a principal scoped on ANY axis is denied outright (ADR-W026, the same
    // guard the config export uses). A no-op for an unscoped operator.
    crate::routes::require_unscoped_for_whole_system(&principal)?;
    let Some(model) = state.boot_model.clone() else {
        return Err(ControlError::Conflict(
            "this run has no boot configuration model (it was not started from a config \
             file), so there is no start state to revert to"
                .to_owned(),
        ));
    };
    let op = match state.idempotency.reserve(idem.0.as_deref()) {
        Reservation::Replay(op) => {
            // The original request already applied (or honestly no-op'd);
            // answer with its id without re-running anything.
            return Ok((
                StatusCode::ACCEPTED,
                Json(RevertToStartBody {
                    operation_id: op.to_string(),
                    replayed: true,
                    reverted: false,
                    shed: 0,
                    summary: Vec::new(),
                    restart_only: Vec::new(),
                }),
            ));
        }
        Reservation::Fresh(op) => op,
    };
    // ADR-W024 MAJOR-C3: hold the same promote/revert mutation serial promote
    // takes, so a revert cannot interleave with a promote's compose→commit (a
    // promote must never commit a document a concurrent revert has already
    // replaced). Compose AFTER acquiring it.
    let _mutation = state.lock_config_mutation().await;
    let (_document, running) = match compose_running_config(&state) {
        Ok(composed) => composed,
        Err(refusal) => {
            // Nothing was applied: release the reservation so a corrected
            // retry with the same key actually runs.
            state.idempotency.release(idem.0.as_deref(), &op);
            return Err(refusal);
        }
    };
    let diff = multiview_config::ConfigDiff::between(&running, model.loaded());
    if diff.is_empty() {
        // Running already equals Loaded. A previously shed revert's warning
        // clears too: any interim edits that re-converged the stores rode the
        // bus themselves, so the engine converged with them.
        clear_revert_incomplete_if_latched(&state, &model);
        return Ok((
            StatusCode::ACCEPTED,
            Json(RevertToStartBody {
                operation_id: op.to_string(),
                replayed: false,
                reverted: false,
                shed: 0,
                summary: Vec::new(),
                restart_only: Vec::new(),
            }),
        ));
    }
    // The ONE apply machinery (ADR-W020/W024), audited under the principal.
    let outcome =
        crate::config_watch::apply_document_diff(&state, &principal.key_id, &diff, model.loaded());
    let restart_only: Vec<String> = outcome.restart.iter().cloned().collect();
    // Review M4: never claim a full revert while engine command(s) were shed
    // on a full bus — and (unlike the file watcher) nothing retries a revert,
    // so a shed revert must apply NOTHING durable: roll the stores back to
    // the pre-revert Running document so a retry's diff(running, loaded) is
    // non-empty again and re-runs the whole (idempotent) revert. Surface it
    // on the same `config-file-apply-incomplete` warning path the watcher
    // uses, with revert-specific remediation; the latch on the boot model
    // lets a later completed revert clear exactly this instance.
    if outcome.shed > 0 {
        crate::config_watch::resync_all_stores(&state, &principal.key_id, &running);
        model.note_revert_incomplete();
        state
            .engine
            .publish_event(multiview_events::Event::HealthWarningRaised(
                revert_incomplete_warning(outcome.shed, state.ack_now().as_nanos(), true),
            ));
        // MAJOR-B round 4: a shed revert applied NOTHING durable (the stores
        // rolled back to the pre-revert Running) — the engine still runs the
        // pre-revert state, which the adopted snapshot already reflects, so the
        // snapshot is left UNCHANGED.
    } else {
        clear_revert_incomplete_if_latched(&state, &model);
        // MAJOR-B round 5: the revert fully landed. The adopted snapshot was
        // already advanced PER-SECTION inside `apply_document_diff` for exactly
        // the sections it applied LIVE (sources via UpsertSource, layout via
        // ApplyLayout) — overlays are restart-only on this path, so the engine
        // keeps the pre-revert overlays and the snapshot's overlays are
        // correctly left unchanged. No wholesale adoption here.
    }
    state.audit(
        &principal.key_id,
        AuditAction::Command,
        "config",
        "revert-to-start",
        Some(serde_json::json!({
            "summary": outcome.parts,
            "restart_only": restart_only,
            "shed": outcome.shed,
        })),
    );
    Ok((
        StatusCode::ACCEPTED,
        Json(RevertToStartBody {
            operation_id: op.to_string(),
            replayed: false,
            reverted: outcome.shed == 0,
            shed: outcome.shed,
            summary: outcome.parts,
            restart_only,
        }),
    ))
}

/// Build the revert-specific `config-file-apply-incomplete` warning (raise
/// and clear share the shape; the warning store coalesces on the code).
fn revert_incomplete_warning(
    shed: u32,
    since: i64,
    active: bool,
) -> multiview_events::HealthWarning {
    multiview_events::HealthWarning {
        code: multiview_events::WarningCode::ConfigFileApplyIncomplete,
        severity: multiview_events::WarningSeverity::Warning,
        subsystem: "config".to_owned(),
        message: format!(
            "revert-to-start applied only PARTIALLY: {shed} engine command(s) were shed on a \
             full command bus; nothing durable was applied (the stores keep the running \
             state)."
        ),
        remediation: "Retry the revert once the command bus drains — a shed revert leaves the \
                      running state untouched, so the retry re-runs the whole revert."
            .to_owned(),
        since,
        active,
    }
}

/// Clear the revert-raised `config-file-apply-incomplete` warning when a
/// revert COMPLETES and a previous shed revert had latched it on the boot
/// model. The latch is revert-scoped: the watcher's own instance of the
/// same warning code is never touched from here.
fn clear_revert_incomplete_if_latched(state: &AppState, model: &crate::boot_model::BootModel) {
    if model.take_revert_incomplete() {
        state
            .engine
            .publish_event(multiview_events::Event::HealthWarningCleared(
                revert_incomplete_warning(0, state.ack_now().as_nanos(), false),
            ));
    }
}

/// The `POST /api/v1/config/promote` response (ADR-W024 §6).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct PromoteBody {
    /// The operation id this promote is correlated under.
    pub operation_id: String,
    /// `true` when a replayed `Idempotency-Key` answered from the original
    /// reservation: the file was NOT rewritten and `path`/`bytes`/`revision`
    /// are absent (the original response carried them).
    pub replayed: bool,
    /// The boot file path the Running document was written to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The number of bytes written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    /// The committed `boot` config revision id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
}

/// `POST /api/v1/config/promote` — write the current Running document to the
/// BOOT file path, server-side (ADR-W024 §6; role: write; `Idempotency-Key`;
/// audited; UI-confirmed).
///
/// Compose → validate → render TOML → announce the write through the
/// installed ADR-W020 watcher's `expect_write` suppression seam (this is
/// the seam's designed caller — the watcher adopts the write as its new
/// baseline without re-applying) → atomic write to the boot path → a
/// config-versioning commit (target `boot`) → audit. With no watcher
/// installed there is nothing to suppress and the step is skipped.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/config/promote",
        tag = "config",
        responses(
            (status = 200, description = "The Running document was written to the boot file and committed as a `boot` config revision.", body = PromoteBody),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to write.", body = crate::problem::Problem),
            (status = 409, description = "This run has no boot model (no config file); there is no boot file to promote to.", body = crate::problem::Problem),
            (status = 422, description = "The live stores do not compose into a valid Running document.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn promote_to_boot(
    State(state): State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
) -> ControlResult<Json<PromoteBody>> {
    principal.role.require(Action::Write)?;
    // Whole-system authorization runs FIRST — before the boot-model lookup,
    // idempotency reservation, config composition, or the boot-file write:
    // promote rewrites the ENTIRE boot document, so a principal scoped on ANY
    // axis is denied outright (ADR-W026). A no-op for an unscoped operator.
    crate::routes::require_unscoped_for_whole_system(&principal)?;
    let Some(model) = state.boot_model.clone() else {
        return Err(ControlError::Conflict(
            "this run has no boot configuration model (it was not started from a config \
             file), so there is no boot file to promote to"
                .to_owned(),
        ));
    };
    let op = match state.idempotency.reserve(idem.0.as_deref()) {
        Reservation::Replay(op) => {
            // The original promote already wrote + committed; a replay must
            // not rewrite the file or commit another revision.
            return Ok(Json(PromoteBody {
                operation_id: op.to_string(),
                replayed: true,
                path: None,
                bytes: None,
                revision: None,
            }));
        }
        Reservation::Fresh(op) => op,
    };
    let release_and = |refusal: ControlError| {
        state.idempotency.release(idem.0.as_deref(), &op);
        refusal
    };
    // ADR-W024 MAJOR-C2/C3: hold the promote/revert mutation serial across the
    // WHOLE compose → bank-token → write → commit critical section. It is
    // composed AFTER acquiring the lock, so a concurrent revert cannot mutate
    // Running between this compose and the commit (C3, a stale commit); and the
    // bank-token + write + confirm happen without a concurrent promote
    // interleaving the watcher's suppression token (C2).
    let _mutation = state.lock_config_mutation().await;
    let (document, running) = compose_running_config(&state).map_err(&release_and)?;
    let toml = running
        .to_toml()
        .map_err(|e| release_and(ControlError::Repository(format!("TOML render failed: {e}"))))?;
    // Announce the server-side write BEFORE writing (ADR-W020 §7 ordering):
    // the watcher adopts the next settled change as its baseline instead of
    // re-applying it. One thin seam, carrying the exact content written.
    let watch_handle = state.watch_handle();
    if let Some(handle) = watch_handle.as_ref() {
        crate::config_watch::expect_server_write(handle, &toml);
    }
    // The write rides the blocking pool under the model's write lock
    // (reviews m1 + M2): it never parks the reactor and never interleaves
    // with another boot-model file write on a deterministic temp name.
    if let Err(error) = crate::boot_model::write_boot_file(Arc::clone(&model), toml.clone()).await {
        // Review B1 (3): the announced content never landed — release the
        // banked token so it cannot eat a later REAL external edit that
        // happens to carry the same content.
        if let Some(handle) = watch_handle.as_ref() {
            let _ = handle.release_write(&toml);
        }
        tracing::warn!(
            path = %model.boot_path().display(),
            error = %error,
            "promote: the boot-file write failed; the announced expect token was \
             released and file watching continues unaffected"
        );
        return Err(release_and(ControlError::Repository(format!(
            "writing the boot file {}: {error}",
            model.boot_path().display()
        ))));
    }
    // The write landed: confirm it (review B1 (2)) so the token is drained
    // if a different settled content supersedes this write before it ever
    // settles — a stale token must never eat a later real edit restoring
    // the same bytes.
    if let Some(handle) = watch_handle.as_ref() {
        crate::config_watch::confirm_server_write(handle, &toml);
    }
    let revision = state
        .config_versions
        .commit(
            "boot",
            document,
            &principal.key_id,
            "promote running configuration to boot",
        )
        .map_err(&release_and)?;
    state.audit(
        &principal.key_id,
        AuditAction::Command,
        "config",
        "promote",
        Some(serde_json::json!({
            "path": model.boot_path().display().to_string(),
            "revision": revision.revision.get(),
        })),
    );
    Ok(Json(PromoteBody {
        operation_id: op.to_string(),
        replayed: false,
        path: Some(model.boot_path().display().to_string()),
        bytes: Some(u64::try_from(toml.len()).unwrap_or(u64::MAX)),
        revision: Some(revision.revision.get()),
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::diverged_sections;

    /// Task #130: a pure REORDER (no per-id content delta) must STILL be named in
    /// the boot-model divergence report — `sources_reordered`/`overlays_reordered`
    /// are real deltas, not silent no-ops. Before the fix `diverged_sections`
    /// keyed only off `diff.sources` non-empty and `changed_sections`, so a pure
    /// reorder reported no divergence (the rule-6 dead-detection the re-review
    /// flagged).
    #[test]
    fn a_pure_source_reorder_is_named_in_divergence() {
        let mut diff = multiview_config::ConfigDiff::default();
        diff.sources_reordered = true;
        let sections = diverged_sections(&diff);
        assert!(
            sections.iter().any(|s| s == "sources"),
            "a source reorder must surface `sources` as diverged; got {sections:?}"
        );
    }

    #[test]
    fn a_pure_overlay_reorder_is_named_in_divergence() {
        let mut diff = multiview_config::ConfigDiff::default();
        diff.overlays_reordered = true;
        let sections = diverged_sections(&diff);
        assert!(
            sections.iter().any(|s| s == "overlays"),
            "an overlay reorder must surface `overlays` as diverged; got {sections:?}"
        );
    }
}
