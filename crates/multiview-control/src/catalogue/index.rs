//! The joined IPTV-ORG index: one row per **stream**, carrying everything the UI
//! filters on (docs/io/iptv-org.md §2.2), plus the place gazetteer with stream
//! counts (`places`). Pure: no I/O, no clock — `synced_at` is injected.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::gazetteer::Gazetteer;
use super::model::{Channel, Feed, RawCatalogue, Stream};

/// Snapshot schema version (bumped when the persisted/joined shape changes).
pub const SCHEMA_VERSION: u32 = 1;

/// The one flag vocabulary (UI, link tracker, docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Flag {
    /// iptv-org says it may be geo-blocked.
    GeoBlocked,
    /// Not on air all day.
    #[serde(rename = "not_24_7")]
    Not247,
    /// Needs a `User-Agent` and/or `Referer`.
    NeedsHeaders,
    /// Plain HTTP, no TLS.
    HttpOnly,
    /// Channel closed.
    Closed,
    /// Channel on the DMCA/NSFW blocklist (hidden by default).
    Blocklisted,
    /// Protocol Conspect cannot ingest.
    Unsupported,
    /// Stream has no channel.
    NoMetadata,
    /// A source is linked to this stream (computed per request).
    AlreadyAdded,
}

impl Flag {
    /// The wire token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GeoBlocked => "geo_blocked",
            Self::Not247 => "not_24_7",
            Self::NeedsHeaders => "needs_headers",
            Self::HttpOnly => "http_only",
            Self::Closed => "closed",
            Self::Blocklisted => "blocklisted",
            Self::Unsupported => "unsupported",
            Self::NoMetadata => "no_metadata",
            Self::AlreadyAdded => "already_added",
        }
    }
}

/// Geographic specificity of a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum GeoLevel {
    /// City-local.
    City,
    /// State / province local.
    Subdivision,
    /// Country-wide.
    Country,
    /// Multi-country region.
    Region,
    /// Unknown.
    None,
}

/// `geo` block of an index row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Geo {
    /// Most specific level the feed's broadcast area reaches.
    pub lvl: GeoLevel,
    /// Place codes at that level (`USICT`, `US-KS`, `US`, `NORAM`).
    pub places: Vec<String>,
}

/// `guide` of an index row: a guide is listed upstream (no feed URL exists).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexGuide {
    /// Guide site.
    pub site: String,
    /// Channel id on that site.
    pub site_id: String,
    /// Guide language.
    pub lang: Option<String>,
}

/// One joined index row (exact design JSON).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexRow {
    /// Stable key `s:<12 hex of sha256(url)>`.
    pub k: String,
    /// Stream URL.
    pub url: String,
    /// Stream title.
    pub title: String,
    /// Channel id.
    pub ch: Option<String>,
    /// Feed id.
    pub feed: Option<String>,
    /// Feed display name (`KSHB-TV`).
    pub feed_name: Option<String>,
    /// Channel name (or stream title when no channel).
    pub name: String,
    /// Alternative names.
    pub alt: Vec<String>,
    /// Network.
    pub net: Option<String>,
    /// Category slugs.
    pub cats: Vec<String>,
    /// Languages (ISO 639-3).
    pub langs: Vec<String>,
    /// Country code.
    pub country: Option<String>,
    /// Broadcast area codes (`ct/USICT`).
    pub area: Vec<String>,
    /// Geo summary.
    pub geo: Geo,
    /// Advertised quality.
    pub q: Option<String>,
    /// Protocol (`hls`, `dash`, `ts`, `rtmp`, `srt`, `rtsp`, `mmsh`, `udp`, `http`).
    pub proto: String,
    /// Computed flags.
    pub flags: Vec<Flag>,
    /// Required `User-Agent`.
    pub ua: Option<String>,
    /// Required `Referer`.
    #[serde(rename = "ref")]
    pub referrer: Option<String>,
    /// Logo key `l:<12 hex>`.
    pub logo: Option<String>,
    /// Absolute upstream logo URL (what a published playlist carries).
    pub logo_url: Option<String>,
    /// A guide listed upstream for this channel/feed.
    pub guide: Option<IndexGuide>,
    /// Closure date.
    pub closed: Option<String>,
    /// Replacement channel id.
    pub replaced_by: Option<String>,
    /// Website.
    pub website: Option<String>,
}

