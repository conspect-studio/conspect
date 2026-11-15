//! Scheduled fill refresh endpoints (issue #472). Every id is object-authorized
//! (fill and its player); another principal's schedule is indistinguishable
//! from an absent one.
use crate::{
    auth::{authorize_object, Action, Principal},
    error::{ControlError, ControlResult},
    fill_refresh::{FillOwner, FillRefreshStatus, FillSource, FillSpec, FirstDue},
    player_load::LoadThen,
    state::AppState,
    web_capture::{CaptureBody, WINDY_TERMS_VERSION},
};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;

/// Register (or replace) a runtime refresh schedule for one fill.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub(crate) struct RefreshScheduleBody {
    /// Refresh period in minutes, 1 through 1440.
    pub minutes: u32,
    /// Landing intent of each scheduled load (default resume).
    #[serde(default = "resume")]
    pub then: LoadThen,
    /// The capture submitted every run; `fill_id` must equal the path id and
    /// `target_player` must name an existing player.
    pub capture: CaptureBody,
    /// Run the first refresh in the current slot instead of the next one.
    #[serde(default)]
    pub start_now: bool,
}
const fn resume() -> LoadThen {
    LoadThen::Resume
}

fn visible(principal: &Principal, s: &FillRefreshStatus) -> bool {
    authorize_object(principal, &s.fill_id).is_ok()
        && authorize_object(principal, &s.player).is_ok()
}

/// Inside the mutation lock: an existing schedule the caller cannot fully
/// see (fill or ANY player it targets, including a staged replacement) is
/// answered exactly like an absent one, before its source is inspected.
fn existing_visible(
    core: &crate::fill_refresh::Scheduler,
    principal: &Principal,
    fill: &str,
) -> ControlResult<Option<FillSource>> {
    let Some(players) = core.players(fill) else {
        return Ok(None);
    };
    if authorize_object(principal, fill).is_err()
        || players
            .iter()
            .any(|player| authorize_object(principal, player).is_err())
    {
        return Err(missing(fill));
    }
    Ok(core.source(fill))
}

fn epoch_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn missing(id: &str) -> ControlError {
    ControlError::NotFound {
        kind: "fill-refresh",
        id: id.to_owned(),
    }
}

