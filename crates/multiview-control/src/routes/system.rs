//! System-wide capability reporting under `/api/v1/system` (ADR-W030).
//!
//! `GET /api/v1/system/capabilities` returns the honest default-build
//! capability and licence surface (which backends are available, the compositor
//! classification, the effective build-profile licence, and the mandatory NDI
//! attribution). It is a **system-global** read — no per-object BOLA axis — so a
//! viewer role suffices; the whole-system `network` posture is omitted for a
//! scoped principal. The value is the static snapshot the binary installed
//! via [`crate::AppState::with_capabilities`]; the handler never touches the
//! engine (invariant #10).
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::auth::{Action, Principal};
use crate::state::AppState;

/// `GET /api/v1/system/capabilities` — the build's codec/compositor backend
/// availability, compositor classification, effective build-profile licence, and
/// NDI attribution (ADR-W030). Role: read. The `network` section (front-door
/// host posture) is included only for an unscoped principal; a scoped key
/// gets the response without it.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/system/capabilities",
        tag = "system",
        responses(
            (status = 200, description = "System capability + licence surface.", body = crate::system::SystemCapabilities),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn system_capabilities(
    State(state): State<AppState>,
    principal: Principal,
) -> Response {
    if let Err(err) = principal.role.require(Action::Read) {
        return err.into_response();
    }
    let mut capabilities = state.capabilities.clone();
    // `network` is whole-system host posture (steering, receive buffer,
    // firewall floor): only a principal that may see the whole system gets
    // it, the same predicate `/api/v1/metrics/frontdoor` enforces
    // (`require_unscoped_for_whole_system`). A scoped key gets the rest.
    if crate::routes::require_unscoped_for_whole_system(&principal).is_err() {
        capabilities.network = None;
    }
    Json(capabilities).into_response()
}