impl IndexRow {
    /// The catalogue labels this row's flags stand for (round-trips to `origin.labels`).
    #[must_use]
    pub fn labels(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.flags.contains(&Flag::GeoBlocked) {
            out.push("Geo-blocked".to_owned());
        }
        if self.flags.contains(&Flag::Not247) {
            out.push("Not 24/7".to_owned());
        }
        out
    }
}

/// `places.countries[]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaceCountry {
    /// ISO code.
    pub code: String,
    /// Name.
    pub name: String,
    /// Flag emoji.
    pub flag: Option<String>,
    /// Streams whose area lists this country directly.
    pub streams: u32,
    /// Streams in the country at any level (country + subdivisions + cities).
    pub streams_within: u32,
}

/// `places.subdivisions[]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaceSubdivision {
    /// Code (`US-KS`).
    pub code: String,
    /// Country code.
    pub country: String,
    /// Name.
    pub name: String,
    /// Direct streams.
    pub streams: u32,
    /// Streams in the subdivision incl. its cities.
    pub streams_within: u32,
}

/// `places.cities[]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaceCity {
    /// Code (`USICT`).
    pub code: String,
    /// Name.
    pub name: String,
    /// Country code.
    pub country: String,
    /// Subdivision code.
    pub subdivision: Option<String>,
    /// Latitude (WGS84), when the gazetteer knows it.
    pub lat: Option<f64>,
    /// Longitude (WGS84).
    pub lon: Option<f64>,
    /// Streams tagged to this city.
    pub streams: u32,
}

/// `places.regions[]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaceRegion {
    /// Code (`NORAM`).
    pub code: String,
    /// Name.
    pub name: String,
    /// Member countries.
    pub countries: Vec<String>,
    /// Streams tagged to this region.
    pub streams: u32,
}

/// The place gazetteer with stream counts.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Places {
    /// Every country.
    pub countries: Vec<PlaceCountry>,
    /// Subdivisions with at least one stream.
    pub subdivisions: Vec<PlaceSubdivision>,
    /// Cities with at least one stream.
    pub cities: Vec<PlaceCity>,
    /// Every region.
    pub regions: Vec<PlaceRegion>,
}

/// One gazetteer entry in the web contract's flat form. `id` uses the same
/// prefixed form as an index row's `area` (`ct/USMKC`, `s/US-KS`, `c/US`,
/// `r/NAM`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Place {
    /// Prefixed place id.
    pub id: String,
    /// `city`, `subdivision`, `country` or `region`.
    pub kind: &'static str,
    /// Display name.
    pub name: String,
    /// Alternative names. Upstream publishes none for places, so always empty.
    pub alt: Vec<String>,
    /// ISO alpha-2 country (null for a region).
    pub country: Option<String>,
    /// Subdivision code for a city / subdivision.
    pub subdivision: Option<String>,
    /// Latitude (cities with a gazetteer entry).
    pub lat: Option<f64>,
    /// Longitude.
    pub lon: Option<f64>,
    /// Flag emoji (countries).
    pub flag: Option<String>,
    /// Member country codes (regions).
    pub members: Vec<String>,
    /// Streams listing this place directly.
    pub streams: u32,
    /// Streams in this place at any level (countries, subdivisions).
    pub streams_within: u32,
}

