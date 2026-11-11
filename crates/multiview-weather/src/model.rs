//! Normalized, render-ready weather data and the process-wide snapshot store.
//!
//! The management-plane poller parses BOM products and publishes one immutable
//! [`LocationWeather`] per configured location into the [`WeatherStore`]. A
//! weather source's render worker reads the store **without blocking**
//! (`try_read`) and re-renders only when a location's `revision` changes.
//! Values are carried exactly as BOM published them: an absent field stays
//! `None` (never zero, never invented).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use chrono::{DateTime, Datelike, FixedOffset, Timelike, Utc, Weekday};
use serde::Serialize;

use crate::{Forecast, ObservationPeriod, Observations};

/// One normalized forecast day (a BOM daily `forecast-period`).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DayForecast {
    /// Period start in the forecast's local offset (RFC 3339, as published).
    pub start_local: String,
    /// Local calendar date `YYYY-MM-DD`.
    pub date: String,
    /// Three-letter weekday (`Mon`…`Sun`) of the local start.
    pub weekday: String,
    /// Display date, e.g. `Fri 6 Nov`.
    pub label: String,
    /// BOM `forecast_icon_code` (1–17) when present and numeric.
    pub icon_code: Option<u8>,
    /// Maximum air temperature, °C.
    pub max_c: Option<f64>,
    /// Minimum air temperature, °C.
    pub min_c: Option<f64>,
    /// Short précis (e.g. `Shower or two.`).
    pub precis: Option<String>,
    /// Probability of precipitation as published (e.g. `50%`).
    pub rain_chance: Option<String>,
    /// Precipitation range as published (e.g. `0 to 1 mm`).
    pub rain_range: Option<String>,
}

/// The normalized forecast for one area.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ForecastSummary {
    /// Product id (e.g. `IDN11060`).
    pub product: String,
    /// Area code (e.g. `NSW_PT131`).
    pub aac: String,
    /// Area description as published (e.g. `Sydney`).
    pub area_name: String,
    /// Product issue time (UTC).
    pub issued_utc: DateTime<Utc>,
    /// Product issue time rendered in the area's local offset (taken from the
    /// first period's published local time), e.g. `9:45 am Fri 6 Nov`.
    pub issued_local: Option<String>,
    /// Product expiry (UTC) when published.
    pub expires_utc: Option<DateTime<Utc>>,
    /// Daily periods in order (BOM publishes 7; fewer are kept as-is).
    pub days: Vec<DayForecast>,
}

/// The latest observation for one station.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Observation {
    /// Product id (e.g. `IDN60920`).
    pub product: String,
    /// Station WMO id.
    pub wmo_id: String,
    /// Station name as published.
    pub station_name: String,
    /// Observation time (UTC).
    pub time_utc: DateTime<Utc>,
    /// Observation time as published in local time (RFC 3339).
    pub time_local: String,
    /// Display time, e.g. `10:50 am`.
    pub time_display: String,
    /// Air temperature, °C.
    pub air_temp_c: Option<f64>,
    /// Apparent temperature, °C.
    pub apparent_temp_c: Option<f64>,
    /// Relative humidity, %.
    pub rel_humidity_pct: Option<f64>,
    /// Wind direction as published (e.g. `ENE`, `CALM`).
    pub wind_dir: Option<String>,
    /// Wind speed, km/h.
    pub wind_kmh: Option<f64>,
    /// Wind gust, km/h.
    pub gust_kmh: Option<f64>,
    /// Mean sea-level pressure, hPa (falls back to station pressure).
    pub pressure_hpa: Option<f64>,
    /// Rain since 9 am, mm.
    pub rain_since_9am_mm: Option<f64>,
}

/// Fetch freshness for one product.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Freshness {
    /// When the last good document was fetched (UTC).
    pub fetched_at: Option<DateTime<Utc>>,
    /// When the last attempt (good or bad) finished (UTC).
    pub attempted_at: Option<DateTime<Utc>>,
    /// The last error, cleared by the next success.
    pub error: Option<String>,
}

/// Everything a weather graphic needs for one location.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LocationWeather {
    /// Configured location id.
    pub location_id: String,
    /// Configured display name.
    pub name: String,
    /// Normalized forecast, or `None` when not (yet) available.
    pub forecast: Option<ForecastSummary>,
    /// Forecast product freshness.
    pub forecast_freshness: Freshness,
    /// Latest observation, or `None` when not configured / not available.
    pub observation: Option<Observation>,
    /// Observation product freshness (`None` when not configured).
    pub observation_freshness: Option<Freshness>,
}

