//! Deserialization model for the iptv-org public API
//! (`streams.json` + `channels.json`) and the local `blocklist.json`.
//!
//! These mirror the documented iptv-org JSON shapes; every field that the API
//! may omit is `Option`, so a schema drift degrades to "absent" rather than a
//! hard parse failure (resilience-first: a quirky catalog is the point).

use serde::Deserialize;

/// One entry from iptv-org `streams.json`.
///
/// A stream references a channel by id (`channel`), carries the playable `url`,
/// and may carry per-stream playback hints (`user_agent`, `referrer`), a free
/// text resilience `label` (e.g. `"Not 24/7"`, `"Geo-blocked"`), and a coarse
/// `quality` string (e.g. `"1080p"`, `"576i"`, `null`).
#[derive(Debug, Clone, Deserialize)]
pub struct Stream {
    /// The channel id this stream belongs to (joins to [`Channel::id`]).
    /// `None` for the small number of channel-less feeds iptv-org carries.
    #[serde(default)]
    pub channel: Option<String>,
    /// The playable master URL (HLS `.m3u8`, DASH `.mpd`, or raw `.ts`).
    pub url: String,
    /// Coarse quality string as iptv-org records it (e.g. `"1080p"`, `"576i"`).
    #[serde(default)]
    pub quality: Option<String>,
    /// A `User-Agent` the origin requires (must be replayed on fetch/probe).
    #[serde(default)]
    pub user_agent: Option<String>,
    /// A `Referer` the origin requires (must be replayed on fetch/probe).
    #[serde(default)]
    pub referrer: Option<String>,
    /// A free-text resilience label (e.g. `"Not 24/7"`, `"Geo-blocked"`).
    #[serde(default)]
    pub label: Option<String>,
}

/// One entry from iptv-org `channels.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct Channel {
    /// The channel id (joins to [`Stream::channel`]).
    pub id: String,
    /// Human-readable channel name.
    #[serde(default)]
    pub name: Option<String>,
    /// Category slugs (e.g. `"news"`, `"sports"`, `"movies"`).
    #[serde(default)]
    pub categories: Vec<String>,
    /// ISO-3166 country code (e.g. `"US"`).
    #[serde(default)]
    pub country: Option<String>,
    /// Whether iptv-org flags the channel as NSFW.
    #[serde(default)]
    pub is_nsfw: bool,
}
