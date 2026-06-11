//! The health-warning REST surface under `/api/v1/health` (ADR-0035 SA-0).
//!
//! One read-only endpoint exposes the control plane's engine-fed health-warning
//! mirror:
//!
//! * `GET /api/v1/health` — list the active health warnings (e.g. "GPU present
//!   but compositing fell back to CPU; here is the fix"). `?active=false` widens
//!   to include cleared warnings. Role: read.
//!
//! This is **not** an extension of `/livez`/`/readyz` (ADR-R009: a capability
//! warning must not flip liveness and restart-loop the container). It is a
//! dedicated read-only surface modelled on [`crate::routes::alarms::list_alarms`].
//! Errors are RFC 9457 problem documents.
//!
//! The store this reads is fed by the read-only, lagged-skip engine subscription
//! ([`crate::warning_ingest`]); nothing here is on the engine's data plane
//! (invariant #10).
use axum::extract::{Query, State};
use axum::Json;
use multiview_events::HealthWarning;
use serde::Deserialize;

use crate::auth::{Action, Principal};
use crate::error::ControlResult;
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
