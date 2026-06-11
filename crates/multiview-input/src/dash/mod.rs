//! MPEG-DASH (ISO/IEC 23009-1) ingest model: an MPD manifest parser plus a pure
//! segment-selection / ABR-ladder-awareness model.
//!
//! DASH delivers media as a **Media Presentation Description** (MPD) — an XML
//! manifest — referencing time-addressed segments grouped into Representations
//! (one per bitrate/resolution) inside `AdaptationSet`s (one per media kind). This
//! module is the **pure decision layer**: it parses the manifest structure
//! Multiview needs (`Period` → `AdaptationSet` → `Representation` →
//! `SegmentTemplate`),
//! exposes the ABR ladder, and computes which segment URL to fetch at a given
//! presentation time. The actual HTTP fetch and demux reuse the existing libav
//! ingest (an MPD URL is opened by the demuxer like any other source); nothing
//! here performs I/O.
//!
//! ## Why a hand-rolled MPD reader
//!
//! The default build is pure-Rust and dependency-free (no XML crate), so this
//! module ships a small, **bounded, panic-free** XML element scanner sufficient
//! for the MPD subset Multiview consumes (the private `xml` submodule). It never
//! recurses unboundedly,
//! never indexes out of range, and surfaces malformed input as a typed
//! [`DashError`].
//!
//! ## Isolation (invariants #1 / #4 / #10)
//!
//! The segment selector is *sampled* by the input pacer — DASH live edges are
//! paced to wall-clock by PTS like any other live/VOD-as-live source (invariant
//! #4); nothing here paces the output clock.

mod xml;

use core::time::Duration;

/// Errors raised while parsing an MPD or selecting a segment.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DashError {
    /// The manifest was not well-formed XML (unbalanced tags, premature end, or
    /// it exceeded the bounded scanner's element budget).
    #[error("malformed mpd xml: {0}")]
    MalformedXml(&'static str),

    /// The root element was not `<MPD>`.
    #[error("not an mpd manifest (root element is not <MPD>)")]
    NotMpd,

    /// A required attribute was missing.
    #[error("mpd missing required attribute: {0}")]
    MissingAttribute(&'static str),

    /// An attribute value could not be parsed (bad number, duration, etc.).
    #[error("mpd attribute {attr} has invalid value {value:?}")]
    BadAttribute {
        /// The attribute name.
        attr: &'static str,
        /// The raw value that failed to parse.
        value: String,
    },

    /// A `SegmentTemplate` referenced a numbering scheme the model cannot
    /// resolve, or a segment index was out of range.
    #[error("dash segment selection: {0}")]
    Selection(&'static str),
}

/// The MPD presentation type: a finite VOD presentation or a live/dynamic one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum PresentationType {
    /// `static` — a complete, finite presentation (VOD).
    #[default]
    Static,
    /// `dynamic` — a live presentation whose segments appear over time.
    Dynamic,
}

/// A `SegmentTemplate`: the URL pattern and timing for number- or time-addressed
/// segments (ISO/IEC 23009-1 §5.3.9.4).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SegmentTemplate {
    /// The `initialization` URL template (with `$RepresentationID$` etc.).
    pub initialization: Option<String>,
    /// The `media` URL template (with `$Number$` / `$Time$`).
    pub media: Option<String>,
    /// The `timescale` (ticks per second) for `duration`/`$Time$`.
    pub timescale: u64,
    /// The fixed segment `duration` in `timescale` ticks (number addressing).
    pub duration: Option<u64>,
    /// The `startNumber` (defaults to 1).
    pub start_number: u64,
}

impl SegmentTemplate {
    /// The segment duration as a [`Duration`], if `duration` and a non-zero
    /// `timescale` are both known.
    #[must_use]
    pub fn segment_duration(&self) -> Option<Duration> {
        let duration = self.duration?;
        if self.timescale == 0 {
            return None;
        }
        // ns = duration / timescale * 1e9, computed in u128 to avoid overflow.
        let ns = u128::from(duration)
            .checked_mul(1_000_000_000)?
            .checked_div(u128::from(self.timescale))?;
        u64::try_from(ns).ok().map(Duration::from_nanos)
    }

