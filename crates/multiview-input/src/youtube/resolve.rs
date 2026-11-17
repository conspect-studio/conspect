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

    /// The stable, lower-case wire label for this status — the exact yt-dlp
    /// `live_status` token (`was_live` / `not_live` / `upcoming` / `post_live`),
    /// or `live`/`unknown` for the remaining variants. Used to build the
    /// operator-facing "not a live stream" message
    /// ([`YoutubeError::operator_message`](super::YoutubeError::operator_message))
    /// so the surfaced reason names the exact classification, not a Rust
    /// `Debug` dump.
    #[must_use]
    pub const fn wire_label(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Upcoming => "upcoming",
            Self::PostLive => "post_live",
            Self::WasLive => "was_live",
            Self::NotLive => "not_live",
            Self::Unknown => "unknown",
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
    /// The family the successful `yt-dlp` invocation was forced to
    /// ([`ResolveFamily::Auto`] when it ran with the system default). Set by
    /// the subprocess shell; [`parse_info_dict`] leaves it `Auto`.
    pub resolved_over: ResolveFamily,
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
            resolved_over: ResolveFamily::Auto,
        }
    }

    /// Record the family the resolve ran over (see [`Self::resolved_over`]).
    #[must_use]
    pub fn with_resolved_over(mut self, family: ResolveFamily) -> Self {
        self.resolved_over = family;
        self
    }

    /// The family the media fetch of this master must use ([`media_family`]).
    #[must_use]
    pub fn media_family(&self) -> MediaFamily {
        media_family(&self.manifest_url, self.resolved_over)
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
        resolved_over: ResolveFamily::Auto,
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

/// The address family the `yt-dlp` **resolve** runs over (issue #498), the
/// `[system.youtube] address_family` option.
///
/// `YouTube` may bot-check one family of a host and not the other (mda001:
/// its IPv6 /64 answers "Sign in to confirm you're not a bot", its IPv4 does
/// not). [`Self::Auto`] runs `yt-dlp` with the system default first and, on a
/// bot check, retries that invocation once with `-4`; [`Self::V4`] / [`Self::V6`]
/// always pass `-4` / `-6`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum ResolveFamily {
    /// System default first; on a bot check, retry over IPv4.
    #[default]
    Auto,
    /// Always resolve over IPv4 (`yt-dlp -4`).
    V4,
    /// Always resolve over IPv6 (`yt-dlp -6`).
    V6,
}

impl ResolveFamily {
    /// Parse the config/env spelling: `auto`, `v4`/`ipv4`, `v6`/`ipv6`
    /// (ASCII case-insensitive, surrounding whitespace ignored). `None` for
    /// anything else.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "v4" | "ipv4" => Some(Self::V4),
            "v6" | "ipv6" => Some(Self::V6),
            _ => None,
        }
    }

    /// The `yt-dlp` flag that forces this family, `None` for [`Self::Auto`].
    #[must_use]
    pub const fn flag(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::V4 => Some("-4"),
            Self::V6 => Some("-6"),
        }
    }
}

/// The address family a resolved source's **media** fetch must use (issue #498).
///
/// googlevideo signs a manifest for the client address that resolved it
/// (`/ip/<addr>/` in the path, or `ip=<addr>` in the query) and answers 403 to
/// a segment fetched from the other family. Every HTTP request for the source
/// (variant pin, segment fetcher, audio rail) is pinned to this family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MediaFamily {
    /// No pin: the system default (dual-stack, IPv6 preferred).
    Any,
    /// IPv4 only.
    V4,
    /// IPv6 only.
    V6,
}

/// The client address a googlevideo URL is signed for: the `/ip/<addr>/` path
/// segment, else an `ip=<addr>` query value (`%3A` decoded to `:`). `None` when
/// absent or not an address. Pure, never panics.
#[must_use]
pub fn parse_signed_ip(url: &str) -> Option<std::net::IpAddr> {
    let decode = |raw: &str| -> Option<std::net::IpAddr> {
        raw.replace("%3A", ":")
            .replace("%3a", ":")
            .parse::<std::net::IpAddr>()
            .ok()
    };
    let (path, query) = match url.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (url, None),
    };
    let path = path.split('#').next().unwrap_or(path);
    let mut segments = path.split('/');
    while let Some(segment) = segments.next() {
        if segment == "ip" {
            if let Some(ip) = segments.next().and_then(decode) {
                return Some(ip);
            }
        }
    }
    let query = query?.split('#').next().unwrap_or_default();
    query
        .split('&')
        .filter_map(|pair| pair.strip_prefix("ip="))
        .find_map(decode)
}

/// The family a resolved master must be fetched over (issue #498): the family of
/// the address the URL is signed for ([`parse_signed_ip`]); with no signed
/// address, the family the resolve was forced to (`resolve`), else
/// [`MediaFamily::Any`]. The URL wins over the setting because it is what the
/// CDN checks: an `auto` resolve that fell back to `-4` yields an `/ip/<v4>`
/// URL, and a re-resolve that returns the other family switches the pin with it.
#[must_use]
pub fn media_family(manifest_url: &str, resolve: ResolveFamily) -> MediaFamily {
    match parse_signed_ip(manifest_url) {
        Some(std::net::IpAddr::V4(_)) => MediaFamily::V4,
        Some(std::net::IpAddr::V6(v6)) if v6.to_ipv4_mapped().is_some() => MediaFamily::V4,
        Some(std::net::IpAddr::V6(_)) => MediaFamily::V6,
        None => match resolve {
            ResolveFamily::Auto => MediaFamily::Any,
            ResolveFamily::V4 => MediaFamily::V4,
            ResolveFamily::V6 => MediaFamily::V6,
        },
    }
}
