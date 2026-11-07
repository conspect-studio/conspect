//! Upstream iptv-org API model (`https://iptv-org.github.io/api/*.json`).
//!
//! Promoted from the `xtask` soak tool's model (`xtask/src/iptv/model.rs`) and
//! widened to the full set the source browser joins. Every field the API may
//! omit is optional/defaulted so schema drift degrades to "absent" instead of a
//! hard parse failure; unknown fields are ignored.

use serde::{Deserialize, Serialize};

/// One `streams.json` row. Streams have no id: identity is `channel@feed` + URL.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Stream {
    /// Channel id (`null` for ~2,000 channel-less streams).
    #[serde(default)]
    pub channel: Option<String>,
    /// Feed id within the channel.
    #[serde(default)]
    pub feed: Option<String>,
    /// Display title.
    #[serde(default)]
    pub title: String,
    /// Playable URL.
    #[serde(default)]
    pub url: String,
    /// Advertised quality (`"720p"`).
    #[serde(default)]
    pub quality: Option<String>,
    /// Labels (`"Geo-blocked"`, `"Not 24/7"`).
    #[serde(default)]
    pub labels: Vec<String>,
    /// Required `User-Agent`.
    #[serde(default)]
    pub user_agent: Option<String>,
    /// Required `Referer`.
    #[serde(default)]
    pub referrer: Option<String>,
}

/// One `channels.json` row.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Channel {
    /// Channel id (`KAKE.us`).
    #[serde(default)]
    pub id: String,
    /// Name.
    #[serde(default)]
    pub name: Option<String>,
    /// Alternative names.
    #[serde(default)]
    pub alt_names: Vec<String>,
    /// Network.
    #[serde(default)]
    pub network: Option<String>,
    /// Owners.
    #[serde(default)]
    pub owners: Vec<String>,
    /// ISO 3166 country code.
    #[serde(default)]
    pub country: Option<String>,
    /// Category slugs.
    #[serde(default)]
    pub categories: Vec<String>,
    /// NSFW flag.
    #[serde(default)]
    pub is_nsfw: bool,
    /// Closure date.
    #[serde(default)]
    pub closed: Option<String>,
    /// Replacement channel id.
    #[serde(default)]
    pub replaced_by: Option<String>,
    /// Website.
    #[serde(default)]
    pub website: Option<String>,
}

/// One `feeds.json` row.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Feed {
    /// Owning channel id.
    #[serde(default)]
    pub channel: String,
    /// Feed id.
    #[serde(default)]
    pub id: String,
    /// Main feed of its channel.
    #[serde(default)]
    pub is_main: bool,
    /// Broadcast area (`r/`, `c/`, `s/`, `ct/` prefixed codes).
    #[serde(default)]
    pub broadcast_area: Vec<String>,
    /// Languages (ISO 639-3).
    #[serde(default)]
    pub languages: Vec<String>,
    /// Timezones.
    #[serde(default)]
    pub timezones: Vec<String>,
    /// Format (`480i`).
    #[serde(default)]
    pub format: Option<String>,
}

/// One `logos.json` row.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Logo {
    /// Channel id.
    #[serde(default)]
    pub channel: String,
    /// Feed id (channel-level when absent).
    #[serde(default)]
    pub feed: Option<String>,
    /// Whether upstream marks it in use.
    #[serde(default)]
    pub in_use: bool,
    /// Image width.
    #[serde(default)]
    pub width: u32,
    /// URL.
    #[serde(default)]
    pub url: String,
}

/// One `cities.json` row.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct City {
    /// Country code.
    #[serde(default)]
    pub country: String,
    /// Subdivision code.
    #[serde(default)]
    pub subdivision: Option<String>,
    /// City code (`USICT`).
    #[serde(default)]
    pub code: String,
    /// Name.
    #[serde(default)]
    pub name: String,
}

/// One `countries.json` row.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Country {
    /// Name.
    #[serde(default)]
    pub name: String,
    /// Code.
    #[serde(default)]
    pub code: String,
    /// Languages.
    #[serde(default)]
    pub languages: Vec<String>,
    /// Flag emoji.
    #[serde(default)]
    pub flag: Option<String>,
}