    /// Resolve the `media` URL for the segment at zero-based `index`, substituting
    /// `$RepresentationID$` and `$Number$`. `$Time$` addressing requires a
    /// `SegmentTimeline` (not modelled here) and returns
    /// [`DashError::Selection`].
    ///
    /// # Errors
    ///
    /// * [`DashError::Selection`] when there is no `media` template, the template
    ///   needs `$Time$` addressing, or the segment number overflows.
    pub fn media_url(&self, representation_id: &str, index: u64) -> Result<String, DashError> {
        let media = self.media.as_deref().ok_or(DashError::Selection(
            "SegmentTemplate has no media attribute",
        ))?;
        if media.contains("$Time$") {
            return Err(DashError::Selection(
                "$Time$ addressing requires a SegmentTimeline (not modelled)",
            ));
        }
        let number = self
            .start_number
            .checked_add(index)
            .ok_or(DashError::Selection("segment number overflow"))?;
        Ok(substitute(media, representation_id, Some(number)))
    }

    /// Resolve the `initialization` URL, substituting `$RepresentationID$`.
    ///
    /// # Errors
    ///
    /// [`DashError::Selection`] when there is no `initialization` template.
    pub fn initialization_url(&self, representation_id: &str) -> Result<String, DashError> {
        let init = self.initialization.as_deref().ok_or(DashError::Selection(
            "SegmentTemplate has no initialization",
        ))?;
        Ok(substitute(init, representation_id, None))
    }
}

/// One `Representation`: a single encoding (bitrate/resolution) of an
/// `AdaptationSet`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Representation {
    /// The `id` (used in URL template substitution).
    pub id: String,
    /// The average `bandwidth` in bits per second.
    pub bandwidth: u64,
    /// The coded `width` in pixels, if a video representation.
    pub width: Option<u32>,
    /// The coded `height` in pixels, if a video representation.
    pub height: Option<u32>,
    /// The `codecs` string (e.g. `avc1.640028`).
    pub codecs: Option<String>,
    /// A representation-level `SegmentTemplate`, if present (overrides the
    /// `AdaptationSet`'s).
    pub segment_template: Option<SegmentTemplate>,
}

/// One `AdaptationSet`: a set of interchangeable `Representation`s of one media
/// kind.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AdaptationSet {
    /// The `contentType` / `mimeType`-derived kind (`video`, `audio`, …).
    pub content_type: Option<String>,
    /// The MIME type.
    pub mime_type: Option<String>,
    /// An AdaptationSet-level `SegmentTemplate` shared by its Representations.
    pub segment_template: Option<SegmentTemplate>,
    /// The Representations (the ABR ladder for this set).
    pub representations: Vec<Representation>,
}

impl AdaptationSet {
    /// Whether this `AdaptationSet` carries video.
    #[must_use]
    pub fn is_video(&self) -> bool {
        self.content_type.as_deref() == Some("video")
            || self
                .mime_type
                .as_deref()
                .is_some_and(|m| m.starts_with("video/"))
    }

    /// Whether this `AdaptationSet` carries audio.
    #[must_use]
    pub fn is_audio(&self) -> bool {
        self.content_type.as_deref() == Some("audio")
            || self
                .mime_type
                .as_deref()
                .is_some_and(|m| m.starts_with("audio/"))
    }

    /// The representations sorted by ascending bandwidth — the ABR ladder rungs
    /// from lowest to highest bitrate.
    #[must_use]
    pub fn ladder(&self) -> Vec<&Representation> {
        let mut rungs: Vec<&Representation> = self.representations.iter().collect();
        rungs.sort_by_key(|r| r.bandwidth);
        rungs
    }

    /// The highest-bandwidth representation that fits within `max_bandwidth` bps,
    /// falling back to the lowest rung when none fit (so playback never stalls
    /// for lack of a rung).
    #[must_use]
    pub fn select_for_bandwidth(&self, max_bandwidth: u64) -> Option<&Representation> {
        let ladder = self.ladder();
        ladder
            .iter()
            .rev()
            .find(|r| r.bandwidth <= max_bandwidth)
            .or_else(|| ladder.first())
            .copied()
    }

    /// The effective `SegmentTemplate` for a representation: the
    /// representation-level template if present, else the set-level one.
    #[must_use]
    pub fn effective_template<'a>(
        &'a self,
        representation: &'a Representation,
    ) -> Option<&'a SegmentTemplate> {
        representation
            .segment_template
            .as_ref()
            .or(self.segment_template.as_ref())
    }
}