impl LocationWeather {
    /// Render-relevant equality: freshness timestamps alone (a refetch of the
    /// same document) must not trigger a re-render.
    #[must_use]
    pub fn same_picture(&self, other: &Self) -> bool {
        self.name == other.name
            && self.forecast == other.forecast
            && self.observation == other.observation
    }
}

fn number(v: Option<&str>) -> Option<f64> {
    v?.trim().parse::<f64>().ok().filter(|n| n.is_finite())
}

fn weekday_short(w: Weekday) -> &'static str {
    match w {
        Weekday::Mon => "Mon",
        Weekday::Tue => "Tue",
        Weekday::Wed => "Wed",
        Weekday::Thu => "Thu",
        Weekday::Fri => "Fri",
        Weekday::Sat => "Sat",
        Weekday::Sun => "Sun",
    }
}

fn month_short(m: u32) -> &'static str {
    const M: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    M.get(usize::try_from(m.saturating_sub(1)).unwrap_or(0))
        .copied()
        .unwrap_or("")
}

/// `9:45 am` style 12-hour time.
#[must_use]
pub fn clock_12h(t: &DateTime<FixedOffset>) -> String {
    let (pm, h) = t.hour12();
    format!("{h}:{:02} {}", t.minute(), if pm { "pm" } else { "am" })
}

/// Normalize the area `aac` out of a parsed forecast product.
#[must_use]
pub fn normalize_forecast(forecast: &Forecast, aac: &str) -> Option<ForecastSummary> {
    let area = forecast.areas.iter().find(|a| a.aac == aac)?;
    let mut offset: Option<FixedOffset> = None;
    let mut days = Vec::with_capacity(area.periods.len());
    let mut periods: Vec<_> = area.periods.iter().collect();
    periods.sort_by_key(|p| p.index);
    for p in periods {
        let local = DateTime::<FixedOffset>::parse_from_rfc3339(&p.start_local).ok();
        if offset.is_none() {
            offset = local.map(|l| *l.offset());
        }
        let text = |kind: &str| {
            p.elements
                .iter()
                .find(|v| v.kind == kind)
                .and_then(|v| v.value.clone())
        };
        let num = |kind: &str| {
            p.elements
                .iter()
                .find(|v| v.kind == kind)
                .and_then(|v| number(v.value.as_deref()))
        };
        days.push(DayForecast {
            start_local: p.start_local.clone(),
            date: local.map_or_else(String::new, |l| l.format("%Y-%m-%d").to_string()),
            weekday: local.map_or_else(String::new, |l| weekday_short(l.weekday()).to_owned()),
            label: local.map_or_else(String::new, |l| {
                format!(
                    "{} {} {}",
                    weekday_short(l.weekday()),
                    l.day(),
                    month_short(l.month())
                )
            }),
            icon_code: p
                .icon_code
                .as_deref()
                .and_then(|c| c.trim().parse::<u8>().ok()),
            max_c: num("air_temperature_maximum"),
            min_c: num("air_temperature_minimum"),
            precis: text("precis"),
            rain_chance: text("probability_of_precipitation"),
            rain_range: text("precipitation_range"),
        });
    }
    let issued_local = offset.map(|o| {
        let t = forecast.provenance.issued.with_timezone(&o);
        format!(
            "{} {} {} {}",
            clock_12h(&t),
            weekday_short(t.weekday()),
            t.day(),
            month_short(t.month())
        )
    });
    Some(ForecastSummary {
        product: forecast.product_id.clone(),
        aac: area.aac.clone(),
        area_name: area.description.clone(),
        issued_utc: forecast.provenance.issued,
        issued_local,
        expires_utc: forecast.provenance.expires,
        days,
    })
}

fn obs_text(p: &ObservationPeriod, kind: &str) -> Option<String> {
    p.elements
        .iter()
        .find(|v| v.kind == kind)
        .and_then(|v| v.value.clone())
}

fn obs_num(p: &ObservationPeriod, kind: &str) -> Option<f64> {
    number(obs_text(p, kind).as_deref())
}

