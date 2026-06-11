//! Per-stream **identity** types for the decoupled-routing matrix (ADR-0034 /
//! [decoupled-routing brief §3](../../docs/research/decoupled-routing.md)).
//!
//! An input is not a single stream: it is a bundle of elementary streams (1+
//! video, multiple audio tracks each with a BCP-47 language + channel layout,
//! subtitle/caption tracks, SCTE-35/KLV data, timecode). To make **every**
//! elementary stream independently addressable (the broadcast router /
//! multiviewer crosspoint model), each one needs:
//!
//! * a **canonical media kind** — [`StreamKind`], the richer superset of the
//!   libav `AVMediaType` mapping (`multiview-ffmpeg`'s `MediaKind`) and of the
//!   coarse encode-fan-out tag (`multiview-ffmpeg`'s `StreamKind`). It adds the
//!   [`StreamKind::Data`] (`SCTE-35` / `KLV`) and [`StreamKind::Timecode`]
//!   routing kinds those two lack;
//! * a validated language tag — [`Bcp47`], which normalises and validates the
//!   raw container `language` string;
//! * a **stable, kind-scoped identifier** — [`StableStreamId`], which survives a
//!   re-probe / PMT-version bump / rendition reorder (the container `index` is a
//!   one-time, volatile snapshot), and carries a [`StabilityTier`] so a soft
//!   (heuristic) key is an operator-visible reorder risk rather than a silent
//!   mis-route.
//!
//! This module is **pure types + classification + parsing**: no engine, no I/O,
//! no FFI. The libav lift (`From<MediaKind>` / `stream_kind_from_media_and_codec`)
//! lives in `multiview-ffmpeg`, which depends on this crate; it delegates the
//! actual classification to [`StreamKind::from_coarse_and_codec`] here so the
//! logic is unit-testable without the native layer.

use core::fmt;
use core::str::FromStr;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};

use crate::time::Rational;

/// The kind of **data** carried by a non-AV elementary stream.
///
/// These are passthrough kinds — discovered and routed, never decoded into
/// essence. The variants are unit, so they serialise as the `snake_case` variant
/// name (`"scte35"` / `"klv"`) and are carried in [`StreamKind::Data`]'s
/// adjacently-tagged `payload`. `#[non_exhaustive]` so further data kinds can
/// be added without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DataKind {
    /// SCTE-35 splice information (ad-insertion cue messages). Discovered as a
    /// TS PMT PID and/or a libav `scte_35` codec; rides the video seam.
    Scte35,
    /// SMPTE ST 0601 KLV metadata (e.g. MISB UAS), carried as a data stream.
    Klv,
}

/// Where a timecode elementary stream's value originates.
///
/// A **core-level mirror** of `multiview-overlay`'s `TcSource` families, defined
/// here so `multiview-core` stays dependency-free (core depends on nothing in
/// the workspace). `multiview-overlay` can map to/from this later.
///
/// Serialised **tagged** (the variant name); never `untagged`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TcSourceKind {
    /// Linear timecode (LTC), conventionally on a separate audio track.
    Ltc,
    /// Vertical-interval timecode (VITC) carried in the picture.
    Vitc,
    /// Ancillary timecode per SMPTE RP 188 / ST 12-2 (ATC), incl. ST 2110-40.
    AtcRp188,
    /// Generated from the output clock (no embedded source timecode).
    Generated,
}

