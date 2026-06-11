//! Timed-text **subtitle/caption** ingest: SRT (`SubRip`) and `WebVTT` parsing into
//! a pure, time-indexed [`CueTrack`], plus the active-cue lookup the renderer
//! turns into text runs (ADR-R007 / resilience-and-av.md).
//!
//! This is the **pure** subtitle path: it depends only on `multiview-core`'s
//! [`MediaTime`] and never touches a native library or a decoded frame, so a
//! cue track is parsed and queried deterministically and is exactly testable. A
//! cue carries its plain text (markup stripped to the displayable lines); the
//! compositor's stage-1 text engine rasterizes the active cue's lines into the
//! overlay sub-pass.
//!
//! Full ASS/SSA with libass styling lives behind the off-by-default `libass`
//! feature (see [`crate::libass`]); when libass is unavailable the engine falls
//! back to this SRT/VTT path (graceful degradation, ADR-R007). SRT and VTT share
//! the same cue grammar here: an index/cue-id line (optional), a
//! `start --> end` timing line, then one or more text lines, blocks separated by
//! a blank line. `WebVTT` additionally allows a `.mmm` fraction (SRT uses `,mmm`)
//! and a leading `WEBVTT` header, both handled.

use multiview_core::time::MediaTime;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// One parsed subtitle cue: its on-screen window `[start, end)` and the
/// displayable text lines (markup stripped). Times are absolute on the media
/// timeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cue {
    /// When the cue becomes visible (inclusive).
    pub start: MediaTime,
    /// When the cue stops being visible (exclusive).
    pub end: MediaTime,
    /// The displayable lines, top to bottom (already markup-stripped).
    pub lines: Vec<String>,
}

impl Cue {
    /// Whether the cue is visible at media time `now` (`start <= now < end`).
    #[must_use]
    pub fn is_active_at(&self, now: MediaTime) -> bool {
        now.as_nanos() >= self.start.as_nanos() && now.as_nanos() < self.end.as_nanos()
    }

    /// The cue text as a single newline-joined string.
    #[must_use]
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }
}

/// Which timed-text format a [`CueTrack`] was parsed from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SubtitleFormat {
    /// `SubRip` (`.srt`): comma-separated millisecond fraction.
    SubRip,
    /// `WebVTT` (`.vtt`): dot-separated millisecond fraction, `WEBVTT` header.
    WebVtt,
}

/// A pure, time-ordered set of subtitle cues with an active-cue lookup.
///
/// Parse one with [`CueTrack::parse_srt`] / [`CueTrack::parse_vtt`]; query the
/// cue visible at a given media time with [`CueTrack::active_cue`]. Cues are
/// sorted by start time so the lookup is a simple scan (cue counts are small).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CueTrack {
    format: SubtitleFormat,
    cues: Vec<Cue>,
}

impl CueTrack {
    /// Parse a `SubRip` (`.srt`) document into a cue track.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidSubtitle`] if a block has a malformed timing line
    /// or an out-of-order `end <= start` window.
    pub fn parse_srt(input: &str) -> Result<Self> {
        Self::parse(input, SubtitleFormat::SubRip)
    }

    /// Parse a `WebVTT` (`.vtt`) document into a cue track.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidSubtitle`] if a block has a malformed timing line
    /// or an out-of-order `end <= start` window.
    pub fn parse_vtt(input: &str) -> Result<Self> {
        Self::parse(input, SubtitleFormat::WebVtt)
    }

    /// The format this track was parsed from.
    #[must_use]
    pub const fn format(&self) -> SubtitleFormat {
        self.format
    }

    /// The parsed cues, ordered by start time.
    #[must_use]
    pub fn cues(&self) -> &[Cue] {
        &self.cues
    }

    /// The number of cues in the track.
    #[must_use]
    pub fn len(&self) -> usize {
        self.cues.len()
    }

    /// Whether the track has no cues.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cues.is_empty()
    }

    /// The cue visible at media time `now`, or [`None`] if none is active.
    ///
    /// When overlapping cues exist (allowed by both formats) the **last** one to
    /// start wins, matching the conventional "newest caption on top" behaviour.
    #[must_use]
    pub fn active_cue(&self, now: MediaTime) -> Option<&Cue> {
        self.cues.iter().rev().find(|cue| cue.is_active_at(now))
    }

    /// Shared SRT/VTT parser: split into blocks, parse the timing line, collect
    /// the text lines, and sort by start time.
    fn parse(input: &str, format: SubtitleFormat) -> Result<Self> {
        let mut cues = Vec::new();
        for block in split_blocks(input) {
            if let Some(cue) = parse_block(&block, format)? {
                cues.push(cue);
            }
        }
        cues.sort_by_key(|c| c.start.as_nanos());
        Ok(Self { format, cues })
    }
}

