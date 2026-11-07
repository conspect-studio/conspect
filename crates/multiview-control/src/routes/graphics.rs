//! The Graphics (CG) API under `/api/v1/graphics`.
//!
//! * `data-sources` — typed providers (`bom_weather` today), with status and
//!   freshness for weather sources.
//! * `templates` — the built-in template registry (read-only) + sample render.
//! * `items` — prepared graphics (template + data binding + style) + thumbnail.
//! * `channels` — playout channels: rundown (GET/PUT) and the playout verbs
//!   `cue` / `take` / `clear` / `next`, with PVW/PGM status.
//!
//! Definitions persist through the canonical desired-state commit
//! (`boot_model::commit_resource`) as the `[graphics]` section, and every data
//! source, item and channel carries a durable `ETag` from the envelope.
//! `If-Match` is enforced when sent (`412`) and optional otherwise; the first
//! mutation migrates legacy `[weather] locations` into
//! `[graphics.data_sources]`; `bom_weather` sources are also mirrored into
//! `[weather] locations` so a rollback to a pre-graphics build keeps them. Playout state is
//! runtime-only. Every id is authorized per object (BOLA); lists drop
//! out-of-scope rows; every mutation is audited; playout changes publish a
//! `graphics.channel_state` realtime event.
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use multiview_config::{GraphicsConfig, GraphicsItem, MultiviewConfig, WeatherConfig};
use multiview_graphics::{ChannelSnapshot, PlayoutError, PREVIEW_SUFFIX};
use serde::{Deserialize, Serialize};

use crate::audit::AuditAction;
use crate::auth::{Action, Principal};
use crate::boot_model::{GRAPHICS_CHANNELS_KEY, GRAPHICS_DATA_SOURCES_KEY, GRAPHICS_ITEMS_KEY};
use crate::concurrency::IfMatch;
use crate::error::{ControlError, ControlResult};
use crate::state::AppState;

/// Audit / error kind for a data source.
pub const DATA_SOURCE_KIND: &str = "graphics_data_source";
/// Audit / error kind for an item.
pub const ITEM_KIND: &str = "graphics_item";
/// Audit / error kind for a channel.
pub const CHANNEL_KIND: &str = "graphics_channel";
/// Audit / error kind for a template.
pub const TEMPLATE_KIND: &str = "graphics_template";

// ------------------------------------------------------------------ renderer

/// Off-clock thumbnail renderer the binary installs (the in-memory harness
/// has none). Called from `spawn_blocking`, never on an engine thread.
pub trait GraphicsThumbnailer: Send + Sync {
    /// Render `item` (as configured) to a JPEG, or `None` when this build
    /// cannot render it.
    fn item_jpeg(&self, item: &GraphicsItem) -> Option<Vec<u8>>;
}

/// The default: no renderer (thumbnail endpoints answer 503).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoThumbnails;

impl GraphicsThumbnailer for NoThumbnails {
    fn item_jpeg(&self, _item: &GraphicsItem) -> Option<Vec<u8>> {
        None
    }
}

/// Shared thumbnailer handle.
pub type SharedThumbnailer = Arc<dyn GraphicsThumbnailer>;

// ---------------------------------------------------------------------- docs

/// A data source document (`[[graphics.data_sources]]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DataSourceDoc {
    /// Data source id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Provider kind (`bom_weather`).
    pub kind: String,
    /// BOM forecast product id (`bom_weather`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forecast_product: Option<String>,
    /// BOM forecast area code (`bom_weather`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aac: Option<String>,
    /// BOM observation product id (`bom_weather`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_product: Option<String>,
    /// Observation station WMO id (`bom_weather`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub station_wmo: Option<String>,
}

/// `PUT` body for a data source.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct DataSourceInput {
    /// Display name.
    pub name: String,
    /// Provider kind (`bom_weather`).
    pub kind: String,
    /// BOM forecast product id.
    #[serde(default)]
    pub forecast_product: Option<String>,
    /// BOM forecast area code.
    #[serde(default)]
    pub aac: Option<String>,
    /// BOM observation product id.
    #[serde(default)]
    pub observation_product: Option<String>,
    /// Observation station WMO id.
    #[serde(default)]
    pub station_wmo: Option<String>,
}

/// A data source with its live status.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DataSourceView {
    /// The configured source.
    pub source: DataSourceDoc,
    /// `ok` | `pending` | `error` | `expired`.
    pub status: String,
    /// Weather data + freshness (`bom_weather` only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weather: Option<super::weather::WeatherLocationView>,
    /// Item ids bound to this source.
    pub used_by: Vec<String>,
}

/// A built-in template.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct TemplateDoc {
    /// Template id (`family.variant`).
    pub id: String,
    /// Template family (`weather`).
    pub family: String,
    /// Display name.
    pub name: String,
    /// What it draws.
    pub description: String,
    /// The data kind it binds (`weather`), or null.
    pub data_kind: Option<String>,
    /// Data fields it reads.
    pub fields: Vec<String>,
    /// Style options it honours.
    pub style_options: Vec<String>,
}

/// Item style.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct StyleDoc {
    /// Theme name (reserved).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// Accent colour `#RRGGBB`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accent: Option<String>,
    /// `gradient` (opaque) or `transparent`.
    #[serde(default = "default_background")]
    pub background: String,
}