/// The **canonical** media kind of an elementary stream in the routing matrix.
///
/// This SUPERSETS the two narrower kinds already in the codebase, **without
/// colliding with or breaking either** (ADR-0034 §1):
///
/// * `multiview-ffmpeg`'s `MediaKind` (`Video | Audio | Subtitle | Other`) — the
///   libav `AVMediaType` mapping. [`StreamKind::Video`] / [`Audio`](Self::Audio)
///   / [`Subtitle`](Self::Subtitle) line up 1:1; `Other` is refined here into
///   [`Data`](Self::Data) / [`Timecode`](Self::Timecode) (or kept as a generic
///   `Data`) using the codec name, which `MediaKind` alone cannot express.
/// * `multiview-output`'s coarse `StreamKind` (`Video | Audio`) — the
///   encode-once-fan-out packet tag (AUD-4). That stays a local, coarse muxer
///   tag; this is the richer canonical inventory kind.
///
/// Serialised **adjacently-tagged** (`#[serde(tag = "kind", content =
/// "payload")]`) per repo conventions; **never `untagged`** (ADR-0010).
/// Adjacent tagging is used (rather than internal) because the
/// [`Data`](Self::Data) / [`Timecode`](Self::Timecode) variants wrap a payload
/// enum: internal tagging on a newtype variant would flatten the inner enum's
/// tag and make `Data(Scte35)` indistinguishable from a `scte35` variant.
/// `#[non_exhaustive]` so future elementary-stream kinds can be added without a
/// breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
#[non_exhaustive]
pub enum StreamKind {
    /// A video elementary stream.
    Video,
    /// An audio elementary stream.
    Audio,
    /// A subtitle / caption elementary stream.
    Subtitle,
    /// A data elementary stream (SCTE-35, KLV) — passthrough, never decoded.
    Data(DataKind),
    /// A timecode elementary stream — carried, not composited.
    Timecode(TcSourceKind),
}

/// The coarse media classification a container demux already exposes — exactly
/// the libav `AVMediaType` mapping (`multiview-ffmpeg`'s `MediaKind`).
///
/// `multiview-core` cannot name `multiview-ffmpeg`'s `MediaKind` (that would be a
/// dependency cycle), so this small mirror lets the codec-driven refinement of
/// `Other` into [`StreamKind::Data`] / [`StreamKind::Timecode`] live here as
/// pure, testable logic. `multiview-ffmpeg` bridges its `MediaKind` to this via
/// `From` and delegates classification to [`StreamKind::from_coarse_and_codec`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CoarseMediaKind {
    /// A video stream.
    Video,
    /// An audio stream.
    Audio,
    /// A subtitle stream.
    Subtitle,
    /// Any other media type (data, attachment, unknown) — refined by codec name.
    Other,
}

impl From<CoarseMediaKind> for StreamKind {
    /// Lift the coarse libav-style kind, leaving `Other` as a generic data
    /// passthrough — callers that have a codec name should prefer
    /// [`StreamKind::from_coarse_and_codec`] to refine `Other`.
    fn from(value: CoarseMediaKind) -> Self {
        match value {
            CoarseMediaKind::Video => Self::Video,
            CoarseMediaKind::Audio => Self::Audio,
            CoarseMediaKind::Subtitle => Self::Subtitle,
            // No codec name available: a bare `Other` is most safely modelled as
            // a generic data passthrough (the conservative routing kind) so it
            // stays addressable rather than being dropped.
            CoarseMediaKind::Other => Self::Data(DataKind::Klv),
        }
    }
}

impl StreamKind {
    /// Classify an elementary stream from its **already-available** coarse kind
    /// and codec name — pure metadata classification, **no decode**.
    ///
    /// `Video` / `Audio` / `Subtitle` lift 1:1. For [`CoarseMediaKind::Other`]
    /// the codec name disambiguates the routing kind:
    ///
    /// * `scte_35` / `scte35` → [`StreamKind::Data`]\([`DataKind::Scte35`]\);
    /// * `klv` / `smpte_klv` → [`StreamKind::Data`]\([`DataKind::Klv`]\);
    /// * `timed_id3` / any `timecode`-bearing name → [`StreamKind::Timecode`];
    /// * anything else → a generic [`StreamKind::Data`]\([`DataKind::Klv`]\)
    ///   passthrough (the conservative default; never silently dropped).
    ///
    /// The codec name is matched case-insensitively against canonical libav
    /// descriptor names; the lift in `multiview-ffmpeg` feeds the name straight
    /// from `codec_id_name`.
    #[must_use]
    pub fn from_coarse_and_codec(coarse: CoarseMediaKind, codec_name: &str) -> Self {
        match coarse {
            CoarseMediaKind::Video => Self::Video,
            CoarseMediaKind::Audio => Self::Audio,
            CoarseMediaKind::Subtitle => Self::Subtitle,
            CoarseMediaKind::Other => Self::classify_other(codec_name),
        }
    }

