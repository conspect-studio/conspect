//! The **account audit + remote-actions** REST surface (Conspect, ADR-0053 §4 +
//! the brief §10/§11 + spec §6/§11). All routes are account-side and live here so
//! they do not churn the existing resource-route files (brief §15).
//!
//! Three surfaces ship here, all **locally drivable** (the spec demands it for
//! local actions + salvos); the transport that fills the queue from the portal is
//! the later server-client item (O1-blocked):
//!
//! * `GET /api/v1/account/audit?cursor&filter` — the **append-only** account
//!   audit log with cursor pagination → `{entries, next_cursor}` (role: read).
//!   There is **no** mutating verb — append-only by route construction.
//! * `GET /api/v1/actions/pending` — the pending remote-actions strip backend
//!   (role: read).
//! * `POST /api/v1/actions/{id}/cancel` — cancel a pending action locally
//!   (**local always wins**) → `{cancelled}`; an already-executed action answers
//!   `410 Gone` `already_executed` (role: write).
//! * `POST /api/v1/salvos/{name}/fire` — fire a named salvo through the engine
//!   command bus → `202 {action_id, queued_at}`; an unknown salvo answers
//!   `404 salvo_unknown` (role: write). Every fire lands in the account audit
//!   store.
//!
//! # Isolation (invariant #1 / #10)
//!
//! Every surface reads/writes control-plane stores only. The salvo fire submits
//! to the bounded, non-blocking command bus (a full bus sheds to `503`, never
//! blocking the engine). No handler holds an engine handle, spawns onto the hot
//! loop, or `.await`s the engine — a wedged client cannot back-pressure the
//! engine, and no account action takes a running program off air.
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::account_audit::{AccountAuditKind, AccountAuditPage};
use crate::auth::{Action, Principal};
use crate::command::{Command, OperationId};
use crate::error::ControlResult;
use crate::pending_actions::{CancelOutcome, PendingAction, PendingActionKind};
use crate::problem::Problem;
use crate::state::AppState;

/// The default page size for the account-audit listing when `?limit=` is absent.
const DEFAULT_AUDIT_PAGE_LIMIT: usize = 100;

/// The maximum page size a client may request (caps the per-request work; a
/// larger `?limit=` is clamped to this — bad-inputs-are-the-purpose).
const MAX_AUDIT_PAGE_LIMIT: usize = 1_000;

