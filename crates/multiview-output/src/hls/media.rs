//! HLS / LL-HLS **media playlist** generation (pure text).
//!
//! A [`MediaPlaylist`] is an append-only, optionally sliding-window list of
//! [`Segment`]s (each optionally subdivided into LL-HLS [`Part`]s). It renders
//! to the exact UTF-8 manifest a player consumes via [`MediaPlaylist::render`].
//!
//! This module is **pure Rust** with no I/O: it builds the text only. The CMAF
//! segmenter and the blocking-reload HTTP server (ADR-0007) live behind the
//! off-by-default transport features and feed this generator.
//!
//! ## Timing discipline
//!
//! Durations are carried as exact seconds-as-`f64` *for the text layer only*
//! (HLS `EXTINF` is decimal seconds). The engine computes them from the tick
//! counter (invariant #3) and passes the result here; this module never does
//! timing arithmetic of its own beyond rounding for `EXT-X-TARGETDURATION`,
//! which the HLS spec defines as an integer number of seconds.
use std::fmt::Write as _;

/// The container the playlist references.
///
/// fMP4/CMAF playlists carry an `EXT-X-MAP` init segment (ADR-0007 is CMAF
/// first); MPEG-TS playlists do not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SegmentType {
    /// Fragmented MP4 / CMAF (the default, ADR-0007). Emits `EXT-X-MAP`.
    Fmp4,
    /// MPEG-2 Transport Stream (legacy reach). No init segment.
    MpegTs,
}

/// `EXT-X-SERVER-CONTROL` parameters for Low-Latency HLS.
///
/// Per the HLS spec, `PART-HOLD-BACK` SHOULD be at least three times the part
/// target; the engine supplies the concrete value (this type does not invent
/// one). Fields left `None` are omitted from the rendered tag.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ServerControl {
    /// `CAN-BLOCK-RELOAD=YES` — the server supports blocking playlist reload.
    pub can_block_reload: bool,
    /// `PART-HOLD-BACK` in seconds (Low-Latency live edge for parts).
    pub part_hold_back: Option<f64>,
    /// `HOLD-BACK` in seconds (live edge for full segments).
    pub hold_back: Option<f64>,
    /// `CAN-SKIP-UNTIL` in seconds (playlist delta updates).
    pub can_skip_until: Option<f64>,
}

/// One LL-HLS partial segment (`EXT-X-PART`).
#[derive(Debug, Clone)]
pub struct Part {
    /// Part URI (relative to the playlist).
    pub uri: String,
    /// Part duration in seconds.
    pub duration: f64,
    /// Whether this part begins with an independently-decodable frame
    /// (`INDEPENDENT=YES`).
    pub independent: bool,
}

impl Part {
    /// Construct a part with the given URI and duration (not independent).
    #[must_use]
    pub fn new(uri: impl Into<String>, duration: f64) -> Self {
        Self {
            uri: uri.into(),
            duration,
            independent: false,
        }
    }

    /// Mark this part as independently decodable (`INDEPENDENT=YES`).
    #[must_use]
    pub fn independent(mut self) -> Self {
        self.independent = true;
        self
    }
}

/// One media segment, optionally subdivided into LL-HLS [`Part`]s.
#[derive(Debug, Clone)]
pub struct Segment {
    /// Segment URI (relative to the playlist).
    pub uri: String,
    /// Segment duration in seconds (`EXTINF`).
    pub duration: f64,
    /// Whether an `EXT-X-DISCONTINUITY` tag precedes this segment.
    pub discontinuity: bool,
    /// LL-HLS parts that make up this segment, in order.
    pub parts: Vec<Part>,
}

impl Segment {
    /// Construct a segment with the given URI and duration (no discontinuity,
    /// no parts).
    #[must_use]
    pub fn new(uri: impl Into<String>, duration: f64) -> Self {
        Self {
            uri: uri.into(),
            duration,
            discontinuity: false,
            parts: Vec::new(),
        }
    }
}

/// A rendition report (`EXT-X-RENDITION-REPORT`) pointing a player at the latest
/// state of a sibling rendition (used for fast blocking reloads across
/// renditions).
#[derive(Debug, Clone)]
struct RenditionReport {
    uri: String,
    last_msn: u64,
    last_part: Option<u32>,
}

/// An HLS / LL-HLS media playlist builder + renderer.
///
/// Push [`Segment`]s as the segmenter produces them; if a sliding window is set
/// via [`MediaPlaylist::set_window`], the oldest segments are evicted and
/// `EXT-X-MEDIA-SEQUENCE` / `EXT-X-DISCONTINUITY-SEQUENCE` advance to stay
/// consistent. Call [`MediaPlaylist::render`] to obtain the manifest text.
#[derive(Debug, Clone)]
pub struct MediaPlaylist {
    segment_type: SegmentType,
    target_duration: u64,
    media_sequence: u64,
    discontinuity_sequence: u64,
    window: Option<usize>,
    finished: bool,
    part_target: Option<f64>,
    server_control: Option<ServerControl>,
    map_uri: String,
    preload_hint: Option<String>,
    segments: Vec<Segment>,
    rendition_reports: Vec<RenditionReport>,
}

