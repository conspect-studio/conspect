//! The **pure** `YouTube` resolver core (ADR-0015 phase P0): parse a `yt-dlp -J`
//! info-dict into a [`ResolvedHls`], classify live status, and read the `expire`
//! deadline off a resolved `*.googlevideo.com` URL.
//!
//! Everything here is I/O-free and panic-free — no network, no subprocess. It is
//! fixture-testable in the default-shaped build and carries the correctness load;
//! the [`super::process`] spawn wrapper is a thin shell around these fns.

use serde::Deserialize;

use super::YoutubeError;

/// `YouTube`'s `live_status` classification, as emitted by `yt-dlp -J`.
///
/// Only [`LiveStatus::Live`] yields a playable live tile; the others are
/// reported so the caller can surface *why* a source is not (yet) ingestable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LiveStatus {
    /// `is_live` — broadcasting right now; a live HLS master is expected.
    Live,
    /// `is_upcoming` — scheduled, not yet started (no playable master yet).
    Upcoming,
    /// `post_live` — just ended, a re-watchable DVR window may remain, but it is
    /// not a *live* tile.
    PostLive,
    /// `was_live` — a finished broadcast now served as VOD.
    WasLive,
    /// `not_live` — an ordinary (never-live) video.
    NotLive,
    /// A `live_status` value yt-dlp emitted that this resolver does not model.
    Unknown,
}

impl LiveStatus {
    /// Classify a raw `live_status` string from the info-dict.
    #[must_use]
    pub fn from_raw(raw: &str) -> Self {
        match raw {
            "is_live" => Self::Live,
            "is_upcoming" => Self::Upcoming,
            "post_live" => Self::PostLive,
            "was_live" => Self::WasLive,
            "not_live" => Self::NotLive,
            _ => Self::Unknown,
        }
    }
}

/// A resolved live `YouTube` source: the HLS master URL libav will open, its
/// classified live status, and the parsed `expire` deadline (when present).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ResolvedHls {
    /// The `*.googlevideo.com` HLS **master** playlist URL (the `m3u8_native`
    /// format's `manifest_url`) — ready to feed to the standard HLS ingest path.
    pub manifest_url: String,
    /// The classified live status (always [`LiveStatus::Live`] for a successful
    /// resolve; carried so the caller can log/telemeter it).
    pub live_status: LiveStatus,
    /// The `expire` Unix timestamp (seconds since 1970-01-01) parsed off the
    /// resolved URL, after which the CDN 403s. `None` when absent — the caller
    /// then falls back to the TTL upper-bound guard (ADR-0015 §4).
    pub expire_unix: Option<i64>,
}

impl ResolvedHls {
    /// Construct a [`ResolvedHls`] from its parts.
    ///
    /// `ResolvedHls` is `#[non_exhaustive]`, so callers in other crates (and the
    /// re-resolution loop's tests/fakes) cannot use a struct literal; this
    /// constructor is the supported way to build one. `expire_unix` is the `expire`
    /// deadline parsed off `manifest_url` (typically via [`parse_expire`]), or
    /// `None` when absent.
    #[must_use]
    pub fn new(
        manifest_url: impl Into<String>,
        live_status: LiveStatus,
        expire_unix: Option<i64>,
    ) -> Self {
        Self {
            manifest_url: manifest_url.into(),
            live_status,
            expire_unix,
        }
    }
}

/// One entry of the `yt-dlp -J` `formats` array. Only the fields the resolver
/// reads are modelled; `yt-dlp` emits many more (all ignored).
#[derive(Debug, Deserialize)]
struct InfoFormat {
    /// The transport protocol; the HLS master is `"m3u8_native"`.
    #[serde(default)]
    protocol: Option<String>,
    /// The master/variant **manifest** URL (present for HLS/DASH formats). This —
    /// not `url` — is the master playlist Multiview feeds to libav.
    #[serde(default)]
    manifest_url: Option<String>,
}

