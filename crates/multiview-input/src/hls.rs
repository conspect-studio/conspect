//! HLS playlist parsers: the **master playlist** (subtitle/audio renditions) and
//! the **media playlist** (`EXT-X-PROGRAM-DATE-TIME` → per-source wall-clock).
//!
//! libav opens an HLS URL as a single program (the chosen variant); it does
//! **not** surface the master playlist's separate `SUBTITLES` rendition as a
//! decodable stream. To decode native HLS captions Multiview must read the MASTER
//! itself, find the subtitle rendition's media-playlist URI, and demux that as a
//! second isolated source (its cues are then sampled at the output tick like any
//! other input — never pacing the engine, invariants #1/#10).
//!
//! ## Wall-clock (ADR-0038, SYNC-0 + the HLS extraction of SYNC-1)
//!
//! A live HLS **media playlist** may carry `#EXT-X-PROGRAM-DATE-TIME` (RFC 8216
//! §4.3.2.6): an ISO-8601 / RFC-3339 instant that binds the **first sample of the
//! segment it precedes** to an absolute wall-clock. [`MediaPlaylist`] scans those
//! anchors and turns the chosen one into a [`WallClockRef`](multiview_core::wallclock::WallClockRef)
//! (the media-PTS→wall-clock affine map), and [`MediaPlaylist::classify_trust`]
//! mirrors the engine's lock-state tolerance pattern (present + monotonic +
//! plausible ⇒ Trusted; jumpy / implausible ⇒ Suspected; absent ⇒ None). The PDT
//! instant is parsed to **integer nanoseconds** past the Unix epoch with exact
//! integer civil-date arithmetic — never a float (invariant #3).
//!
//! These are the pure, dependency-free parsers (no I/O, no `url`/`chrono` crate):
//! bounded, panic-free, line-oriented scanners over the subset of RFC 8216 tags
//! Multiview needs. The HTTP fetch reuses the existing ingest.

use multiview_core::stream::{
    Bcp47, StableStreamId, StreamDescriptor, StreamDetail, StreamInventory, StreamKind,
};
use multiview_core::time::Rational;
use multiview_core::wallclock::{WallClockOrigin, WallClockRef, WallClockTier, WallClockTrust};
use thiserror::Error;

/// Upper bound on lines scanned, so a pathological input cannot make the parser
/// loop unboundedly. A real master playlist is a few dozen lines.
const MAX_LINES: usize = 100_000;

/// Errors raised while parsing an HLS master or media playlist.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum HlsError {
    /// The text did not begin with the mandatory `#EXTM3U` tag, so it is not an
    /// HLS playlist.
    #[error("not an HLS playlist (missing #EXTM3U)")]
    NotAPlaylist,

    /// The playlist exceeded the bounded line budget ([`MAX_LINES`]).
    #[error("playlist exceeds the {0}-line scan budget")]
    TooLarge(usize),

    /// An `#EXT-X-PROGRAM-DATE-TIME` value could not be parsed as an ISO-8601 /
    /// RFC-3339 instant.
    #[error("invalid EXT-X-PROGRAM-DATE-TIME: {0}")]
    BadProgramDateTime(&'static str),
}

/// The media type of an `#EXT-X-MEDIA` rendition (RFC 8216 §4.3.4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MediaType {
    /// `TYPE=AUDIO`.
    Audio,
    /// `TYPE=VIDEO`.
    Video,
    /// `TYPE=SUBTITLES` — a separate `WebVTT` media playlist.
    Subtitles,
    /// `TYPE=CLOSED-CAPTIONS` — in-band CEA-608/708 (no separate `URI`).
    ClosedCaptions,
    /// A `TYPE` value this parser does not model.
    Other,
}

impl MediaType {
    /// Classify an `#EXT-X-MEDIA` `TYPE` attribute value (case-insensitive).
    fn from_attr(value: &str) -> Self {
        if value.eq_ignore_ascii_case("AUDIO") {
            Self::Audio
        } else if value.eq_ignore_ascii_case("VIDEO") {
            Self::Video
        } else if value.eq_ignore_ascii_case("SUBTITLES") {
            Self::Subtitles
        } else if value.eq_ignore_ascii_case("CLOSED-CAPTIONS") {
            Self::ClosedCaptions
        } else {
            Self::Other
        }
    }
}

/// One `#EXT-X-MEDIA` rendition (an alternative audio/subtitle/CC track).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct MediaRendition {
    /// The rendition's media type.
    pub media_type: MediaType,
    /// `GROUP-ID` — the rendition group a variant references.
    pub group_id: String,
    /// `NAME` — a human-readable label.
    pub name: String,
    /// `LANGUAGE` (RFC 5646 tag), if declared.
    pub language: Option<String>,
    /// `DEFAULT=YES`.
    pub default: bool,
    /// `AUTOSELECT=YES`.
    pub autoselect: bool,
    /// `FORCED=YES`.
    pub forced: bool,
    /// `URI` of the rendition's media playlist (absent for in-band
    /// `CLOSED-CAPTIONS`).
    pub uri: Option<String>,
}

/// One `#EXT-X-STREAM-INF` variant stream and the rendition groups it links.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct VariantStream {
    /// The variant's media-playlist URI (the line following the tag).
    pub uri: String,
    /// `SUBTITLES="group"` — the subtitle rendition group this variant uses.
    pub subtitles_group: Option<String>,
    /// `BANDWIDTH` — the variant's peak bits/second, if declared. Used to pick the
    /// best variant when no display height is known (highest = best quality).
    pub bandwidth: Option<u64>,
    /// The vertical resolution (the `h` of `RESOLUTION=wxh`), if declared. Used to
    /// pick the variant nearest the displayed tile height
    /// (decode-at-display-resolution, invariant #6).
    pub resolution_height: Option<u32>,
}

/// A parsed HLS master playlist: its renditions and variant streams.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct MasterPlaylist {
    /// All `#EXT-X-MEDIA` renditions, in document order.
    pub renditions: Vec<MediaRendition>,
    /// All `#EXT-X-STREAM-INF` variant streams, in document order.
    pub variants: Vec<VariantStream>,
}

