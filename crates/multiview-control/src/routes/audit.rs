//! The **read-only** change-audit-log route under `/api/v1/audit`.
//!
//! The audit log records every successful mutation (who/what/when). It is
//! exposed only for reading: the route registers a `GET` handler and no mutating
//! verb, so the log is append-only from the engine/handler side and tamper-
//! resistant from the API side. Reads are role-gated to [`Action::Read`].
use axum::extract::{Query, State};
use axum::Json;
use serde::Deserialize;

use crate::audit::AuditEntry;
use crate::auth::{Action, Principal};
use crate::error::ControlResult;
use crate::state::AppState;

/// Optional query parameters for the audit listing.
#[derive(Debug, Default, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::IntoParams))]
pub struct AuditQuery {
    /// Restrict the listing to a single object id.
    #[serde(default)]
    pub object_id: Option<String>,
}

/// `GET /api/v1/audit` — list audit entries newest-first (role: read).
///
/// An optional `?object_id=` filters to a single object's history.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/audit",
        tag = "audit",
        params(AuditQuery),
        responses(
            (status = 200, description = "Audit entries, newest first.", body = [AuditEntry]),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to read.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn list_audit(
    State(state): State<AppState>,
    principal: Principal,
    Query(query): Query<AuditQuery>,
) -> ControlResult<Json<Vec<AuditEntry>>> {
    principal.role.require(Action::Read)?;
    let entries = state.audit.list(query.object_id.as_deref())?;
    Ok(Json(entries))
}
