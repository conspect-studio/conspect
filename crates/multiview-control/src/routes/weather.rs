//! BOM weather locations under `/api/v1/weather/locations`.
//!
//! A location (`[[weather.locations]]`) names a BOM forecast product + area
//! code and optionally an observation product + station. Mutations persist
//! through the canonical desired-state commit (ADR-W024: `commit_resource`),
//! then hand the new location set to the management-plane poller. Reads
//! return the normalized latest data (forecast days + latest observation) and
//! per-product freshness straight from the poller's snapshot store.
//! Authorization: role gates plus per-object (`authorize_object`) on every id,
//! and the list drops out-of-scope rows (BOLA).
//!
//! Since the Graphics module, this API is a thin compatibility alias over the
//! `bom_weather` data sources of `/api/v1/graphics/data-sources`: the truth is
//! the graphics hub, and mutations go through `graphics::commit_graphics`
//! (which migrates legacy `[weather] locations` into `[graphics]`).
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::audit::AuditAction;
use crate::auth::{Action, Principal};
use crate::boot_model::GRAPHICS_DATA_SOURCES_KEY;
use crate::concurrency::IfMatch;
use crate::error::{ControlError, ControlResult};
use crate::state::AppState;

/// Resource kind label for errors and audit.
pub const WEATHER_LOCATION_KIND: &str = "weather_location";

/// A configured location (the `[[weather.locations]]` document).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct WeatherLocationDoc {
    /// Location id (`[A-Za-z0-9_-]`, 1–64).
    pub id: String,
    /// Display name drawn on the graphic.
    pub name: String,
    /// BOM forecast product id (e.g. `IDN11060`).
    pub forecast_product: String,
    /// BOM forecast area code (e.g. `NSW_PT131`).
    pub aac: String,
    /// BOM observation product id (e.g. `IDN60920`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_product: Option<String>,
    /// Observation station WMO id (e.g. `94768`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub station_wmo: Option<String>,
}

/// The `PUT` body: the location without its id (the id is the path).
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct WeatherLocationInput {
    /// Display name.
    pub name: String,
    /// BOM forecast product id.
    pub forecast_product: String,
    /// BOM forecast area code.
    pub aac: String,
    /// BOM observation product id.
    #[serde(default)]
    pub observation_product: Option<String>,
    /// Observation station WMO id.
    #[serde(default)]
    pub station_wmo: Option<String>,
}

/// Fetch freshness of one product.
#[derive(Debug, Clone, Default, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct FreshnessDoc {
    /// Last successful fetch (RFC 3339 UTC).
    pub fetched_at: Option<String>,
    /// Last attempt, good or bad (RFC 3339 UTC).
    pub attempted_at: Option<String>,
    /// Last error (cleared on success).
    pub error: Option<String>,
}

/// One location with its latest data and freshness.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct WeatherLocationView {
    /// The configured location.
    pub location: WeatherLocationDoc,
    /// `ok` | `pending` (no data yet) | `error` (last fetch failed, may still
    /// hold last-good data) | `expired` (forecast past its expiry).
    pub status: String,
    /// Normalized forecast: `{product, aac, area_name, issued_utc,
    /// issued_local, expires_utc, days: [{date, weekday, icon_code, max_c,
    /// min_c, precis, rain_chance, rain_range, start_local}]}` or `null`.
    #[cfg_attr(feature = "openapi", schema(value_type = Option<Object>))]
    pub forecast: Option<serde_json::Value>,
    /// Latest observation: `{station_name, wmo_id, time_local, time_display,
    /// air_temp_c, apparent_temp_c, rel_humidity_pct, wind_dir, wind_kmh,
    /// gust_kmh, pressure_hpa, rain_since_9am_mm}` or `null`.
    #[cfg_attr(feature = "openapi", schema(value_type = Option<Object>))]
    pub observation: Option<serde_json::Value>,
    /// Forecast product freshness.
    pub forecast_freshness: FreshnessDoc,
    /// Observation product freshness (`null` when not configured).
    pub observation_freshness: Option<FreshnessDoc>,
    /// Required on-screen attribution.
    pub attribution: String,
}

fn freshness_doc(f: &multiview_weather::Freshness) -> FreshnessDoc {
    FreshnessDoc {
        fetched_at: f.fetched_at.map(|t| t.to_rfc3339()),
        attempted_at: f.attempted_at.map(|t| t.to_rfc3339()),
        error: f.error.clone(),
    }
}

