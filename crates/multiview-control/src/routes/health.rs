//! The health-warning REST surface under `/api/v1/health` (ADR-0035 SA-0).
//!
//! One read-only endpoint exposes the control plane's engine-fed health-warning
//! mirror:
//!
//! * `GET /api/v1/health` — list the active health warnings (e.g. "GPU present
//!   but compositing fell back to CPU; here is the fix"). `?active=false` widens
//!   to include cleared warnings. Role: read.
//!
//! * `POST /api/v1/health/{code}/ack` — acknowledge a persisted warning
//!   (#324). Only `accepted-state-rejected` is acknowledgeable: it is the one
//!   warning backed by a file (`.multiview/boot-fallback.json`) that keeps it
//!   raised across restarts; every other warning clears itself when its cause
//!   goes away. Role: write.
//!
//! This is **not** an extension of `/livez`/`/readyz` (ADR-R009: a capability
//! warning must not flip liveness and restart-loop the container). It is a
//! dedicated read-only surface modelled on [`crate::routes::alarms::list_alarms`].
//! Errors are RFC 9457 problem documents.
//!
//! The store this reads is fed by the read-only, lagged-skip engine subscription
//! ([`crate::warning_ingest`]); nothing here is on the engine's data plane
//! (invariant #10).
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use multiview_events::{HealthWarning, WarningCode};
use serde::Deserialize;

use crate::auth::{Action, Principal};
use crate::error::{ControlError, ControlResult};
use crate::state::AppState;
use crate::warning_store::WarningFilter;

/// The query parameters accepted by `GET /api/v1/health`.
///
/// `active` defaults to `true` (the operator wants the warnings that are
/// currently firing); pass `?active=false` to include cleared/historical ones.
#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::IntoParams))]
pub struct HealthQuery {
    /// Keep only active (`true`, the default) or include cleared (`false`)
    /// warnings.
    #[serde(default)]
    pub active: Option<bool>,
}

impl HealthQuery {
    /// Translate the query into a repository [`WarningFilter`], defaulting to
    /// active-only when the operator did not specify.
    #[must_use]
    pub fn into_filter(self) -> WarningFilter {
        WarningFilter {
            active: Some(self.active.unwrap_or(true)),
        }
    }
}