impl MasterPlaylist {
    /// Parse a master playlist's text.
    ///
    /// # Errors
    /// - [`HlsError::NotAPlaylist`] if the first non-empty line is not `#EXTM3U`.
    /// - [`HlsError::TooLarge`] if the text exceeds [`MAX_LINES`].
    pub fn parse(text: &str) -> Result<Self, HlsError> {
        let mut lines = text.lines();
        // The first non-empty line must be the #EXTM3U tag.
        let first = lines
            .by_ref()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("");
        if first != "#EXTM3U" {
            return Err(HlsError::NotAPlaylist);
        }

        let mut renditions = Vec::new();
        let mut variants = Vec::new();
        // The attributes parsed from the most recent `#EXT-X-STREAM-INF`, awaiting
        // its URI on the following non-comment line. `None` until a STREAM-INF tag.
        let mut pending_variant: Option<PendingVariant> = None;
        let mut scanned = 1usize;

        for raw in lines {
            scanned = scanned.saturating_add(1);
            if scanned > MAX_LINES {
                return Err(HlsError::TooLarge(MAX_LINES));
            }
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(attrs) = line.strip_prefix("#EXT-X-MEDIA:") {
                renditions.push(parse_media(attrs));
                pending_variant = None;
            } else if let Some(attrs) = line.strip_prefix("#EXT-X-STREAM-INF:") {
                // The URI is on the following non-comment line; stash the variant's
                // group + selection metrics (bandwidth, resolution height).
                pending_variant = Some(PendingVariant {
                    subtitles_group: attr_value(attrs, "SUBTITLES"),
                    bandwidth: attr_value(attrs, "BANDWIDTH").and_then(|v| v.parse().ok()),
                    resolution_height: attr_value(attrs, "RESOLUTION")
                        .and_then(|v| parse_resolution_height(&v)),
                });
            } else if line.starts_with('#') {
                // Any other tag — ignore, but do not consume a pending variant.
            } else if let Some(pending) = pending_variant.take() {
                variants.push(VariantStream {
                    uri: line.to_owned(),
                    subtitles_group: pending.subtitles_group,
                    bandwidth: pending.bandwidth,
                    resolution_height: pending.resolution_height,
                });
            }
            // A bare URI with no pending STREAM-INF is ignored (not a variant).
        }

        Ok(Self {
            renditions,
            variants,
        })
    }

    /// Pick the **video variant** media playlist to pin the main demuxer to,
    /// given the displayed tile height (`target_height`).
    ///
    /// Pinning the main video/audio demuxer to one variant media playlist — rather
    /// than opening the master with its selectable `TYPE=SUBTITLES` group — is what
    /// stops libav (notably `FFmpeg` 8.x, which dropped the `strict` rendition
    /// gate) from ever fetching the `WebVTT` rendition's `.vtt` segments and
    /// aborting the open (ADR-T011). A variant media playlist carries only its own
    /// video/audio segments, no subtitle rendition.
    ///
    /// Selection (decode-at-display-resolution, invariant #6):
    /// * With a `target_height`: the variant with the **smallest** declared height
    ///   that still **meets or exceeds** the target (so a 360-px tile decodes the
    ///   360p rung, never 1080p); if every declared height is below the target,
    ///   the **tallest** available; variants with no declared height are considered
    ///   only when none carry a height.
    /// * With no `target_height`: the **highest-`BANDWIDTH`** variant (best
    ///   quality).
    /// * If no variant carries either metric, the **first** declared variant.
    ///
    /// Returns `None` only when the playlist declares no variants at all (it is a
    /// media playlist, not a master) — the caller then leaves the URL unchanged.
    #[must_use]
    pub fn pick_video_variant(&self, target_height: Option<u32>) -> Option<&VariantStream> {
        if self.variants.is_empty() {
            return None;
        }
        if let Some(target) = target_height {
            // Prefer the smallest height that meets/exceeds the target.
            let at_or_above = self
                .variants
                .iter()
                .filter(|v| v.resolution_height.is_some_and(|h| h >= target))
                .min_by_key(|v| v.resolution_height.unwrap_or(u32::MAX));
            if let Some(v) = at_or_above {
                return Some(v);
            }
            // Every declared height is below the target: take the tallest declared.
            let tallest = self
                .variants
                .iter()
                .filter(|v| v.resolution_height.is_some())
                .max_by_key(|v| v.resolution_height.unwrap_or(0));
            if let Some(v) = tallest {
                return Some(v);
            }
            // No declared heights at all: fall through to the bandwidth/first pick.
        }
        // No target (or no heights declared): the highest-bandwidth variant, else
        // the first declared variant.
        self.variants
            .iter()
            .filter(|v| v.bandwidth.is_some())
            .max_by_key(|v| v.bandwidth.unwrap_or(0))
            .or_else(|| self.variants.first())
    }

    /// Iterate the `TYPE=SUBTITLES` renditions.
    pub fn subtitle_renditions(&self) -> impl Iterator<Item = &MediaRendition> {
        self.renditions
            .iter()
            .filter(|r| r.media_type == MediaType::Subtitles)
    }

    /// Choose a subtitle rendition to decode.
    ///
    /// Preference order: an exact `language` match (case-insensitive) when a
    /// language is requested, then `DEFAULT=YES`, then `AUTOSELECT=YES`, then the
    /// first subtitle rendition. Returns `None` if the master carries no subtitle
    /// rendition with a `URI`.
    #[must_use]
    pub fn pick_subtitle(&self, language: Option<&str>) -> Option<&MediaRendition> {
        let with_uri = || self.subtitle_renditions().filter(|r| r.uri.is_some());
        if let Some(lang) = language {
            if let Some(found) = with_uri().find(|r| {
                r.language
                    .as_deref()
                    .is_some_and(|l| l.eq_ignore_ascii_case(lang))
            }) {
                return Some(found);
            }
        }
        with_uri()
            .find(|r| r.default)
            .or_else(|| with_uri().find(|r| r.autoselect))
            .or_else(|| with_uri().next())
    }