/// One Period of the presentation timeline.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Period {
    /// The `id`.
    pub id: Option<String>,
    /// The period `start` offset.
    pub start: Option<Duration>,
    /// The period `duration`.
    pub duration: Option<Duration>,
    /// The `AdaptationSet`s in this period.
    pub adaptation_sets: Vec<AdaptationSet>,
}

impl Period {
    /// The first video `AdaptationSet`, if any.
    #[must_use]
    pub fn video(&self) -> Option<&AdaptationSet> {
        self.adaptation_sets.iter().find(|a| a.is_video())
    }

    /// The first audio `AdaptationSet`, if any.
    #[must_use]
    pub fn audio(&self) -> Option<&AdaptationSet> {
        self.adaptation_sets.iter().find(|a| a.is_audio())
    }
}

/// A parsed MPD manifest.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Mpd {
    /// `static` (VOD) or `dynamic` (live).
    pub presentation_type: PresentationType,
    /// `minBufferTime`.
    pub min_buffer_time: Option<Duration>,
    /// `mediaPresentationDuration` (VOD only).
    pub media_presentation_duration: Option<Duration>,
    /// The Periods of the presentation.
    pub periods: Vec<Period>,
}

impl Mpd {
    /// Whether this is a live (dynamic) presentation.
    #[must_use]
    pub fn is_live(&self) -> bool {
        matches!(self.presentation_type, PresentationType::Dynamic)
    }

    /// Parse an MPD manifest from its XML text.
    ///
    /// # Errors
    ///
    /// Any [`DashError`] from XML scanning or attribute parsing.
    pub fn parse(manifest: &str) -> Result<Self, DashError> {
        xml::parse_mpd(manifest)
    }
}

/// Parse an ISO 8601 duration of the `PT…H…M…S` form used by DASH (and the
/// leading `P…D` day component) into a [`Duration`].
///
/// Supports days, hours, minutes, and fractional seconds — the forms MPDs use.
///
/// # Errors
///
/// [`DashError::BadAttribute`] when the string is not a recognised ISO 8601
/// duration.
pub fn parse_iso8601_duration(value: &str) -> Result<Duration, DashError> {
    let bad = || DashError::BadAttribute {
        attr: "duration",
        value: value.to_owned(),
    };
    let rest = value.strip_prefix('P').ok_or_else(bad)?;
    let mut total_secs: f64 = 0.0;

    let time_part = if let Some((days_part, time_part)) = rest.split_once('T') {
        if let Some(days) = days_part.strip_suffix('D') {
            if !days.is_empty() {
                let d: f64 = days.parse().map_err(|_e| bad())?;
                total_secs += d * 86_400.0;
            }
        } else if !days_part.is_empty() {
            return Err(bad());
        }
        time_part
    } else if let Some(days) = rest.strip_suffix('D') {
        let d: f64 = days.parse().map_err(|_e| bad())?;
        return secs_to_duration(d * 86_400.0);
    } else {
        return Err(bad());
    };

    // Time component: H, M, S (S may be fractional).
    let mut number = String::new();
    for ch in time_part.chars() {
        match ch {
            '0'..='9' | '.' => number.push(ch),
            'H' | 'M' | 'S' => {
                let n: f64 = number.parse().map_err(|_e| bad())?;
                let mult = match ch {
                    'H' => 3600.0,
                    'M' => 60.0,
                    _ => 1.0,
                };
                total_secs += n * mult;
                number.clear();
            }
            _ => return Err(bad()),
        }
    }
    secs_to_duration(total_secs)
}

/// Convert a non-negative seconds value to a [`Duration`], rejecting NaN /
/// negative / overflowing values.
fn secs_to_duration(secs: f64) -> Result<Duration, DashError> {
    if !secs.is_finite() || secs < 0.0 {
        return Err(DashError::BadAttribute {
            attr: "duration",
            value: "non-finite or negative".to_owned(),
        });
    }
    Ok(Duration::from_secs_f64(secs))
}

/// Substitute the DASH URL-template identifiers used by Multiview
/// (`$RepresentationID$`, `$Number$`, and the literal `$$` escape).
fn substitute(template: &str, representation_id: &str, number: Option<u64>) -> String {
    let mut out = template.replace("$$", "\u{0}"); // protect literal dollars
    out = out.replace("$RepresentationID$", representation_id);
    if let Some(n) = number {
        out = out.replace("$Number$", &n.to_string());
    }
    out.replace('\u{0}', "$")
}