fn spec_of(l: &multiview_config::WeatherLocation) -> multiview_weather::service::LocationSpec {
    multiview_weather::service::LocationSpec {
        id: l.id.clone(),
        name: l.name.clone(),
        forecast_product: l.forecast_product.clone(),
        aac: l.aac.clone(),
        observation: l.observation_product.clone().zip(l.station_wmo.clone()),
    }
}

/// Map config locations to poller specs (the binary uses this at boot).
#[must_use]
pub fn location_specs(
    config: &multiview_config::WeatherConfig,
) -> Vec<multiview_weather::service::LocationSpec> {
    config.locations.iter().map(spec_of).collect()
}

fn doc_of(spec: &multiview_weather::service::LocationSpec) -> WeatherLocationDoc {
    WeatherLocationDoc {
        id: spec.id.clone(),
        name: spec.name.clone(),
        forecast_product: spec.forecast_product.clone(),
        aac: spec.aac.clone(),
        observation_product: spec.observation.as_ref().map(|(p, _)| p.clone()),
        station_wmo: spec.observation.as_ref().map(|(_, w)| w.clone()),
    }
}

pub(crate) fn view_of(
    state: &AppState,
    spec: &multiview_weather::service::LocationSpec,
) -> WeatherLocationView {
    let published = state.weather.store().get(&spec.id);
    let data = published.as_ref().map(|p| &p.data);
    let forecast = data.and_then(|d| d.forecast.as_ref());
    let observation = data.and_then(|d| d.observation.as_ref());
    let forecast_freshness = state
        .weather
        .product_freshness(&spec.forecast_product)
        .unwrap_or_default();
    let observation_freshness = spec
        .observation
        .as_ref()
        .map(|(p, _)| state.weather.product_freshness(p).unwrap_or_default());
    let errored = forecast_freshness.error.is_some()
        || observation_freshness
            .as_ref()
            .is_some_and(|f| f.error.is_some());
    let expired = forecast
        .and_then(|f| f.expires_utc)
        .is_some_and(|e| e < multiview_weather::chrono::Utc::now());
    let status = if forecast.is_none() && !errored {
        "pending"
    } else if errored {
        "error"
    } else if expired {
        "expired"
    } else {
        "ok"
    };
    WeatherLocationView {
        location: doc_of(spec),
        status: status.to_owned(),
        forecast: forecast.and_then(|f| serde_json::to_value(f).ok()),
        observation: observation.and_then(|o| serde_json::to_value(o).ok()),
        forecast_freshness: freshness_doc(&forecast_freshness),
        observation_freshness: observation_freshness.as_ref().map(freshness_doc),
        attribution: "Source: Bureau of Meteorology".to_owned(),
    }
}

