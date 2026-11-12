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
/// `accepted-state-rejected` warning (role: write, #324).
///
/// Removes the start-up fallback marker (on the blocking pool — file I/O never
/// runs on a runtime worker, and never on the engine), records the warning as
/// cleared in the store and publishes `health.warning.cleared` through the
/// drop-oldest lane (inv #10). The operator's saved configuration is not
/// touched: acknowledging only silences the alarm.
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
            (status = 403, description = "Not authorized to acknowledge.", body = crate::problem::Problem),
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
    let boot_path = model.boot_path().to_path_buf();
    let removed = tokio::task::spawn_blocking(move || {
        let marker = crate::boot_model::read_boot_fallback_marker(&boot_path)
            .ok()
            .flatten();
        crate::boot_model::clear_boot_fallback_marker(&boot_path).map(|removed| (removed, marker))
    })
    .await
    .map_err(|e| ControlError::Repository(format!("acknowledge task failed: {e}")))?
    .map_err(|e| ControlError::Repository(format!("removing the start-up fallback marker: {e}")))?;
    let (marker_removed, marker) = removed;
    let active = state
        .warnings
        .list(&WarningFilter::active_only())?
        .into_iter()
        .find(|w| w.code == WarningCode::AcceptedStateRejected);
    if !marker_removed && active.is_none() {
        return Err(not_found());
    }
    let message = active
        .as_ref()
        .map(|w| w.message.clone())
        .or_else(|| marker.map(|m| m.message))
        .unwrap_or_default();
    let since = active.as_ref().map_or(0, |w| w.since);
    let cleared = crate::warning_ingest::emit_accepted_state_rejected_cleared(
        state.engine.as_ref(),
        message,
        since,
    );
    // Upsert directly as well: the ingest mirrors the same event, but a client
    // that acknowledges and immediately re-reads must see it cleared.
    state.warnings.upsert(cleared)?;
    tracing::info!(
        key = %principal.key_id,
        "accepted-state-rejected acknowledged; start-up fallback marker removed"
    );
    Ok(StatusCode::NO_CONTENT)
}