/// Split a document into blocks separated by one or more blank lines. A leading
/// `WEBVTT` header line (with optional trailing metadata) and `NOTE`/`STYLE`
/// blocks are returned too — [`parse_block`] ignores blocks without a timing
/// line, so they fall away without special-casing here.
fn split_blocks(input: &str) -> Vec<Vec<String>> {
    let mut blocks = Vec::new();
    let mut current: Vec<String> = Vec::new();
    for raw in input.lines() {
        let line = raw.trim_end_matches(['\r', '\u{feff}']);
        if line.trim().is_empty() {
            if !current.is_empty() {
                blocks.push(std::mem::take(&mut current));
            }
        } else {
            current.push(line.to_owned());
        }
    }
    if !current.is_empty() {
        blocks.push(current);
    }
    blocks
}

/// Parse one block into a [`Cue`], or [`None`] if it has no timing line (header,
/// `NOTE`, `STYLE`, stray index, etc).
fn parse_block(lines: &[String], format: SubtitleFormat) -> Result<Option<Cue>> {
    // The timing line is the first line containing the `-->` arrow. Anything
    // before it (an SRT index, a VTT cue identifier) is ignored; everything
    // after it is the cue text.
    let Some(timing_idx) = lines.iter().position(|l| l.contains("-->")) else {
        return Ok(None);
    };
    let timing = lines.get(timing_idx).map_or("", String::as_str);
    let (start, end) = parse_timing(timing, format)?;
    if end.as_nanos() <= start.as_nanos() {
        return Err(Error::InvalidSubtitle(format!(
            "cue end {} is not after start {}",
            end.as_nanos(),
            start.as_nanos()
        )));
    }
    let text_lines: Vec<String> = lines
        .iter()
        .skip(timing_idx.saturating_add(1))
        .map(|l| strip_markup(l))
        .filter(|l| !l.is_empty())
        .collect();
    if text_lines.is_empty() {
        return Ok(None);
    }
    Ok(Some(Cue {
        start,
        end,
        lines: text_lines,
    }))
}

/// Parse a `start --> end` timing line into a pair of [`MediaTime`]s. `WebVTT`
/// timing lines may carry trailing cue settings after the end timestamp
/// (`position:..`, `align:..`); they are ignored here.
fn parse_timing(line: &str, format: SubtitleFormat) -> Result<(MediaTime, MediaTime)> {
    let mut parts = line.split("-->");
    let start_str = parts
        .next()
        .map(str::trim)
        .ok_or_else(|| Error::InvalidSubtitle(format!("missing start in timing line {line:?}")))?;
    let end_field = parts
        .next()
        .map(str::trim)
        .ok_or_else(|| Error::InvalidSubtitle(format!("missing end in timing line {line:?}")))?;
    // Drop any trailing VTT cue settings after the end timestamp.
    let end_str = end_field.split_whitespace().next().unwrap_or(end_field);
    let start = parse_timestamp(start_str, format)?;
    let end = parse_timestamp(end_str, format)?;
    Ok((start, end))
}

/// Parse a single `HH:MM:SS,mmm` (SRT) or `HH:MM:SS.mmm` / `MM:SS.mmm` (VTT)
/// timestamp into a [`MediaTime`]. The fraction separator differs by format but
/// both are accepted leniently (either `,` or `.`), so a mislabelled file still
/// parses.
fn parse_timestamp(field: &str, _format: SubtitleFormat) -> Result<MediaTime> {
    let bad = || Error::InvalidSubtitle(format!("malformed timestamp {field:?}"));
    // Split the fractional milliseconds off (either separator).
    let (hms, millis) = match field.split_once([',', '.']) {
        Some((h, m)) => (h, m),
        None => (field, "0"),
    };
    let millis: i64 = parse_fraction_millis(millis).ok_or_else(bad)?;
    // HH:MM:SS or MM:SS (VTT permits the hours to be omitted).
    let mut secs: i64 = 0;
    let mut count = 0_u32;
    for component in hms.split(':') {
        let value: i64 = component.trim().parse().map_err(|_| bad())?;
        secs = secs.saturating_mul(60).saturating_add(value);
        count = count.saturating_add(1);
    }
    if count == 0 || count > 3 {
        return Err(bad());
    }
    let total_ns = secs
        .saturating_mul(1_000)
        .saturating_add(millis)
        .saturating_mul(1_000_000);
    Ok(MediaTime::from_nanos(total_ns))
}

/// Parse a millisecond fraction (`"500"` → 500, `"5"` → 500, `"50"` → 500),
/// normalizing to thousandths regardless of how many digits are written.
fn parse_fraction_millis(frac: &str) -> Option<i64> {
    let digits: String = frac
        .trim()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    if digits.is_empty() {
        return Some(0);
    }
    // Pad/truncate to exactly three digits (milliseconds).
    let mut padded = digits;
    while padded.len() < 3 {
        padded.push('0');
    }
    let three = padded.get(0..3)?;
    three.parse().ok()
}

/// Strip the small inline markup both formats permit down to plain display text:
/// SRT font/bold/italic tags (`<b>`, `<i>`, `<font ...>`) and VTT cue tags
/// (`<c.classname>`, `<v Speaker>`, timestamps `<00:00:01.000>`). Everything
/// between `<` and the matching `>` is removed; the remaining text is trimmed.
fn strip_markup(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_tag = false;
    for ch in line.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.trim().to_owned()
}