fn default_background() -> String {
    "gradient".to_owned()
}

impl Default for StyleDoc {
    fn default() -> Self {
        Self {
            theme: None,
            accent: None,
            background: default_background(),
        }
    }
}

/// A graphics item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ItemDoc {
    /// Item id.
    pub id: String,
    /// Operator-facing name.
    pub name: String,
    /// Template id.
    pub template: String,
    /// Bound data source id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_source: Option<String>,
    /// Provider-specific key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Style options.
    #[serde(default)]
    pub style: StyleDoc,
}

/// `PUT` body for an item.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ItemInput {
    /// Operator-facing name.
    pub name: String,
    /// Template id.
    pub template: String,
    /// Bound data source id.
    #[serde(default)]
    pub data_source: Option<String>,
    /// Provider-specific key.
    #[serde(default)]
    pub key: Option<String>,
    /// Style options.
    #[serde(default)]
    pub style: Option<StyleDoc>,
}

/// A transition preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct TransitionDoc {
    /// `cut` or `mix`.
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub kind: multiview_config::TransitionKind,
    /// Mix duration in output frames (1–600).
    pub frames: u32,
}

/// A channel with its live PVW/PGM state.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ChannelView {
    /// Channel id.
    pub id: String,
    /// Operator-facing name.
    pub name: String,
    /// Ordered item ids.
    pub rundown: Vec<String>,
    /// Transition preset.
    pub transition: TransitionDoc,
    /// Cued item id.
    pub preview: Option<String>,
    /// On-air item id.
    pub program: Option<String>,
    /// Increments on every program change.
    pub seq: u64,
    /// Suggested source id carrying the program bus (`<channel>`).
    pub program_source: String,
    /// Suggested source id carrying the preview bus (`<channel>-preview`).
    pub preview_source: String,
}

/// `PUT /channels/{id}/rundown` body.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct RundownInput {
    /// Ordered item ids (full replacement).
    pub rundown: Vec<String>,
    /// New transition preset (unchanged when absent).
    #[serde(default)]
    pub transition: Option<TransitionDoc>,
    /// New channel name (unchanged when absent; required to create).
    #[serde(default)]
    pub name: Option<String>,
}

/// `POST take|clear|next` body (all fields optional; send `{}`).
#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct PlayoutInput {
    /// One-shot transition override (the channel preset otherwise).
    #[serde(default)]
    pub transition: Option<TransitionDoc>,
}

/// `POST cue` body.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct CueInput {
    /// The item to put on preview.
    pub item: String,
}

// ------------------------------------------------------------------- helpers

fn validation(e: impl std::fmt::Display) -> ControlError {
    ControlError::Validation(e.to_string())
}

fn source_doc(d: &multiview_config::GraphicsDataSource) -> ControlResult<DataSourceDoc> {
    serde_json::to_value(d)
        .and_then(serde_json::from_value)
        .map_err(|e| ControlError::Repository(format!("data source serialization: {e}")))
}

fn item_doc(i: &GraphicsItem) -> ControlResult<ItemDoc> {
    serde_json::to_value(i)
        .and_then(serde_json::from_value)
        .map_err(|e| ControlError::Repository(format!("item serialization: {e}")))
}

fn transition_of(t: TransitionDoc) -> multiview_config::Transition {
    multiview_config::Transition::new(t.kind, t.frames)
}

fn channel_view(state: &AppState, ch: &multiview_config::GraphicsChannel) -> ChannelView {
    let snap = state.graphics.channel(&ch.id);
    ChannelView {
        id: ch.id.clone(),
        name: ch.name.clone(),
        rundown: ch.rundown.clone(),
        transition: TransitionDoc {
            kind: ch.transition.kind,
            frames: ch.transition.frames,
        },
        preview: snap.as_ref().and_then(|s| s.preview.clone()),
        program: snap.as_ref().and_then(|s| s.program.clone()),
        seq: snap.map_or(0, |s| s.seq),
        program_source: ch.id.clone(),
        preview_source: format!("{}{PREVIEW_SUFFIX}", ch.id),
    }
}

/// The weather poller specs for every `bom_weather` source in `config`.
#[must_use]
pub fn weather_specs(config: &GraphicsConfig) -> Vec<multiview_weather::service::LocationSpec> {
    let mut locations = WeatherConfig::default();
    locations.locations = config
        .data_sources
        .iter()
        .filter_map(multiview_config::GraphicsDataSource::as_weather_location)
        .collect();
    super::weather::location_specs(&locations)
}

/// The effective graphics config for a loaded document: legacy `[weather]`
/// locations folded in as `bom_weather` data sources (used at boot and by the
/// config-file watcher).
#[must_use]
pub fn effective_graphics(config: &MultiviewConfig) -> GraphicsConfig {
    let mut g = (*config.graphics).clone();
    g.data_sources = multiview_config::graphics::effective_data_sources(&g, &config.weather);
    g
}