    /// Fold the master playlist's AUDIO + SUBTITLES alternate renditions into a
    /// typed [`StreamInventory`] (RT-2, ADR-0034 §3).
    ///
    /// libav opens an HLS URL as a single program and does **not** surface the
    /// master's separate AUDIO / SUBTITLES renditions as decodable streams, so
    /// these would otherwise be invisible to the router. This fold-in makes each
    /// one an addressable [`StreamDescriptor`]:
    ///
    /// * keyed by a **hard** [`StableStreamId::from_hls`]`(group_id, name)` (RFC
    ///   8216 keys a rendition by group + name; these survive a rendition
    ///   reorder). HLS requires a non-empty `NAME`, but real playlists omit it —
    ///   a **deterministic, non-empty** name is synthesised from `group_id` + the
    ///   rendition's ordinal within its `(media_type, group_id)` so two name-less
    ///   renditions in one group get **distinct, stable** ids rather than
    ///   colliding on the empty name;
    /// * carrying the rendition's validated [`Bcp47`] language, `DEFAULT` flag,
    ///   and (for subtitles) the `FORCED` flag.
    ///
    /// `CLOSED-CAPTIONS` renditions (no separate media playlist) and `VIDEO`
    /// renditions (the variant the libav path already decodes) are not folded in
    /// here. The owning input id is left unset (the ingest binder fills it).
    #[must_use]
    pub fn stream_inventory(&self) -> StreamInventory {
        // Per-(media_type, group_id) ordinal so a synthesised name is stable and
        // unique within its group; iterate in document order so the ordinal is
        // deterministic across a re-parse.
        let mut ordinals: std::collections::HashMap<(MediaType, String), u32> =
            std::collections::HashMap::new();
        let mut streams = Vec::new();
        for rendition in &self.renditions {
            let kind = match rendition.media_type {
                MediaType::Audio => StreamKind::Audio,
                MediaType::Subtitles => StreamKind::Subtitle,
                // VIDEO renditions are the libav-decoded variant; CLOSED-CAPTIONS
                // and any other type are not separate routable renditions here.
                MediaType::Video | MediaType::ClosedCaptions | MediaType::Other => continue,
            };
            let key = (rendition.media_type, rendition.group_id.clone());
            let ordinal = ordinals.entry(key).or_insert(0);
            let this_ordinal = *ordinal;
            *ordinal = ordinal.saturating_add(1);
            streams.push(rendition_descriptor(rendition, kind, this_ordinal));
        }
        StreamInventory::from_streams(streams)
    }
}

/// The shared resolver that turns one AUDIO/SUBTITLES [`MediaRendition`] into a
/// [`StreamDescriptor`] — used for both rendition families so audio and subtitle
/// fold-in stay consistent (RT-2 §3).
///
/// `ordinal` is the rendition's position within its `(media_type, group_id)`,
/// used only to synthesise a non-empty `NAME` when the playlist omits one.
fn rendition_descriptor(
    rendition: &MediaRendition,
    kind: StreamKind,
    ordinal: u32,
) -> StreamDescriptor {
    let name = synthesised_name(&rendition.group_id, &rendition.name, ordinal);
    let id = StableStreamId::from_hls(kind, &rendition.group_id, &name);
    let language = rendition
        .language
        .as_deref()
        .and_then(|l| Bcp47::parse(l).ok());
    let detail = match kind {
        StreamKind::Subtitle => StreamDetail::Subtitle {
            forced: rendition.forced,
        },
        // An HLS master rendition carries no channel layout / sample rate (that
        // needs decoding the media playlist's segments); the audio detail is the
        // zero-valued shape, refined by a later decode probe.
        _ => StreamDetail::Audio {
            channels: 0,
            sample_rate: 0,
        },
    };
    // The rendition's NAME is the natural title; for a synthesised name there is
    // no operator-facing title, so leave it unset.
    let title = if rendition.name.trim().is_empty() {
        None
    } else {
        Some(rendition.name.clone())
    };
    StreamDescriptor::new(id, kind, "hls", detail)
        .with_language(language)
        .with_title(title)
        .with_default(rendition.default)
}

/// Synthesise a **non-empty, deterministic** rendition name.
///
/// RFC 8216 requires `#EXT-X-MEDIA:NAME` to be present and unique within its
/// group, but real playlists omit it. An empty name would make two renditions in
/// one group collide on the same `(group_id, "")` [`StableStreamId`]. When `name`
/// is empty (or whitespace-only) this returns a stable `"{group_id}#{ordinal}"`
/// so the ids are distinct and survive a re-parse (the ordinal is the rendition's
/// deterministic document-order position within its group).
fn synthesised_name(group_id: &str, name: &str, ordinal: u32) -> String {
    if name.trim().is_empty() {
        format!("{group_id}#{ordinal}")
    } else {
        name.to_owned()
    }
}

/// The attributes parsed from an `#EXT-X-STREAM-INF` tag, held until its URI line
/// (the following non-comment line) is read and the [`VariantStream`] is built.
struct PendingVariant {
    /// `SUBTITLES="group"`, if declared.
    subtitles_group: Option<String>,
    /// `BANDWIDTH`, if declared (a valid non-negative integer).
    bandwidth: Option<u64>,
    /// The `h` of `RESOLUTION=wxh`, if declared.
    resolution_height: Option<u32>,
}

/// Parse the vertical resolution (the `h`) from a `RESOLUTION=wxh` attribute value
/// (e.g. `"1280x720"` → `Some(720)`). A malformed value yields `None`.
fn parse_resolution_height(value: &str) -> Option<u32> {
    // RFC 8216 §4.2 decimal-resolution is `<width>x<height>` (lowercase `x`); be
    // lenient and also accept an uppercase `X`.
    let (_, height) = value.split_once(['x', 'X'])?;
    height.trim().parse().ok()
}