/// Query parameters for `GET /api/v1/account/audit`.
#[derive(Debug, Default, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::IntoParams))]
pub struct AccountAuditQuery {
    /// Resume strictly after this `seq` cursor (from a previous `next_cursor`).
    /// Absent fetches from the beginning of the log.
    #[serde(default)]
    pub cursor: Option<u64>,
    /// Restrict the listing to a single action kind (`kebab-case` slug).
    #[serde(default)]
    pub filter: Option<AccountAuditKind>,
    /// The maximum number of entries to return (clamped to
    /// [`MAX_AUDIT_PAGE_LIMIT`]; defaults to [`DEFAULT_AUDIT_PAGE_LIMIT`]).
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `GET /api/v1/account/audit` — the append-only account audit log, paginated
/// (role: read).
///
/// Returns `{entries, next_cursor}`: entries oldest-first within the page,
/// resumable via `?cursor=next_cursor`, optionally `?filter=<kind>`. There is no
/// mutating verb on this resource — the log is append-only by construction.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/account/audit",
        tag = "account",
        params(AccountAuditQuery),
        responses(
            (status = 200, description = "A page of account-audit entries (oldest-first) + the resume cursor.", body = AccountAuditPage),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to read.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn list_account_audit(
    State(state): State<AppState>,
    principal: Principal,
    Query(query): Query<AccountAuditQuery>,
) -> ControlResult<Json<AccountAuditPage>> {
    principal.role.require(Action::Read)?;
    let limit = query
        .limit
        .unwrap_or(DEFAULT_AUDIT_PAGE_LIMIT)
        .clamp(1, MAX_AUDIT_PAGE_LIMIT);
    let page = state.account_audit.page(query.cursor, query.filter, limit);
    Ok(Json(page))
}

/// `GET /api/v1/actions/pending` — the pending remote-actions strip (role: read).
///
/// Returns the actions awaiting local execution or cancellation, oldest-first.
/// An action that has executed or been cancelled drops out of this list.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/actions/pending",
        tag = "account",
        responses(
            (status = 200, description = "The pending remote actions (oldest-first).", body = [PendingAction]),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to read.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn list_pending_actions(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<Vec<PendingAction>>> {
    principal.role.require(Action::Read)?;
    Ok(Json(state.pending_actions.list_pending()))
}

/// The `200` body of a successful cancel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CancelledBody {
    /// Always `true` on the `200` path — the action is cancelled.
    pub cancelled: bool,
}

/// `POST /api/v1/actions/{id}/cancel` — cancel a pending action locally (role:
/// write).
///
/// **Local always wins:** a pending action the operator cancels here is
/// cancelled, and any later (portal-fed) execution attempt is refused. An action
/// that has **already executed** answers `410 Gone` `already_executed` — the
/// truthful "too late", never a fake success. An unknown id is `404`. The cancel
/// is recorded in the append-only account audit store.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/actions/{id}/cancel",
        tag = "account",
        params(("id" = String, Path, description = "The action id to cancel.")),
        responses(
            (status = 200, description = "The action was pending and is now cancelled (local wins).", body = CancelledBody),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to cancel.", body = crate::problem::Problem),
            (status = 404, description = "No action with that id.", body = crate::problem::Problem),
            (status = 410, description = "The action has already executed — too late to cancel.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn cancel_action(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> Response {
    if let Err(err) = principal.role.require(Action::Write) {
        return err.into_response();
    }
    match state.pending_actions.cancel(&id) {
        // The first cancel effects the transition — record it once in the trail.
        CancelOutcome::Cancelled => {
            state.audit_account(
                &principal.key_id,
                AccountAuditKind::ActionCancelled,
                Some(json!({ "action_id": id })),
            );
            (StatusCode::OK, Json(CancelledBody { cancelled: true })).into_response()
        }
        // A re-cancel is idempotently a success (the action is already cancelled)
        // but records nothing new — the cancel was already audited.
        CancelOutcome::AlreadyCancelled => {
            (StatusCode::OK, Json(CancelledBody { cancelled: true })).into_response()
        }
        CancelOutcome::AlreadyExecuted => {
            Problem::new(410, "already_executed", "Action already executed")
                .with_detail(
                    "the action has already executed; it is too late to cancel (local cancel only \
             wins before execution)",
                )
                .into_response()
        }
        CancelOutcome::NotFound => Problem::new(404, "not-found", "Action not found")
            .with_detail(format!("no pending action with id {id:?}"))
            .into_response(),
    }
}

/// The `202` body of a successful salvo fire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct FiredBody {
    /// The queued action id (also the pending-action + audit correlation id).
    pub action_id: String,
    /// The media-timeline timestamp (nanoseconds, from the ack-clock) the fire
    /// was queued — the audit entry and the pending action share this instant.
    pub queued_at_nanos: i64,
}

/// `POST /api/v1/salvos/{id}/fire` — fire a named salvo through the command bus
/// (role: write) → `202 {action_id, queued_at}`.
///
/// The named salvo must exist (else `404 salvo_unknown`). On success a `Salvo`
/// pending action is queued, the engine `TakeSalvo` command is submitted to the
/// bounded bus (a full bus sheds to `503`, never blocking the engine), the action
/// is marked executed, and the fire is recorded in the account audit store. The
/// action id is the pending-action id (the operator can correlate it on the
/// strip + the audit trail).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/salvos/{id}/fire",
        tag = "account",
        params(("id" = String, Path, description = "The named salvo to fire.")),
        responses(
            (status = 202, description = "Fire queued + submitted; outcome on the realtime stream.", body = FiredBody),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to fire.", body = crate::problem::Problem),
            (status = 404, description = "No salvo with that name.", body = crate::problem::Problem),
            (status = 503, description = "Engine command bus at capacity; shed.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn fire_salvo(
    State(state): State<AppState>,
    principal: Principal,
    Path(name): Path<String>,
) -> Response {
    if let Err(err) = principal.role.require(Action::Write) {
        return err.into_response();
    }
    // Per-object authz (BOLA): a scoped principal may only address its salvos.
    if let Err(err) = crate::auth::authorize_object(&principal, &name) {
        return err.into_response();
    }
    // The named salvo must exist — a clean 404 rather than an engine no-op.
    if state.salvos.get(&name).is_err() {
        return Problem::new(404, "salvo_unknown", "Unknown salvo")
            .with_detail(format!("no salvo named {name:?} is defined"))
            .into_response();
    }

    // Queue the action so the strip + audit can correlate it. The action id is
    // the pending-action id (a fresh op id reused as the action id).
    let op = OperationId::new();
    let action_id = op.to_string();
    let queued = state.ack_now();
    let _action: PendingAction = state.pending_actions.enqueue(
        action_id.clone(),
        PendingActionKind::Salvo,
        &principal.key_id,
        queued,
        Some(json!({ "salvo": name })),
    );

    // Submit the named-salvo take through the bounded, non-blocking bus. A full
    // bus sheds to 503 (invariant #10): the engine is never blocked. On a shed we
    // do NOT mark the action executed (it never reached the engine).
    let command = Command::TakeSalvo {
        op,
        salvo: Some(name.clone()),
        head: None,
    };
    if state.commands.try_submit(command).is_err() {
        // Shed: leave the action pending (it can be retried) and record the
        // request honestly — the fire was attempted but not executed.
        state.audit_account(
            &principal.key_id,
            AccountAuditKind::ActionRequested,
            Some(json!({ "salvo": name, "action_id": action_id, "shed": true })),
        );
        return Problem::new(503, "engine-busy", "Engine command bus at capacity")
            .with_detail("the control command queue is full; retry the fire shortly")
            .into_response();
    }

    // The command reached the engine: the action has executed (from the control
    // plane's perspective the fire was dispatched). Mark it + audit it.
    let _ = state.pending_actions.mark_executed(&action_id);
    state.audit_account(
        &principal.key_id,
        AccountAuditKind::ActionRequested,
        Some(json!({ "salvo": name, "action_id": action_id })),
    );
    state.audit_account(
        &principal.key_id,
        AccountAuditKind::ActionExecuted,
        Some(json!({ "salvo": name, "action_id": action_id })),
    );

    (
        StatusCode::ACCEPTED,
        Json(FiredBody {
            action_id,
            queued_at_nanos: queued.as_nanos(),
        }),
    )
        .into_response()
}