/// Persist `next` as `[graphics]` (plus the `[weather]` mirror of its
/// `bom_weather` sources) through the canonical desired
/// commit, then install it in the runtime hub and the weather poller. Call
/// with the config-mutation lock held.
///
/// # Errors
/// Validation (422) or a failed durable commit.
pub async fn commit_graphics(state: &AppState, next: GraphicsConfig) -> ControlResult<()> {
    next.validate(&WeatherConfig::default())
        .map_err(validation)?;
    let body = serde_json::to_value(&next)
        .map_err(|e| ControlError::Repository(format!("graphics serialization: {e}")))?;
    let committed = crate::boot_model::commit_resource(
        state,
        crate::boot_model::GRAPHICS_KEY,
        "section",
        Some(body),
        None,
    )
    .await;
    // An indeterminate commit is already visible on disk, so the hub (the base
    // of the next mutation) must follow it even though the request reports it.
    if matches!(
        committed,
        Ok(()) | Err(ControlError::CommitIndeterminate(_))
    ) {
        state.weather.set_locations(weather_specs(&next));
        state.graphics.set_config(next);
    }
    committed
}

/// Attach the durable `ETag` of one graphics resource. A run without a boot
/// model has no durable version, and a failed reread omits the header rather
/// than failing an already committed write.
pub(crate) fn with_version(
    state: &AppState,
    key: &str,
    id: &str,
    mut response: Response,
) -> Response {
    if let Ok(Some(version)) = crate::boot_model::accepted_version(state, key, id) {
        if let Ok(value) = header::HeaderValue::from_str(&version.to_etag()) {
            response.headers_mut().insert(header::ETAG, value);
        }
    }
    response
}

/// Enforce an `If-Match` the client sent. It is optional here: these routes
/// predate `ETags`, and existing clients write without one. Call with the
/// config-mutation lock held.
///
/// # Errors
/// A stale tag, or any tag for a resource that does not exist, is `412`.
pub(crate) fn check_if_match(
    state: &AppState,
    if_match: &IfMatch,
    target: (&str, &'static str),
    id: &str,
    exists: bool,
) -> ControlResult<()> {
    let (key, kind) = target;
    let Some(raw) = &if_match.raw else {
        return Ok(());
    };
    if !exists {
        return Err(ControlError::VersionConflict {
            kind,
            id: id.to_owned(),
            expected: raw.clone(),
            actual: "absent".to_owned(),
        });
    }
    match crate::boot_model::accepted_version(state, key, id)? {
        Some(current) => if_match.require(kind, id, current),
        None => Ok(()),
    }
}

fn current(state: &AppState) -> GraphicsConfig {
    (*state.graphics.config()).clone()
}

fn publish(state: &AppState, snap: &ChannelSnapshot, action: &str) {
    state
        .engine
        .publish_event(multiview_events::Event::GraphicsChannelState(
            multiview_events::GraphicsChannelEvent {
                channel: snap.channel.clone(),
                preview: snap.preview.clone(),
                program: snap.program.clone(),
                seq: snap.seq,
                action: action.to_owned(),
            },
        ));
}

fn playout_err(e: PlayoutError) -> ControlError {
    match e {
        PlayoutError::UnknownChannel(id) => ControlError::NotFound {
            kind: CHANNEL_KIND,
            id,
        },
        PlayoutError::UnknownItem(id) => ControlError::NotFound {
            kind: ITEM_KIND,
            id,
        },
        other => ControlError::Conflict(other.to_string()),
    }
}

fn jpeg(bytes: Vec<u8>) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "image/jpeg"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        bytes,
    )
        .into_response()
}

async fn render_thumb(state: &AppState, item: GraphicsItem) -> ControlResult<Response> {
    let thumbs = Arc::clone(&state.graphics_thumbnails);
    let bytes = tokio::task::spawn_blocking(move || thumbs.item_jpeg(&item))
        .await
        .map_err(|e| ControlError::Repository(format!("thumbnail task failed: {e}")))?;
    match bytes {
        Some(b) => Ok(jpeg(b)),
        None => Ok((
            StatusCode::SERVICE_UNAVAILABLE,
            "graphics rendering is not available in this build",
        )
            .into_response()),
    }
}

// --------------------------------------------------------------- data sources

fn data_source_view(
    state: &AppState,
    config: &GraphicsConfig,
    d: &multiview_config::GraphicsDataSource,
) -> ControlResult<DataSourceView> {
    let mut single = GraphicsConfig::default();
    single.data_sources.push(d.clone());
    let weather = weather_specs(&single)
        .first()
        .map(|spec| super::weather::view_of(state, spec));
    Ok(DataSourceView {
        source: source_doc(d)?,
        status: weather
            .as_ref()
            .map_or_else(|| "ok".to_owned(), |w| w.status.clone()),
        weather,
        used_by: config
            .items
            .iter()
            .filter(|i| i.data_source.as_deref() == Some(d.id.as_str()))
            .map(|i| i.id.clone())
            .collect(),
    })
}

/// `GET /api/v1/graphics/data-sources` (role: read; scoped rows only).
#[cfg_attr(feature = "openapi", utoipa::path(get, path = "/api/v1/graphics/data-sources", tag = "graphics",
    responses((status = 200, description = "In-scope data sources with status.", body = [DataSourceView]),
        (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem))))]
