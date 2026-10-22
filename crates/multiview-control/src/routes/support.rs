//! The **LOCAL** support/ticketing REST surface (Conspect, ADR-0053 §3 / brief
//! §10/§11, spec §7/§11). Eight endpoints under `/api/v1/support` (+ the shared
//! `/api/v1/account/audit` trail the audited actions land in):
//!
//! * `GET /support/entitlement` — tier-derived routing (free → community, with
//!   the one quiet line). Always `200` — even the free tier gets an answer.
//! * `GET /support/tickets` · `POST /support/tickets` · `GET /support/tickets/{id}`
//!   · `POST /support/tickets/{id}/reply` · `POST /support/tickets/{id}/close` —
//!   the local ticket store. Entitled tiers only (`403 not_entitled` for the free
//!   tier, before RBAC). A reply to a closed ticket is `409 ticket_closed`.
//! * `POST /support/bundle` → `202 {bundle_id}` · `GET /support/bundle/{id}` — the
//!   previewable, redacted, **media-free** context-pack composer. Entitled tiers
//!   only. Composes regardless of telemetry consent (§7.2 — no consent check).
//! * `POST /support/data-request/{id}/{approve,deny}` — local approval of an
//!   inbound egress request (**nothing leaves without a local yes**). An expired
//!   request is `410 request_expired`.
//!
//! Every audited action (ticket raise/reply, bundle compose, data-request
//! approve/deny) lands an entry in the append-only account audit store.
//!
//! # Isolation (invariant #1 / #10)
//!
//! Every handler reads/writes control-plane stores only (tickets, data requests,
//! the composed-bundle store, the consent-independent retention store, the config
//! resource stores). No handler holds an engine handle, spawns onto the hot loop,
//! or `.await`s the engine — a wedged client cannot back-pressure the engine, and
//! no support action takes a running program off air. Composing a bundle is pure
//! assembly over already-recorded control-plane state.
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::account_audit::AccountAuditKind;
use crate::auth::{Action, Principal};
use crate::error::ControlResult;
use crate::problem::Problem;
use crate::state::AppState;
use crate::support_bundle::{compose_bundle, Bundle, BundleAccepted, BundleRequest, ConfigSources};
use crate::support_store::{
    support_entitlement, CloseOutcome, DataRequestOutcome, NewTicket, ReplyOutcome,
    SupportEntitlement, Ticket, TicketSeverity, TicketSummary,
};