    /// Refine a `kind=Other` stream into its routing kind by codec name.
    fn classify_other(codec_name: &str) -> Self {
        // Normalise separators/case so `scte-35`/`scte_35`/`SCTE35` all match.
        let compact: String = codec_name
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .map(|c| c.to_ascii_lowercase())
            .collect();
        if compact.contains("scte35") {
            Self::Data(DataKind::Scte35)
        } else if compact.contains("klv") {
            // Covers `klv` and `smpte_klv`.
            Self::Data(DataKind::Klv)
        } else if compact.contains("timedid3") || compact.contains("timecode") {
            // Timed ID3 (HLS metadata) and any explicit timecode track. Without
            // a finer libav signal the source family is unknown → ATC RP-188 is
            // the broadcast default for an embedded timecode stream.
            Self::Timecode(TcSourceKind::AtcRp188)
        } else {
            // Unknown data essence: keep it routable as a generic passthrough
            // rather than dropping it (the as-built `best_stream` returns `None`
            // for `Other`, discarding it — RT-0 makes it addressable).
            Self::Data(DataKind::Klv)
        }
    }

    /// Whether this is a [`StreamKind::Video`] stream.
    #[must_use]
    pub const fn is_video(self) -> bool {
        matches!(self, Self::Video)
    }

    /// Whether this is an [`StreamKind::Audio`] stream.
    #[must_use]
    pub const fn is_audio(self) -> bool {
        matches!(self, Self::Audio)
    }

    /// Whether this is a [`StreamKind::Subtitle`] stream.
    #[must_use]
    pub const fn is_subtitle(self) -> bool {
        matches!(self, Self::Subtitle)
    }

    /// Whether this is a [`StreamKind::Data`] stream (SCTE-35 / KLV).
    #[must_use]
    pub const fn is_data(self) -> bool {
        matches!(self, Self::Data(_))
    }

    /// Whether this is a [`StreamKind::Timecode`] stream.
    #[must_use]
    pub const fn is_timecode(self) -> bool {
        matches!(self, Self::Timecode(_))
    }

    /// A short, stable discriminant character used to scope a [`StableStreamId`]
    /// to its kind (so a PID and a hash never alias across kinds).
    const fn scope_char(self) -> char {
        match self {
            Self::Video => 'v',
            Self::Audio => 'a',
            Self::Subtitle => 's',
            Self::Data(_) => 'd',
            Self::Timecode(_) => 't',
        }
    }
}

/// A validated, normalised **BCP-47 / ISO-639** language tag.
///
/// Wraps the raw container `language` string (which libav stores verbatim and
/// does **not** normalise). Construction validates the shape and normalises
/// case (ISO-639 language subtag lower-case, region subtag upper-case) so two
/// spellings of the same tag compare equal. It is lenient enough for the real
/// tags containers carry (`"eng"`, `"en"`, `"en-US"`, `"spa"`) and rejects
/// clearly-invalid input (empty, digits, oversized subtags, stray separators).
///
/// The ISO-639 "undetermined" tag `"und"` is **rejected** so callers model an
/// unknown language as `Option::None` rather than a meaningless `Bcp47`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Bcp47(String);

/// The error returned when a raw container language string is not a usable
/// BCP-47 / ISO-639 tag.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Bcp47Error {
    /// The tag was empty or contained no subtags.
    #[error("language tag is empty")]
    Empty,
    /// The primary language subtag was not 2–3 ASCII letters (ISO-639-1/2/3).
    #[error("invalid primary language subtag: {0:?}")]
    InvalidPrimary(String),
    /// A non-primary subtag had a length or character set BCP-47 disallows.
    #[error("invalid subtag: {0:?}")]
    InvalidSubtag(String),
    /// The tag was the ISO-639 "undetermined" value `und`; model as `None`.
    #[error("undetermined language ('und'); model as None")]
    Undetermined,
}