pub(crate) async fn list_data_sources(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<Vec<DataSourceView>>> {
    principal.role.require(Action::Read)?;
    let config = state.graphics.config();
    let mut out = Vec::new();
    for d in &config.data_sources {
        if crate::auth::authorize_object(&principal, &d.id).is_ok() {
            out.push(data_source_view(&state, &config, d)?);
        }
    }
    out.sort_by(|a, b| a.source.id.cmp(&b.source.id));
    Ok(Json(out))
}

/// `GET /api/v1/graphics/data-sources/{id}` (role: read; per-object).
#[cfg_attr(feature = "openapi", utoipa::path(get, path = "/api/v1/graphics/data-sources/{id}", tag = "graphics",
    params(("id" = String, Path, description = "Data source id.")),
    responses((status = 200, description = "The data source.", body = DataSourceView),
        (status = 403, description = "Out of scope.", body = crate::problem::Problem),
        (status = 404, description = "Unknown id.", body = crate::problem::Problem))))]
pub(crate) async fn get_data_source(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Read)?;
    crate::auth::authorize_object(&principal, &id)?;
    // The envelope commits before the hub follows, so pair body and tag under
    // the mutation lock: a new tag on an old body would defeat `If-Match`.
    let _snapshot = state.lock_config_mutation().await;
    let config = state.graphics.config();
    let d = config
        .data_sources
        .iter()
        .find(|d| d.id == id)
        .ok_or_else(|| ControlError::NotFound {
            kind: DATA_SOURCE_KIND,
            id: id.clone(),
        })?;
    let view = Json(data_source_view(&state, &config, d)?).into_response();
    Ok(with_version(&state, GRAPHICS_DATA_SOURCES_KEY, &id, view))
}

/// Normalize + validate a data-source body into its config form.
fn data_source_of(
    id: &str,
    input: DataSourceInput,
) -> ControlResult<multiview_config::GraphicsDataSource> {
    let trim = |v: Option<String>| v.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
    let doc = DataSourceDoc {
        id: id.to_owned(),
        name: input.name.trim().to_owned(),
        kind: input.kind.trim().to_owned(),
        forecast_product: trim(input.forecast_product).map(|s| s.to_ascii_uppercase()),
        aac: trim(input.aac).map(|s| s.to_ascii_uppercase()),
        observation_product: trim(input.observation_product).map(|s| s.to_ascii_uppercase()),
        station_wmo: trim(input.station_wmo),
    };
    let d: multiview_config::GraphicsDataSource = serde_json::to_value(&doc)
        .and_then(serde_json::from_value)
        .map_err(|e| ControlError::Validation(format!("data source {id:?}: {e}")))?;
    d.validate().map_err(validation)?;
    Ok(d)
}

/// Create or replace a data source in the current config.
///
/// # Errors
/// Validation or a failed commit.
pub(crate) async fn upsert_data_source(
    state: &AppState,
    d: multiview_config::GraphicsDataSource,
) -> ControlResult<bool> {
    let mut next = current(state);
    let created = if let Some(slot) = next.data_sources.iter_mut().find(|x| x.id == d.id) {
        *slot = d;
        false
    } else {
        next.data_sources.push(d);
        true
    };
    commit_graphics(state, next).await?;
    Ok(created)
}

/// `PUT /api/v1/graphics/data-sources/{id}` (role: write; per-object).
#[cfg_attr(feature = "openapi", utoipa::path(put, path = "/api/v1/graphics/data-sources/{id}", tag = "graphics",
    params(("id" = String, Path, description = "Data source id.")), request_body = DataSourceInput,
    responses((status = 200, description = "Replaced.", body = DataSourceView),
        (status = 201, description = "Created.", body = DataSourceView),
        (status = 403, description = "Not authorized.", body = crate::problem::Problem),
        (status = 412, description = "A sent If-Match is stale.", body = crate::problem::Problem),
        (status = 422, description = "Invalid body.", body = crate::problem::Problem))))]
pub(crate) async fn put_data_source(
    State(state): State<AppState>,
    principal: Principal,
    if_match: IfMatch,
    Path(id): Path<String>,
    Json(input): Json<DataSourceInput>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    let d = data_source_of(&id, input)?;
    let _mutation = state.lock_config_mutation().await;
    let exists = state
        .graphics
        .config()
        .data_sources
        .iter()
        .any(|x| x.id == id);
    check_if_match(
        &state,
        &if_match,
        (GRAPHICS_DATA_SOURCES_KEY, DATA_SOURCE_KIND),
        &id,
        exists,
    )?;
    let created = upsert_data_source(&state, d.clone()).await?;
    state.audit(
        &principal.key_id,
        if created {
            AuditAction::Create
        } else {
            AuditAction::Update
        },
        DATA_SOURCE_KIND,
        &id,
        serde_json::to_value(&d).ok(),
    );
    let config = state.graphics.config();
    let view = data_source_view(&state, &config, &d)?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok(with_version(
        &state,
        GRAPHICS_DATA_SOURCES_KEY,
        &id,
        (status, Json(view)).into_response(),
    ))
}