/// `GET /api/v1/weather/locations` — every in-scope location with its latest
/// data (role: read).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/weather/locations",
        tag = "weather",
        responses(
            (status = 200, description = "All in-scope locations with freshness, id-sorted.", body = [WeatherLocationView]),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to read.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn list_locations(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<Vec<WeatherLocationView>>> {
    principal.role.require(Action::Read)?;
    let mut specs = current_specs(&state);
    specs.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(Json(
        specs
            .iter()
            .filter(|s| crate::auth::authorize_object(&principal, &s.id).is_ok())
            .map(|s| view_of(&state, s))
            .collect(),
    ))
}

/// `GET /api/v1/weather/locations/{id}` — one location (role: read;
/// per-object authz).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/weather/locations/{id}",
        tag = "weather",
        params(("id" = String, Path, description = "Location id.")),
        responses(
            (status = 200, description = "The location with normalized forecast days, latest observation and freshness.", body = WeatherLocationView),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to read this location.", body = crate::problem::Problem),
            (status = 404, description = "No location with that id.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_location(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Read)?;
    crate::auth::authorize_object(&principal, &id)?;
    // Pair body and tag under the mutation lock (see `graphics::get_item`).
    let _snapshot = state.lock_config_mutation().await;
    let spec = current_specs(&state)
        .into_iter()
        .find(|s| s.id == id)
        .ok_or_else(|| ControlError::NotFound {
            kind: WEATHER_LOCATION_KIND,
            id: id.clone(),
        })?;
    let view = Json(view_of(&state, &spec)).into_response();
    Ok(super::graphics::with_version(
        &state,
        GRAPHICS_DATA_SOURCES_KEY,
        &id,
        view,
    ))
}

/// The `bom_weather` data sources as poller specs (the alias's truth).
fn current_specs(state: &AppState) -> Vec<multiview_weather::service::LocationSpec> {
    super::graphics::weather_specs(&state.graphics.config())
}

/// `PUT /api/v1/weather/locations/{id}` — create or replace a location
/// (role: write; per-object authz).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        put,
        path = "/api/v1/weather/locations/{id}",
        tag = "weather",
        params(("id" = String, Path, description = "Location id.")),
        request_body = WeatherLocationInput,
        responses(
            (status = 200, description = "The location was replaced.", body = WeatherLocationView),
            (status = 201, description = "The location was created.", body = WeatherLocationView),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to write this location.", body = crate::problem::Problem),
            (status = 412, description = "A sent If-Match is stale.", body = crate::problem::Problem),
            (status = 422, description = "The body is not a valid location (detail names the field).", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn put_location(
    State(state): State<AppState>,
    principal: Principal,
    if_match: IfMatch,
    Path(id): Path<String>,
    Json(input): Json<WeatherLocationInput>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    let blank = |v: Option<String>| v.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
    let mut location = multiview_config::WeatherLocation::new(
        id.clone(),
        input.name.trim(),
        input.forecast_product.trim().to_ascii_uppercase(),
        input.aac.trim().to_ascii_uppercase(),
    );
    location.observation_product = blank(input.observation_product).map(|p| p.to_ascii_uppercase());
    location.station_wmo = blank(input.station_wmo);
    location
        .validate()
        .map_err(|e| ControlError::Validation(e.to_string()))?;
    let _mutation = state.lock_config_mutation().await;
    let exists = state
        .graphics
        .config()
        .data_sources
        .iter()
        .any(|d| d.id == id);
    super::graphics::check_if_match(
        &state,
        &if_match,
        (GRAPHICS_DATA_SOURCES_KEY, WEATHER_LOCATION_KIND),
        &id,
        exists,
    )?;
    let created = super::graphics::upsert_data_source(
        &state,
        multiview_config::GraphicsDataSource::from_weather_location(&location),
    )
    .await?;
    state.audit(
        &principal.key_id,
        if created {
            AuditAction::Create
        } else {
            AuditAction::Update
        },
        WEATHER_LOCATION_KIND,
        &id,
        serde_json::to_value(&location).ok(),
    );
    let view = view_of(&state, &spec_of(&location));
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok(super::graphics::with_version(
        &state,
        GRAPHICS_DATA_SOURCES_KEY,
        &id,
        (status, Json(view)).into_response(),
    ))
}

/// `DELETE /api/v1/weather/locations/{id}` — remove a location (role:
/// administer; per-object authz). Weather sources bound to it then render an
/// honest "unavailable" card.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        delete,
        path = "/api/v1/weather/locations/{id}",
        tag = "weather",
        params(("id" = String, Path, description = "Location id.")),
        responses(
            (status = 204, description = "The location was deleted."),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to administer this location.", body = crate::problem::Problem),
            (status = 404, description = "No location with that id.", body = crate::problem::Problem),
            (status = 412, description = "A sent If-Match is stale.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn delete_location(
    State(state): State<AppState>,
    principal: Principal,
    if_match: IfMatch,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Administer)?;
    crate::auth::authorize_object(&principal, &id)?;
    let _mutation = state.lock_config_mutation().await;
    if !current_specs(&state).iter().any(|s| s.id == id) {
        return Err(ControlError::NotFound {
            kind: WEATHER_LOCATION_KIND,
            id,
        });
    }
    super::graphics::check_if_match(
        &state,
        &if_match,
        (GRAPHICS_DATA_SOURCES_KEY, WEATHER_LOCATION_KIND),
        &id,
        true,
    )?;
    super::graphics::remove_data_source(&state, &id).await?;
    state.audit(
        &principal.key_id,
        AuditAction::Delete,
        WEATHER_LOCATION_KIND,
        &id,
        None,
    );
    Ok(StatusCode::NO_CONTENT.into_response())
}
