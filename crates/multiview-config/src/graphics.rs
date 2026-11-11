//! `[graphics]` — the Graphics (CG) domain model.
//!
//! ```text
//! data sources ──▶ templates ──▶ items ("pages") ──▶ channel rundown ──▶ source
//! (bom_weather…)   (registry)    (template+binding     (PVW / PGM,        (gfx channel
//!                                 +style)               take/clear/next)   program/preview)
//! ```
//!
//! * [`GraphicsDataSource`]: a typed provider (`kind = "bom_weather"` today).
//!   New providers (e.g. `rss`) are one more [`DataProviderKind`] variant plus
//!   a fetcher; nothing else in the model changes.
//! * Templates are **built in** ([`TEMPLATES`]): each declares the data kind
//!   it binds, its fields and its style options.
//! * [`GraphicsItem`]: a named, prepared graphic = template + data binding +
//!   style. This is what an operator builds in the library.
//! * [`GraphicsChannel`]: a playout channel with a persisted rundown and
//!   transition preset. Its live PVW/PGM state is runtime-only (a restart comes
//!   up clear — the safe broadcast default).
//!
//! Back-compat: the legacy `[weather] locations` section still loads; the
//! effective data-source list is [`effective_data_sources`] (legacy locations
//! appear as `bom_weather` sources), and the first graphics mutation migrates
//! them into `[graphics.data_sources]`.

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;
use crate::weather::{is_location_id, WeatherBackground, WeatherConfig, WeatherLocation};

/// Bound on data sources (bounded poller work).
pub const MAX_DATA_SOURCES: usize = 64;
/// Bound on graphics items.
pub const MAX_ITEMS: usize = 256;
/// Bound on graphics channels.
pub const MAX_CHANNELS: usize = 8;
/// Bound on one channel's rundown.
pub const MAX_RUNDOWN: usize = 256;
/// Longest transition (10 s at 60 fps).
pub const MAX_TRANSITION_FRAMES: u32 = 600;

/// The `[graphics]` section.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GraphicsConfig {
    /// Typed data providers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub data_sources: Vec<GraphicsDataSource>,
    /// Prepared graphics items (template + binding + style).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<GraphicsItem>,
    /// Playout channels (rundown + transition preset).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub channels: Vec<GraphicsChannel>,
}

/// One typed data provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GraphicsDataSource {
    /// Stable id (`[A-Za-z0-9_-]`, 1–64).
    pub id: String,
    /// Display name (drawn by templates, e.g. the city).
    pub name: String,
    /// The provider and its settings (`kind = "…"`).
    #[serde(flatten)]
    pub provider: DataProviderKind,
}

/// The provider kinds. Adding one (e.g. `Rss { url, max_items }`) is a new
/// variant + a [`DataKind`] + a fetcher; templates name the [`DataKind`] they
/// need, so binding validation follows automatically.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum DataProviderKind {
    /// Bureau of Meteorology forecast (+ optional observation).
    BomWeather {
        /// BOM forecast product id (e.g. `IDN11060`).
        forecast_product: String,
        /// BOM forecast area code (e.g. `NSW_PT131`).
        aac: String,
        /// BOM observation product id (e.g. `IDN60920`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        observation_product: Option<String>,
        /// Station WMO id (e.g. `94768`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        station_wmo: Option<String>,
    },
}

/// The shape of data a provider yields / a template consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DataKind {
    /// Forecast days + latest observation.
    Weather,
}

impl DataProviderKind {
    /// The data kind this provider yields.
    #[must_use]
    pub const fn data_kind(&self) -> DataKind {
        match self {
            Self::BomWeather { .. } => DataKind::Weather,
        }
    }

    /// The wire `kind` tag.
    #[must_use]
    pub const fn tag(&self) -> &'static str {
        match self {
            Self::BomWeather { .. } => "bom_weather",
        }
    }
}