/// Remove a data source; `409` while an item is bound to it.
///
/// # Errors
/// Not found, conflict, or a failed commit.
pub(crate) async fn remove_data_source(state: &AppState, id: &str) -> ControlResult<()> {
    let mut next = current(state);
    let before = next.data_sources.len();
    next.data_sources.retain(|d| d.id != id);
    if next.data_sources.len() == before {
        return Err(ControlError::NotFound {
            kind: DATA_SOURCE_KIND,
            id: id.to_owned(),
        });
    }
    let users: Vec<&str> = next
        .items
        .iter()
        .filter(|i| i.data_source.as_deref() == Some(id))
        .map(|i| i.id.as_str())
        .collect();
    if !users.is_empty() {
        return Err(ControlError::Conflict(format!(
            "data source {id:?} is used by graphics items {users:?}; rebind or delete them first"
        )));
    }
    commit_graphics(state, next).await
}

/// `DELETE /api/v1/graphics/data-sources/{id}` (role: administer; per-object).
#[cfg_attr(feature = "openapi", utoipa::path(delete, path = "/api/v1/graphics/data-sources/{id}", tag = "graphics",
    params(("id" = String, Path, description = "Data source id.")),
    responses((status = 204, description = "Deleted."),
        (status = 403, description = "Not authorized.", body = crate::problem::Problem),
        (status = 404, description = "Unknown id.", body = crate::problem::Problem),
        (status = 412, description = "A sent If-Match is stale.", body = crate::problem::Problem),
        (status = 409, description = "Still bound by an item.", body = crate::problem::Problem))))]
pub(crate) async fn delete_data_source(
    State(state): State<AppState>,
    principal: Principal,
    if_match: IfMatch,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Administer)?;
    crate::auth::authorize_object(&principal, &id)?;
    let _mutation = state.lock_config_mutation().await;
    if state
        .graphics
        .config()
        .data_sources
        .iter()
        .any(|x| x.id == id)
    {
        check_if_match(
            &state,
            &if_match,
            (GRAPHICS_DATA_SOURCES_KEY, DATA_SOURCE_KIND),
            &id,
            true,
        )?;
    }
    remove_data_source(&state, &id).await?;
    state.audit(
        &principal.key_id,
        AuditAction::Delete,
        DATA_SOURCE_KIND,
        &id,
        None,
    );
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ------------------------------------------------------------------ templates

fn template_doc(t: &multiview_config::TemplateSpec) -> TemplateDoc {
    TemplateDoc {
        id: t.id.to_owned(),
        family: t.family.to_owned(),
        name: t.name.to_owned(),
        description: t.description.to_owned(),
        data_kind: t.data_kind.map(|k| match k {
            multiview_config::DataKind::Weather | _ => "weather".to_owned(),
        }),
        fields: t.fields.iter().map(|s| (*s).to_owned()).collect(),
        style_options: t.style_options.iter().map(|s| (*s).to_owned()).collect(),
    }
}

/// `GET /api/v1/graphics/templates` (role: read).
#[cfg_attr(feature = "openapi", utoipa::path(get, path = "/api/v1/graphics/templates", tag = "graphics",
    responses((status = 200, description = "Built-in templates.", body = [TemplateDoc]))))]
pub(crate) async fn list_templates(principal: Principal) -> ControlResult<Json<Vec<TemplateDoc>>> {
    principal.role.require(Action::Read)?;
    Ok(Json(
        multiview_config::graphics::TEMPLATES
            .iter()
            .map(template_doc)
            .collect(),
    ))
}

/// `GET /api/v1/graphics/templates/{id}/sample.jpg` — a sample render bound
/// to the first in-scope data source of the right kind (role: read).
#[cfg_attr(feature = "openapi", utoipa::path(get, path = "/api/v1/graphics/templates/{id}/sample.jpg", tag = "graphics",
    params(("id" = String, Path, description = "Template id.")),
    responses((status = 200, description = "JPEG sample.", content_type = "image/jpeg"),
        (status = 404, description = "Unknown template.", body = crate::problem::Problem),
        (status = 503, description = "Rendering unavailable in this build."))))]
pub(crate) async fn template_sample(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Read)?;
    let spec = multiview_config::graphics::template(&id).ok_or_else(|| ControlError::NotFound {
        kind: TEMPLATE_KIND,
        id: id.clone(),
    })?;
    let config = state.graphics.config();
    let source = spec.data_kind.and_then(|kind| {
        config
            .data_sources
            .iter()
            .find(|d| {
                d.provider.data_kind() == kind
                    && crate::auth::authorize_object(&principal, &d.id).is_ok()
            })
            .map(|d| d.id.clone())
    });
    let item = GraphicsItem::new("sample", spec.name, spec.id, source);
    render_thumb(&state, item).await
}

// ---------------------------------------------------------------------- items

/// `GET /api/v1/graphics/items` (role: read; scoped rows only).
#[cfg_attr(feature = "openapi", utoipa::path(get, path = "/api/v1/graphics/items", tag = "graphics",
    responses((status = 200, description = "In-scope items.", body = [ItemDoc]))))]
pub(crate) async fn list_items(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<Vec<ItemDoc>>> {
    principal.role.require(Action::Read)?;
    let config = state.graphics.config();
    let mut out = Vec::new();
    for i in &config.items {
        if crate::auth::authorize_object(&principal, &i.id).is_ok() {
            out.push(item_doc(i)?);
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(Json(out))
}

fn find_item(state: &AppState, id: &str) -> ControlResult<GraphicsItem> {
    state
        .graphics
        .config()
        .item(id)
        .cloned()
        .ok_or_else(|| ControlError::NotFound {
            kind: ITEM_KIND,
            id: id.to_owned(),
        })
}

/// `GET /api/v1/graphics/items/{id}` (role: read; per-object).
#[cfg_attr(feature = "openapi", utoipa::path(get, path = "/api/v1/graphics/items/{id}", tag = "graphics",
    params(("id" = String, Path, description = "Item id.")),
    responses((status = 200, description = "The item.", body = ItemDoc),
        (status = 403, description = "Out of scope.", body = crate::problem::Problem),
        (status = 404, description = "Unknown id.", body = crate::problem::Problem))))]