/// Build a [`MediaRendition`] from an `#EXT-X-MEDIA` attribute list.
fn parse_media(attrs: &str) -> MediaRendition {
    let media_type = attr_value(attrs, "TYPE")
        .as_deref()
        .map_or(MediaType::Other, MediaType::from_attr);
    MediaRendition {
        media_type,
        group_id: attr_value(attrs, "GROUP-ID").unwrap_or_default(),
        name: attr_value(attrs, "NAME").unwrap_or_default(),
        language: attr_value(attrs, "LANGUAGE"),
        default: attr_flag(attrs, "DEFAULT"),
        autoselect: attr_flag(attrs, "AUTOSELECT"),
        forced: attr_flag(attrs, "FORCED"),
        uri: attr_value(attrs, "URI"),
    }
}

/// Return the (quote-stripped) value of `key` in an attribute list, if present.
fn attr_value(list: &str, key: &str) -> Option<String> {
    split_attrs(list).into_iter().find_map(|attr| {
        let (k, v) = split_kv(attr)?;
        if k.eq_ignore_ascii_case(key) {
            Some(v.to_owned())
        } else {
            None
        }
    })
}

/// Return whether an enumerated attribute equals `YES` (case-insensitive).
fn attr_flag(list: &str, key: &str) -> bool {
    attr_value(list, key).is_some_and(|v| v.eq_ignore_ascii_case("YES"))
}

/// Split a comma-separated attribute list, honouring quoted values that may
/// themselves contain commas (e.g. `CODECS="avc1.4d401f,mp4a.40.2"`).
fn split_attrs(list: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut in_quotes = false;
    let mut start = 0usize;
    for (i, c) in list.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                if let Some(seg) = list.get(start..i) {
                    out.push(seg);
                }
                // ',' is one byte, so `i + 1` is a valid char boundary.
                start = i.saturating_add(1);
            }
            _ => {}
        }
    }
    if let Some(seg) = list.get(start..) {
        out.push(seg);
    }
    out
}

/// Split one `KEY=VALUE` attribute, trimming whitespace and surrounding quotes.
fn split_kv(attr: &str) -> Option<(&str, &str)> {
    let (key, value) = attr.trim().split_once('=')?;
    let value = value.trim();
    let unquoted = value
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .unwrap_or(value);
    Some((key.trim(), unquoted))
}

// ───────────────────────── media playlist + PROGRAM-DATE-TIME ─────────────────
//
// RFC 8216 §4.3.2.6: `#EXT-X-PROGRAM-DATE-TIME:<date-time>` associates the FIRST
// sample of the next Media Segment with an absolute ISO-8601 / RFC-3339 instant.
// Multiview scans those anchors to derive a per-source media-PTS→wall-clock map
// (ADR-0038, SYNC-0 + the HLS extraction of SYNC-1).

/// The default HLS media media-rate used when binding a PDT instant to a media
/// PTS: 90 kHz (the MPEG-TS / fMP4 presentation timebase). A `WallClockRef`'s
/// caller can supply a different rate; this is the conventional HLS default.
const HLS_MEDIA_RATE_HZ: i64 = 90_000;

/// One `#EXT-X-PROGRAM-DATE-TIME` anchor: the absolute instant (integer ns past the
/// Unix epoch) bound to the **first sample of the segment it precedes**, together
/// with that segment's document-order index within the media playlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProgramDateTime {
    /// The wall-clock instant, in integer nanoseconds past the Unix epoch
    /// (1970-01-01T00:00:00Z), parsed exactly from the RFC-3339 value.
    pub wall_ns: i64,
    /// The zero-based document-order index of the segment this PDT precedes (the
    /// PDT binds that segment's first sample).
    pub segment_index: usize,
}

impl ProgramDateTime {
    /// Build the media-PTS→wall-clock affine map binding this anchor's wall-clock
    /// instant to the segment's first-sample media PTS.
    ///
    /// `first_sample_pts` is the media PTS (in `rate` units, e.g. 90 kHz ticks) of
    /// the first sample of the segment this PDT precedes — the per-source PTS the
    /// normalizer will see for that sample. `rate` is the media timebase as
    /// ticks/second (`num/den`). The resulting [`WallClockRef`] maps any later PTS
    /// to its wall-clock instant by exact rational rescale.
    #[must_use]
    pub const fn wallclock_ref(self, first_sample_pts: i64, rate: Rational) -> WallClockRef {
        WallClockRef::new(self.wall_ns, first_sample_pts, rate)
    }
}

/// A parsed HLS **media playlist**, scanned for `EXT-X-PROGRAM-DATE-TIME` anchors
/// and the per-segment durations needed to sanity-check them.
///
/// This is the pure structural model; the HTTP fetch reuses the existing ingest.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct MediaPlaylist {
    /// The `EXT-X-PROGRAM-DATE-TIME` anchors, in document order (each bound to the
    /// segment index it precedes). Empty when the playlist carries no PDT.
    pub pdt_anchors: Vec<ProgramDateTime>,
    /// Per-segment `#EXTINF` durations in nanoseconds, in document order (one per
    /// media segment). Used to validate that a PDT's asserted wall-gap is
    /// plausible vs the media timeline.
    pub segment_durations_ns: Vec<i64>,
}