#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/media/fills/refresh",
        operation_id = "list_fill_refresh",
        tag = "media-captures",
        responses(
            (status = 200, description = "Refresh status of every visible scheduled fill.", body = [crate::fill_refresh::FillRefreshStatus]),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to read.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn list(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<Vec<FillRefreshStatus>>> {
    principal.role.require(Action::Read)?;
    Ok(Json(
        state
            .fill_refresh
            .status()
            .iter()
            .filter(|s| visible(&principal, s))
            .cloned()
            .collect(),
    ))
}

#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/media/fills/{id}/refresh",
        operation_id = "get_fill_refresh",
        tag = "media-captures",
        params(("id" = String, Path, description = "Fill id.")),
        responses(
            (status = 200, description = "Refresh status of one scheduled fill.", body = crate::fill_refresh::FillRefreshStatus),
            (status = 403, description = "Not authorized for this fill.", body = crate::problem::Problem),
            (status = 404, description = "No schedule for this fill.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Json<FillRefreshStatus>> {
    principal.role.require(Action::Read)?;
    authorize_object(&principal, &id)?;
    state
        .fill_refresh
        .status()
        .iter()
        .find(|s| s.fill_id == id && visible(&principal, s))
        .cloned()
        .map(Json)
        .ok_or_else(|| missing(&id))
}

#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        put,
        path = "/api/v1/media/fills/{id}/refresh",
        operation_id = "put_fill_refresh",
        tag = "media-captures",
        params(("id" = String, Path, description = "Fill id.")),
        request_body = RefreshScheduleBody,
        responses(
            (status = 200, description = "Schedule installed; runs as the calling key.", body = crate::fill_refresh::FillRefreshStatus),
            (status = 403, description = "Not authorized for this fill or player.", body = crate::problem::Problem),
            (status = 404, description = "Unknown target player.", body = crate::problem::Problem),
            (status = 409, description = "Config-declared fill or schedule capacity reached.", body = crate::problem::Problem),
            (status = 422, description = "Invalid interval, body identity or terms.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn put(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
    Json(body): Json<RefreshScheduleBody>,
) -> ControlResult<Json<FillRefreshStatus>> {
    principal.role.require(Action::Write)?;
    authorize_object(&principal, &id)?;
    let player = body
        .capture
        .target_player
        .clone()
        .ok_or_else(|| ControlError::Validation("capture.target_player is required".into()))?;
    authorize_object(&principal, &player)?;
    state.media_players.get(&player)?;
    if body.capture.fill_id != id {
        return Err(ControlError::Validation(
            "capture.fill_id must equal the path fill id".into(),
        ));
    }
    if !body.capture.terms.accepted || body.capture.terms.version != WINDY_TERMS_VERSION {
        return Err(ControlError::Validation(
            "current Windy terms acceptance required".into(),
        ));
    }
    let spec = FillSpec {
        fill_id: id.clone(),
        player,
        minutes: body.minutes,
        then: body.then,
        body: body.capture,
        owner: FillOwner::Key(principal.key_id.clone()),
        source: FillSource::Api,
    };
    let first = if body.start_now {
        FirstDue::CurrentSlot
    } else {
        FirstDue::NextSlot
    };
    let refresh = state.fill_refresh.clone();
    let fill = id.clone();
    let actor = principal.clone();
    tokio::task::spawn_blocking(move || {
        refresh.with_core(|core| {
            if existing_visible(core, &actor, &fill)? == Some(FillSource::Config) {
                return Err(ControlError::Conflict(
                    "fill is declared in config; edit [[media_fill]] instead".into(),
                ));
            }
            core.upsert(spec, epoch_now(), first)
        })
    })
    .await
    .map_err(|_| ControlError::Repository("fill refresh worker failed".into()))??;
    state.audit(
        &principal.key_id,
        crate::audit::AuditAction::Update,
        "fill-refresh",
        &id,
        Some(serde_json::json!({"minutes": body.minutes})),
    );
    state
        .fill_refresh
        .status()
        .iter()
        .find(|s| s.fill_id == id)
        .cloned()
        .map(Json)
        .ok_or_else(|| missing(&id))
}

#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        delete,
        path = "/api/v1/media/fills/{id}/refresh",
        operation_id = "delete_fill_refresh",
        tag = "media-captures",
        params(("id" = String, Path, description = "Fill id.")),
        responses(
            (status = 204, description = "Schedule removed; the current clip keeps playing."),
            (status = 403, description = "Not authorized for this fill.", body = crate::problem::Problem),
            (status = 404, description = "No schedule for this fill.", body = crate::problem::Problem),
            (status = 409, description = "Config-declared fill.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn delete(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<StatusCode> {
    principal.role.require(Action::Write)?;
    authorize_object(&principal, &id)?;
    let refresh = state.fill_refresh.clone();
    let fill = id.clone();
    let actor = principal.clone();
    tokio::task::spawn_blocking(move || {
        refresh.with_core(|core| match existing_visible(core, &actor, &fill)? {
            None => Err(missing(&fill)),
            Some(FillSource::Config) => Err(ControlError::Conflict(
                "fill is declared in config; edit [[media_fill]] instead".into(),
            )),
            Some(FillSource::Api) => {
                core.remove(&fill);
                Ok(())
            }
        })
    })
    .await
    .map_err(|_| ControlError::Repository("fill refresh worker failed".into()))??;
    state.audit(
        &principal.key_id,
        crate::audit::AuditAction::Delete,
        "fill-refresh",
        &id,
        None,
    );
    Ok(StatusCode::NO_CONTENT)
}