impl Places {
    /// The flat web-contract list: countries, subdivisions, cities, regions.
    #[must_use]
    pub fn flat(&self) -> Vec<Place> {
        let mut out = Vec::new();
        for c in &self.countries {
            out.push(Place {
                id: format!("c/{}", c.code),
                kind: "country",
                name: c.name.clone(),
                alt: Vec::new(),
                country: Some(c.code.clone()),
                subdivision: None,
                lat: None,
                lon: None,
                flag: c.flag.clone(),
                members: Vec::new(),
                streams: c.streams,
                streams_within: c.streams_within,
            });
        }
        for s in &self.subdivisions {
            out.push(Place {
                id: format!("s/{}", s.code),
                kind: "subdivision",
                name: s.name.clone(),
                alt: Vec::new(),
                country: Some(s.country.clone()),
                subdivision: Some(s.code.clone()),
                lat: None,
                lon: None,
                flag: None,
                members: Vec::new(),
                streams: s.streams,
                streams_within: s.streams_within,
            });
        }
        for c in &self.cities {
            out.push(Place {
                id: format!("ct/{}", c.code),
                kind: "city",
                name: c.name.clone(),
                alt: Vec::new(),
                country: Some(c.country.clone()),
                subdivision: c.subdivision.clone(),
                lat: c.lat,
                lon: c.lon,
                flag: None,
                members: Vec::new(),
                streams: c.streams,
                streams_within: c.streams,
            });
        }
        for r in &self.regions {
            out.push(Place {
                id: format!("r/{}", r.code),
                kind: "region",
                name: r.name.clone(),
                alt: Vec::new(),
                country: None,
                subdivision: None,
                lat: None,
                lon: None,
                flag: None,
                members: r.countries.clone(),
                streams: r.streams,
                streams_within: r.streams,
            });
        }
        out
    }
}

/// An immutable joined snapshot.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// Schema version.
    pub schema: u32,
    /// Content identity (changes iff any upstream file changed) — the index `ETag` base.
    pub etag: String,
    /// RFC 3339 sync time.
    pub synced_at: String,
    /// Index rows in upstream order.
    pub rows: Vec<IndexRow>,
    /// Places with counts.
    pub places: Places,
    /// Logo key → upstream URL.
    pub logos: HashMap<String, String>,
    /// Category `(id, name)` lookup.
    pub categories: Vec<(String, String)>,
    /// Language `(code, name)` lookup.
    pub languages: Vec<(String, String)>,
    by_key: HashMap<String, usize>,
}

impl Snapshot {
    /// Attach the category/language display-name lookups.
    #[must_use]
    pub fn with_lookups(
        mut self,
        categories: Vec<(String, String)>,
        languages: Vec<(String, String)>,
    ) -> Self {
        self.categories = categories;
        self.languages = languages;
        self
    }

    /// Row by stable key.
    #[must_use]
    pub fn row(&self, k: &str) -> Option<&IndexRow> {
        self.by_key.get(k).and_then(|i| self.rows.get(*i))
    }

    /// Reconstruct a snapshot from rows (used by tests and the persisted path).
    #[must_use]
    pub fn from_parts(
        etag: String,
        synced_at: String,
        rows: Vec<IndexRow>,
        places: Places,
        logos: HashMap<String, String>,
    ) -> Self {
        let by_key = rows
            .iter()
            .enumerate()
            .map(|(i, r)| (r.k.clone(), i))
            .collect();
        Self {
            schema: SCHEMA_VERSION,
            etag,
            synced_at,
            rows,
            places,
            logos,
            categories: Vec::new(),
            languages: Vec::new(),
            by_key,
        }
    }
}

fn hex12(input: &str) -> String {
    super::store::hex_prefix(&Sha256::digest(input.as_bytes()), 6)
}

/// The stable stream key for a URL.
#[must_use]
pub fn stream_key(url: &str) -> String {
    format!("s:{}", hex12(url))
}

/// The protocol token for a URL.
#[must_use]
pub fn protocol_of(url: &str) -> &'static str {
    let lower = url.to_ascii_lowercase();
    let scheme = lower.split("://").next().unwrap_or("");
    match scheme {
        "rtmp" | "rtmps" | "rtmpe" => "rtmp",
        "srt" => "srt",
        "rtsp" | "rtsps" => "rtsp",
        "mmsh" | "mms" | "mmst" => "mmsh",
        "udp" | "rtp" => "udp",
        "http" | "https" => {
            let path = lower.split(['?', '#']).next().unwrap_or("");
            let ext = std::path::Path::new(path)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            if ext.eq_ignore_ascii_case("mpd") {
                "dash"
            } else if ext.eq_ignore_ascii_case("ts") {
                "ts"
            } else if ext.eq_ignore_ascii_case("m3u8") || lower.contains("m3u8") {
                "hls"
            } else {
                "http"
            }
        }
        _ => "http",
    }
}