impl MediaPlaylist {
    /// Parse a media playlist's text.
    ///
    /// # Errors
    /// - [`HlsError::NotAPlaylist`] if the first non-empty line is not `#EXTM3U`.
    /// - [`HlsError::TooLarge`] if the text exceeds [`MAX_LINES`].
    /// - [`HlsError::BadProgramDateTime`] if a PDT value is not a parseable
    ///   ISO-8601 / RFC-3339 instant.
    pub fn parse(text: &str) -> Result<Self, HlsError> {
        let mut lines = text.lines();
        let first = lines
            .by_ref()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("");
        if first != "#EXTM3U" {
            return Err(HlsError::NotAPlaylist);
        }

        let mut pdt_anchors = Vec::new();
        let mut segment_durations_ns = Vec::new();
        // A PDT tag binds the NEXT media segment; stash it until the segment's URI
        // line is seen (the segment that increments the index).
        let mut pending_pdt_ns: Option<i64> = None;
        let mut pending_extinf_ns: Option<i64> = None;
        let mut segment_index: usize = 0;
        let mut scanned = 1usize;

        for raw in lines {
            scanned = scanned.saturating_add(1);
            if scanned > MAX_LINES {
                return Err(HlsError::TooLarge(MAX_LINES));
            }
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(value) = line.strip_prefix("#EXT-X-PROGRAM-DATE-TIME:") {
                let wall_ns = parse_rfc3339_ns(value.trim())
                    .ok_or(HlsError::BadProgramDateTime("not an ISO-8601 instant"))?;
                pending_pdt_ns = Some(wall_ns);
            } else if let Some(value) = line.strip_prefix("#EXTINF:") {
                pending_extinf_ns = Some(parse_extinf_ns(value));
            } else if line.starts_with('#') {
                // Any other tag (TARGETDURATION, VERSION, DISCONTINUITY, …): ignore,
                // but do not consume the pending PDT/EXTINF — they bind the segment.
            } else {
                // A non-comment line is the segment URI: this segment is now at
                // `segment_index`. Bind any pending PDT to it, then advance.
                if let Some(wall_ns) = pending_pdt_ns.take() {
                    pdt_anchors.push(ProgramDateTime {
                        wall_ns,
                        segment_index,
                    });
                }
                segment_durations_ns.push(pending_extinf_ns.take().unwrap_or(0));
                segment_index = segment_index.saturating_add(1);
            }
        }

        Ok(Self {
            pdt_anchors,
            segment_durations_ns,
        })
    }

    /// The earliest `EXT-X-PROGRAM-DATE-TIME` anchor (the one binding the first
    /// dated segment), or `None` when the playlist carries no PDT.
    #[must_use]
    pub fn first_program_date_time(&self) -> Option<ProgramDateTime> {
        self.pdt_anchors.first().copied()
    }

    /// Iterate all `EXT-X-PROGRAM-DATE-TIME` anchors in document order.
    pub fn program_date_times(&self) -> impl Iterator<Item = ProgramDateTime> + '_ {
        self.pdt_anchors.iter().copied()
    }

    /// Build the media-PTS→wall-clock affine map from the first PDT anchor, binding
    /// it to the supplied first-sample media PTS at the default HLS 90 kHz media
    /// rate. Returns `None` when the playlist carries no PDT.
    ///
    /// `first_sample_pts` is the per-source media PTS (90 kHz ticks) of the first
    /// sample of the dated segment, as the normalizer will see it.
    #[must_use]
    pub fn wallclock_ref(&self, first_sample_pts: i64) -> Option<WallClockRef> {
        self.first_program_date_time()
            .map(|pdt| pdt.wallclock_ref(first_sample_pts, Rational::new(HLS_MEDIA_RATE_HZ, 1)))
    }

    /// Classify this source's wall-clock trust from its PDT anchors, mirroring the
    /// engine lock-state classifier's tolerance pattern (ADR-0038 §1):
    ///
    /// * **None** — no PDT present (origin [`WallClockOrigin::None`]);
    /// * **Suspected** — PDT present but non-monotonic (a backwards jump) or
    ///   asserting a wall-gap that drifts from the media timeline beyond tolerance
    ///   (the wall-clock assertion is implausible/jittery — like `Holdover`);
    /// * **Trusted** — PDT present, monotonic, and plausible vs the segment
    ///   durations (like `Locked`).
    ///
    /// PDT is a wall-clock **assertion** (an NTP-disciplined origin), so a single
    /// plausible monotonic anchor is trusted-by-assertion (Loose, ms-class). The
    /// operator's Use/Discard verb is layered on separately (config); here the
    /// authored `choice` defaults to `Use`.
    #[must_use]
    pub fn classify_trust(&self, config: &PdtTrustConfig) -> WallClockTrust {
        let mut trust = WallClockTrust::none();
        let Some(_first) = self.pdt_anchors.first() else {
            // No PDT anchor: honest None, reclock-to-house.
            return trust;
        };
        trust.origin = WallClockOrigin::ProgramDateTime;
        trust.tier = if self.pdt_is_plausible(*config) {
            WallClockTier::Trusted
        } else {
            // Present but jumpy / implausible — coast at Suspected (reclock-to-house),
            // mirroring the engine's Holdover.
            WallClockTier::Suspected
        };
        trust
    }

    /// Whether the PDT anchors are monotonically non-decreasing AND each
    /// consecutive pair's asserted wall-gap matches the media timeline (the sum of
    /// the intervening segment durations) within `config.drift_tolerance_ns`.
    fn pdt_is_plausible(&self, config: PdtTrustConfig) -> bool {
        // Examine each consecutive pair of anchors.
        let mut prev: Option<ProgramDateTime> = None;
        for anchor in self.pdt_anchors.iter().copied() {
            if let Some(p) = prev {
                // (1) Monotonic: a backwards PDT is an implausible wall-clock.
                if anchor.wall_ns < p.wall_ns {
                    return false;
                }
                // (2) Drift: the asserted wall-gap must match the media-time gap
                // (sum of segment durations between the two anchors) within
                // tolerance. A media gap of 0 (missing EXTINF) skips the check.
                let asserted = anchor.wall_ns.saturating_sub(p.wall_ns);
                let media_gap = self.media_gap_ns(p.segment_index, anchor.segment_index);
                if media_gap > 0 {
                    let drift = asserted.saturating_sub(media_gap).saturating_abs();
                    if drift > config.drift_tolerance_ns {
                        return false;
                    }
                }
            }
            prev = Some(anchor);
        }
        true
    }

    /// Sum of `#EXTINF` segment durations (ns) for segments in `[from, to)` — the
    /// media-time gap between two PDT anchors at those segment indices.
    fn media_gap_ns(&self, from: usize, to: usize) -> i64 {
        let mut sum = 0i64;
        for idx in from..to {
            if let Some(d) = self.segment_durations_ns.get(idx) {
                sum = sum.saturating_add(*d);
            }
        }
        sum
    }
}