impl Bcp47 {
    /// The normalised tag as a string slice (e.g. `"en-US"`).
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Parse and normalise a raw language tag.
    ///
    /// # Errors
    /// Returns a [`Bcp47Error`] if the tag is empty, is the undetermined `und`
    /// sentinel, or has a malformed primary / non-primary subtag.
    pub fn parse(raw: &str) -> Result<Self, Bcp47Error> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(Bcp47Error::Empty);
        }

        let mut subtags = trimmed.split(['-', '_']);
        let Some(primary_raw) = subtags.next() else {
            return Err(Bcp47Error::Empty);
        };
        // BCP-47 / ISO-639 primary subtag: 2–3 ASCII letters (alpha-2 or
        // alpha-3). BCP-47 reserves 4 for future use and 5–8 for registered
        // language subtags; real container tags are alpha-2/3, so we require
        // that and reject the rest as invalid for a container language.
        let primary = primary_raw.to_ascii_lowercase();
        if !(2..=3).contains(&primary.len()) || !primary.bytes().all(|b| b.is_ascii_alphabetic()) {
            return Err(Bcp47Error::InvalidPrimary(primary_raw.to_owned()));
        }
        if primary == "und" {
            return Err(Bcp47Error::Undetermined);
        }

        let mut normalised = primary;
        for sub_raw in subtags {
            if sub_raw.is_empty()
                || sub_raw.len() > 8
                || !sub_raw.bytes().all(|b| b.is_ascii_alphanumeric())
            {
                return Err(Bcp47Error::InvalidSubtag(sub_raw.to_owned()));
            }
            normalised.push('-');
            normalised.push_str(&normalise_subtag(sub_raw));
        }

        Ok(Self(normalised))
    }
}

/// Normalise a non-primary BCP-47 subtag per the common conventions:
/// a 2-letter region → upper-case; a 4-letter script → title-case; everything
/// else → lower-case.
fn normalise_subtag(sub: &str) -> String {
    let is_alpha = sub.bytes().all(|b| b.is_ascii_alphabetic());
    match sub.len() {
        2 if is_alpha => sub.to_ascii_uppercase(),
        4 if is_alpha => {
            // Title-case a script subtag (e.g. `latn` -> `Latn`).
            let mut chars = sub.chars();
            chars.next().map_or_else(String::new, |first| {
                let mut out = first.to_ascii_uppercase().to_string();
                out.push_str(&chars.as_str().to_ascii_lowercase());
                out
            })
        }
        _ => sub.to_ascii_lowercase(),
    }
}

impl FromStr for Bcp47 {
    type Err = Bcp47Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl TryFrom<&str> for Bcp47 {
    type Error = Bcp47Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl TryFrom<String> for Bcp47 {
    type Error = Bcp47Error;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<Bcp47> for String {
    fn from(value: Bcp47) -> Self {
        value.0
    }
}

impl fmt::Display for Bcp47 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// How stable a [`StableStreamId`] is across a re-probe / PMT-version bump /
/// rendition reorder.
///
/// Surfaced (in the API + UI later) as a badge so a soft-keyed crosspoint is a
/// **known, operator-visible reorder risk** rather than a silent mis-route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StabilityTier {
    /// A genuinely stable key: a TS PID, or an HLS `group_id + name`. These
    /// survive a re-probe / PMT-version bump / rendition reorder unchanged.
    Hard,
    /// A heuristic key derived from `(kind, ordinal, codec, language, title)`.
    /// Stable only while those disambiguating fields are unchanged; the
    /// container `index`/ordinal is volatile, so this is flagged as a reorder
    /// risk.
    Soft,
}

/// A **kind-scoped, stable identifier** for one elementary stream **within an
/// input**.
///
/// The container `index` is a one-time, volatile snapshot (it reorders across a
/// re-probe / PMT-version bump / rendition reorder), so a crosspoint must not
/// bind by it. `StableStreamId` is the stable key a crosspoint binds to; it
/// carries a [`StabilityTier`] describing how trustworthy that stability is.
///
/// Constructors choose the key by container family:
///
/// * [`StableStreamId::from_ts_pid`] — TS = the MPEG-TS PID. **Hard.**
/// * [`StableStreamId::from_hls`] — HLS = `group_id + name`. **Hard.**
/// * [`StableStreamId::from_general`] — general/libav = a hash of
///   `(kind, ordinal-within-kind, codec, language, title)`. **Soft**, because
///   the ordinal is the only discriminator when codec/language/title collide.
///
/// The id is **stable under index/order permutation** for the hard tiers
/// (PID / group+name do not move), and for the general tier as long as the
/// disambiguating fields (codec / language / title) are unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StableStreamId {
    /// The kind this id is scoped to (a PID and a hash never alias across kinds).
    kind_scope: char,
    /// The opaque, stable key string.
    key: String,
    /// How stable the key is.
    tier: StabilityTier,
}

impl StableStreamId {
    /// Build a **hard** id from an MPEG-TS PID (TS / SRT inputs).
    ///
    /// The PID is the genuinely-stable identity of an elementary stream in an
    /// MPEG-TS multiplex; it is scoped to `kind` so a video and audio PID that
    /// happened to share a number can never collide.
    #[must_use]
    pub fn from_ts_pid(kind: StreamKind, pid: u16) -> Self {
        Self {
            kind_scope: kind.scope_char(),
            key: format!("pid:{pid}"),
            tier: StabilityTier::Hard,
        }
    }