/// `GET /api/v1/support/entitlement` — the tier-derived support routing (role:
/// read). Always `200`: the free tier gets `eligible:false` + the community
/// route + the one quiet line, an eligible tier gets its support queue + SLA.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/support/entitlement",
        tag = "support",
        responses(
            (status = 200, description = "The tier-derived support entitlement + route (always 200).", body = SupportEntitlement),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to read.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_entitlement(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<SupportEntitlement>> {
    principal.role.require(Action::Read)?;
    Ok(Json(support_entitlement(state.licence_tier().as_deref())))
}

/// The refusal a free-tier machine gets on an entitled-only support surface —
/// the `403 not_entitled` problem the spec pins. Routing to `community` is not an
/// entitled support queue, so a ticket/bundle cannot be raised against it.
fn not_entitled() -> Response {
    Problem::new(403, "not_entitled", "Not entitled to support")
        .with_detail(
            "this machine's tier routes to the community channel, which is not an entitled \
             support queue; an entitled tier is required to raise a ticket or compose a bundle",
        )
        .into_response()
}

/// Whether this machine is eligible for an entitled support queue (else the
/// caller answers `not_entitled`).
fn eligible(state: &AppState) -> bool {
    support_entitlement(state.licence_tier().as_deref()).eligible
}

/// The `POST /api/v1/support/tickets` request body.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct RaiseTicketRequest {
    /// The operator-supplied subject line.
    pub subject: String,
    /// The opening body (seeds the append-only thread).
    pub body: String,
    /// The declared severity.
    pub severity: TicketSeverity,
    /// Attachment ids the operator references (recorded on the opening update's
    /// context; the deliberate-attachment path is the bundle composer). Optional.
    #[serde(default)]
    pub attachments: Vec<String>,
}

/// `GET /api/v1/support/tickets` — list local ticket summaries newest-first
/// (role: read; entitled tiers only).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/support/tickets",
        tag = "support",
        responses(
            (status = 200, description = "Local ticket summaries (newest-first).", body = [TicketSummary]),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized, or the free tier is not entitled.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn list_tickets(State(state): State<AppState>, principal: Principal) -> Response {
    if let Err(err) = principal.role.require(Action::Read) {
        return err.into_response();
    }
    if !eligible(&state) {
        return not_entitled();
    }
    (StatusCode::OK, Json(state.tickets.list())).into_response()
}

/// `POST /api/v1/support/tickets` — raise a local ticket (role: write; entitled
/// tiers only → else `403 not_entitled`). The store mints the `CS-xxxx` id, seeds
/// the thread with the opening body, and auto-attaches the machine context
/// (§7.1). Every raise lands a `ticket` account-audit entry. → `201 Created`.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/support/tickets",
        tag = "support",
        request_body = RaiseTicketRequest,
        responses(
            (status = 201, description = "The ticket was raised; the full thread is returned.", body = Ticket),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized, or the free tier is not entitled.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn raise_ticket(
    State(state): State<AppState>,
    principal: Principal,
    Json(req): Json<RaiseTicketRequest>,
) -> Response {
    if let Err(err) = principal.role.require(Action::Write) {
        return err.into_response();
    }
    if !eligible(&state) {
        return not_entitled();
    }
    let ticket = state.tickets.raise(
        NewTicket {
            author: principal.key_id.clone(),
            subject: req.subject,
            body: req.body,
            severity: req.severity,
            context: state.ticket_context(),
            route: state.support_route(),
        },
        state.ack_now(),
    );
    state.audit_account(
        &principal.key_id,
        AccountAuditKind::Ticket,
        Some(json!({ "ticket_id": ticket.ticket_id, "action": "raise" })),
    );
    (StatusCode::CREATED, Json(ticket)).into_response()
}

/// `GET /api/v1/support/tickets/{id}` — fetch one ticket (full thread) (role:
/// read; entitled tiers only). Unknown id → `404`.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/support/tickets/{id}",
        tag = "support",
        params(("id" = String, Path, description = "The CS-xxxx ticket id.")),
        responses(
            (status = 200, description = "The ticket with its full thread + machine context.", body = Ticket),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized, or the free tier is not entitled.", body = crate::problem::Problem),
            (status = 404, description = "No ticket with that id.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_ticket(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> Response {
    if let Err(err) = principal.role.require(Action::Read) {
        return err.into_response();
    }
    if !eligible(&state) {
        return not_entitled();
    }
    match state.tickets.get(&id) {
        Some(ticket) => (StatusCode::OK, Json(ticket)).into_response(),
        None => ticket_not_found(&id),
    }
}

/// The `POST /api/v1/support/tickets/{id}/reply` request body.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ReplyRequest {
    /// The reply text to append to the thread.
    pub body: String,
}

/// `POST /api/v1/support/tickets/{id}/reply` — append a reply to a ticket's
/// thread (role: write; entitled tiers only). A reply to a **closed** ticket is
/// `409 ticket_closed` (pinned). Unknown id → `404`. Each reply lands a `ticket`
/// account-audit entry. → `200` with the updated thread.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/support/tickets/{id}/reply",
        tag = "support",
        params(("id" = String, Path, description = "The CS-xxxx ticket id.")),
        request_body = ReplyRequest,
        responses(
            (status = 200, description = "The reply appended; the updated thread is returned.", body = Ticket),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized, or the free tier is not entitled.", body = crate::problem::Problem),
            (status = 404, description = "No ticket with that id.", body = crate::problem::Problem),
            (status = 409, description = "The ticket is closed — replies are refused.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn reply_ticket(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
    Json(req): Json<ReplyRequest>,
) -> Response {
    if let Err(err) = principal.role.require(Action::Write) {
        return err.into_response();
    }
    if !eligible(&state) {
        return not_entitled();
    }
    match state
        .tickets
        .reply(&id, &principal.key_id, req.body, state.ack_now())
    {
        ReplyOutcome::Appended(ticket) => {
            state.audit_account(
                &principal.key_id,
                AccountAuditKind::Ticket,
                Some(json!({ "ticket_id": id, "action": "reply" })),
            );
            (StatusCode::OK, Json(*ticket)).into_response()
        }
        ReplyOutcome::Closed => Problem::new(409, "ticket_closed", "Ticket closed")
            .with_detail("the ticket is closed; reopen a new ticket to continue the conversation")
            .into_response(),
        ReplyOutcome::NotFound => ticket_not_found(&id),
    }
}

/// `POST /api/v1/support/tickets/{id}/close` — close a ticket locally (role:
/// write; entitled tiers only). Idempotent (re-closing an already-closed ticket
/// is still `200`). Unknown id → `404`. → `200`.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/support/tickets/{id}/close",
        tag = "support",
        params(("id" = String, Path, description = "The CS-xxxx ticket id.")),
        responses(
            (status = 200, description = "The ticket is closed (idempotent).", body = Ticket),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized, or the free tier is not entitled.", body = crate::problem::Problem),
            (status = 404, description = "No ticket with that id.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn close_ticket(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> Response {
    if let Err(err) = principal.role.require(Action::Write) {
        return err.into_response();
    }
    if !eligible(&state) {
        return not_entitled();
    }
    match state.tickets.close(&id) {
        CloseOutcome::Closed | CloseOutcome::AlreadyClosed => match state.tickets.get(&id) {
            Some(ticket) => {
                state.audit_account(
                    &principal.key_id,
                    AccountAuditKind::Ticket,
                    Some(json!({ "ticket_id": id, "action": "close" })),
                );
                (StatusCode::OK, Json(ticket)).into_response()
            }
            None => ticket_not_found(&id),
        },
        CloseOutcome::NotFound => ticket_not_found(&id),
    }
}

/// The shared `404` for an unknown ticket id.
fn ticket_not_found(id: &str) -> Response {
    Problem::new(404, "not-found", "Ticket not found")
        .with_detail(format!("no ticket with id {id:?}"))
        .into_response()
}

/// `POST /api/v1/support/bundle` — compose a previewable, redacted, media-free
/// context-pack (role: write; entitled tiers only). Composing performs **no
/// consent check** (§7.2): consent governs the daily outbound pipe, not a
/// deliberate operator attachment. Every compose lands a `bundle-compose`
/// account-audit entry. → `202 {bundle_id}`; read the preview at
/// `GET /support/bundle/{bundle_id}`.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/support/bundle",
        tag = "support",
        request_body = BundleRequest,
        responses(
            (status = 202, description = "The bundle was composed; read the preview by id.", body = BundleAccepted),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized, or the free tier is not entitled.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn compose(
    State(state): State<AppState>,
    principal: Principal,
    Json(req): Json<BundleRequest>,
) -> Response {
    if let Err(err) = principal.role.require(Action::Write) {
        return err.into_response();
    }
    // Per-object visibility (BOLA, ADR-W005/ADR-W025): a support bundle's `config`
    // section embeds the WHOLE config — every device id / `device_ref` /
    // sync-group member (redacted only for secrets/URLs, not ids). Like the config
    // export, an OBJECT-scoped principal must not compose one (it would disclose
    // every object outside its allowlist wholesale); the bundle is confined to a
    // principal that can see the whole system. Unscoped principals are unaffected.
    if let Err(err) = crate::routes::require_unscoped_for_whole_system(&principal) {
        return err.into_response();
    }
    if !eligible(&state) {
        return not_entitled();
    }
    // The wall second the windowed retention queries read against — derived from
    // the control-plane ack clock (off the engine hot loop). Negative clocks
    // (pathological) floor to 0.
    let now = state.ack_now().as_nanos().div_euclid(1_000_000_000);
    let now_unix_seconds = u64::try_from(now).unwrap_or(0);
    let cfg = ConfigSources {
        sources: state.sources.as_ref(),
        outputs: state.outputs.as_ref(),
        overlays: state.overlays.as_ref(),
        probes: state.probes.as_ref(),
        devices: state.devices.as_ref(),
    };
    let bundle = compose_bundle(
        &req,
        state.retention.as_ref(),
        &cfg,
        now_unix_seconds,
        state.ack_now(),
        crate::support_bundle::new_bundle_id,
    );
    let bundle_id = bundle.bundle_id.clone();
    state.support_bundles.put(bundle);
    state.audit_account(
        &principal.key_id,
        AccountAuditKind::BundleCompose,
        Some(json!({ "bundle_id": bundle_id })),
    );
    (StatusCode::ACCEPTED, Json(BundleAccepted { bundle_id })).into_response()
}

/// `GET /api/v1/support/bundle/{id}` — read a composed bundle's preview (role:
/// read; entitled tiers only). Unknown/evicted id → `404`. The preview lists
/// every redaction and carries no media.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/support/bundle/{id}",
        tag = "support",
        params(("id" = String, Path, description = "The composed bundle id.")),
        responses(
            (status = 200, description = "The composed bundle preview (redacted, media-free).", body = Bundle),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized, or the free tier is not entitled.", body = crate::problem::Problem),
            (status = 404, description = "No bundle with that id.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_bundle(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> Response {
    if let Err(err) = principal.role.require(Action::Read) {
        return err.into_response();
    }
    // Per-object visibility (BOLA, ADR-W005/ADR-W025): the bundle preview embeds
    // the whole config (every device id / ref / member); an object-scoped
    // principal must not read it — defence in depth alongside the compose guard,
    // so it cannot read even an admin-composed bundle. Unscoped principals are
    // unaffected.
    if let Err(err) = crate::routes::require_unscoped_for_whole_system(&principal) {
        return err.into_response();
    }
    if !eligible(&state) {
        return not_entitled();
    }
    match state.support_bundles.get(&id) {
        Some(bundle) => (StatusCode::OK, Json(bundle)).into_response(),
        None => Problem::new(404, "not-found", "Bundle not found")
            .with_detail(format!("no composed bundle with id {id:?}"))
            .into_response(),
    }
}

/// The `200` body of a data-request approve/deny: the request's new terminal
/// state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DataRequestActioned {
    /// The new state slug (`approved` / `denied`).
    pub state: crate::support_store::DataRequestState,
}

/// `POST /api/v1/support/data-request/{id}/approve` — approve an inbound egress
/// request **locally** (role: write). **Nothing leaves without this yes:** the
/// request transitions to `approved` and egress is gated on that state. An
/// already-expired request is `410 request_expired` (and stays expired). Unknown
/// id → `404`. Lands a `data-request-approve` account-audit entry.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/support/data-request/{id}/approve",
        tag = "support",
        params(("id" = String, Path, description = "The DR-xxxx data-request id.")),
        responses(
            (status = 200, description = "The request was approved locally.", body = DataRequestActioned),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to approve.", body = crate::problem::Problem),
            (status = 404, description = "No request with that id.", body = crate::problem::Problem),
            (status = 410, description = "The request window elapsed — too late to approve.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn approve_data_request(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> Response {
    action_data_request(&state, &principal, &id, DataRequestVerb::Approve)
}

/// `POST /api/v1/support/data-request/{id}/deny` — deny an inbound egress request
/// locally (role: write). Nothing leaves. Symmetric to approve.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/support/data-request/{id}/deny",
        tag = "support",
        params(("id" = String, Path, description = "The DR-xxxx data-request id.")),
        responses(
            (status = 200, description = "The request was denied locally.", body = DataRequestActioned),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to deny.", body = crate::problem::Problem),
            (status = 404, description = "No request with that id.", body = crate::problem::Problem),
            (status = 410, description = "The request window elapsed — too late to deny.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn deny_data_request(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> Response {
    action_data_request(&state, &principal, &id, DataRequestVerb::Deny)
}

/// Which terminal transition a data-request action drives.
#[derive(Debug, Clone, Copy)]
enum DataRequestVerb {
    /// Approve → egress permitted.
    Approve,
    /// Deny → nothing leaves.
    Deny,
}

/// The shared approve/deny handler: gate on write, drive the one-way transition,
/// map the outcome to the HTTP contract, and audit a recorded action.
fn action_data_request(
    state: &AppState,
    principal: &Principal,
    id: &str,
    verb: DataRequestVerb,
) -> Response {
    if let Err(err) = principal.role.require(Action::Write) {
        return err.into_response();
    }
    let outcome = match verb {
        DataRequestVerb::Approve => state.data_requests.approve(id),
        DataRequestVerb::Deny => state.data_requests.deny(id),
    };
    match outcome {
        DataRequestOutcome::Actioned(new_state)
        | DataRequestOutcome::AlreadyActioned(new_state) => {
            // Audit only a genuine local approve/deny that this call effected.
            if matches!(outcome, DataRequestOutcome::Actioned(_)) {
                let kind = match verb {
                    DataRequestVerb::Approve => AccountAuditKind::DataRequestApprove,
                    DataRequestVerb::Deny => AccountAuditKind::DataRequestDeny,
                };
                state.audit_account(&principal.key_id, kind, Some(json!({ "request_id": id })));
            }
            (
                StatusCode::OK,
                Json(DataRequestActioned { state: new_state }),
            )
                .into_response()
        }
        DataRequestOutcome::Expired => Problem::new(410, "request_expired", "Data request expired")
            .with_detail(
                "the request window elapsed before it was actioned; nothing has left the \
                     machine (egress is gated on a local approval that can no longer be given)",
            )
            .into_response(),
        DataRequestOutcome::NotFound => Problem::new(404, "not-found", "Data request not found")
            .with_detail(format!("no inbound data request with id {id:?}"))
            .into_response(),
    }
}
