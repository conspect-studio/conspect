//! `[weather]` — Bureau of Meteorology (BOM) weather locations.
//!
//! Each location binds a public BOM forecast product (e.g. `IDN11060`) plus a
//! forecast area code (`aac`, e.g. `NSW_PT131` = Sydney), and optionally an
//! observation product (e.g. `IDN60920`) plus a station WMO id (e.g. `94768`).
//! The management-plane poller fetches each distinct product; a
//! [`SourceKind::Weather`](crate::SourceKind::Weather) source renders one
//! location. The whole section is optional (serde default) so existing
//! documents keep loading.

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// Upper bound on configured weather locations (bounded poller work).
pub const MAX_WEATHER_LOCATIONS: usize = 64;

/// The `[weather]` section.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WeatherConfig {
    /// Configured locations, each with a unique `id`.
    #[serde(default)]
    pub locations: Vec<WeatherLocation>,
}

/// One weather location (forecast area + optional observation station).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WeatherLocation {
    /// Stable id (`[A-Za-z0-9_-]`, 1–64 chars) referenced by weather sources.
    pub id: String,
    /// Display name drawn on the graphic (e.g. `Sydney`).
    pub name: String,
    /// BOM forecast product id (e.g. `IDN11060`).
    pub forecast_product: String,
    /// BOM forecast area code within that product (e.g. `NSW_PT131`).
    pub aac: String,
    /// BOM observation product id (e.g. `IDN60920`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_product: Option<String>,
    /// Station WMO id within the observation product (e.g. `94768`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub station_wmo: Option<String>,
}

impl WeatherLocation {
    /// Build a location (the struct is `#[non_exhaustive]`).
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        forecast_product: impl Into<String>,
        aac: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            forecast_product: forecast_product.into(),
            aac: aac.into(),
            observation_product: None,
            station_wmo: None,
        }
    }

    /// Attach an observation product + station.
    #[must_use]
    pub fn with_observation(
        mut self,
        product: impl Into<String>,
        station_wmo: impl Into<String>,
    ) -> Self {
        self.observation_product = Some(product.into());
        self.station_wmo = Some(station_wmo.into());
        self
    }

    /// Structural validation of one location.
    ///
    /// # Errors
    /// A [`ConfigError::Validation`] naming the offending field.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let bad = |what: &str| {
            Err(ConfigError::Validation(format!(
                "weather location {:?}: {what}",
                self.id
            )))
        };
        if !is_location_id(&self.id) {
            return bad("id must be 1-64 chars of [A-Za-z0-9_-]");
        }
        let name = self.name.trim();
        if name.is_empty() || name.chars().count() > 64 {
            return bad("name must be 1-64 characters");
        }
        if !is_bom_product_id(&self.forecast_product) {
            return bad("forecast_product must look like a BOM product id (e.g. IDN11060)");
        }
        if !is_area_code(&self.aac) {
            return bad("aac must be a BOM area code (e.g. NSW_PT131)");
        }
        match (&self.observation_product, &self.station_wmo) {
            (None, None) => {}
            (Some(product), Some(wmo)) => {
                if !is_bom_product_id(product) {
                    return bad(
                        "observation_product must look like a BOM product id (e.g. IDN60920)",
                    );
                }
                if wmo.is_empty() || wmo.len() > 8 || !wmo.bytes().all(|b| b.is_ascii_digit()) {
                    return bad("station_wmo must be a numeric WMO station id (e.g. 94768)");
                }
            }
            _ => return bad("observation_product and station_wmo must be set together"),
        }
        Ok(())
    }
}

impl WeatherConfig {
    /// Whether no location is configured (serialization skip predicate).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.locations.is_empty()
    }

    /// Look up a location by id.
    #[must_use]
    pub fn location(&self, id: &str) -> Option<&WeatherLocation> {
        self.locations.iter().find(|l| l.id == id)
    }

    /// Validate every location, id uniqueness, and the location bound.
    ///
    /// # Errors
    /// A [`ConfigError::Validation`] naming the first problem.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.locations.len() > MAX_WEATHER_LOCATIONS {
            return Err(ConfigError::Validation(format!(
                "weather declares {} locations; at most {MAX_WEATHER_LOCATIONS} are allowed",
                self.locations.len()
            )));
        }
        let mut seen = std::collections::HashSet::new();
        for location in &self.locations {
            location.validate()?;
            if !seen.insert(location.id.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "duplicate weather location id {:?}",
                    location.id
                )));
            }
        }
        Ok(())
    }
}

/// Validate a weather source's `location` reference shape (the location may
/// be configured later through the API, so existence is not required).
///
/// # Errors
/// A [`ConfigError::Validation`] naming the source.
pub fn validate_source_ref(source_id: &str, location: &str) -> Result<(), ConfigError> {
    if is_location_id(location) {
        Ok(())
    } else {
        Err(ConfigError::Validation(format!(
            "source {source_id:?}: weather location {location:?} must be 1-64 chars of [A-Za-z0-9_-]"
        )))
    }
}

/// Whether `id` is a valid weather location id.
#[must_use]
pub fn is_location_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Whether `id` has the shape of a BOM product id: `ID` + one state letter +
/// five digits (e.g. `IDN11060`). This also bounds the fetch path the poller
/// builds (never an arbitrary user URL).
#[must_use]
pub fn is_bom_product_id(id: &str) -> bool {
    let b = id.as_bytes();
    b.len() == 8
        && b.starts_with(b"ID")
        && b.get(2).is_some_and(u8::is_ascii_uppercase)
        && b.get(3..).is_some_and(|d| d.iter().all(u8::is_ascii_digit))
}

/// Whether `aac` has the shape of a BOM area code (`NSW_PT131`).
#[must_use]
pub fn is_area_code(aac: &str) -> bool {
    !aac.is_empty()
        && aac.len() <= 32
        && aac
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

/// Which graphic a [`SourceKind::Weather`](crate::SourceKind::Weather) draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WeatherTemplate {
    /// Seven daily columns: weekday, icon, max/min, précis, rain chance.
    #[default]
    SevenDay,
    /// Today's detail panel plus the latest observation.
    Today,
}

/// The background a weather source renders behind its panels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WeatherBackground {
    /// An opaque dark gradient (cheapest; composites on the GPU path).
    #[default]
    Gradient,
    /// Transparent: only the translucent panels carry alpha (NV12 + alpha
    /// plane), so the cell can be layered over a looped media source.
    Transparent,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_and_area_shapes() {
        assert!(is_bom_product_id("IDN11060"));
        assert!(is_bom_product_id("IDN60920"));
        assert!(!is_bom_product_id("IDN1106"));
        assert!(!is_bom_product_id("../etc/x"));
        assert!(!is_bom_product_id("idn11060"));
        assert!(is_area_code("NSW_PT131"));
        assert!(!is_area_code("nsw pt"));
    }

    #[test]
    fn validates_pairs_and_duplicates() {
        let ok = WeatherLocation::new("sydney", "Sydney", "IDN11060", "NSW_PT131")
            .with_observation("IDN60920", "94768");
        assert!(ok.validate().is_ok());
        let mut half = ok.clone();
        half.station_wmo = None;
        assert!(half.validate().is_err());
        let cfg = WeatherConfig {
            locations: vec![ok.clone(), ok],
        };
        assert!(cfg.validate().is_err());
    }
}