fn compute_flags(
    stream: &Stream,
    channel: Option<&Channel>,
    blocklisted: bool,
    proto: &str,
) -> Vec<Flag> {
    let mut flags = Vec::new();
    let has = |l: &str| stream.labels.iter().any(|x| x.eq_ignore_ascii_case(l));
    if has("Geo-blocked") {
        flags.push(Flag::GeoBlocked);
    }
    if has("Not 24/7") {
        flags.push(Flag::Not247);
    }
    if stream.user_agent.is_some() || stream.referrer.is_some() {
        flags.push(Flag::NeedsHeaders);
    }
    if stream.url.to_ascii_lowercase().starts_with("http://") {
        flags.push(Flag::HttpOnly);
    }
    if channel.is_some_and(|c| c.closed.is_some()) {
        flags.push(Flag::Closed);
    }
    if blocklisted {
        flags.push(Flag::Blocklisted);
    }
    if matches!(proto, "mmsh" | "dash") {
        flags.push(Flag::Unsupported);
    }
    if stream.channel.is_none() {
        flags.push(Flag::NoMetadata);
    }
    flags
}

fn geo_of(area: &[String]) -> Geo {
    for (prefix, lvl) in [
        ("ct/", GeoLevel::City),
        ("s/", GeoLevel::Subdivision),
        ("c/", GeoLevel::Country),
        ("r/", GeoLevel::Region),
    ] {
        let places: Vec<String> = area
            .iter()
            .filter_map(|a| a.strip_prefix(prefix))
            .map(str::to_owned)
            .collect();
        if !places.is_empty() {
            return Geo { lvl, places };
        }
    }
    Geo {
        lvl: GeoLevel::None,
        places: Vec::new(),
    }
}

