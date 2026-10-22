//! The **local-mesh** REST surface under `/api/v1/mesh` (Conspect, ADR-0051 /
//! the brief §11).
//!
//! Three endpoints render + toggle the always-on local-mesh discovery/relay
//! plane, all **local** (no licence-server calls):
//!
//! * `GET /api/v1/mesh/status` — the discovery + relay summary: `{discovery:
//!   "always_on", relay_enabled, role, via?, peers_count}`. Discovery is
//!   **always-on** and has no off switch (the spec's locked row). Role: read.
//! * `PUT /api/v1/mesh/relay` `{enabled}` — opt this machine **in/out** of
//!   relaying neighbours (brief §9.2). A real, persisted toggle: the new state is
//!   written to the shared mesh state and returned as the updated status. Role:
//!   write.
//! * `GET /api/v1/mesh/peers` — the **untrusted** discovered-peer inventory: each
//!   peer is a salted-digest id, an optional name (only once adopted), a
//!   `claimed` flag, `last_seen`, and `relaying_for_us`. A peer is never
//!   auto-trusted (ADR-0041 doctrine). Role: read.
//!
//! There is intentionally **no** endpoint that disables discovery — discovery
//! runs whenever the account plane runs (ADR-0051 §2).
//!
//! # Isolation (invariant #10)
//!
//! These routes read/toggle an in-memory [`MeshState`](multiview_mesh::MeshState)
//! — an `RwLock` over a bounded peer inventory + the relay opt-in. The store holds
//! **no** engine handle and is touched off the hot loop; a wedged client of these
//! routes can never back-pressure the engine. The live mDNS announce/browse loop
//! (the mesh crate's `mdns` feature) maintains the same store off the hot loop and
//! is likewise best-effort.

use axum::extract::State;
use axum::Json;
use multiview_mesh::{MeshStatus, Peer};
use serde::{Deserialize, Serialize};

use crate::audit::AuditAction;
use crate::auth::{Action, Principal};
use crate::error::ControlResult;
use crate::state::AppState;

/// The `PUT /api/v1/mesh/relay` request body: the relay opt-in state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct RelaySetRequest {
    /// Whether this machine should relay neighbours' heartbeats (brief §9.2).
    pub enabled: bool,
}

/// `GET /api/v1/mesh/status` — the always-on discovery + relay summary (role:
/// read).
///
/// Always `200`: discovery is always-on, so this is a pure data report —
/// `{discovery, relay_enabled, role, via?, peers_count}`.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/mesh/status",
        tag = "mesh",
        responses(
            (status = 200, description = "The always-on mesh discovery + relay status.", body = crate::openapi_schemas::MeshStatusDoc),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to read.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_status(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<MeshStatus>> {
    principal.role.require(Action::Read)?;
    Ok(Json(state.mesh.status()))
}

/// `PUT /api/v1/mesh/relay` — opt this machine in/out of relaying neighbours
/// (role: write).
///
/// A real, persisted toggle: the new state is written to the shared mesh state
/// and the updated status is returned. Discovery is unaffected (always-on) — this
/// toggles only the **relay** opt-in (brief §9.2). Every successful toggle is
/// recorded in **both** trails: the change-audit log and — as an
/// actor-attributed [`AccountAuditKind::RelayToggle`](crate::account_audit::AccountAuditKind::RelayToggle)
/// entry — the append-only account-audit store (the spec's relay opt-in/out row,
/// brief §10/§11).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        put,
        path = "/api/v1/mesh/relay",
        tag = "mesh",
        request_body = RelaySetRequest,
        responses(
            (status = 200, description = "Relay opt-in updated; the new mesh status.", body = crate::openapi_schemas::MeshStatusDoc),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to toggle relay.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn set_relay(
    State(state): State<AppState>,
    principal: Principal,
    Json(req): Json<RelaySetRequest>,
) -> ControlResult<Json<MeshStatus>> {
    principal.role.require(Action::Write)?;
    state.mesh.set_relay_enabled(req.enabled);
    // Record the opt-in change in the change-audit log (the engine/config change
    // trail). The mesh has no serial/identifier to log; the object id is the
    // coarse `relay` toggle, the detail the new state.
    state.audit(
        &principal.key_id,
        AuditAction::Update,
        "mesh.relay",
        "relay",
        Some(serde_json::json!({ "enabled": req.enabled })),
    );
    // And the account-side evidence trail (Conspect ADR-0053 §4 / brief §10/§11):
    // a relay opt-in/out is an immutable, timestamped, actor-attributed account
    // action (the spec's `relay opt-in/out` row). The actor is the toggling
    // principal; the detail carries only the new boolean state — never a raw
    // identifier (data minimisation, brief §8). Written off the hot loop into a
    // control-plane store (inv #10).
    state.audit_account(
        &principal.key_id,
        crate::account_audit::AccountAuditKind::RelayToggle,
        Some(serde_json::json!({ "enabled": req.enabled })),
    );
    Ok(Json(state.mesh.status()))
}

/// `GET /api/v1/mesh/peers` — the untrusted discovered-peer inventory (role:
/// read).
///
/// Each peer is a salted-digest id, an optional name (only once adopted), a
/// `claimed` flag, `last_seen` (seconds), and `relaying_for_us`. A peer is
/// **never** auto-trusted; relaying is an explicit operator confirm-adopt
/// (ADR-0041 doctrine). The ids are pure hex — never a raw identifier (brief §8).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/mesh/peers",
        tag = "mesh",
        responses(
            (status = 200, description = "The untrusted discovered-peer inventory.", body = [crate::openapi_schemas::MeshPeerDoc]),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to read.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn list_peers(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<Vec<Peer>>> {
    principal.role.require(Action::Read)?;
    Ok(Json(state.mesh.peers()))
}
