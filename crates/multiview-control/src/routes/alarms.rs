//! The alarm REST surface under `/api/v1/alarms`.
//!
//! Two endpoints expose the control plane's engine-fed alarm mirror
//! (broadcast-multiviewer brief §4):
//!
//! * `GET /api/v1/alarms` — list active and historical alarms, filterable by
//!   `severity` (minimum X.733 severity), `active` (true/false), and `scope`
//!   (the scope kind). Role: read.
//! * `POST /api/v1/alarms/{id}/ack` — acknowledge an alarm with `ETag`/`If-Match`
//!   optimistic concurrency (ADR-W006), so two operators cannot silently clobber
//!   each other's ack. Role: write (operator).
//!
//! Each alarm response carries the resource's `ETag` (from its version) so a
//! follow-up acknowledge can present a matching `If-Match` and get `412` on a
//! stale precondition. Errors are RFC 9457 problem documents.
//!
//! The store these read/write is fed by the read-only, lagged-skip engine
//! subscription ([`crate::alarm_ingest`]); nothing here is on the engine's data
//! plane (invariant #10).
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use multiview_core::alarm::{AlarmId, AlarmRecord, PerceivedSeverity};
use serde::Deserialize;

use crate::alarm_store::{AlarmFilter, VersionedAlarm, ALARM_KIND};
use crate::auth::{Action, Principal};
use crate::concurrency::IfMatch;
use crate::error::ControlResult;
use crate::state::AppState;

/// The query parameters accepted by `GET /api/v1/alarms`.
///
/// All filters are optional. `severity` is the **minimum** X.733 severity to
/// include and is matched case-insensitively against the severity names
/// (`cleared`/`indeterminate`/`warning`/`minor`/`major`/`critical`).
#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::IntoParams))]
pub struct AlarmQuery {
    /// Keep only alarms at or above this X.733 severity (e.g. `major`).
    #[serde(default)]
    pub severity: Option<String>,
    /// Keep only active (`true`) or only cleared/historical (`false`) alarms.
    #[serde(default)]
    pub active: Option<bool>,
    /// Keep only alarms whose scope kind matches (e.g. `tile`, `probe`).
    #[serde(default)]
    pub scope: Option<String>,
}

/// Parse a severity name (case-insensitive) into a [`PerceivedSeverity`].
///
/// Returns [`None`] for an unrecognised name so the handler can reject it with a
/// `422` rather than silently ignoring an operator's typo'd filter.
#[must_use]
pub fn parse_severity(name: &str) -> Option<PerceivedSeverity> {
    match name.trim().to_ascii_lowercase().as_str() {
        "cleared" => Some(PerceivedSeverity::Cleared),
        "indeterminate" => Some(PerceivedSeverity::Indeterminate),
        "warning" => Some(PerceivedSeverity::Warning),
        "minor" => Some(PerceivedSeverity::Minor),
        "major" => Some(PerceivedSeverity::Major),
        "critical" => Some(PerceivedSeverity::Critical),
        _ => None,
    }
}

impl AlarmQuery {
    /// Translate the query into a repository [`AlarmFilter`].
    ///
    /// # Errors
    ///
    /// [`ControlError::Validation`](crate::error::ControlError::Validation) if
    /// `severity` is present but not a recognised severity name.
    pub fn into_filter(self) -> ControlResult<AlarmFilter> {
        let min_severity = match self.severity {
            None => None,
            Some(name) => Some(parse_severity(&name).ok_or_else(|| {
                crate::error::ControlError::Validation(format!("unknown severity {name:?}"))
            })?),
        };
        Ok(AlarmFilter {
            min_severity,
            active: self.active,
            scope_kind: self.scope,
        })
    }
}

/// Attach the alarm's `ETag` to a successful response carrying the record.
fn alarm_response(status: StatusCode, versioned: &VersionedAlarm) -> Response {
    let etag = versioned.version.to_etag();
    let mut response = (status, Json(versioned.record.clone())).into_response();
    if let Ok(value) = header::HeaderValue::from_str(&etag) {
        response.headers_mut().insert(header::ETAG, value);
    }
    response
}

/// `GET /api/v1/alarms` — list active/historical alarms (role: read).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/alarms",
        tag = "alarms",
        params(AlarmQuery),
        responses(
            (status = 200, description = "Matching alarms, id-sorted.", body = [crate::openapi_schemas::AlarmRecordDoc]),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to read.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn list_alarms(
    State(state): State<AppState>,
    principal: Principal,
    Query(query): Query<AlarmQuery>,
) -> ControlResult<Json<Vec<AlarmRecord>>> {
    principal.role.require(Action::Read)?;
    let filter = query.into_filter()?;
    let alarms = state
        .alarms
        .list(&filter)?
        .into_iter()
        .map(|v| v.record)
        .collect();
    Ok(Json(alarms))
}

/// `POST /api/v1/alarms/{id}/ack` — acknowledge an alarm (role: write; If-Match).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/alarms/{id}/ack",
        tag = "alarms",
        params(("id" = String, Path, description = "Alarm id to acknowledge.")),
        responses(
            (status = 200, description = "The acknowledged alarm.", body = crate::openapi_schemas::AlarmRecordDoc),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to acknowledge.", body = crate::problem::Problem),
            (status = 404, description = "No alarm with that id.", body = crate::problem::Problem),
            (status = 412, description = "If-Match precondition failed.", body = crate::problem::Problem),
            (status = 428, description = "If-Match precondition required.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn ack_alarm(
    State(state): State<AppState>,
    principal: Principal,
    if_match: IfMatch,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    let alarm_id = AlarmId::new(id.clone());
    // Enforce the optimistic-concurrency precondition against the live version
    // before mutating, exactly like the layout mutations.
    let current = state.alarms.get(&alarm_id)?;
    if_match.require(ALARM_KIND, &id, current.version)?;
    let when = state.ack_now();
    let acked = state
        .alarms
        .acknowledge(&alarm_id, &principal.key_id, when)?;
    Ok(alarm_response(StatusCode::OK, &acked))
}