impl MediaPlaylist {
    /// Create an empty playlist for the given container type.
    #[must_use]
    pub fn new(segment_type: SegmentType) -> Self {
        Self {
            segment_type,
            target_duration: 0,
            media_sequence: 0,
            discontinuity_sequence: 0,
            window: None,
            finished: false,
            part_target: None,
            server_control: None,
            map_uri: "init.mp4".to_owned(),
            preload_hint: None,
            segments: Vec::new(),
            rendition_reports: Vec::new(),
        }
    }

    /// Set `EXT-X-TARGETDURATION` (integer seconds) explicitly.
    pub fn set_target_duration(&mut self, seconds: u64) {
        self.target_duration = seconds;
    }

    /// The current `EXT-X-TARGETDURATION` value.
    #[must_use]
    pub const fn target_duration(&self) -> u64 {
        self.target_duration
    }

    /// Recompute `EXT-X-TARGETDURATION` as the rounded maximum segment duration
    /// currently in the window (the HLS spec requires it be `>=` every
    /// `EXTINF`, expressed as an integer; we round to nearest).
    pub fn recompute_target_duration(&mut self) {
        let mut max = 0u64;
        for seg in &self.segments {
            let rounded = round_to_u64(seg.duration);
            if rounded > max {
                max = rounded;
            }
        }
        self.target_duration = max;
    }

    /// Set the starting `EXT-X-MEDIA-SEQUENCE`. As segments are evicted by the
    /// sliding window this advances automatically.
    pub fn set_media_sequence(&mut self, msn: u64) {
        self.media_sequence = msn;
    }

    /// The current `EXT-X-MEDIA-SEQUENCE` (the media sequence number of the
    /// first segment still listed).
    #[must_use]
    pub const fn media_sequence(&self) -> u64 {
        self.media_sequence
    }

    /// Set the starting `EXT-X-DISCONTINUITY-SEQUENCE`.
    pub fn set_discontinuity_sequence(&mut self, seq: u64) {
        self.discontinuity_sequence = seq;
    }

    /// The current `EXT-X-DISCONTINUITY-SEQUENCE`.
    #[must_use]
    pub const fn discontinuity_sequence(&self) -> u64 {
        self.discontinuity_sequence
    }

    /// Limit the playlist to the most recent `window` segments. Pushing beyond
    /// the window evicts the oldest segments and advances the media (and, where
    /// an evicted segment carried a discontinuity, discontinuity) sequence.
    pub fn set_window(&mut self, window: usize) {
        self.window = Some(window);
        self.trim_to_window();
    }

    /// Mark the playlist as finished (`EXT-X-ENDLIST`).
    pub fn set_finished(&mut self, finished: bool) {
        self.finished = finished;
    }

    /// Enable LL-HLS by declaring `EXT-X-PART-INF:PART-TARGET`.
    pub fn set_part_target(&mut self, seconds: f64) {
        self.part_target = Some(seconds);
    }

    /// Set the `EXT-X-SERVER-CONTROL` parameters (LL-HLS blocking reload).
    pub fn set_server_control(&mut self, control: ServerControl) {
        self.server_control = Some(control);
    }

    /// Override the `EXT-X-MAP` init-segment URI (fMP4 only; default
    /// `"init.mp4"`).
    pub fn set_map_uri(&mut self, uri: impl Into<String>) {
        self.map_uri = uri.into();
    }

    /// Set (or clear) the trailing `EXT-X-PRELOAD-HINT` part URI.
    pub fn set_preload_hint(&mut self, uri: Option<String>) {
        self.preload_hint = uri;
    }

    /// Add an `EXT-X-RENDITION-REPORT` for a sibling rendition.
    pub fn add_rendition_report(
        &mut self,
        uri: impl Into<String>,
        last_msn: u64,
        last_part: Option<u32>,
    ) {
        self.rendition_reports.push(RenditionReport {
            uri: uri.into(),
            last_msn,
            last_part,
        });
    }

    /// Append a segment, applying the sliding window if one is configured.
    pub fn push_segment(&mut self, segment: Segment) {
        self.segments.push(segment);
        self.trim_to_window();
    }

    /// Number of segments currently listed.
    #[must_use]
    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    /// Evict the oldest segments beyond the window, advancing media and
    /// discontinuity sequence counters to keep the manifest consistent.
    fn trim_to_window(&mut self) {
        let Some(window) = self.window else {
            return;
        };
        while self.segments.len() > window {
            // `segments` is non-empty here (len > window >= 0), so the first
            // element exists; `first()` keeps us off `indexing_slicing`.
            if let Some(evicted) = self.segments.first() {
                if evicted.discontinuity {
                    self.discontinuity_sequence = self.discontinuity_sequence.saturating_add(1);
                }
            }
            // Drop the front segment and advance the media sequence by one.
            self.segments.remove(0);
            self.media_sequence = self.media_sequence.saturating_add(1);
        }
    }