/// Join a raw catalogue into a [`Snapshot`].
///
/// NSFW channels (flag or blocklist reason) are dropped; DMCA-blocklisted channels
/// are kept and flagged `blocklisted`.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn build_snapshot(
    raw: &RawCatalogue,
    gazetteer: &Gazetteer,
    etag: String,
    synced_at: String,
) -> Snapshot {
    let channels: HashMap<&str, &Channel> =
        raw.channels.iter().map(|c| (c.id.as_str(), c)).collect();
    let mut feeds_by_key: HashMap<(&str, &str), &Feed> = HashMap::new();
    let mut main_feed: HashMap<&str, &Feed> = HashMap::new();
    let mut first_feed: HashMap<&str, &Feed> = HashMap::new();
    for f in &raw.feeds {
        feeds_by_key.insert((f.channel.as_str(), f.id.as_str()), f);
        if f.is_main {
            main_feed.entry(f.channel.as_str()).or_insert(f);
        }
        first_feed.entry(f.channel.as_str()).or_insert(f);
    }
    let blocked: HashMap<&str, &str> = raw
        .blocklist
        .iter()
        .map(|b| (b.channel.as_str(), b.reason.as_str()))
        .collect();
    let mut logos_by_channel: HashMap<&str, Vec<&super::model::Logo>> = HashMap::new();
    for l in &raw.logos {
        logos_by_channel
            .entry(l.channel.as_str())
            .or_default()
            .push(l);
    }

    let mut guides_by_channel: HashMap<&str, Vec<&super::model::Guide>> = HashMap::new();
    for g in &raw.guides {
        if let Some(ch) = g.channel.as_deref() {
            guides_by_channel.entry(ch).or_default().push(g);
        }
    }

    let mut rows: Vec<IndexRow> = Vec::with_capacity(raw.streams.len());
    let mut seen: HashSet<String> = HashSet::new();
    let mut logos: HashMap<String, String> = HashMap::new();
    for stream in &raw.streams {
        if stream.url.is_empty() {
            continue;
        }
        let channel = stream
            .channel
            .as_deref()
            .and_then(|id| channels.get(id).copied());
        let block_reason = stream
            .channel
            .as_deref()
            .and_then(|id| blocked.get(id).copied());
        if channel.is_some_and(|c| c.is_nsfw) || block_reason == Some("nsfw") {
            continue;
        }
        let feed = stream.channel.as_deref().and_then(|ch| {
            stream
                .feed
                .as_deref()
                .and_then(|f| feeds_by_key.get(&(ch, f)).copied())
                .or_else(|| main_feed.get(ch).copied())
                .or_else(|| first_feed.get(ch).copied())
        });
        let area = feed.map(|f| f.broadcast_area.clone()).unwrap_or_default();
        let proto = protocol_of(&stream.url);
        let mut k = stream_key(&stream.url);
        if !seen.insert(k.clone()) {
            k = stream_key(&format!(
                "{}|{}@{}",
                stream.url,
                stream.channel.as_deref().unwrap_or(""),
                stream.feed.as_deref().unwrap_or("")
            ));
            if !seen.insert(k.clone()) {
                continue;
            }
        }
        let guide = stream.channel.as_deref().and_then(|ch| {
            let candidates = guides_by_channel.get(ch)?;
            let pick = candidates
                .iter()
                .find(|g| g.feed.is_some() && g.feed == stream.feed)
                .or_else(|| candidates.iter().find(|g| g.feed.is_none()))
                .or_else(|| candidates.first())?;
            Some(IndexGuide {
                site: pick.site.clone(),
                site_id: pick.site_id.clone(),
                lang: pick.lang.clone(),
            })
        });
        let (logo, logo_url) = stream.channel.as_deref().map_or((None, None), |ch| {
            let Some(candidates) = logos_by_channel.get(ch) else {
                return (None, None);
            };
            let Some(pick) = candidates
                .iter()
                .find(|l| l.in_use && l.feed.is_some() && l.feed == stream.feed)
                .or_else(|| candidates.iter().find(|l| l.in_use && l.feed.is_none()))
                .or_else(|| candidates.iter().find(|l| l.in_use))
                .or_else(|| candidates.first())
            else {
                return (None, None);
            };
            let key = format!("l:{}", hex12(&pick.url));
            logos.insert(key.clone(), pick.url.clone());
            (Some(key), Some(pick.url.clone()))
        });
        rows.push(IndexRow {
            k,
            url: stream.url.clone(),
            title: stream.title.clone(),
            ch: stream.channel.clone(),
            feed: stream.feed.clone(),
            feed_name: feed.and_then(|f| f.name.clone()),
            name: channel
                .and_then(|c| c.name.clone())
                .unwrap_or_else(|| stream.title.clone()),
            alt: channel.map(|c| c.alt_names.clone()).unwrap_or_default(),
            net: channel.and_then(|c| c.network.clone()),
            cats: channel.map(|c| c.categories.clone()).unwrap_or_default(),
            langs: feed.map(|f| f.languages.clone()).unwrap_or_default(),
            country: channel.and_then(|c| c.country.clone()),
            geo: geo_of(&area),
            area,
            q: stream.quality.clone(),
            proto: proto.to_owned(),
            flags: compute_flags(stream, channel, block_reason.is_some(), proto),
            ua: stream.user_agent.clone(),
            referrer: stream.referrer.clone(),
            logo,
            logo_url,
            guide,
            closed: channel.and_then(|c| c.closed.clone()),
            replaced_by: channel.and_then(|c| c.replaced_by.clone()),
            website: channel.and_then(|c| c.website.clone()),
        });
    }
    let places = build_places(raw, gazetteer, &rows);
    let categories = raw
        .categories
        .iter()
        .filter_map(|c| Some((c.id.clone()?, c.name.clone())))
        .collect();
    let languages = raw
        .languages
        .iter()
        .filter_map(|l| Some((l.code.clone()?, l.name.clone())))
        .collect();
    Snapshot::from_parts(etag, synced_at, rows, places, logos).with_lookups(categories, languages)
}