/// One `subdivisions.json` row.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Subdivision {
    /// Country code.
    #[serde(default)]
    pub country: String,
    /// Code (`US-KS`).
    #[serde(default)]
    pub code: String,
    /// Name.
    #[serde(default)]
    pub name: String,
    /// Parent subdivision.
    #[serde(default)]
    pub parent: Option<String>,
}

/// One `regions.json` row.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Region {
    /// Code (`NORAM`).
    #[serde(default)]
    pub code: String,
    /// Name.
    #[serde(default)]
    pub name: String,
    /// Member countries.
    #[serde(default)]
    pub countries: Vec<String>,
}

/// One `blocklist.json` row.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BlockEntry {
    /// Channel id.
    #[serde(default)]
    pub channel: String,
    /// `nsfw` or `dmca`.
    #[serde(default)]
    pub reason: String,
}

/// A `categories.json` / `languages.json` row (`id`/`code` + `name`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Named {
    /// Category id (`categories.json`).
    #[serde(default)]
    pub id: Option<String>,
    /// Language code (`languages.json`).
    #[serde(default)]
    pub code: Option<String>,
    /// Name.
    #[serde(default)]
    pub name: String,
}

/// Every upstream file the index is joined from.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RawCatalogue {
    /// `streams.json`.
    #[serde(default)]
    pub streams: Vec<Stream>,
    /// `channels.json`.
    #[serde(default)]
    pub channels: Vec<Channel>,
    /// `feeds.json`.
    #[serde(default)]
    pub feeds: Vec<Feed>,
    /// `logos.json`.
    #[serde(default)]
    pub logos: Vec<Logo>,
    /// `cities.json`.
    #[serde(default)]
    pub cities: Vec<City>,
    /// `countries.json`.
    #[serde(default)]
    pub countries: Vec<Country>,
    /// `subdivisions.json`.
    #[serde(default)]
    pub subdivisions: Vec<Subdivision>,
    /// `regions.json`.
    #[serde(default)]
    pub regions: Vec<Region>,
    /// `blocklist.json`.
    #[serde(default)]
    pub blocklist: Vec<BlockEntry>,
    /// `categories.json`.
    #[serde(default)]
    pub categories: Vec<Named>,
    /// `languages.json`.
    #[serde(default)]
    pub languages: Vec<Named>,
}

/// The upstream file names, in sync order. `(name, required)`.
pub const FILES: &[(&str, bool)] = &[
    ("streams", true),
    ("channels", true),
    ("feeds", true),
    ("logos", false),
    ("cities", false),
    ("countries", false),
    ("subdivisions", false),
    ("regions", false),
    ("blocklist", false),
    ("categories", false),
    ("languages", false),
];

impl RawCatalogue {
    /// Parse one named file's JSON bytes into the matching field.
    ///
    /// # Errors
    ///
    /// A message when `name` is unknown or the JSON does not match the model.
    pub fn set_file(&mut self, name: &str, bytes: &[u8]) -> Result<(), String> {
        fn p<T: serde::de::DeserializeOwned>(b: &[u8]) -> Result<Vec<T>, String> {
            serde_json::from_slice(b).map_err(|e| e.to_string())
        }
        match name {
            "streams" => self.streams = p(bytes)?,
            "channels" => self.channels = p(bytes)?,
            "feeds" => self.feeds = p(bytes)?,
            "logos" => self.logos = p(bytes)?,
            "cities" => self.cities = p(bytes)?,
            "countries" => self.countries = p(bytes)?,
            "subdivisions" => self.subdivisions = p(bytes)?,
            "regions" => self.regions = p(bytes)?,
            "blocklist" => self.blocklist = p(bytes)?,
            "categories" => self.categories = p(bytes)?,
            "languages" => self.languages = p(bytes)?,
            other => return Err(format!("unknown catalogue file {other:?}")),
        }
        Ok(())
    }

    /// Row count for a named file.
    #[must_use]
    pub fn len_of(&self, name: &str) -> usize {
        match name {
            "streams" => self.streams.len(),
            "channels" => self.channels.len(),
            "feeds" => self.feeds.len(),
            "logos" => self.logos.len(),
            "cities" => self.cities.len(),
            "countries" => self.countries.len(),
            "subdivisions" => self.subdivisions.len(),
            "regions" => self.regions.len(),
            "blocklist" => self.blocklist.len(),
            "categories" => self.categories.len(),
            "languages" => self.languages.len(),
            _ => 0,
        }
    }
}