/// The subset of the `yt-dlp -J` top-level info-dict the resolver reads.
#[derive(Debug, Deserialize)]
struct InfoDict {
    /// `live_status ∈ {is_live, post_live, is_upcoming, was_live, not_live}`.
    #[serde(default)]
    live_status: Option<String>,
    /// Legacy boolean live flag; used only when `live_status` is absent.
    #[serde(default)]
    is_live: Option<bool>,
    /// The available formats (renditions). May be empty for an upcoming stream.
    #[serde(default)]
    formats: Vec<InfoFormat>,
}

impl InfoDict {
    /// Resolve the effective [`LiveStatus`], preferring the explicit
    /// `live_status` string and falling back to the legacy `is_live` boolean.
    fn classify(&self) -> LiveStatus {
        match self.live_status.as_deref() {
            Some(raw) => LiveStatus::from_raw(raw),
            None => match self.is_live {
                Some(true) => LiveStatus::Live,
                Some(false) => LiveStatus::NotLive,
                None => LiveStatus::Unknown,
            },
        }
    }
}

/// Parse a `yt-dlp -J` JSON info-dict into a [`ResolvedHls`].
///
/// Reads the top-level `live_status`/`is_live`, requires it to classify as
/// [`LiveStatus::Live`], then selects the first HLS master (`protocol ==
/// "m3u8_native"` with a `manifest_url`) and parses the `expire` deadline off it.
/// The processed `manifest_url` yt-dlp emits is used verbatim — the raw
/// `streamingData` URL is never reconstructed (ADR-0015 §3).
///
/// # Errors
///
/// - [`YoutubeError::Json`] if `json` is not a well-formed info-dict.
/// - [`YoutubeError::NotLive`] (carrying the classification) if the stream is not
///   live (upcoming / post-live / VOD).
/// - [`YoutubeError::NoHlsMaster`] if a live stream carries no `m3u8_native`
///   format with a `manifest_url`.
pub fn parse_info_dict(json: &str) -> Result<ResolvedHls, YoutubeError> {
    let info: InfoDict = serde_json::from_str(json)?;

    let live_status = info.classify();
    if live_status != LiveStatus::Live {
        return Err(YoutubeError::NotLive(live_status));
    }

    let manifest_url = info
        .formats
        .iter()
        .filter(|f| f.protocol.as_deref() == Some("m3u8_native"))
        .find_map(|f| f.manifest_url.clone())
        .ok_or(YoutubeError::NoHlsMaster)?;

    let expire_unix = parse_expire(&manifest_url);

    Ok(ResolvedHls {
        manifest_url,
        live_status,
        expire_unix,
    })
}

/// Read the `expire` Unix timestamp (seconds since 1970-01-01) from a resolved
/// `*.googlevideo.com` URL.
///
/// googlevideo carries `expire` in one of two forms — a `?…&expire=<n>&…` query
/// value or an `/expire/<n>/` path segment — and after that instant the CDN
/// returns HTTP 403. Both forms are accepted; a non-numeric or absent `expire`
/// yields `None` (the caller then falls back to the TTL upper-bound guard).
/// Never panics.
#[must_use]
pub fn parse_expire(url: &str) -> Option<i64> {
    // Query form: split off the `?…` part, then scan `&`-separated `k=v` pairs.
    if let Some((_, query)) = url.split_once('?') {
        for pair in query.split('&') {
            if let Some(value) = pair.strip_prefix("expire=") {
                if let Ok(ts) = value.parse::<i64>() {
                    return Some(ts);
                }
            }
        }
    }

    // Path form: `/expire/<digits>/`. Find the marker, then take the next segment.
    if let Some((_, after)) = url.split_once("/expire/") {
        let segment = after.split('/').next().unwrap_or(after);
        if let Ok(ts) = segment.parse::<i64>() {
            return Some(ts);
        }
    }

    None
}