pub(crate) async fn get_item(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Read)?;
    crate::auth::authorize_object(&principal, &id)?;
    // The envelope commits before the hub follows, so pair body and tag under
    // the mutation lock: a new tag on an old body would defeat `If-Match`.
    let _snapshot = state.lock_config_mutation().await;
    let doc = Json(item_doc(&find_item(&state, &id)?)?).into_response();
    Ok(with_version(&state, GRAPHICS_ITEMS_KEY, &id, doc))
}

/// `PUT /api/v1/graphics/items/{id}` (role: write; per-object on the item and
/// its data source).
#[cfg_attr(feature = "openapi", utoipa::path(put, path = "/api/v1/graphics/items/{id}", tag = "graphics",
    params(("id" = String, Path, description = "Item id.")), request_body = ItemInput,
    responses((status = 200, description = "Replaced.", body = ItemDoc),
        (status = 201, description = "Created.", body = ItemDoc),
        (status = 403, description = "Not authorized.", body = crate::problem::Problem),
        (status = 412, description = "A sent If-Match is stale.", body = crate::problem::Problem),
        (status = 422, description = "Invalid body.", body = crate::problem::Problem))))]
pub(crate) async fn put_item(
    State(state): State<AppState>,
    principal: Principal,
    if_match: IfMatch,
    Path(id): Path<String>,
    Json(input): Json<ItemInput>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    let data_source = input
        .data_source
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    if let Some(ds) = &data_source {
        crate::auth::authorize_object(&principal, ds)?;
    }
    let doc = ItemDoc {
        id: id.clone(),
        name: input.name.trim().to_owned(),
        template: input.template.trim().to_owned(),
        data_source,
        key: input.key.filter(|k| !k.is_empty()),
        style: input.style.unwrap_or_default(),
    };
    let item: GraphicsItem = serde_json::to_value(&doc)
        .and_then(serde_json::from_value)
        .map_err(|e| ControlError::Validation(format!("graphics item {id:?}: {e}")))?;
    let _mutation = state.lock_config_mutation().await;
    let mut next = current(&state);
    let exists = next.items.iter().any(|x| x.id == id);
    check_if_match(
        &state,
        &if_match,
        (GRAPHICS_ITEMS_KEY, ITEM_KIND),
        &id,
        exists,
    )?;
    let created = if let Some(slot) = next.items.iter_mut().find(|x| x.id == id) {
        *slot = item.clone();
        false
    } else {
        next.items.push(item.clone());
        true
    };
    commit_graphics(&state, next).await?;
    state.audit(
        &principal.key_id,
        if created {
            AuditAction::Create
        } else {
            AuditAction::Update
        },
        ITEM_KIND,
        &id,
        serde_json::to_value(&item).ok(),
    );
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    let doc = (status, Json(item_doc(&item)?)).into_response();
    Ok(with_version(&state, GRAPHICS_ITEMS_KEY, &id, doc))
}

/// `DELETE /api/v1/graphics/items/{id}` (role: administer; per-object). The
/// item leaves every rundown; if it is on air it is cut to clear.
#[cfg_attr(feature = "openapi", utoipa::path(delete, path = "/api/v1/graphics/items/{id}", tag = "graphics",
    params(("id" = String, Path, description = "Item id.")),
    responses((status = 204, description = "Deleted."),
        (status = 403, description = "Not authorized.", body = crate::problem::Problem),
        (status = 412, description = "A sent If-Match is stale.", body = crate::problem::Problem),
        (status = 404, description = "Unknown id.", body = crate::problem::Problem))))]
pub(crate) async fn delete_item(
    State(state): State<AppState>,
    principal: Principal,
    if_match: IfMatch,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Administer)?;
    crate::auth::authorize_object(&principal, &id)?;
    let _mutation = state.lock_config_mutation().await;
    let mut next = current(&state);
    let before = next.items.len();
    next.items.retain(|i| i.id != id);
    if next.items.len() == before {
        return Err(ControlError::NotFound {
            kind: ITEM_KIND,
            id,
        });
    }
    check_if_match(
        &state,
        &if_match,
        (GRAPHICS_ITEMS_KEY, ITEM_KIND),
        &id,
        true,
    )?;
    let affected: Vec<String> = next
        .channels
        .iter()
        .filter(|c| {
            state.graphics.channel(&c.id).is_some_and(|s| {
                s.program.as_deref() == Some(&id) || s.preview.as_deref() == Some(&id)
            }) || c.rundown.contains(&id)
        })
        .map(|c| c.id.clone())
        .collect();
    for ch in &mut next.channels {
        ch.rundown.retain(|i| *i != id);
    }
    commit_graphics(&state, next).await?;
    state.audit(&principal.key_id, AuditAction::Delete, ITEM_KIND, &id, None);
    for ch in affected {
        if let Some(snap) = state.graphics.channel(&ch) {
            publish(&state, &snap, "rundown");
        }
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `GET /api/v1/graphics/items/{id}/thumbnail.jpg` (role: read; per-object).
#[cfg_attr(feature = "openapi", utoipa::path(get, path = "/api/v1/graphics/items/{id}/thumbnail.jpg", tag = "graphics",
    params(("id" = String, Path, description = "Item id.")),
    responses((status = 200, description = "JPEG thumbnail.", content_type = "image/jpeg"),
        (status = 404, description = "Unknown id.", body = crate::problem::Problem),
        (status = 503, description = "Rendering unavailable in this build."))))]