/// Tuning for [`MediaPlaylist::classify_trust`] (ADR-0038 §1).
///
/// Mirrors the engine lock-state classifier's tolerance approach: a PDT whose
/// asserted wall-gap drifts from the media timeline by more than
/// `drift_tolerance_ns` is treated as out-of-tolerance and degrades the tier to
/// Suspected (analogous to the servo's `lock_tolerance_ns`). All thresholds are
/// integer nanoseconds (never float seconds — invariant #3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct PdtTrustConfig {
    /// Maximum `|asserted_wall_gap − media_time_gap|` (ns) between two consecutive
    /// PDT anchors for the wall-clock to count as plausible/in-tolerance.
    pub drift_tolerance_ns: i64,
}

impl PdtTrustConfig {
    /// A sane Loose-tier default: a 2 s drift tolerance between a PDT's asserted
    /// wall-gap and the media-timeline gap. HLS PDT is NTP-disciplined and
    /// ms-class; 2 s comfortably absorbs producer rounding / a single missing
    /// segment while still rejecting a gross drift (e.g. a 600 s jump over 6 s of
    /// segments).
    #[must_use]
    pub const fn new_default() -> Self {
        Self {
            drift_tolerance_ns: 2_000_000_000,
        }
    }
}

impl Default for PdtTrustConfig {
    fn default() -> Self {
        Self::new_default()
    }
}

/// Parse an `#EXTINF` value (`<duration>[,<title>]`, seconds with optional
/// fraction) into nanoseconds, exactly via integer math. Returns `0` on a value
/// that does not parse (the duration is advisory for the drift check; a bad value
/// simply skips that check rather than failing the parse).
fn parse_extinf_ns(value: &str) -> i64 {
    let dur = value.split(',').next().unwrap_or("").trim();
    parse_decimal_seconds_ns(dur).unwrap_or(0)
}

/// Parse a decimal seconds string (e.g. `6` or `6.000` or `2.5`) into integer
/// nanoseconds. Returns `None` on a malformed value or one with more than 9
/// fractional digits of significance beyond ns. Pure integer arithmetic.
fn parse_decimal_seconds_ns(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (int_part, frac_part) = match s.split_once('.') {
        Some((i, f)) => (i, f),
        None => (s, ""),
    };
    let secs: i64 = int_part.parse().ok()?;
    // Take up to 9 fractional digits, right-padding to nanoseconds.
    let mut nanos: i64 = 0;
    let mut scale: i64 = 100_000_000; // 1e8 (weight of the first fractional digit)
    for ch in frac_part.chars().take(9) {
        let digit = ch.to_digit(10)?;
        nanos = nanos.saturating_add(i64::from(digit).saturating_mul(scale));
        scale /= 10;
    }
    secs.checked_mul(1_000_000_000)
        .and_then(|s_ns| s_ns.checked_add(nanos))
}

/// Parse an ISO-8601 / RFC-3339 date-time (as carried by
/// `EXT-X-PROGRAM-DATE-TIME`) into **integer nanoseconds past the Unix epoch**.
///
/// Accepts `YYYY-MM-DDTHH:MM:SS[.fff…][Z|±HH:MM|±HHMM]`. The `T` separator may be a
/// space (lenient, as some producers emit). All arithmetic is exact integer math
/// (no float, no `chrono`) using the days-from-civil algorithm (Howard Hinnant);
/// the result is ns past 1970-01-01T00:00:00Z. Returns `None` on any malformed
/// field or out-of-range value.
fn parse_rfc3339_ns(s: &str) -> Option<i64> {
    let s = s.trim();
    // Split date and time on 'T' (or a space).
    let (date, rest) = s
        .split_once('T')
        .or_else(|| s.split_once(' '))
        .or_else(|| s.split_once('t'))?;

    // Date: YYYY-MM-DD.
    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: u32 = date_parts.next()?.parse().ok()?;
    let day: u32 = date_parts.next()?.parse().ok()?;
    if date_parts.next().is_some() {
        return None;
    }

    // Separate the timezone suffix from the clock time.
    let (clock, tz_offset_sec) = split_timezone(rest)?;

    // Clock: HH:MM:SS[.fff…].
    let mut clock_parts = clock.split(':');
    let hour: i64 = clock_parts.next()?.parse().ok()?;
    let minute: i64 = clock_parts.next()?.parse().ok()?;
    let sec_field = clock_parts.next()?;
    if clock_parts.next().is_some() {
        return None;
    }
    let (whole_sec, frac_ns) = match sec_field.split_once('.') {
        Some((w, f)) => {
            let secs: i64 = w.parse().ok()?;
            let frac = fractional_to_nanos(f)?;
            (secs, frac)
        }
        None => (sec_field.parse().ok()?, 0),
    };

    // Range guards (calendar validity is enforced by days_from_civil for month/day
    // shape; reject obviously-out-of-range clock fields).
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) || !(0..=60).contains(&whole_sec) {
        // Allow a leap second (60) defensively; it folds into the next minute.
        return None;
    }

    let days = days_from_civil(year, month, day)?;
    let day_seconds = days.checked_mul(86_400)?;
    let tod_seconds = hour
        .checked_mul(3600)?
        .checked_add(minute.checked_mul(60)?)?
        .checked_add(whole_sec)?;
    // Subtract the timezone offset to reach UTC: a +HH:MM zone is AHEAD of UTC.
    let utc_seconds = day_seconds
        .checked_add(tod_seconds)?
        .checked_sub(tz_offset_sec)?;
    utc_seconds.checked_mul(1_000_000_000)?.checked_add(frac_ns)
}