    /// Build a **hard** id from an HLS rendition's `group_id` + `name`.
    ///
    /// RFC 8216 keys a `MediaRendition` by its group + name; these are stable
    /// across a master-playlist rendition reorder. (The caller is responsible
    /// for synthesising a non-empty `name` when the playlist omits it, per the
    /// brief — an empty name would not be a stable key.)
    #[must_use]
    pub fn from_hls(kind: StreamKind, group_id: &str, name: &str) -> Self {
        // Length-prefix the two fields so `("ab","c")` and `("a","bc")` cannot
        // produce the same key.
        Self {
            kind_scope: kind.scope_char(),
            key: format!(
                "hls:{}:{}:{}:{}",
                group_id.len(),
                group_id,
                name.len(),
                name
            ),
            tier: StabilityTier::Hard,
        }
    }

    /// Build a **soft** id for a general/libav stream from its disambiguating
    /// fields.
    ///
    /// The key is a hash of `(kind, ordinal-within-kind, codec, language,
    /// title)`. It is **stable under index permutation** as long as the
    /// `codec` / `language` / `title` are unchanged — but because the `ordinal`
    /// (the position among same-kind streams) is folded in, two streams that
    /// are identical in every other field are distinguished only by ordinal, so
    /// the tier is [`StabilityTier::Soft`] (an ordinal-only fallback is a
    /// reorder risk).
    #[must_use]
    pub fn from_general(
        kind: StreamKind,
        ordinal: u32,
        codec: &str,
        language: Option<&Bcp47>,
        title: Option<&str>,
    ) -> Self {
        let mut hasher = DefaultHasher::new();
        // Hash the *canonical* kind discriminant (incl. the data/timecode
        // payload) so two kinds never collide.
        kind.hash(&mut hasher);
        ordinal.hash(&mut hasher);
        codec.to_ascii_lowercase().hash(&mut hasher);
        // `Option`'s `Hash` already distinguishes `None` from `Some("")`.
        language.map(Bcp47::as_str).hash(&mut hasher);
        title.hash(&mut hasher);
        let digest = hasher.finish();
        Self {
            kind_scope: kind.scope_char(),
            key: format!("gen:{digest:016x}"),
            tier: StabilityTier::Soft,
        }
    }

    /// The stability tier of this id.
    #[must_use]
    pub const fn tier(&self) -> StabilityTier {
        self.tier
    }

    /// The kind-scope discriminant character this id is scoped to.
    #[must_use]
    pub const fn kind_scope(&self) -> char {
        self.kind_scope
    }

    /// The opaque, stable key string (kind-scope excluded).
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }
}

impl fmt::Display for StableStreamId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.kind_scope, self.key)
    }
}