impl GraphicsDataSource {
    /// A BOM weather data source from a legacy `[weather]` location.
    #[must_use]
    pub fn from_weather_location(l: &WeatherLocation) -> Self {
        Self {
            id: l.id.clone(),
            name: l.name.clone(),
            provider: DataProviderKind::BomWeather {
                forecast_product: l.forecast_product.clone(),
                aac: l.aac.clone(),
                observation_product: l.observation_product.clone(),
                station_wmo: l.station_wmo.clone(),
            },
        }
    }

    /// The equivalent weather location (for the BOM poller) when this is a
    /// `bom_weather` source.
    #[must_use]
    pub fn as_weather_location(&self) -> Option<WeatherLocation> {
        match &self.provider {
            DataProviderKind::BomWeather {
                forecast_product,
                aac,
                observation_product,
                station_wmo,
            } => {
                let mut l = WeatherLocation::new(
                    self.id.clone(),
                    self.name.clone(),
                    forecast_product.clone(),
                    aac.clone(),
                );
                l.observation_product.clone_from(observation_product);
                l.station_wmo.clone_from(station_wmo);
                Some(l)
            }
        }
    }

    /// Structural validation.
    ///
    /// # Errors
    /// A [`ConfigError::Validation`] naming the offending field.
    pub fn validate(&self) -> Result<(), ConfigError> {
        match self.as_weather_location() {
            Some(l) => l.validate().map_err(|e| {
                ConfigError::Validation(e.to_string().replace("weather location", "data source"))
            }),
            None => Ok(()),
        }
    }
}

// ------------------------------------------------------------------ templates

/// One built-in template's declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct TemplateSpec {
    /// Stable id (`family.variant`).
    pub id: &'static str,
    /// Template family (`weather`; later `lower_third`, `ticker`, `full_screen`).
    pub family: &'static str,
    /// Display name.
    pub name: &'static str,
    /// What it draws.
    pub description: &'static str,
    /// The data kind it binds (`None` for a data-free template).
    pub data_kind: Option<DataKind>,
    /// The data fields it reads from the bound source.
    pub fields: &'static [&'static str],
    /// The style options it honours.
    pub style_options: &'static [&'static str],
}

/// The built-in template registry. A new family is one more entry here plus
/// its renderer arm.
pub const TEMPLATES: &[TemplateSpec] = &[
    TemplateSpec {
        id: "weather.seven_day",
        family: "weather",
        name: "Weather — 7-day outlook",
        description: "Seven daily columns: weekday, icon, max/min, précis and rain chance.",
        data_kind: Some(DataKind::Weather),
        fields: &[
            "name",
            "forecast.issued_local",
            "forecast.days[].weekday",
            "forecast.days[].icon_code",
            "forecast.days[].max_c",
            "forecast.days[].min_c",
            "forecast.days[].precis",
            "forecast.days[].rain_chance",
        ],
        style_options: &["background", "accent"],
    },
    TemplateSpec {
        id: "weather.today",
        family: "weather",
        name: "Weather — today",
        description: "Today's forecast detail plus the latest observation.",
        data_kind: Some(DataKind::Weather),
        fields: &[
            "name",
            "forecast.days[0]",
            "observation.air_temp_c",
            "observation.apparent_temp_c",
            "observation.rel_humidity_pct",
            "observation.wind_dir",
            "observation.wind_kmh",
            "observation.rain_since_9am_mm",
        ],
        style_options: &["background", "accent"],
    },
];

/// Look up a template by id.
#[must_use]
pub fn template(id: &str) -> Option<&'static TemplateSpec> {
    TEMPLATES.iter().find(|t| t.id == id)
}

// ---------------------------------------------------------------------- items

/// Style options for one item. Every field is optional; a template ignores
/// options it does not declare.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GraphicsStyle {
    /// Theme name (reserved; `default` today).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// Accent colour `#RRGGBB` (highlights / maxima).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accent: Option<String>,
    /// Opaque gradient or transparent (alpha) background.
    #[serde(default)]
    pub background: WeatherBackground,
}