fn build_places(raw: &RawCatalogue, gazetteer: &Gazetteer, rows: &[IndexRow]) -> Places {
    let cities: HashMap<&str, &super::model::City> =
        raw.cities.iter().map(|c| (c.code.as_str(), c)).collect();
    let subs: HashMap<&str, &super::model::Subdivision> = raw
        .subdivisions
        .iter()
        .map(|s| (s.code.as_str(), s))
        .collect();
    let mut direct: BTreeMap<String, u32> = BTreeMap::new();
    let mut within: BTreeMap<String, u32> = BTreeMap::new();
    for row in rows {
        let mut direct_set: BTreeSet<String> = BTreeSet::new();
        let mut within_set: BTreeSet<String> = BTreeSet::new();
        for a in &row.area {
            direct_set.insert(a.clone());
            within_set.insert(a.clone());
            if let Some(code) = a.strip_prefix("ct/") {
                if let Some(city) = cities.get(code) {
                    if let Some(sub) = &city.subdivision {
                        within_set.insert(format!("s/{sub}"));
                    }
                    within_set.insert(format!("c/{}", city.country));
                }
            } else if let Some(code) = a.strip_prefix("s/") {
                if let Some(sub) = subs.get(code) {
                    within_set.insert(format!("c/{}", sub.country));
                }
            }
        }
        for a in direct_set {
            *direct.entry(a).or_default() += 1;
        }
        for a in within_set {
            *within.entry(a).or_default() += 1;
        }
    }
    let get = |m: &BTreeMap<String, u32>, k: String| m.get(&k).copied().unwrap_or(0);
    let countries = raw
        .countries
        .iter()
        .map(|c| PlaceCountry {
            code: c.code.clone(),
            name: c.name.clone(),
            flag: c.flag.clone(),
            streams: get(&direct, format!("c/{}", c.code)),
            streams_within: get(&within, format!("c/{}", c.code)),
        })
        .collect();
    let subdivisions = raw
        .subdivisions
        .iter()
        .filter_map(|s| {
            let within_n = get(&within, format!("s/{}", s.code));
            (within_n > 0).then(|| PlaceSubdivision {
                code: s.code.clone(),
                country: s.country.clone(),
                name: s.name.clone(),
                streams: get(&direct, format!("s/{}", s.code)),
                streams_within: within_n,
            })
        })
        .collect();
    let city_places = raw
        .cities
        .iter()
        .filter_map(|c| {
            let n = get(&direct, format!("ct/{}", c.code));
            (n > 0).then(|| {
                let coords = gazetteer.get(&c.code);
                PlaceCity {
                    code: c.code.clone(),
                    name: c.name.clone(),
                    country: c.country.clone(),
                    subdivision: c.subdivision.clone(),
                    lat: coords.map(|p| p.0),
                    lon: coords.map(|p| p.1),
                    streams: n,
                }
            })
        })
        .collect();
    let regions = raw
        .regions
        .iter()
        .map(|r| PlaceRegion {
            code: r.code.clone(),
            name: r.name.clone(),
            countries: r.countries.clone(),
            streams: get(&direct, format!("r/{}", r.code)),
        })
        .collect();
    Places {
        countries,
        subdivisions,
        cities: city_places,
        regions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_tokens() {
        assert_eq!(protocol_of("https://h/x.m3u8?t=1"), "hls");
        assert_eq!(protocol_of("rtmp://h/x"), "rtmp");
        assert_eq!(protocol_of("mmsh://h/x"), "mmsh");
        assert_eq!(protocol_of("http://h/x.mpd"), "dash");
        assert_eq!(protocol_of("udp://[ff3e::1]:5000"), "udp");
    }

    #[test]
    fn geo_prefers_the_most_specific_level() {
        let g = geo_of(&["c/AD".to_owned(), "ct/ESLSL".to_owned()]);
        assert_eq!(g.lvl, GeoLevel::City);
        assert_eq!(g.places, vec!["ESLSL"]);
        assert_eq!(geo_of(&[]).lvl, GeoLevel::None);
    }

    #[test]
    fn flag_wire_tokens_match_the_design_vocabulary() {
        for f in [
            Flag::GeoBlocked,
            Flag::Not247,
            Flag::NeedsHeaders,
            Flag::HttpOnly,
            Flag::Closed,
            Flag::Blocklisted,
            Flag::Unsupported,
            Flag::NoMetadata,
            Flag::AlreadyAdded,
        ] {
            assert_eq!(
                serde_json::to_string(&f).unwrap(),
                format!("\"{}\"", f.as_str())
            );
        }
    }

    #[test]
    fn stream_key_is_stable_and_12_hex() {
        let k = stream_key("https://a/b.m3u8");
        assert!(k.starts_with("s:") && k.len() == 14);
        assert_eq!(k, stream_key("https://a/b.m3u8"));
    }
}