pub(crate) async fn item_thumbnail(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Read)?;
    crate::auth::authorize_object(&principal, &id)?;
    let item = find_item(&state, &id)?;
    render_thumb(&state, item).await
}

// ------------------------------------------------------------------- channels

/// `GET /api/v1/graphics/channels` (role: read; scoped rows only).
#[cfg_attr(feature = "openapi", utoipa::path(get, path = "/api/v1/graphics/channels", tag = "graphics",
    responses((status = 200, description = "In-scope channels with PVW/PGM.", body = [ChannelView]))))]
pub(crate) async fn list_channels(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<Vec<ChannelView>>> {
    principal.role.require(Action::Read)?;
    let config = state.graphics.config();
    Ok(Json(
        config
            .channels
            .iter()
            .filter(|c| crate::auth::authorize_object(&principal, &c.id).is_ok())
            .map(|c| channel_view(&state, c))
            .collect(),
    ))
}

fn find_channel(state: &AppState, id: &str) -> ControlResult<multiview_config::GraphicsChannel> {
    state
        .graphics
        .config()
        .channel(id)
        .cloned()
        .ok_or_else(|| ControlError::NotFound {
            kind: CHANNEL_KIND,
            id: id.to_owned(),
        })
}

/// `GET /api/v1/graphics/channels/{id}` — status (role: read; per-object).
#[cfg_attr(feature = "openapi", utoipa::path(get, path = "/api/v1/graphics/channels/{id}", tag = "graphics",
    params(("id" = String, Path, description = "Channel id.")),
    responses((status = 200, description = "The channel with PVW/PGM.", body = ChannelView),
        (status = 403, description = "Out of scope.", body = crate::problem::Problem),
        (status = 404, description = "Unknown id.", body = crate::problem::Problem))))]
pub(crate) async fn get_channel(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Read)?;
    crate::auth::authorize_object(&principal, &id)?;
    // The envelope commits before the hub follows, so pair body and tag under
    // the mutation lock: a new tag on an old body would defeat `If-Match`.
    let _snapshot = state.lock_config_mutation().await;
    let view = Json(channel_view(&state, &find_channel(&state, &id)?)).into_response();
    Ok(with_version(&state, GRAPHICS_CHANNELS_KEY, &id, view))
}

/// `PUT /api/v1/graphics/channels/{id}/rundown` — replace the rundown (and
/// optionally the transition preset / name); creates the channel when new
/// (role: write; per-object on the channel and every listed item).
#[cfg_attr(feature = "openapi", utoipa::path(put, path = "/api/v1/graphics/channels/{id}/rundown", tag = "graphics",
    params(("id" = String, Path, description = "Channel id.")), request_body = RundownInput,
    responses((status = 200, description = "The channel.", body = ChannelView),
        (status = 403, description = "Not authorized.", body = crate::problem::Problem),
        (status = 412, description = "A sent If-Match is stale.", body = crate::problem::Problem),
        (status = 422, description = "Unknown item / invalid preset.", body = crate::problem::Problem))))]
pub(crate) async fn put_rundown(
    State(state): State<AppState>,
    principal: Principal,
    if_match: IfMatch,
    Path(id): Path<String>,
    Json(input): Json<RundownInput>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    for item in &input.rundown {
        crate::auth::authorize_object(&principal, item)?;
    }
    let _mutation = state.lock_config_mutation().await;
    let mut next = current(&state);
    let created = !next.channels.iter().any(|c| c.id == id);
    check_if_match(
        &state,
        &if_match,
        (GRAPHICS_CHANNELS_KEY, CHANNEL_KIND),
        &id,
        !created,
    )?;
    if created {
        next.channels.push(multiview_config::GraphicsChannel::new(
            id.clone(),
            input.name.clone().unwrap_or_else(|| id.clone()),
        ));
    }
    let ch = next
        .channels
        .iter_mut()
        .find(|c| c.id == id)
        .ok_or_else(|| ControlError::Repository("channel vanished".into()))?;
    ch.rundown.clone_from(&input.rundown);
    if let Some(t) = input.transition {
        ch.transition = transition_of(t);
    }
    if let Some(name) = &input.name {
        ch.name = name.trim().to_owned();
    }
    let ch = ch.clone();
    commit_graphics(&state, next).await?;
    state.audit(
        &principal.key_id,
        if created {
            AuditAction::Create
        } else {
            AuditAction::Update
        },
        CHANNEL_KIND,
        &id,
        serde_json::to_value(&ch).ok(),
    );
    if let Some(snap) = state.graphics.channel(&id) {
        publish(&state, &snap, "rundown");
    }
    let view = Json(channel_view(&state, &ch)).into_response();
    Ok(with_version(&state, GRAPHICS_CHANNELS_KEY, &id, view))
}