/// A prepared graphic: template + data binding + style.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GraphicsItem {
    /// Stable id (`[A-Za-z0-9_-]`, 1–64).
    pub id: String,
    /// Operator-facing name.
    pub name: String,
    /// Template id from [`TEMPLATES`].
    pub template: String,
    /// The bound data-source id (required when the template needs data).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_source: Option<String>,
    /// A provider-specific key within the source (e.g. an RSS item index).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Style options.
    #[serde(default)]
    pub style: GraphicsStyle,
}

impl GraphicsItem {
    /// Build an item (the struct is `#[non_exhaustive]`).
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        template: impl Into<String>,
        data_source: Option<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            template: template.into(),
            data_source,
            key: None,
            style: GraphicsStyle::default(),
        }
    }
}

// ------------------------------------------------------------------- channels

/// Cut or mix (fade).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TransitionKind {
    /// Instant.
    #[default]
    Cut,
    /// Opacity ramp over `frames` output ticks.
    Mix,
}

/// A transition preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Transition {
    /// Cut or mix.
    #[serde(default)]
    pub kind: TransitionKind,
    /// Duration in output frames (mix only).
    #[serde(default = "default_frames")]
    pub frames: u32,
}

const fn default_frames() -> u32 {
    12
}

impl Default for Transition {
    fn default() -> Self {
        Self {
            kind: TransitionKind::Cut,
            frames: default_frames(),
        }
    }
}

impl Transition {
    /// Build a transition.
    #[must_use]
    pub const fn new(kind: TransitionKind, frames: u32) -> Self {
        Self { kind, frames }
    }
}

/// A playout channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GraphicsChannel {
    /// Stable id (`[A-Za-z0-9_-]`, 1–64), e.g. `gfx-channel-1`.
    pub id: String,
    /// Operator-facing name.
    pub name: String,
    /// Ordered item ids.
    #[serde(default)]
    pub rundown: Vec<String>,
    /// Transition preset used by TAKE / CLEAR.
    #[serde(default)]
    pub transition: Transition,
}

impl GraphicsChannel {
    /// Build a channel (the struct is `#[non_exhaustive]`).
    #[must_use]
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            rundown: Vec::new(),
            transition: Transition::default(),
        }
    }
}

/// Which bus a channel source shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum GraphicsBus {
    /// What is on air (transparent when clear).
    #[default]
    Program,
    /// What is cued.
    Preview,
}

// ----------------------------------------------------------------- validation

fn bad(msg: String) -> Result<(), ConfigError> {
    Err(ConfigError::Validation(msg))
}

fn check_name(what: &str, id: &str, name: &str) -> Result<(), ConfigError> {
    let n = name.trim();
    if n.is_empty() || n.chars().count() > 64 {
        return bad(format!("{what} {id:?}: name must be 1-64 characters"));
    }
    Ok(())
}

/// Serialization skip predicate for the boxed section.
#[allow(clippy::borrowed_box)] // reason: serde passes `&Box<T>` for a `Box<T>` field.
#[must_use]
pub fn is_empty_boxed(g: &Box<GraphicsConfig>) -> bool {
    g.is_empty()
}