/// Split the time-of-day from its timezone suffix, returning `(clock, offset_secs)`
/// where `offset_secs` is the zone's offset east of UTC in seconds (`Z` → 0).
fn split_timezone(rest: &str) -> Option<(&str, i64)> {
    if let Some(clock) = rest.strip_suffix('Z').or_else(|| rest.strip_suffix('z')) {
        return Some((clock, 0));
    }
    // Find a trailing +/- offset. Scan from the right for the sign that introduces
    // the offset (the time field itself contains no +/-).
    if let Some(pos) = rest.rfind(['+', '-']) {
        let (clock, off) = rest.split_at(pos);
        let offset = parse_tz_offset(off)?;
        return Some((clock, offset));
    }
    // No suffix: RFC 8216 PDT is required to carry one, but accept a bare local
    // time as UTC defensively.
    Some((rest, 0))
}

/// Parse a `±HH:MM` or `±HHMM` or `±HH` timezone offset into seconds east of UTC.
fn parse_tz_offset(off: &str) -> Option<i64> {
    let (sign, digits) = match off.strip_prefix('+') {
        Some(d) => (1i64, d),
        None => (-1i64, off.strip_prefix('-')?),
    };
    let digits = digits.trim();
    let (hh, mm): (i64, i64) = if let Some((h, m)) = digits.split_once(':') {
        (h.parse().ok()?, m.parse().ok()?)
    } else if digits.len() == 4 {
        let (h, m) = digits.split_at(2);
        (h.parse().ok()?, m.parse().ok()?)
    } else {
        (digits.parse().ok()?, 0)
    };
    if !(0..=14).contains(&hh) || !(0..=59).contains(&mm) {
        return None;
    }
    Some(
        sign.saturating_mul(
            hh.saturating_mul(3600)
                .saturating_add(mm.saturating_mul(60)),
        ),
    )
}

/// Convert a fractional-second digit string into nanoseconds (e.g. `"25"` → 250 ms
/// = `250_000_000` ns). Up to 9 digits are significant; extras are truncated. Returns
/// `None` on a non-digit character.
fn fractional_to_nanos(frac: &str) -> Option<i64> {
    let mut nanos: i64 = 0;
    let mut scale: i64 = 100_000_000; // weight of the first fractional digit
    for ch in frac.chars().take(9) {
        let digit = ch.to_digit(10)?;
        nanos = nanos.saturating_add(i64::from(digit).saturating_mul(scale));
        scale /= 10;
    }
    // Any remaining characters beyond 9 must still be digits to be well-formed.
    if frac.chars().skip(9).any(|c| !c.is_ascii_digit()) {
        return None;
    }
    Some(nanos)
}