/// `GET /api/v1/graphics/channels/{id}/rundown` (same as the channel status).
#[cfg_attr(feature = "openapi", utoipa::path(get, path = "/api/v1/graphics/channels/{id}/rundown", tag = "graphics",
    params(("id" = String, Path, description = "Channel id.")),
    responses((status = 200, description = "The channel with its rundown.", body = ChannelView),
        (status = 404, description = "Unknown id.", body = crate::problem::Problem))))]
pub(crate) async fn get_rundown(
    state: State<AppState>,
    principal: Principal,
    id: Path<String>,
) -> ControlResult<Response> {
    get_channel(state, principal, id).await
}

fn after_verb(
    state: &AppState,
    principal: &Principal,
    id: &str,
    verb: &str,
    result: Result<ChannelSnapshot, PlayoutError>,
) -> ControlResult<Json<ChannelView>> {
    let snap = result.map_err(playout_err)?;
    state.audit(
        &principal.key_id,
        AuditAction::Command,
        CHANNEL_KIND,
        id,
        Some(serde_json::json!({ "verb": verb, "preview": snap.preview, "program": snap.program })),
    );
    publish(state, &snap, verb);
    Ok(Json(channel_view(state, &find_channel(state, id)?)))
}

/// `POST /api/v1/graphics/channels/{id}/cue` (role: write; per-object on the
/// channel and the item).
#[cfg_attr(feature = "openapi", utoipa::path(post, path = "/api/v1/graphics/channels/{id}/cue", tag = "graphics",
    params(("id" = String, Path, description = "Channel id.")), request_body = CueInput,
    responses((status = 200, description = "The channel after the cue.", body = ChannelView),
        (status = 403, description = "Not authorized.", body = crate::problem::Problem),
        (status = 404, description = "Unknown channel or item.", body = crate::problem::Problem))))]
pub(crate) async fn cue(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
    Json(input): Json<CueInput>,
) -> ControlResult<Json<ChannelView>> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    crate::auth::authorize_object(&principal, &input.item)?;
    let r = state.graphics.cue(&id, &input.item);
    after_verb(&state, &principal, &id, "cue", r)
}

/// `POST /api/v1/graphics/channels/{id}/take` — preview to air (role: write;
/// per-object).
#[cfg_attr(feature = "openapi", utoipa::path(post, path = "/api/v1/graphics/channels/{id}/take", tag = "graphics",
    params(("id" = String, Path, description = "Channel id.")), request_body = PlayoutInput,
    responses((status = 200, description = "The channel after the take.", body = ChannelView),
        (status = 404, description = "Unknown channel.", body = crate::problem::Problem),
        (status = 409, description = "Nothing cued.", body = crate::problem::Problem))))]
pub(crate) async fn take(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
    Json(input): Json<PlayoutInput>,
) -> ControlResult<Json<ChannelView>> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    if let Some(t) = input.transition {
        validate_transition(t)?;
    }
    let r = state
        .graphics
        .take(&id, input.transition.map(transition_of));
    after_verb(&state, &principal, &id, "take", r)
}

fn validate_transition(t: TransitionDoc) -> ControlResult<()> {
    if t.frames == 0 || t.frames > multiview_config::graphics::MAX_TRANSITION_FRAMES {
        return Err(ControlError::Validation(format!(
            "transition frames must be 1-{}",
            multiview_config::graphics::MAX_TRANSITION_FRAMES
        )));
    }
    Ok(())
}

/// `POST /api/v1/graphics/channels/{id}/clear` — program off air (role:
/// write; per-object).
#[cfg_attr(feature = "openapi", utoipa::path(post, path = "/api/v1/graphics/channels/{id}/clear", tag = "graphics",
    params(("id" = String, Path, description = "Channel id.")), request_body = PlayoutInput,
    responses((status = 200, description = "The channel after the clear.", body = ChannelView),
        (status = 404, description = "Unknown channel.", body = crate::problem::Problem))))]
pub(crate) async fn clear(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
    Json(input): Json<PlayoutInput>,
) -> ControlResult<Json<ChannelView>> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    if let Some(t) = input.transition {
        validate_transition(t)?;
    }
    let r = state
        .graphics
        .clear(&id, input.transition.map(transition_of));
    after_verb(&state, &principal, &id, "clear", r)
}

/// `POST /api/v1/graphics/channels/{id}/next` — cue the next rundown entry
/// (role: write; per-object).
#[cfg_attr(feature = "openapi", utoipa::path(post, path = "/api/v1/graphics/channels/{id}/next", tag = "graphics",
    params(("id" = String, Path, description = "Channel id.")), request_body = PlayoutInput,
    responses((status = 200, description = "The channel after the cue.", body = ChannelView),
        (status = 404, description = "Unknown channel.", body = crate::problem::Problem),
        (status = 409, description = "End of rundown.", body = crate::problem::Problem))))]
pub(crate) async fn next(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
    Json(_input): Json<PlayoutInput>,
) -> ControlResult<Json<ChannelView>> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    let r = state.graphics.next(&id);
    after_verb(&state, &principal, &id, "next", r)
}