/// Kind-specific detail for one elementary stream, carried alongside the coarse
/// [`StreamKind`] in a [`StreamDescriptor`].
///
/// The detail is **adjacently tagged** (`#[serde(tag = "detail", content =
/// "params")]`, `snake_case`) so it serialises unambiguously across TOML and JSON
/// and never relies on `untagged` (ADR-0010 / repo conventions). Each variant
/// carries only the fields that kind has to offer from a container probe; a
/// non-AV passthrough stream (SCTE-35 / KLV / timecode) carries no extra detail
/// and is modelled as [`StreamDetail::Passthrough`].
///
/// `#[non_exhaustive]` so future kinds can grow detail fields without a breaking
/// change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "detail", content = "params", rename_all = "snake_case")]
#[non_exhaustive]
pub enum StreamDetail {
    /// Video-stream geometry + cadence, as probed from the container.
    Video {
        /// Coded width in pixels (`0` if the container did not declare it).
        width: u32,
        /// Coded height in pixels (`0` if the container did not declare it).
        height: u32,
        /// The container's declared average frame rate, if any (an exact
        /// rational — **never** a float fps, invariant #3).
        frame_rate: Option<Rational>,
    },
    /// Audio-track layout, as probed from the container.
    Audio {
        /// Channel count (`0` if the container did not declare it).
        channels: u16,
        /// Sample rate in Hz (`0` if the container did not declare it).
        sample_rate: u32,
    },
    /// Subtitle / caption track flags.
    Subtitle {
        /// Whether the track is flagged "forced" (narrative subtitles shown even
        /// when subtitles are otherwise off).
        forced: bool,
    },
    /// A passthrough stream (SCTE-35 / KLV data, or a timecode track) that
    /// carries no kind-specific AV detail.
    Passthrough,
}

impl StreamDetail {
    /// The coded geometry `(width, height)` for a [`StreamDetail::Video`] stream,
    /// or [`None`] for any other detail.
    #[must_use]
    pub const fn video_geometry(&self) -> Option<(u32, u32)> {
        if let Self::Video { width, height, .. } = *self {
            Some((width, height))
        } else {
            None
        }
    }

    /// The `(channels, sample_rate)` for a [`StreamDetail::Audio`] stream, or
    /// [`None`] for any other detail.
    #[must_use]
    pub const fn audio_layout(&self) -> Option<(u16, u32)> {
        if let Self::Audio {
            channels,
            sample_rate,
        } = *self
        {
            Some((channels, sample_rate))
        } else {
            None
        }
    }
}

/// A typed, owned, `Send` record describing **one** elementary stream an input
/// offers — the unit of the [`StreamInventory`] the router matrix consumes.
///
/// This is the canonical model surface (it lives in `multiview-core` so config /
/// control / web can consume it without depending on `multiview-ffmpeg`). The
/// libav / TS / HLS discovery paths each build a `Vec<StreamDescriptor>` and wrap
/// it in a [`StreamInventory`]; nothing here is FFI or I/O.
///
/// Serialises with the [`StreamKind`] flattened (so a descriptor carries a single
/// `kind` / `payload` tag pair) plus an adjacently-tagged [`StreamDetail`]; no
/// field is `untagged`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct StreamDescriptor {
    /// The stable, kind-scoped identity a crosspoint binds to (survives a
    /// re-probe / PMT-version bump / rendition reorder; see [`StableStreamId`]).
    pub id: StableStreamId,
    /// The canonical media kind of this elementary stream.
    #[serde(flatten)]
    pub kind: StreamKind,
    /// The validated language tag, if the container declared a usable one
    /// (unparseable / `und` tags are dropped to `None`, never failed).
    pub language: Option<Bcp47>,
    /// The codec descriptor name as reported by the container (e.g. `"h264"`,
    /// `"aac"`, `"dvbsub"`).
    pub codec: String,
    /// The track title / handler name, if the container declared one.
    pub title: Option<String>,
    /// Whether the container flags this stream as a default for its kind.
    pub default: bool,
    /// The kind-specific detail (geometry / layout / flags / passthrough).
    pub detail: StreamDetail,
}

impl StreamDescriptor {
    /// Construct a descriptor for one elementary stream.
    ///
    /// `language`, `title`, and `default` start empty / `false`; set them with
    /// the builder methods.
    #[must_use]
    pub fn new(
        id: StableStreamId,
        kind: StreamKind,
        codec: impl Into<String>,
        detail: StreamDetail,
    ) -> Self {
        Self {
            id,
            kind,
            language: None,
            codec: codec.into(),
            title: None,
            default: false,
            detail,
        }
    }

