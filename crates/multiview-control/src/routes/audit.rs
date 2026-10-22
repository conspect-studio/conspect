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
    // BOLA re-disclosure defense (ADR-W005/ADR-W025): the audit log carries every
    // mutation's `object_id` AND a `detail` body with full resource contents
    // (device ids, `device_ref`, members). An object-scoped principal must not
    // re-enumerate objects it could not `GET` through this history.
    //
    // (a) An explicit `?object_id=<out-of-scope>` is a per-object probe — denied
    //     `403`, exactly as a single-object `GET` of that id would be.
    if let Some(object_id) = query.object_id.as_deref() {
        crate::auth::authorize_object(&principal, object_id)?;
    }
    let mut entries = state.audit.list(query.object_id.as_deref())?;
    // (b)/(c) For a scoped principal: keep only entries whose `object_id` is in
    // the allowlist, and redact any out-of-scope `device_ref`/`members[].device`
    // still embedded in a surviving in-scope entry's `detail` body. No-op for an
    // unscoped principal (the common admin/operator/viewer).
    if principal.is_scoped() {
        entries.retain(|entry| crate::auth::authorize_object(&principal, &entry.object_id).is_ok());
        for entry in &mut entries {
            if let Some(detail) = entry.detail.as_mut() {
                crate::routes::redact_device_refs_in_body(&principal, detail);
            }
        }
    }
    Ok(Json(entries))
}