/// Whether `s` is a `#RRGGBB` colour.
#[must_use]
pub fn is_hex_rgb(s: &str) -> bool {
    s.len() == 7
        && s.strip_prefix('#')
            .is_some_and(|h| h.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// The data sources in effect: `[graphics.data_sources]` plus any legacy
/// `[weather] locations` not shadowed by an id already present.
#[must_use]
pub fn effective_data_sources(
    graphics: &GraphicsConfig,
    weather: &WeatherConfig,
) -> Vec<GraphicsDataSource> {
    let mut out = graphics.data_sources.clone();
    for l in &weather.locations {
        if !out.iter().any(|d| d.id == l.id) {
            out.push(GraphicsDataSource::from_weather_location(l));
        }
    }
    out
}

impl GraphicsConfig {
    /// Whether nothing is configured (serialization skip predicate).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data_sources.is_empty() && self.items.is_empty() && self.channels.is_empty()
    }

    /// Look up an item.
    #[must_use]
    pub fn item(&self, id: &str) -> Option<&GraphicsItem> {
        self.items.iter().find(|i| i.id == id)
    }

    /// Look up a channel.
    #[must_use]
    pub fn channel(&self, id: &str) -> Option<&GraphicsChannel> {
        self.channels.iter().find(|c| c.id == id)
    }

    /// Validate the section against the effective data sources (which include
    /// legacy `[weather]` locations).
    ///
    /// # Errors
    /// A [`ConfigError::Validation`] naming the first problem.
    pub fn validate(&self, weather: &WeatherConfig) -> Result<(), ConfigError> {
        let sources = effective_data_sources(self, weather);
        if sources.len() > MAX_DATA_SOURCES {
            return bad(format!(
                "graphics declares {} data sources; at most {MAX_DATA_SOURCES} are allowed",
                sources.len()
            ));
        }
        let mut seen = std::collections::HashSet::new();
        for d in &self.data_sources {
            if !is_location_id(&d.id) {
                return bad(format!(
                    "data source {:?}: id must be 1-64 chars of [A-Za-z0-9_-]",
                    d.id
                ));
            }
            check_name("data source", &d.id, &d.name)?;
            d.validate()?;
            if !seen.insert(d.id.as_str()) {
                return bad(format!("duplicate data source id {:?}", d.id));
            }
        }
        if self.items.len() > MAX_ITEMS {
            return bad(format!("at most {MAX_ITEMS} graphics items are allowed"));
        }
        let mut items = std::collections::HashSet::new();
        for item in &self.items {
            validate_item(item, &sources)?;
            if !items.insert(item.id.as_str()) {
                return bad(format!("duplicate graphics item id {:?}", item.id));
            }
        }
        if self.channels.len() > MAX_CHANNELS {
            return bad(format!(
                "at most {MAX_CHANNELS} graphics channels are allowed"
            ));
        }
        let mut chans = std::collections::HashSet::new();
        for ch in &self.channels {
            validate_channel(ch, &items)?;
            if !chans.insert(ch.id.as_str()) {
                return bad(format!("duplicate graphics channel id {:?}", ch.id));
            }
        }
        Ok(())
    }
}

/// Validate one item against the available data sources.
///
/// # Errors
/// A [`ConfigError::Validation`] naming the field.
pub fn validate_item(
    item: &GraphicsItem,
    sources: &[GraphicsDataSource],
) -> Result<(), ConfigError> {
    if !is_location_id(&item.id) {
        return bad(format!(
            "graphics item {:?}: id must be 1-64 chars of [A-Za-z0-9_-]",
            item.id
        ));
    }
    check_name("graphics item", &item.id, &item.name)?;
    let Some(spec) = template(&item.template) else {
        return bad(format!(
            "graphics item {:?}: unknown template {:?}",
            item.id, item.template
        ));
    };
    match (spec.data_kind, item.data_source.as_deref()) {
        (Some(_), None) => {
            return bad(format!(
                "graphics item {:?}: template {} needs a data_source",
                item.id, spec.id
            ))
        }
        (Some(kind), Some(ds)) => match sources.iter().find(|d| d.id == ds) {
            None => {
                return bad(format!(
                    "graphics item {:?}: unknown data_source {ds:?}",
                    item.id
                ))
            }
            Some(d) if d.provider.data_kind() != kind => {
                return bad(format!(
                    "graphics item {:?}: data_source {ds:?} ({}) cannot feed template {}",
                    item.id,
                    d.provider.tag(),
                    spec.id
                ))
            }
            Some(_) => {}
        },
        (None, _) => {}
    }
    if let Some(key) = &item.key {
        if key.chars().count() > 256 {
            return bad(format!("graphics item {:?}: key too long", item.id));
        }
    }
    if let Some(accent) = &item.style.accent {
        if !is_hex_rgb(accent) {
            return bad(format!(
                "graphics item {:?}: accent must be #RRGGBB",
                item.id
            ));
        }
    }
    if let Some(theme) = &item.style.theme {
        if theme.chars().count() > 32 {
            return bad(format!("graphics item {:?}: theme too long", item.id));
        }
    }
    Ok(())
}

fn validate_channel(
    ch: &GraphicsChannel,
    items: &std::collections::HashSet<&str>,
) -> Result<(), ConfigError> {
    if !is_location_id(&ch.id) {
        return bad(format!(
            "graphics channel {:?}: id must be 1-64 chars of [A-Za-z0-9_-]",
            ch.id
        ));
    }
    check_name("graphics channel", &ch.id, &ch.name)?;
    if ch.rundown.len() > MAX_RUNDOWN {
        return bad(format!(
            "graphics channel {:?}: at most {MAX_RUNDOWN} rundown entries",
            ch.id
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for id in &ch.rundown {
        if !items.contains(id.as_str()) {
            return bad(format!(
                "graphics channel {:?}: rundown names unknown item {id:?}",
                ch.id
            ));
        }
        if !seen.insert(id.as_str()) {
            return bad(format!(
                "graphics channel {:?}: item {id:?} appears twice in the rundown",
                ch.id
            ));
        }
    }
    if ch.transition.frames == 0 || ch.transition.frames > MAX_TRANSITION_FRAMES {
        return bad(format!(
            "graphics channel {:?}: transition frames must be 1-{MAX_TRANSITION_FRAMES}",
            ch.id
        ));
    }
    Ok(())
}

/// Validate a `graphic` / `graphics_channel` source reference shape (the
/// target may be created later through the API).
///
/// # Errors
/// A [`ConfigError::Validation`] naming the source.
pub fn validate_source_ref(source_id: &str, what: &str, id: &str) -> Result<(), ConfigError> {
    if is_location_id(id) {
        Ok(())
    } else {
        bad(format!(
            "source {source_id:?}: graphics {what} {id:?} must be 1-64 chars of [A-Za-z0-9_-]"
        ))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
    use super::*;

    fn sydney() -> GraphicsDataSource {
        GraphicsDataSource::from_weather_location(
            &WeatherLocation::new("sydney", "Sydney", "IDN11060", "NSW_PT131")
                .with_observation("IDN60920", "94768"),
        )
    }

    #[test]
    fn registry_has_weather_templates() {
        assert_eq!(template("weather.seven_day").unwrap().family, "weather");
        assert!(template("weather.today").is_some());
        assert!(template("lower_third.basic").is_none());
    }

    #[test]
    fn validates_binding_and_rundown() {
        let mut g = GraphicsConfig {
            data_sources: vec![sydney()],
            items: vec![GraphicsItem::new(
                "syd7",
                "Sydney 7-day",
                "weather.seven_day",
                Some("sydney".into()),
            )],
            channels: vec![GraphicsChannel::new("gfx-channel-1", "Channel 1")],
        };
        g.channels[0].rundown.push("syd7".into());
        g.validate(&WeatherConfig::default()).unwrap();

        let mut unknown = g.clone();
        unknown.items[0].data_source = Some("nope".into());
        assert!(unknown.validate(&WeatherConfig::default()).is_err());

        let mut missing = g.clone();
        missing.channels[0].rundown.push("ghost".into());
        assert!(missing.validate(&WeatherConfig::default()).is_err());

        let mut accent = g.clone();
        accent.items[0].style.accent = Some("red".into());
        assert!(accent.validate(&WeatherConfig::default()).is_err());

        let mut tmpl = g;
        tmpl.items[0].template = "weather.nope".into();
        assert!(tmpl.validate(&WeatherConfig::default()).is_err());
    }

    #[test]
    fn legacy_weather_locations_are_effective_data_sources() {
        let weather = WeatherConfig {
            locations: vec![WeatherLocation::new(
                "sydney",
                "Sydney",
                "IDN11060",
                "NSW_PT131",
            )],
        };
        let g = GraphicsConfig {
            items: vec![GraphicsItem::new(
                "a",
                "A",
                "weather.today",
                Some("sydney".into()),
            )],
            ..GraphicsConfig::default()
        };
        g.validate(&weather).unwrap();
        let eff = effective_data_sources(&g, &weather);
        assert_eq!(eff.len(), 1);
        assert_eq!(eff[0].provider.tag(), "bom_weather");
    }
}