    /// Set the validated language tag (builder style).
    #[must_use]
    pub fn with_language(mut self, language: Option<Bcp47>) -> Self {
        self.language = language;
        self
    }

    /// Set the track title (builder style).
    #[must_use]
    pub fn with_title(mut self, title: Option<String>) -> Self {
        self.title = title;
        self
    }

    /// Set the default flag (builder style).
    #[must_use]
    pub fn with_default(mut self, default: bool) -> Self {
        self.default = default;
        self
    }
}

/// The full, typed inventory of every elementary stream an input offers.
///
/// This is what RT-1 surfaces: the libav demux already enumerates **all** streams
/// (with language), but the ingest path historically kept only the best video
/// stream. The inventory keeps every row — addressable by [`StreamKind`] and by
/// [`StableStreamId`] — so audio tracks, subtitle tracks, and SCTE-35 / KLV /
/// timecode data streams survive discovery (the decoupled-routing crosspoint
/// model, ADR-0034).
///
/// Pure data: no engine, no I/O. Serialises cleanly (every field tagged, never
/// `untagged`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct StreamInventory {
    /// The owning input's id, if known (the discovery layer often probes a
    /// container before it is bound to a configured input id).
    pub input_id: Option<String>,
    /// Every elementary stream the input offers, in container order.
    pub streams: Vec<StreamDescriptor>,
}

impl StreamInventory {
    /// An empty inventory with no owning input id.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Build an inventory from a stream list, with no owning input id yet.
    #[must_use]
    pub fn from_streams(streams: Vec<StreamDescriptor>) -> Self {
        Self {
            input_id: None,
            streams,
        }
    }

    /// Set the owning input id (builder style).
    #[must_use]
    pub fn with_input_id(mut self, input_id: impl Into<String>) -> Self {
        self.input_id = Some(input_id.into());
        self
    }

    /// Every descriptor whose [`StreamKind`] matches `predicate`, in container
    /// order.
    ///
    /// `StreamKind` carries a payload for [`StreamKind::Data`] /
    /// [`StreamKind::Timecode`], so this takes a predicate rather than an
    /// equality target — callers can match a coarse family (`StreamKind::is_audio`)
    /// or an exact kind (`|k| k == StreamKind::Data(DataKind::Scte35)`).
    pub fn by_kind<'a>(
        &'a self,
        predicate: impl Fn(StreamKind) -> bool + 'a,
    ) -> impl Iterator<Item = &'a StreamDescriptor> + 'a {
        self.streams.iter().filter(move |s| predicate(s.kind))
    }

    /// Every video stream, in container order.
    pub fn video(&self) -> impl Iterator<Item = &StreamDescriptor> {
        self.by_kind(StreamKind::is_video)
    }

    /// Every audio track, in container order.
    pub fn audio_tracks(&self) -> impl Iterator<Item = &StreamDescriptor> {
        self.by_kind(StreamKind::is_audio)
    }

    /// Every subtitle / caption track, in container order.
    pub fn subtitle_tracks(&self) -> impl Iterator<Item = &StreamDescriptor> {
        self.by_kind(StreamKind::is_subtitle)
    }

    /// Every data (SCTE-35 / KLV) stream, in container order.
    pub fn data(&self) -> impl Iterator<Item = &StreamDescriptor> {
        self.by_kind(StreamKind::is_data)
    }

    /// Every timecode stream, in container order.
    pub fn timecode(&self) -> impl Iterator<Item = &StreamDescriptor> {
        self.by_kind(StreamKind::is_timecode)
    }

    /// The default stream for a kind family, if any: the first descriptor of that
    /// family flagged `default`, else the first of that family at all (the
    /// container-order fallback — there is always *a* sensible default when the
    /// family is non-empty).
    #[must_use]
    pub fn default_for(&self, predicate: impl Fn(StreamKind) -> bool) -> Option<&StreamDescriptor> {
        let mut first = None;
        for s in self.streams.iter().filter(|s| predicate(s.kind)) {
            if s.default {
                return Some(s);
            }
            if first.is_none() {
                first = Some(s);
            }
        }
        first
    }
}