    /// The HLS protocol version this playlist requires: 9 when LL-HLS
    /// (`EXT-X-PART`) features are in use, otherwise 7 (fMP4/CMAF baseline).
    #[must_use]
    fn version(&self) -> u32 {
        if self.part_target.is_some() || self.segments.iter().any(|s| !s.parts.is_empty()) {
            9
        } else {
            7
        }
    }

    /// Render the playlist to its exact UTF-8 manifest text.
    ///
    /// Output is deterministic and stable: the tag order matches the HLS spec's
    /// recommended layout (header tags, then segments in order, then trailing
    /// rendition reports / preload hint / end marker).
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        // Header. `write!`/`writeln!` into a `String` cannot fail, so the
        // `Result` is intentionally discarded with `let _ =` (no `unwrap`).
        let _ = writeln!(out, "#EXTM3U");
        let _ = writeln!(out, "#EXT-X-VERSION:{}", self.version());
        let _ = writeln!(out, "#EXT-X-TARGETDURATION:{}", self.target_duration);
        if let Some(control) = &self.server_control {
            let _ = writeln!(out, "{}", render_server_control(control));
        }
        if let Some(part_target) = self.part_target {
            let _ = writeln!(
                out,
                "#EXT-X-PART-INF:PART-TARGET={}",
                fmt_seconds(part_target)
            );
        }
        let _ = writeln!(out, "#EXT-X-MEDIA-SEQUENCE:{}", self.media_sequence);
        if self.discontinuity_sequence != 0 {
            let _ = writeln!(
                out,
                "#EXT-X-DISCONTINUITY-SEQUENCE:{}",
                self.discontinuity_sequence
            );
        }
        if self.segment_type == SegmentType::Fmp4 {
            let _ = writeln!(out, "#EXT-X-MAP:URI=\"{}\"", self.map_uri);
        }

        // Segments (and their parts) in order.
        for seg in &self.segments {
            if seg.discontinuity {
                let _ = writeln!(out, "#EXT-X-DISCONTINUITY");
            }
            for part in &seg.parts {
                let _ = write!(
                    out,
                    "#EXT-X-PART:DURATION={},URI=\"{}\"",
                    fmt_seconds(part.duration),
                    part.uri
                );
                if part.independent {
                    let _ = write!(out, ",INDEPENDENT=YES");
                }
                let _ = writeln!(out);
            }
            let _ = writeln!(out, "#EXTINF:{},", fmt_seconds(seg.duration));
            let _ = writeln!(out, "{}", seg.uri);
        }

        // Trailing tags.
        for report in &self.rendition_reports {
            let _ = write!(
                out,
                "#EXT-X-RENDITION-REPORT:URI=\"{}\",LAST-MSN={}",
                report.uri, report.last_msn
            );
            if let Some(part) = report.last_part {
                let _ = write!(out, ",LAST-PART={part}");
            }
            let _ = writeln!(out);
        }
        if let Some(hint) = &self.preload_hint {
            let _ = writeln!(out, "#EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"{hint}\"");
        }
        if self.finished {
            let _ = writeln!(out, "#EXT-X-ENDLIST");
        }
        out
    }
}

/// Render the `EXT-X-SERVER-CONTROL` attribute list.
fn render_server_control(control: &ServerControl) -> String {
    let mut attrs: Vec<String> = Vec::new();
    if control.can_block_reload {
        attrs.push("CAN-BLOCK-RELOAD=YES".to_owned());
    }
    if let Some(v) = control.can_skip_until {
        attrs.push(format!("CAN-SKIP-UNTIL={}", fmt_seconds(v)));
    }
    if let Some(v) = control.hold_back {
        attrs.push(format!("HOLD-BACK={}", fmt_seconds(v)));
    }
    if let Some(v) = control.part_hold_back {
        attrs.push(format!("PART-HOLD-BACK={}", fmt_seconds(v)));
    }
    format!("#EXT-X-SERVER-CONTROL:{}", attrs.join(","))
}

/// Format a seconds value with exactly three decimal places (HLS decimal
/// duration), e.g. `0.5 -> "0.500"`, `2.0 -> "2.000"`.
fn fmt_seconds(seconds: f64) -> String {
    format!("{seconds:.3}")
}

/// Round a non-negative seconds value to the nearest `u64`, saturating instead
/// of using a lossy `as` cast (which the lint policy forbids). Negative or
/// non-finite inputs round to `0`.
fn round_to_u64(seconds: f64) -> u64 {
    if !seconds.is_finite() || seconds <= 0.0 {
        return 0;
    }
    let rounded = seconds.round();
    // `u64::MAX as f64` rounds up to 2^64; anything at/above that saturates.
    if rounded >= 18_446_744_073_709_551_616.0 {
        return u64::MAX;
    }
    // `rounded` is now a finite, non-negative integer-valued `f64` strictly
    // below `2^64`. Format it as an integer (no `as` cast, lint-clean) and
    // parse back to `u64`; this is exact for integer-valued `f64`. The parse
    // cannot fail given the guards above, but we saturate rather than unwrap.
    let digits = format!("{rounded:.0}");
    digits.parse::<u64>().unwrap_or(u64::MAX)
}