/// Normalize the latest period of station `wmo` out of a parsed observations
/// product.
#[must_use]
pub fn normalize_observation(obs: &Observations, wmo: &str) -> Option<Observation> {
    let station = obs
        .stations
        .iter()
        .find(|s| s.wmo_id.as_deref() == Some(wmo))?;
    let p = station.periods.iter().max_by_key(|p| p.time_utc)?;
    let local = DateTime::<FixedOffset>::parse_from_rfc3339(&p.time_local).ok();
    Some(Observation {
        product: obs.provenance.product_id.clone(),
        wmo_id: wmo.to_owned(),
        station_name: station.name.clone(),
        time_utc: p.time_utc,
        time_local: p.time_local.clone(),
        time_display: local.map_or_else(String::new, |l| clock_12h(&l)),
        air_temp_c: obs_num(p, "air_temperature"),
        apparent_temp_c: obs_num(p, "apparent_temp"),
        rel_humidity_pct: obs_num(p, "rel-humidity"),
        wind_dir: obs_text(p, "wind_dir"),
        wind_kmh: obs_num(p, "wind_spd_kmh"),
        gust_kmh: obs_num(p, "gust_kmh"),
        pressure_hpa: obs_num(p, "msl_pres").or_else(|| obs_num(p, "pres")),
        rain_since_9am_mm: obs_num(p, "rainfall"),
    })
}

/// The icon family a BOM `forecast_icon_code` maps to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WeatherIcon {
    /// Sunny / frost (clear sky by day).
    Sun,
    /// Clear night.
    Night,
    /// Partly cloudy / mostly sunny.
    PartCloud,
    /// Cloudy.
    Cloud,
    /// Rain / light rain.
    Rain,
    /// Showers / light showers / snow.
    Showers,
    /// Thunderstorm.
    Storm,
    /// Fog / haze.
    Fog,
    /// Windy / dust.
    Wind,
}

/// Map a BOM `forecast_icon_code` (1–17) to an icon family; `None` for an
/// unknown/unassigned code (BOM does not assign 5 or 7) (the graphic then shows no icon rather than a guess).
#[must_use]
pub const fn icon_for_code(code: u8) -> Option<WeatherIcon> {
    Some(match code {
        1 | 14 => WeatherIcon::Sun,
        2 => WeatherIcon::Night,
        3 => WeatherIcon::PartCloud,
        4 => WeatherIcon::Cloud,
        6 | 10 => WeatherIcon::Fog,
        8 | 12 => WeatherIcon::Rain,
        9 | 13 => WeatherIcon::Wind,
        11 | 15 | 17 => WeatherIcon::Showers,
        16 => WeatherIcon::Storm,
        _ => return None,
    })
}

/// A published location snapshot plus the revision a renderer keys on.
#[derive(Clone, Debug)]
pub struct Published {
    /// Monotonic per-picture revision (bumped only when the picture changes).
    pub revision: u64,
    /// The data.
    pub data: Arc<LocationWeather>,
}

/// Process-wide weather snapshot store (management plane writes, render
/// workers read). Bounded by the configured location count.
#[derive(Debug, Default)]
pub struct WeatherStore {
    entries: RwLock<BTreeMap<String, Published>>,
    next_revision: AtomicU64,
}

impl WeatherStore {
    /// The process-global store shared by the poller and the renderers.
    #[must_use]
    pub fn global() -> &'static Arc<Self> {
        static GLOBAL: OnceLock<Arc<WeatherStore>> = OnceLock::new();
        GLOBAL.get_or_init(|| Arc::new(Self::default()))
    }

    /// Replace the full set of locations. A location whose picture is
    /// unchanged keeps its revision (no re-render); removed ids disappear.
    pub fn publish_all(&self, all: Vec<LocationWeather>) {
        let Ok(mut entries) = self.entries.write() else {
            return;
        };
        let mut next = BTreeMap::new();
        for data in all {
            let revision = match entries.get(&data.location_id) {
                Some(prev) if prev.data.same_picture(&data) => prev.revision,
                _ => self.next_revision.fetch_add(1, Ordering::Relaxed) + 1,
            };
            next.insert(
                data.location_id.clone(),
                Published {
                    revision,
                    data: Arc::new(data),
                },
            );
        }
        *entries = next;
    }

    /// Blocking read of one location (management plane).
    #[must_use]
    pub fn get(&self, id: &str) -> Option<Published> {
        self.entries.read().ok()?.get(id).cloned()
    }

    /// Non-blocking read for render workers: `Err(())` when the writer holds
    /// the lock right now (the caller keeps its cached frame and retries next
    /// tick); `Ok(None)` for an unknown location.
    ///
    /// # Errors
    /// `Err(())` when the lock is momentarily held by the publisher.
    #[allow(clippy::result_unit_err)]
    pub fn try_get(&self, id: &str) -> Result<Option<Published>, ()> {
        match self.entries.try_read() {
            Ok(entries) => Ok(entries.get(id).cloned()),
            Err(_) => Err(()),
        }
    }

    /// All published locations (management plane).
    #[must_use]
    pub fn all(&self) -> Vec<Published> {
        self.entries
            .read()
            .map(|e| e.values().cloned().collect())
            .unwrap_or_default()
    }
}