/// `GET /api/v1/health` — list the active health warnings (role: read).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/health",
        tag = "health",
        params(HealthQuery),
        responses(
            (status = 200, description = "Active health warnings, code-sorted.", body = [crate::openapi_schemas::HealthWarningDoc]),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to read.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn list_health(
    State(state): State<AppState>,
    principal: Principal,
    Query(query): Query<HealthQuery>,
) -> ControlResult<Json<Vec<HealthWarning>>> {
    principal.role.require(Action::Read)?;
    let filter = query.into_filter();
    let warnings = state.warnings.list(&filter)?;
    Ok(Json(warnings))
}

/// The problem `kind` a missing/unacknowledgeable warning is reported under.
const HEALTH_WARNING_KIND: &str = "health-warning";

/// `POST /api/v1/health/{code}/ack` — acknowledge the persisted
/// `accepted-state-rejected` warning (role: write, unscoped; #324).
///
/// Removes the start-up fallback marker (on the blocking pool, under the
/// config-mutation lock — file I/O never runs on a runtime worker, and never on
/// the engine), then publishes `health.warning.cleared` through the
/// drop-oldest lane (inv #10) and waits briefly for the warning ingest to apply
/// it. Acknowledging silences the CURRENT alarm only: a later start that still
/// cannot use the saved configuration raises it again. The saved configuration
/// is not touched. An unreadable or unparsable marker still counts as pending
/// and is removed.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/health/{code}/ack",
        tag = "health",
        params(("code" = String, Path, description = "Warning code to acknowledge; only `accepted-state-rejected` is acknowledgeable.")),
        responses(
            (status = 204, description = "Acknowledged: the persisted marker is removed and the warning cleared."),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to acknowledge (read-only role, or a scoped key: this is a whole-system action).", body = crate::problem::Problem),
            (status = 404, description = "Not an acknowledgeable code, no config-file run, or nothing pending.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn ack_health_warning(
    State(state): State<AppState>,
    principal: Principal,
    Path(code): Path<String>,
) -> ControlResult<StatusCode> {
    principal.role.require(Action::Write)?;
    // A system-wide alarm about the whole saved configuration: a scoped
    // principal must not silence it (BOLA — it is outside any allowlist).
    crate::routes::require_unscoped_for_whole_system(&principal)?;
    let not_found = || ControlError::NotFound {
        kind: HEALTH_WARNING_KIND,
        id: code.clone(),
    };
    if code != WarningCode::AcceptedStateRejected.as_str() {
        return Err(not_found());
    }
    let Some(model) = state.boot_model.clone() else {
        return Err(not_found());
    };
    // Serialize with every other state-file writer (a concurrent commit or a
    // second acknowledge) across the read + remove.
    let mutation = state.lock_config_mutation().await;
    let boot_path = model.boot_path().to_path_buf();
    let (marker_removed, marker_message) = tokio::task::spawn_blocking(move || {
        // An unreadable/oversized/unparsable marker still means "pending":
        // its message is unknown, but it is removed all the same.
        let message = crate::boot_model::read_boot_fallback_marker(&boot_path)
            .ok()
            .flatten()
            .map(|m| m.message);
        crate::boot_model::clear_boot_fallback_marker(&boot_path).map(|removed| (removed, message))
    })
    .await
    .map_err(|e| ControlError::Repository(format!("acknowledge task failed: {e}")))?
    .map_err(|e| ControlError::Repository(format!("removing the start-up fallback marker: {e}")))?;
    // The file work is done: release the config-mutation lock BEFORE waiting
    // for the ingest, so an acknowledge never stalls config mutations or the
    // running-state persister (review lens 2 round 2).
    drop(mutation);
    let active = active_rejection(&state)?;
    if !marker_removed && active.is_none() {
        return Err(not_found());
    }
    let message = active
        .as_ref()
        .map(|w| w.message.clone())
        .or(marker_message)
        .unwrap_or_default();
    let since = active.as_ref().map_or(0, |w| w.since);
    let cleared = crate::warning_ingest::emit_accepted_state_rejected_cleared(
        state.engine.as_ref(),
        &message,
        since,
    );
    // The broadcast is FIFO: once the ingest applied OUR clear, any raise
    // published before it was applied first, so the store cannot flip back.
    // Never upsert ahead of the ingest (a raise still queued would then
    // overwrite the clear). Bounded wait on the control plane only; on timeout
    // (no ingest running, or it lagged past our event) write it directly.
    // "Applied" = the store holds the warning AND it is inactive: an absent
    // entry may just mean an earlier raise has not been ingested yet.
    let deadline = tokio::time::Instant::now() + ACK_INGEST_WAIT;
    while !rejection_cleared(&state)? {
        if tokio::time::Instant::now() >= deadline {
            state.warnings.upsert(cleared)?;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    tracing::info!(
        key = %principal.key_id,
        "accepted-state-rejected acknowledged; start-up fallback marker removed"
    );
    Ok(StatusCode::NO_CONTENT)
}

/// How long an acknowledge waits for the warning ingest to apply its clear.
/// The ingest is one subscriber draining a bounded (64-deep) broadcast into an
/// in-memory store, so our clear lands within a few events; 250 ms is ample,
/// and on timeout the route writes the clear itself.
const ACK_INGEST_WAIT: std::time::Duration = std::time::Duration::from_millis(250);

/// The active `accepted-state-rejected` warning in the store, if any.
fn active_rejection(state: &AppState) -> ControlResult<Option<HealthWarning>> {
    Ok(state
        .warnings
        .list(&WarningFilter::active_only())?
        .into_iter()
        .find(|w| w.code == WarningCode::AcceptedStateRejected))
}

/// Whether the store holds the `accepted-state-rejected` warning as cleared.
fn rejection_cleared(state: &AppState) -> ControlResult<bool> {
    Ok(state
        .warnings
        .list(&WarningFilter { active: None })?
        .into_iter()
        .any(|w| w.code == WarningCode::AcceptedStateRejected && !w.active))
}
