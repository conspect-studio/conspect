//! Authorized read-only source event snapshots, and the operator
//! test-injection aid (ADR-0183 item 5). No automation hooks live here.
use crate::{
    audit::AuditAction,
    auth::{authorize_object, Action, Principal},
    problem::Problem,
    source_events::{TestEventBoundary, TestEventOutcome},
    state::AppState,
};
use axum::{
    extract::{Path, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;

#[cfg_attr(feature = "openapi", utoipa::path(
    get, path = "/api/v1/inputs/{id}/events", tag = "inputs",
    params(("id" = String, Path, description = "Exact source identity; no suffix normalization")),
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Bounded observational snapshot (possibly empty). Cache-Control: no-store.", body = crate::source_events::SourceEventsDto),
        (status = 401, description = "Missing or invalid credentials", body = Problem),
        (status = 403, description = "Source outside principal scope", body = Problem),
        (status = 404, description = "Unknown source", body = Problem),
        (status = 503, description = "Source event provider unavailable", body = Problem),
    )
))]
pub(crate) async fn get_source_events(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> Response {
    let mut response = if let Err(error) = principal.role.require(Action::Read) {
        error.into_response()
    } else if let Err(error) = authorize_object(&principal, &id) {
        error.into_response()
    } else if let Some(provider) = &state.source_events {
        match provider.source_events(&id) {
            Some(snapshot) if snapshot.matches_source(&id) => Json(snapshot).into_response(),
            Some(_) => Problem::new(
                503,
                "source-events-unavailable",
                "Invalid source event snapshot",
            )
            .into_response(),
            None => Problem::new(404, "not-found", "Unknown source").into_response(),
        }
    } else {
        Problem::new(
            503,
            "source-events-unavailable",
            "Source event provider unavailable",
        )
        .into_response()
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Request body for the operator test-injection aid.
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub(crate) struct TestEventRequest {
    /// `"start"` or `"end"` — see [`TestEventBoundary`].
    boundary: TestEventRequestBoundary,
    /// Declared break duration in nanoseconds (only meaningful for `"start"`).
    #[serde(default)]
    duration_ns: Option<i64>,
}
/// Wire boundary selector, internally tagged (never untagged).
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub(crate) enum TestEventRequestBoundary {
    /// Announce/confirm a synthetic break start.
    Start,
    /// Confirm a synthetic break end.
    End,
}

/// `POST /api/v1/inputs/{id}/events/test` — inject a clearly-labelled synthetic
/// event for operator/automation testing (ADR-0183 item 5; role: administer).
///
/// This can only affect a source that already has an ACTIVE event-tracking
/// generation (a source the running engine has ingested, live or previously
/// connected) — it never allocates a new source identity or epoch, and the
/// injected evidence is tagged `origin: "external"` with a `test:` event id
/// prefix so it is unmistakably distinguishable from real evidence in the
/// API/UI. It never bypasses the bounded worker queue or payload limits real
/// evidence uses.
#[cfg_attr(feature = "openapi", utoipa::path(
    post, path = "/api/v1/inputs/{id}/events/test", tag = "inputs",
    params(("id" = String, Path, description = "Exact source identity; no suffix normalization")),
    request_body = TestEventRequest,
    security(("bearer_auth" = [])),
    responses(
        (status = 202, description = "Synthetic observation accepted into the bounded worker queue."),
        (status = 401, description = "Missing or invalid credentials", body = Problem),
        (status = 403, description = "Source outside principal scope, or insufficient role", body = Problem),
        (status = 404, description = "Unknown or never-active source", body = Problem),
        (status = 503, description = "Source event test-injection adapter unavailable, or the queue is full", body = Problem),
    )
))]
pub(crate) async fn post_source_event_test(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
    Json(request): Json<TestEventRequest>,
) -> Response {
    if let Err(error) = principal.role.require(Action::Administer) {
        return error.into_response();
    }
    if let Err(error) = authorize_object(&principal, &id) {
        return error.into_response();
    }
    let Some(injector) = &state.source_event_test_injector else {
        return Problem::new(
            503,
            "source-events-unavailable",
            "Source event test-injection adapter unavailable",
        )
        .into_response();
    };
    let boundary = match request.boundary {
        TestEventRequestBoundary::Start => TestEventBoundary::Start {
            duration_ns: request.duration_ns,
        },
        TestEventRequestBoundary::End => TestEventBoundary::End,
    };
    let outcome = injector.inject_test_event(&id, boundary);
    let response = match outcome {
        TestEventOutcome::Queued => StatusCode::ACCEPTED.into_response(),
        TestEventOutcome::Overflow => Problem::new(
            503,
            "source-events-unavailable",
            "Source event queue is full; this source's evidence is now degraded",
        )
        .into_response(),
        TestEventOutcome::Unknown => {
            Problem::new(404, "not-found", "Unknown or never-active source").into_response()
        }
    };
    if matches!(outcome, TestEventOutcome::Queued) {
        state.audit(
            &principal.key_id,
            AuditAction::Command,
            "source",
            &id,
            Some(serde_json::json!({ "command": "source_event_test", "boundary": match request.boundary {
                TestEventRequestBoundary::Start => "start",
                TestEventRequestBoundary::End => "end",
            } })),
        );
    }
    response
}

