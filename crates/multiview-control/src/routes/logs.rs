//! The read-only structured log-tail REST surface under `/api/v1/logs`
//! (ADR-0060 §5.2).
//!
//! `GET /api/v1/logs` returns recent buffered [`LogRecord`]s from the bounded,
//! drop-oldest log ring ([`AppState::logs`]), filterable by:
//!
//! * `resource_id` — only records attributed to this source/output/layout id;
//! * `kind` — only records of this resource kind (`source`/`output`/…);
//! * `level` — minimum severity (`trace`/`debug`/`info`/`warn`/`error`);
//! * `since` — only records with a sequence strictly greater than this cursor
//!   (the live-tail paging cursor; pair with the `seq` of the last seen record);
//! * `limit` — at most this many of the most-recent matching records.
//!
//! Role: **read**. Errors are RFC 9457 problem documents (a typo'd `level` /
//! `kind` is a `422`). The ring is the producer fed by the telemetry
//! `LogCaptureLayer`; nothing here is on the engine's data plane, and the ring is
//! bounded drop-oldest — the log tail can never back-pressure the engine
//! (invariant #10). The live WebSocket tail on `Topic::Logs` is a separate
//! transport owned by the realtime/web surface; this is the buffered read.
use axum::extract::{Query, State};
use axum::Json;
use multiview_telemetry::{LogFilter, LogLevel, LogRecord, LogResourceKind};
use serde::Deserialize;

use crate::auth::{Action, Principal};
use crate::error::{ControlError, ControlResult};
use crate::state::AppState;

/// The query parameters accepted by `GET /api/v1/logs`. All optional.
#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::IntoParams))]
pub struct LogQuery {
    /// Keep only records attributed to this stable config resource id.
    #[serde(default)]
    pub resource_id: Option<String>,
    /// Keep only records of this resource kind (`source`/`output`/`layout`/
    /// `program`/`device`).
    #[serde(default)]
    pub kind: Option<String>,
    /// Keep only records at or above this severity (`trace`/`debug`/`info`/
    /// `warn`/`error`).
    #[serde(default)]
    pub level: Option<String>,
    /// Keep only records with a `seq` strictly greater than this cursor.
    #[serde(default)]
    pub since: Option<u64>,
    /// Return at most this many of the most-recent matching records.
    #[serde(default)]
    pub limit: Option<usize>,
}

impl LogQuery {
    /// Translate the query into a [`LogFilter`], rejecting an unrecognised
    /// `level` or `kind` with a `422` rather than silently ignoring the typo.
    ///
    /// # Errors
    ///
    /// [`ControlError::Validation`] if `level` or `kind` is present but not a
    /// recognised name.
    pub fn into_filter(self) -> ControlResult<LogFilter> {
        let min_level =
            match self.level {
                None => None,
                Some(name) => Some(LogLevel::parse(&name).ok_or_else(|| {
                    ControlError::Validation(format!("unknown log level {name:?}"))
                })?),
            };
        let resource_kind = match self.kind {
            None => None,
            Some(name) => Some(LogResourceKind::parse(&name).ok_or_else(|| {
                ControlError::Validation(format!("unknown resource kind {name:?}"))
            })?),
        };
        Ok(LogFilter {
            resource_id: self.resource_id,
            resource_kind,
            min_level,
            since_seq: self.since,
            limit: self.limit,
        })
    }
}

/// `GET /api/v1/logs` — recent buffered structured log records (role: read).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/logs",
        tag = "logs",
        params(LogQuery),
        responses(
            (status = 200, description = "Matching log records, oldest first.", body = [crate::openapi_schemas::LogRecordDoc]),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to read.", body = crate::problem::Problem),
            (status = 422, description = "An unrecognised level or kind filter.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn list_logs(
    State(state): State<AppState>,
    principal: Principal,
    Query(query): Query<LogQuery>,
) -> ControlResult<Json<Vec<LogRecord>>> {
    principal.role.require(Action::Read)?;
    let filter = query.into_filter()?;
    // BOLA (SEC-12, ADR-W005/W025): an explicit `?resource_id=` is a per-object
    // probe — authorize it on the axis its `kind` names before returning anything
    // (an out-of-scope id is a 403, exactly as a single-resource GET would be).
    if let Some(resource_id) = filter.resource_id.as_deref() {
        authorize_log_resource(&principal, filter.resource_kind, resource_id)?;
    }
    let mut records = state.logs.query(&filter);
    // Row-filter the tail for a scoped principal (kind-aware; unattributed and
    // program records fail closed). A no-op for an unscoped principal.
    if !principal.is_global() {
        records.retain(|record| log_record_visible(&principal, record));
    }
    Ok(Json(records))
}

/// Authorize an explicit `?resource_id=` log probe on the axis its `kind` names
/// (BOLA, SEC-12). With no `kind` the id is ambiguous across the object and output
/// axes, so a scoped principal must clear BOTH; a future/unknown kind fails closed
/// (only an unrestricted principal may probe it).
///
/// # Errors
///
/// [`ControlError::Forbidden`] if the principal's relevant axis denies `resource_id`.
fn authorize_log_resource(
    principal: &Principal,
    kind: Option<LogResourceKind>,
    resource_id: &str,
) -> ControlResult<()> {
    match kind {
        Some(LogResourceKind::Source | LogResourceKind::Layout | LogResourceKind::Device) => {
            crate::auth::authorize_object(principal, resource_id)?;
        }
        Some(LogResourceKind::Output) => {
            crate::auth::authorize_output(principal, resource_id)?;
        }
        // No kind: ambiguous across the object AND output axes — require both so a
        // principal scoped on either cannot probe the other's id space.
        None => {
            crate::auth::authorize_object(principal, resource_id)?;
            crate::auth::authorize_output(principal, resource_id)?;
        }
        // Program (whole-system), or a future/unknown kind (the enum is
        // `#[non_exhaustive]`): allowed only to an unrestricted principal — fail
        // closed for a scoped one.
        Some(_) => {
            crate::routes::require_unscoped_for_whole_system(principal)?;
        }
    }
    Ok(())
}

/// Whether a scoped principal may see one log record (BOLA row filter, SEC-12).
///
/// Kind-aware: Source/Layout/Device → object axis, Output → output axis,
/// Program → unrestricted-only. A record with no resource id, or an unknown /
/// future (`#[non_exhaustive]`) kind, is unattributable and **fails closed**
/// (dropped) — its message may span resources the principal cannot see. Applied
/// only to a scoped principal; an unscoped one sees every record.
fn log_record_visible(principal: &Principal, record: &LogRecord) -> bool {
    let Some(resource_id) = record.resource_id.as_deref() else {
        return false;
    };
    match record.resource_kind {
        Some(LogResourceKind::Source | LogResourceKind::Layout | LogResourceKind::Device) => {
            crate::auth::authorize_object(principal, resource_id).is_ok()
        }
        Some(LogResourceKind::Output) => {
            crate::auth::authorize_output(principal, resource_id).is_ok()
        }
        // Program is whole-system: only an unrestricted principal sees it (this fn
        // runs only for scoped principals, so a program record is dropped).
        Some(LogResourceKind::Program) => principal.is_global(),
        // Absent or future/unknown kind: unattributable, fail closed.
        None | Some(_) => false,
    }
}