/// Days from the Unix epoch (1970-01-01) to the given proleptic-Gregorian civil
/// date, by Howard Hinnant's `days_from_civil` algorithm (exact integer math).
/// Returns `None` only on arithmetic overflow (not reachable for real dates).
fn days_from_civil(year: i64, month: u32, day: u32) -> Option<i64> {
    let m = i64::from(month);
    let d = i64::from(day);
    // Shift the year so March is the first month (so the leap day is last).
    let y = if m <= 2 { year.checked_sub(1)? } else { year };
    let era = if y >= 0 { y } else { y.checked_sub(399)? } / 400;
    let yoe = y.checked_sub(era.checked_mul(400)?)?; // [0, 399]
    let doy = {
        // (153*(m + (m>2 ? -3 : 9)) + 2)/5 + d - 1
        let mp = if m > 2 { m - 3 } else { m + 9 };
        (153i64.checked_mul(mp)?.checked_add(2)? / 5)
            .checked_add(d)?
            .checked_sub(1)?
    }; // [0, 365]
    let doe = yoe
        .checked_mul(365)?
        .checked_add(yoe / 4)?
        .checked_sub(yoe / 100)?
        .checked_add(doy)?; // [0, 146096]
    era.checked_mul(146_097)?
        .checked_add(doe)?
        .checked_sub(719_468) // shift epoch from 0000-03-01 to 1970-01-01
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A representative Apple-style master playlist with audio + a subtitles
    /// group (the structure verified live against the bipbop master).
    const MASTER: &str = "#EXTM3U\n\
        #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=YES,AUTOSELECT=YES,URI=\"audio/eng/prog_index.m3u8\"\n\
        #EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=YES,AUTOSELECT=YES,FORCED=NO,URI=\"subtitles/eng/prog_index.m3u8\"\n\
        #EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"Espanol\",LANGUAGE=\"es\",DEFAULT=NO,AUTOSELECT=YES,FORCED=NO,URI=\"subtitles/spa/prog_index.m3u8\"\n\
        #EXT-X-STREAM-INF:BANDWIDTH=2000000,CODECS=\"avc1.4d401f,mp4a.40.2\",AUDIO=\"aud\",SUBTITLES=\"subs\"\n\
        video/variant_2000.m3u8\n";

    #[test]
    fn parses_subtitle_renditions_and_resolves_english_uri() {
        let master = MasterPlaylist::parse(MASTER).expect("parse");
        assert_eq!(master.subtitle_renditions().count(), 2);
        let en = master.pick_subtitle(Some("en")).expect("english subs");
        assert_eq!(en.uri.as_deref(), Some("subtitles/eng/prog_index.m3u8"));
        assert_eq!(en.language.as_deref(), Some("en"));
        assert_eq!(en.media_type, MediaType::Subtitles);
        let es = master.pick_subtitle(Some("es")).expect("spanish subs");
        assert_eq!(es.uri.as_deref(), Some("subtitles/spa/prog_index.m3u8"));
    }

    #[test]
    fn quoted_codecs_comma_does_not_split_the_attribute_list() {
        let master = MasterPlaylist::parse(MASTER).expect("parse");
        let variant = master.variants.first().expect("one variant");
        assert_eq!(variant.uri, "video/variant_2000.m3u8");
        assert_eq!(variant.subtitles_group.as_deref(), Some("subs"));
    }

    /// A multi-rendition master with several `#EXT-X-STREAM-INF` variants at
    /// different resolutions + bandwidths and a SUBTITLES group — the shape of the
    /// ABC-News-AU footgun master that aborts the libav 8.x HLS open. The fix pins
    /// the MAIN demuxer to one VIDEO variant media playlist (which carries no
    /// SUBTITLES rendition), so libav never fetches the broken `.vtt`.
    const ABR_MASTER: &str = "#EXTM3U\n\
        #EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=YES,AUTOSELECT=YES,URI=\"index_7_0.m3u8\"\n\
        #EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,CODECS=\"avc1.4d401e\",SUBTITLES=\"subs\"\n\
        index_0.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1280x720,CODECS=\"avc1.4d401f\",SUBTITLES=\"subs\"\n\
        index_2.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080,CODECS=\"avc1.640028\",SUBTITLES=\"subs\"\n\
        index_4.m3u8\n";

    #[test]
    fn variant_parsing_captures_bandwidth_and_resolution_height() {
        let master = MasterPlaylist::parse(ABR_MASTER).expect("parse");
        assert_eq!(master.variants.len(), 3);
        let v0 = &master.variants[0];
        assert_eq!(v0.uri, "index_0.m3u8");
        assert_eq!(v0.bandwidth, Some(800_000));
        assert_eq!(v0.resolution_height, Some(360));
        let v2 = &master.variants[2];
        assert_eq!(v2.bandwidth, Some(5_000_000));
        assert_eq!(v2.resolution_height, Some(1080));
    }

    #[test]
    fn pick_video_variant_targets_the_closest_height_at_or_above_the_tile() {
        let master = MasterPlaylist::parse(ABR_MASTER).expect("parse");
        // A 360-px tile: the 360p variant is the smallest that meets/exceeds it
        // (decode-at-display-resolution, invariant #6 — never decode 1080p for a
        // 360p tile).
        let pick = master.pick_video_variant(Some(360)).expect("a variant");
        assert_eq!(pick.uri, "index_0.m3u8");
        // A 700-px tile: 360p is too small, so the 720p variant is the smallest
        // that meets/exceeds the target.
        let pick = master.pick_video_variant(Some(700)).expect("a variant");
        assert_eq!(pick.uri, "index_2.m3u8");
    }

    #[test]
    fn pick_video_variant_above_the_top_rung_takes_the_highest() {
        let master = MasterPlaylist::parse(ABR_MASTER).expect("parse");
        // A tile taller than every rung: take the tallest available (1080p), never
        // nothing.
        let pick = master.pick_video_variant(Some(4000)).expect("a variant");
        assert_eq!(pick.uri, "index_4.m3u8");
    }

    #[test]
    fn pick_video_variant_with_no_target_takes_the_highest_bandwidth() {
        let master = MasterPlaylist::parse(ABR_MASTER).expect("parse");
        // No tile size known: prefer the highest-bandwidth (best-quality) variant.
        let pick = master.pick_video_variant(None).expect("a variant");
        assert_eq!(pick.uri, "index_4.m3u8");
    }

    #[test]
    fn pick_video_variant_falls_back_to_first_when_no_metrics() {
        // A master whose variants carry neither RESOLUTION nor BANDWIDTH: pick the
        // first declared variant rather than nothing.
        let text = "#EXTM3U\n\
            #EXT-X-STREAM-INF:CODECS=\"avc1.4d401e\"\n\
            a.m3u8\n\
            #EXT-X-STREAM-INF:CODECS=\"avc1.4d401f\"\n\
            b.m3u8\n";
        let master = MasterPlaylist::parse(text).expect("parse");
        let pick = master.pick_video_variant(Some(720)).expect("a variant");
        assert_eq!(pick.uri, "a.m3u8");
        let pick = master.pick_video_variant(None).expect("a variant");
        assert_eq!(pick.uri, "a.m3u8");
    }

    #[test]
    fn pick_video_variant_none_for_a_media_playlist_with_no_variants() {
        // A media playlist (no #EXT-X-STREAM-INF) parses to zero variants, so the
        // pin has nothing to select — the caller leaves the URL unchanged.
        let text = "#EXTM3U\n\
            #EXT-X-TARGETDURATION:6\n\
            #EXTINF:6.0,\n\
            seg0.ts\n";
        let master = MasterPlaylist::parse(text).expect("parse");
        assert!(master.variants.is_empty());
        assert!(master.pick_video_variant(Some(360)).is_none());
        assert!(master.pick_video_variant(None).is_none());
    }

    #[test]
    fn pick_falls_back_to_default_then_first_without_language() {
        let master = MasterPlaylist::parse(MASTER).expect("parse");
        // No language requested -> DEFAULT=YES (the English track) wins.
        let pick = master.pick_subtitle(None).expect("a default");
        assert_eq!(pick.language.as_deref(), Some("en"));
    }

    #[test]
    fn master_without_subtitles_yields_none() {
        let text = "#EXTM3U\n\
            #EXT-X-STREAM-INF:BANDWIDTH=800000\n\
            low.m3u8\n";
        let master = MasterPlaylist::parse(text).expect("parse");
        assert_eq!(master.subtitle_renditions().count(), 0);
        assert!(master.pick_subtitle(Some("en")).is_none());
        assert_eq!(master.variants.len(), 1);
        assert!(master.variants[0].subtitles_group.is_none());
    }

    #[test]
    fn non_playlist_text_is_rejected() {
        assert_eq!(
            MasterPlaylist::parse("not a playlist\n"),
            Err(HlsError::NotAPlaylist)
        );
        assert_eq!(MasterPlaylist::parse(""), Err(HlsError::NotAPlaylist));
    }

    #[test]
    fn a_closed_captions_rendition_has_no_uri() {
        let text = "#EXTM3U\n\
            #EXT-X-MEDIA:TYPE=CLOSED-CAPTIONS,GROUP-ID=\"cc\",NAME=\"CC1\",INSTREAM-ID=\"CC1\"\n";
        let master = MasterPlaylist::parse(text).expect("parse");
        let cc = master.renditions.first().expect("one rendition");
        assert_eq!(cc.media_type, MediaType::ClosedCaptions);
        assert!(cc.uri.is_none());
        // CLOSED-CAPTIONS is not a separate subtitle media playlist.
        assert_eq!(master.subtitle_renditions().count(), 0);
    }
}