/// Automation rule + live status for one source (ADR-0183).
#[derive(Debug, serde::Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub(crate) struct AutomationView {
    /// The configured rule (absent when none is configured).
    #[cfg_attr(feature = "openapi", schema(value_type = Option<crate::openapi_schemas::EventAutomationRuleDoc>))]
    rule: Option<multiview_config::schema::EventAutomationRule>,
    /// Live executor status.
    status: crate::automation::AutomationStatus,
    /// Bounded action log, newest first.
    log: Vec<crate::automation::AutomationLogEntry>,
    /// Ids of configured `bars` sources (used by the `bars`/`bars_tone` targets).
    bars_sources: Vec<String>,
    /// Whether the executor is running in this process.
    executor_running: bool,
}

/// Request body for `PUT .../events/automation`: the rule, or `null` to remove it.
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub(crate) struct AutomationPut {
    /// Replacement rule; `null` deletes it. `armed` here is ignored unless the
    /// rule is already armed — arm with `POST .../arm` (explicit confirmation).
    #[cfg_attr(feature = "openapi", schema(value_type = Option<crate::openapi_schemas::EventAutomationRuleDoc>))]
    rule: Option<multiview_config::schema::EventAutomationRule>,
}

fn automation_view(state: &AppState, id: &str) -> crate::error::ControlResult<AutomationView> {
    let sources: Vec<serde_json::Value> = crate::routes::sources::accepted_sources(state)?
        .into_iter()
        .map(|entry| entry.resource.body)
        .collect();
    let mut found = false;
    let mut rule = None;
    let mut bars_sources = Vec::new();
    for body in sources {
        let Ok(source) = serde_json::from_value::<multiview_config::Source>(body) else {
            continue;
        };
        if matches!(source.kind, multiview_config::SourceKind::Bars) {
            bars_sources.push(source.id.clone());
        }
        if source.id == id {
            found = true;
            rule = source.events.and_then(|e| e.automation);
        }
    }
    if !found {
        return Err(crate::error::ControlError::NotFound {
            kind: "source",
            id: id.to_owned(),
        });
    }
    let (status, log) = state.automation.as_ref().map_or_else(
        crate::automation::AutomationExecutor::idle_status,
        |executor| executor.status(id),
    );
    Ok(AutomationView {
        rule,
        status,
        log,
        bars_sources,
        executor_running: state.automation.is_some(),
    })
}

fn authorize(principal: &Principal, id: &str, action: Action) -> crate::error::ControlResult<()> {
    principal.role.require(action)?;
    authorize_object(principal, id)?;
    Ok(())
}

