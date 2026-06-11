//! Container detection + the quirk classifier.
//!
//! The classifier maps a [`JoinedStream`]'s observable attributes (label, scheme,
//! per-stream UA/referrer, declared quality, container) onto a set of
//! [`QuirkTag`]s. These tags are what make the soak set *adversarial*: they
//! steer the stratified sampler toward the ingest-resilience edge cases (the
//! pinned ABC News `WebVTT` class, geo-blocked feeds, part-time channels, non-TLS
//! origins, header-gated origins, and resolution extremes) so the
//! `LIVE -> STALE -> RECONNECTING -> NO_SIGNAL` tile state machine is
//! systematically exercised.

use std::collections::BTreeSet;

use serde::Serialize;

use crate::iptv::join::JoinedStream;

/// The delivery container of a stream URL, detected from its path/extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Container {
    /// HLS — an `.m3u8` master/media playlist.
    Hls,
    /// MPEG-DASH — an `.mpd` manifest.
    Dash,
    /// A raw MPEG-TS (`.ts`) elementary segment / endpoint.
    RawTs,
    /// Anything else (e.g. `rtsp://`, `udp://`, unknown extensions).
    Other,
}

impl Container {
    /// Detect the container from a URL, ignoring any query string / fragment.
    #[must_use]
    pub fn from_url(url: &str) -> Self {
        // Strip the query and fragment so `…/playlist.m3u8?token=…` still matches,
        // then compare the final path-segment extension case-insensitively.
        let path = url.split(['?', '#']).next().unwrap_or(url);
        let last_segment = path.rsplit('/').next().unwrap_or(path);
        let ext = last_segment
            .rsplit_once('.')
            .map(|(_, ext)| ext.to_ascii_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "m3u8" => Self::Hls,
            "mpd" => Self::Dash,
            "ts" => Self::RawTs,
            _ => Self::Other,
        }
    }
}

/// A single quirk dimension a soak source can exhibit. The sampler stratifies
/// over these so each ingest-resilience edge case is represented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QuirkTag {
    /// HLS container (`.m3u8`).
    ContainerHls,
    /// DASH container (`.mpd`).
    ContainerDash,
    /// Raw MPEG-TS container (`.ts`).
    ContainerRawTs,
    /// A container we do not recognise (steers an "unknown transport" test).
    ContainerOther,
    /// iptv-org labels the feed `"Not 24/7"` — it goes dark on a schedule.
    NotAroundTheClock,
    /// iptv-org labels the feed geo-restricted — likely a dead/region tile.
    GeoBlocked,
    /// The origin requires a non-default `User-Agent` (header-gated).
    CustomUserAgent,
    /// The origin requires a `Referer` header (header-gated).
    CustomReferrer,
    /// The URL is plain `http://` (no TLS).
    NonTls,
    /// The declared quality is interlaced (`…i`, e.g. `576i`).
    Interlaced,
    /// The declared quality is missing/odd/unparseable.
    OddQuality,
    /// 2160p or above — the high-resolution decode-budget extreme.
    UltraHighRes,
    /// 240p or below — the low-resolution extreme.
    UltraLowRes,
    /// The pinned ABC News `WebVTT`/subtitle resilience class (always included).
    PinnedSubtitleClass,
}

/// Coarse parse of an iptv-org `quality` string into a vertical line count.
/// Returns `None` for missing/odd strings (which become [`QuirkTag::OddQuality`]).
fn vertical_lines(quality: &str) -> Option<u32> {
    let q = quality.trim().to_ascii_lowercase();
    // Accept the common `<N>p` / `<N>i` forms; reject anything else.
    let digits: String = q.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    // The remainder must be exactly the `p`/`i` scan marker for this to be a
    // recognised quality (so "abc" or "4k" fall through to OddQuality).
    let rest = &q[digits.len()..];
    if rest != "p" && rest != "i" {
        return None;
    }
    digits.parse::<u32>().ok()
}

/// Compute the full quirk-tag set for a joined stream.
#[must_use]
pub fn classify_quirks(joined: &JoinedStream) -> BTreeSet<QuirkTag> {
    let mut tags = BTreeSet::new();

    // Container axis.
    match Container::from_url(&joined.url) {
        Container::Hls => tags.insert(QuirkTag::ContainerHls),
        Container::Dash => tags.insert(QuirkTag::ContainerDash),
        Container::RawTs => tags.insert(QuirkTag::ContainerRawTs),
        Container::Other => tags.insert(QuirkTag::ContainerOther),
    };

    // Resilience labels (case-insensitive substring match, as iptv-org labels
    // are free text such as "Geo-blocked (GB)" or "Not 24/7").
    if let Some(label) = &joined.label {
        let l = label.to_ascii_lowercase();
        if l.contains("not 24/7") || l.contains("not 24-7") {
            tags.insert(QuirkTag::NotAroundTheClock);
        }
        if l.contains("geo-block") || l.contains("geo block") || l.contains("geoblock") {
            tags.insert(QuirkTag::GeoBlocked);
        }
    }

    // Header-gated origins.
    if joined.user_agent.is_some() {
        tags.insert(QuirkTag::CustomUserAgent);
    }
    if joined.referrer.is_some() {
        tags.insert(QuirkTag::CustomReferrer);
    }

    // Non-TLS scheme.
    if joined.url.starts_with("http://") {
        tags.insert(QuirkTag::NonTls);
    }

    // Quality axis.
    match joined.quality.as_deref() {
        None => {
            tags.insert(QuirkTag::OddQuality);
        }
        Some(q) => {
            if q.trim().to_ascii_lowercase().ends_with('i') {
                tags.insert(QuirkTag::Interlaced);
            }
            match vertical_lines(q) {
                None => {
                    tags.insert(QuirkTag::OddQuality);
                }
                Some(lines) => {
                    if lines >= 2160 {
                        tags.insert(QuirkTag::UltraHighRes);
                    } else if lines <= 240 {
                        tags.insert(QuirkTag::UltraLowRes);
                    }
                }
            }
        }
    }

    tags
}