/// `GET /api/v1/inputs/{id}/events/automation` — rule, live status and log.
#[cfg_attr(feature = "openapi", utoipa::path(
    get, path = "/api/v1/inputs/{id}/events/automation", tag = "inputs",
    params(("id" = String, Path, description = "Exact source identity")),
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Automation rule, live status and bounded action log.", body = AutomationView),
        (status = 401, description = "Missing or invalid credentials", body = Problem),
        (status = 403, description = "Source outside principal scope", body = Problem),
        (status = 404, description = "Unknown source", body = Problem),
    )
))]
pub(crate) async fn get_automation(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> crate::error::ControlResult<Response> {
    authorize(&principal, &id, Action::Read)?;
    let mut response = Json(automation_view(&state, &id)?).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

fn authorize_rule_targets(
    principal: &Principal,
    rule: &multiview_config::schema::EventAutomationRule,
) -> crate::error::ControlResult<()> {
    let target = rule.on_break_start.target.as_str();
    if target != crate::automation::TARGET_BARS && target != crate::automation::TARGET_BARS_TONE {
        authorize_object(principal, target)?;
    }
    for cell in rule.on_break_start.cells.iter().flatten() {
        authorize_object(principal, cell)?;
    }
    Ok(())
}

/// `PUT /api/v1/inputs/{id}/events/automation` — replace (or remove) the rule,
/// persisted through the canonical durable source path. Never arms: a new or
/// edited rule keeps the CURRENT armed state (disarmed if new).
#[cfg_attr(feature = "openapi", utoipa::path(
    put, path = "/api/v1/inputs/{id}/events/automation", tag = "inputs",
    params(("id" = String, Path, description = "Exact source identity")),
    request_body = AutomationPut,
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "The stored rule and live status.", body = AutomationView),
        (status = 401, description = "Missing or invalid credentials", body = Problem),
        (status = 403, description = "Source, target or cell outside principal scope, or insufficient role", body = Problem),
        (status = 404, description = "Unknown source", body = Problem),
        (status = 422, description = "Invalid rule", body = Problem),
    )
))]
pub(crate) async fn put_automation(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
    Json(request): Json<AutomationPut>,
) -> crate::error::ControlResult<Response> {
    authorize(&principal, &id, Action::Write)?;
    if let Some(rule) = &request.rule {
        authorize_rule_targets(&principal, rule)?;
        rule.validate(&id)
            .map_err(crate::error::ControlError::Validation)?;
    }
    let rule = request.rule;
    crate::routes::sources::update_source_automation(&state, &principal.key_id, &id, |body| {
        let events = body
            .entry("events")
            .or_insert_with(|| serde_json::json!({}));
        let events = events.as_object_mut().ok_or_else(|| {
            crate::error::ControlError::Validation("source events is not an object".into())
        })?;
        let was_armed = events
            .get("automation")
            .and_then(|a| a.get("armed"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        match rule {
            Some(mut rule) => {
                rule.armed = was_armed;
                let value = serde_json::to_value(rule)
                    .map_err(|e| crate::error::ControlError::Repository(e.to_string()))?;
                events.insert("automation".into(), value);
            }
            None => {
                events.remove("automation");
            }
        }
        if events.is_empty() {
            body.remove("events");
        }
        Ok(())
    })
    .await?;
    if let Some(executor) = &state.automation {
        executor.invalidate_config();
    }
    Ok(Json(automation_view(&state, &id)?).into_response())
}

async fn set_armed(
    state: &AppState,
    principal: &Principal,
    id: &str,
    armed: bool,
) -> crate::error::ControlResult<Response> {
    authorize(principal, id, Action::Write)?;
    // Re-authorize the stored targets for THIS principal before arming.
    if armed {
        if let Some(rule) = automation_view(state, id)?.rule {
            authorize_rule_targets(principal, &rule)?;
        }
    }
    crate::routes::sources::update_source_automation(state, &principal.key_id, id, |body| {
        let automation = body
            .get_mut("events")
            .and_then(|e| e.get_mut("automation"))
            .and_then(serde_json::Value::as_object_mut)
            .ok_or_else(|| {
                crate::error::ControlError::Validation(
                    "no automation rule is configured for this source".into(),
                )
            })?;
        automation.insert("armed".into(), serde_json::Value::Bool(armed));
        Ok(())
    })
    .await?;
    state.audit(
        &principal.key_id,
        AuditAction::Command,
        "source",
        id,
        Some(serde_json::json!({ "command": if armed { "automation_arm" } else { "automation_disarm" } })),
    );
    if let Some(executor) = &state.automation {
        executor.invalidate_config();
    }
    Ok(Json(automation_view(state, id)?).into_response())
}

/// `POST /api/v1/inputs/{id}/events/automation/arm` — arm the stored rule.
#[cfg_attr(feature = "openapi", utoipa::path(
    post, path = "/api/v1/inputs/{id}/events/automation/arm", tag = "inputs",
    params(("id" = String, Path, description = "Exact source identity")),
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Armed; rule and live status.", body = AutomationView),
        (status = 401, description = "Missing or invalid credentials", body = Problem),
        (status = 403, description = "Source, target or cell outside principal scope, or insufficient role", body = Problem),
        (status = 404, description = "Unknown source", body = Problem),
        (status = 422, description = "No rule configured", body = Problem),
    )
))]
pub(crate) async fn post_automation_arm(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> crate::error::ControlResult<Response> {
    set_armed(&state, &principal, &id, true).await
}

/// `POST /api/v1/inputs/{id}/events/automation/disarm` — disarm. A break in
/// progress is restored by the executor on its next tick.
#[cfg_attr(feature = "openapi", utoipa::path(
    post, path = "/api/v1/inputs/{id}/events/automation/disarm", tag = "inputs",
    params(("id" = String, Path, description = "Exact source identity")),
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Disarmed; rule and live status.", body = AutomationView),
        (status = 401, description = "Missing or invalid credentials", body = Problem),
        (status = 403, description = "Source outside principal scope, or insufficient role", body = Problem),
        (status = 404, description = "Unknown source", body = Problem),
        (status = 422, description = "No rule configured", body = Problem),
    )
))]
pub(crate) async fn post_automation_disarm(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> crate::error::ControlResult<Response> {
    set_armed(&state, &principal, &id, false).await
}

/// `POST /api/v1/inputs/{id}/events/automation/restore` — restore now.
#[cfg_attr(feature = "openapi", utoipa::path(
    post, path = "/api/v1/inputs/{id}/events/automation/restore", tag = "inputs",
    params(("id" = String, Path, description = "Exact source identity")),
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Restore dispatched; rule and live status.", body = AutomationView),
        (status = 401, description = "Missing or invalid credentials", body = Problem),
        (status = 403, description = "Source outside principal scope, or insufficient role", body = Problem),
        (status = 404, description = "Unknown source", body = Problem),
        (status = 409, description = "Nothing to restore, or an operator change already took over", body = Problem),
        (status = 503, description = "Executor unavailable or command bus busy", body = Problem),
    )
))]
pub(crate) async fn post_automation_restore(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> crate::error::ControlResult<Response> {
    authorize(&principal, &id, Action::Write)?;
    // 404 for unknown sources before touching the executor.
    let _ = automation_view(&state, &id)?;
    let Some(executor) = &state.automation else {
        return Ok(Problem::new(
            503,
            "automation-unavailable",
            "Event automation executor is not running",
        )
        .into_response());
    };
    let result = executor.manual_restore(&id);
    state.audit(
        &principal.key_id,
        AuditAction::Command,
        "source",
        &id,
        Some(serde_json::json!({ "command": "automation_restore", "ok": result.is_ok() })),
    );
    match result {
        Ok(()) => Ok(Json(automation_view(&state, &id)?).into_response()),
        Err(crate::automation::ManualRestoreError::NothingToRestore) => Ok(Problem::new(
            409,
            "nothing-to-restore",
            "No automated replacement is on air for this source",
        )
        .into_response()),
        Err(crate::automation::ManualRestoreError::ManualOverride) => Ok(Problem::new(
            409,
            "manual-override",
            "An operator change took over; automation will not overwrite it",
        )
        .into_response()),
        Err(crate::automation::ManualRestoreError::Busy) => Ok(Problem::new(
            503,
            "engine-busy",
            "Command bus busy; the restore will retry automatically",
        )
        .into_response()),
    }
}
