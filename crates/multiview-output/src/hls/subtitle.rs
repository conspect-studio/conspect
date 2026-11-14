//! `WebVTT` subtitle renditions for the TS-segment HLS sink (slice F #345,
//! ADR-0110 §12.3).
//!
//! Pure Rust (no native dependency), compiled in every build:
//!
//! - [`subtitle_feed`] — the bounded cue feed between the engine's off-clock
//!   cue sampler and the sink thread. The sender side only ever `try_send`s
//!   (drop-and-count, never blocks: invariants #1/#10); the sink drains the
//!   receiver non-blocking at every segment close.
//! - [`SubtitleCue`] — one closed cue on the **output timeline** (tick-derived
//!   ns, the same timeline as the video packets' PTS).
//! - [`SlotCues`] — the bounded per-slot cue holder the sink keeps between
//!   segment closes.
//! - [`bound_cue_lines`] — the compact, bounded text projection every cue
//!   carries (≤ [`MAX_CUE_LINES`] lines, ≤ [`MAX_CUE_TEXT_BYTES`] bytes, fresh
//!   exact-size allocations), applied by the sampler before it retains,
//!   compares or enqueues text and again (defensively) at the feed boundary.
//! - [`render_vtt`] — one `.vtt` segment: the `X-TIMESTAMP-MAP` header and the
//!   cues overlapping the segment, clipped and made relative to its start.
//! - [`unique_rendition_names`] — distinct `NAME`s for the `subs` group.
//! - [`first_video_pts_90k`] — the 90 kHz PTS the first video PES of a written
//!   MPEG-TS segment actually carries (the `MPEGTS` side of the map).

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};

use multiview_core::time::MediaTime;

/// Capacity of the engine → sink cue feed. Cues are change-driven (a few per
/// second at most per slot), so a full feed means the sink thread is wedged:
/// newer cues are dropped and counted, the engine never waits.
pub const SUBTITLE_FEED_CAPACITY: usize = 256;
/// Most cues a slot holds between segment closes; the oldest-ended is dropped
/// (and counted) beyond it.
pub const MAX_HELD_CUES_PER_SLOT: usize = 64;
/// Longest cue text kept, in bytes (all lines with their separators).
pub const MAX_CUE_TEXT_BYTES: usize = 512;
/// Most lines a cue keeps.
pub const MAX_CUE_LINES: usize = 4;

/// A cue's display lines after [`bound_cue_lines`] (the only constructor, so
/// the bound is an invariant of the type).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BoundedLines {
    /// At most [`MAX_CUE_LINES`] non-blank lines, at most
    /// [`MAX_CUE_TEXT_BYTES`] bytes with their separators; every `String` and
    /// the `Vec` are fresh allocations of exactly the kept size.
    lines: Vec<String>,
    /// Whether anything was cut.
    truncated: bool,
}

impl BoundedLines {
    /// The bounded lines.
    #[must_use]
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// The bounded lines, by value.
    #[must_use]
    pub fn into_lines(self) -> Vec<String> {
        self.lines
    }

    /// Whether the projection cut anything.
    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.truncated
    }

    /// Whether no line is left.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

/// Project `lines` onto a compact, bounded copy: blank lines dropped, at most
/// [`MAX_CUE_LINES`] lines and [`MAX_CUE_TEXT_BYTES`] bytes (one byte per
/// separating newline), cut on a UTF-8 char boundary. Only the kept bytes are
/// copied (never the source allocation), so its work and memory are bounded
/// whatever the source size.
#[must_use]
pub fn bound_cue_lines<S: AsRef<str>>(lines: &[S]) -> BoundedLines {
    let mut kept: Vec<&str> = Vec::with_capacity(MAX_CUE_LINES);
    let mut truncated = false;
    let mut budget = MAX_CUE_TEXT_BYTES;
    for line in lines.iter().map(AsRef::as_ref) {
        if line.trim().is_empty() {
            continue;
        }
        if kept.len() == MAX_CUE_LINES || budget == 0 {
            truncated = true;
            break;
        }
        let mut at = line.len().min(budget);
        while !line.is_char_boundary(at) {
            at -= 1;
        }
        if at < line.len() {
            truncated = true;
        }
        let part = &line[..at];
        if part.is_empty() {
            break;
        }
        // One byte for the newline separating it from the next line.
        budget = budget.saturating_sub(part.len() + 1);
        kept.push(part);
    }
    let mut out = Vec::with_capacity(kept.len());
    out.extend(kept.into_iter().map(String::from));
    BoundedLines {
        lines: out,
        truncated,
    }
}

/// Whether `lines` already satisfy [`bound_cue_lines`]'s bounds, including
/// allocation capacity (so a payload retaining a large source allocation is
/// re-projected).
fn is_compact(lines: &[String], capacity: usize) -> bool {
    capacity <= MAX_CUE_LINES
        && lines.len() <= MAX_CUE_LINES
        && lines
            .iter()
            .map(|l| l.len() + 1)
            .sum::<usize>()
            .saturating_sub(1)
            <= MAX_CUE_TEXT_BYTES
        && lines
            .iter()
            .all(|l| l.capacity() <= MAX_CUE_TEXT_BYTES && !l.trim().is_empty())
}

/// One subtitle rendition the master advertises (one per subtitle slot).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleRendition {
    /// Player menu label.
    pub name: String,
    /// Optional BCP-47 language tag.
    pub language: Option<String>,
}

/// Distinct player-menu labels for `renditions`, in order (RFC 8216
/// §4.3.4.1.1: every member of a group has a different `NAME`). Config
/// validation already refuses duplicates; this is the defence for anything
/// that reaches the master unvalidated: a repeated label gets ` (i+1)` (and
/// more ` (i+1)` suffixes until unique) — deterministic, first one kept.
#[must_use]
pub fn unique_rendition_names(renditions: &[SubtitleRendition]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(renditions.len());
    for (i, rendition) in renditions.iter().enumerate() {
        let mut name = rendition.name.clone();
        while out.contains(&name) {
            name = format!("{name} ({})", i + 1);
        }
        out.push(name);
    }
    out
}

/// One cue on the output timeline (tick-derived ns).
///
/// The engine sends a cue twice: **open** when it starts (`end` =
/// [`SubtitleCue::OPEN_END`]) and **closed** when it changes (the real end).
/// The sink merges the two by `(slot, start)`, so a cue that spans a segment
/// boundary is written into every segment it covers (an open cue is clipped
/// to the closing segment's end and kept).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleCue {
    /// The subtitle slot (index into the sink's renditions).
    pub slot: u8,
    /// First output instant the cue is shown (inclusive).
    pub start: MediaTime,
    /// Output instant the cue stops being shown (exclusive).
    pub end: MediaTime,
    /// Display lines, top to bottom.
    pub lines: Vec<String>,
}

impl SubtitleCue {
    /// The `end` of a cue that has started and not ended yet.
    pub const OPEN_END: MediaTime = MediaTime::from_nanos(i64::MAX);

    /// Whether the cue is still open.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.end == Self::OPEN_END
    }
}

/// Counters of every subtitle drop, shared by the feed's two ends.
#[derive(Debug, Default)]
pub struct SubtitleStats {
    feed_dropped: AtomicU64,
    held_dropped: AtomicU64,
    truncated: AtomicU64,
    written: AtomicU64,
}

impl SubtitleStats {
    /// Cues refused because the feed was full (or the sink gone).
    #[must_use]
    pub fn feed_dropped(&self) -> u64 {
        self.feed_dropped.load(Ordering::Relaxed)
    }

    /// Cues dropped because a slot already held [`MAX_HELD_CUES_PER_SLOT`].
    #[must_use]
    pub fn held_dropped(&self) -> u64 {
        self.held_dropped.load(Ordering::Relaxed)
    }

    /// Cues whose text was cut to [`MAX_CUE_LINES`] / [`MAX_CUE_TEXT_BYTES`].
    #[must_use]
    pub fn truncated(&self) -> u64 {
        self.truncated.load(Ordering::Relaxed)
    }

    /// `.vtt` segments written.
    #[must_use]
    pub fn segments_written(&self) -> u64 {
        self.written.load(Ordering::Relaxed)
    }

    #[cfg_attr(not(feature = "ffmpeg"), allow(dead_code))]
    pub(crate) fn note_segment_written(&self) {
        self.written.fetch_add(1, Ordering::Relaxed);
    }
}

/// The engine side of the cue feed: `try_send` only, never blocks.
#[derive(Debug, Clone)]
pub struct SubtitleFeed {
    tx: SyncSender<SubtitleCue>,
    stats: Arc<SubtitleStats>,
}

impl SubtitleFeed {
    /// Offer one cue. Returns whether it was queued; a full feed (or a sink
    /// that already stopped) drops it and counts the drop. The text is
    /// bounded first (see [`SubtitleFeed::try_offer`]).
    #[allow(clippy::must_use_candidate)]
    // reason: the caller may ignore the outcome; the drop is counted.
    pub fn offer(&self, cue: SubtitleCue) -> bool {
        if self.try_offer(cue).is_ok() {
            true
        } else {
            self.note_dropped();
            false
        }
    }

    /// Offer one cue without dropping it: a full feed (or a stopped sink)
    /// hands it back so the caller can retry on a later tick (never blocks;
    /// nothing is counted). The feed boundary enforces the text bound
    /// defensively: a payload that is not already compact is re-projected
    /// through [`bound_cue_lines`] (a fresh allocation; truncation counted)
    /// before it is queued, so no queued cue retains an oversized allocation.
    ///
    /// # Errors
    /// The cue, when the feed is full or the sink is gone.
    pub fn try_offer(&self, mut cue: SubtitleCue) -> Result<(), SubtitleCue> {
        if !is_compact(&cue.lines, cue.lines.capacity()) {
            let bounded = bound_cue_lines(&cue.lines);
            if bounded.truncated {
                self.note_truncated();
            }
            cue.lines = bounded.lines;
        }
        match self.tx.try_send(cue) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(cue) | TrySendError::Disconnected(cue)) => Err(cue),
        }
    }

    /// Count one cue dropped on the engine side (a refused or superseded
    /// pending cue).
    pub fn note_dropped(&self) {
        self.stats.feed_dropped.fetch_add(1, Ordering::Relaxed);
    }

    /// Count one cue whose text was cut by [`bound_cue_lines`].
    pub fn note_truncated(&self) {
        self.stats.truncated.fetch_add(1, Ordering::Relaxed);
    }

    /// The shared drop counters.
    #[must_use]
    pub fn stats(&self) -> Arc<SubtitleStats> {
        Arc::clone(&self.stats)
    }
}

/// The sink side of the cue feed, drained non-blocking on the sink thread.
#[derive(Debug)]
pub struct SubtitleFeedReceiver {
    // A `Mutex` only so the sink holding it stays `Sync`; the one consumer is
    // the sink thread, so it is never contended.
    rx: Mutex<Receiver<SubtitleCue>>,
    stats: Arc<SubtitleStats>,
}

impl SubtitleFeedReceiver {
    /// Every cue queued so far (non-blocking).
    #[must_use]
    pub fn drain(&self) -> Vec<SubtitleCue> {
        let rx = self.rx.lock().unwrap_or_else(PoisonError::into_inner);
        rx.try_iter().collect()
    }

    /// The shared drop counters.
    #[must_use]
    pub fn stats(&self) -> Arc<SubtitleStats> {
        Arc::clone(&self.stats)
    }
}

/// A bounded cue feed of [`SUBTITLE_FEED_CAPACITY`].
#[must_use]
pub fn subtitle_feed() -> (SubtitleFeed, SubtitleFeedReceiver) {
    subtitle_feed_with_capacity(SUBTITLE_FEED_CAPACITY)
}

/// A bounded cue feed of `capacity` (tests use a small one).
#[must_use]
pub fn subtitle_feed_with_capacity(capacity: usize) -> (SubtitleFeed, SubtitleFeedReceiver) {
    let (tx, rx) = std::sync::mpsc::sync_channel(capacity);
    let stats = Arc::new(SubtitleStats::default());
    (
        SubtitleFeed {
            tx,
            stats: Arc::clone(&stats),
        },
        SubtitleFeedReceiver {
            rx: Mutex::new(rx),
            stats,
        },
    )
}

/// The cues one slot holds between segment closes, bounded.
#[derive(Debug, Default)]
pub struct SlotCues {
    cues: VecDeque<SubtitleCue>,
}

impl SlotCues {
    /// Hold `cue` (its text bounded); beyond [`MAX_HELD_CUES_PER_SLOT`] the
    /// cue that ends first is dropped and counted. A cue with the start of a
    /// held open cue replaces it (its close); any other held open cue is
    /// closed at this cue's start (a lost close never shows a cue forever
    /// past its successor).
    pub fn push(&mut self, mut cue: SubtitleCue, stats: &SubtitleStats) {
        if cue.end <= cue.start {
            // An empty close still retires its open cue.
            self.cues.retain(|c| !(c.is_open() && c.start == cue.start));
            return;
        }
        self.cues.retain(|c| !(c.is_open() && c.start == cue.start));
        for held in &mut self.cues {
            if held.is_open() && held.start < cue.start {
                held.end = cue.start;
            }
        }
        if !is_compact(&cue.lines, cue.lines.capacity()) {
            let bounded = bound_cue_lines(&cue.lines);
            if bounded.truncated {
                stats.truncated.fetch_add(1, Ordering::Relaxed);
            }
            cue.lines = bounded.lines;
        }
        if cue.lines.is_empty() {
            return;
        }
        self.cues.push_back(cue);
        while self.cues.len() > MAX_HELD_CUES_PER_SLOT {
            let oldest = self
                .cues
                .iter()
                .enumerate()
                .min_by_key(|(_, c)| c.end)
                .map(|(i, _)| i);
            if let Some(i) = oldest {
                self.cues.remove(i);
                stats.held_dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// The cues overlapping `[start, end)`, in start order.
    #[must_use]
    pub fn overlapping(&self, start: MediaTime, end: MediaTime) -> Vec<&SubtitleCue> {
        let mut out: Vec<&SubtitleCue> = self
            .cues
            .iter()
            .filter(|c| c.start < end && c.end > start)
            .collect();
        out.sort_by_key(|c| (c.start, c.end));
        out
    }

    /// Forget every cue that ended at or before `edge` (already written).
    pub fn retire_before(&mut self, edge: MediaTime) {
        self.cues.retain(|c| c.end > edge);
    }

    /// Number of cues held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.cues.len()
    }

    /// Whether no cue is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cues.is_empty()
    }
}

/// `HH:MM:SS.mmm` of a non-negative span in ns.
fn vtt_time(ns: i64) -> String {
    let ms = ns.max(0) / 1_000_000;
    let (h, rest) = (ms / 3_600_000, ms % 3_600_000);
    let (m, rest) = (rest / 60_000, rest % 60_000);
    let (s, ms) = (rest / 1000, rest % 1000);
    format!("{h:02}:{m:02}:{s:02}.{ms:03}")
}

/// Escape cue text for `WebVTT` (`&`, `<`, `>`); a `-->` sequence cannot
/// survive the `>` escape.
fn escape_cue_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\r' | '\n' => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// The 33-bit 90 kHz value of an output instant (fallback map when the
/// written segment cannot be read).
#[must_use]
pub fn pts_90k_of(at: MediaTime) -> u64 {
    let ticks = multiview_core::time::rescale(
        at.as_nanos(),
        multiview_core::time::Rational::new(1, 1_000_000_000),
        multiview_core::time::Rational::new(1, 90_000),
    );
    u64::try_from(ticks.rem_euclid(1 << 33)).unwrap_or(0)
}

/// Render one `.vtt` segment for `[seg_start, seg_end)`: the header with
/// `X-TIMESTAMP-MAP=MPEGTS:<mpegts>,LOCAL:00:00:00.000` (LOCAL zero = the
/// segment start), then every cue overlapping the segment, clipped to it,
/// with times relative to its start. No cue ⇒ a header-only (valid) file.
#[must_use]
pub fn render_vtt(
    cues: &[&SubtitleCue],
    seg_start: MediaTime,
    seg_end: MediaTime,
    mpegts: u64,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "WEBVTT");
    let _ = writeln!(out, "X-TIMESTAMP-MAP=MPEGTS:{mpegts},LOCAL:00:00:00.000");
    let span = seg_end.as_nanos().saturating_sub(seg_start.as_nanos());
    for cue in cues {
        let start = cue
            .start
            .as_nanos()
            .saturating_sub(seg_start.as_nanos())
            .max(0);
        let end = cue
            .end
            .as_nanos()
            .saturating_sub(seg_start.as_nanos())
            .min(span);
        if end <= start || cue.lines.is_empty() {
            continue;
        }
        let _ = writeln!(out);
        let _ = writeln!(out, "{} --> {}", vtt_time(start), vtt_time(end));
        for line in &cue.lines {
            let _ = writeln!(out, "{}", escape_cue_text(line));
        }
    }
    out
}

/// The PTS (33-bit, 90 kHz) the first video PES of an MPEG-TS byte stream
/// carries: the first payload-unit-start packet whose PES `stream_id` is a
/// video stream (`0xE0..=0xEF`) with a PTS. `None` if there is none.
#[must_use]
pub fn first_video_pts_90k(ts: &[u8]) -> Option<u64> {
    for packet in ts.chunks_exact(188) {
        let header = packet.get(..4)?;
        if header.first() != Some(&0x47) {
            return None;
        }
        let flags = *header.get(1)?;
        let control = *header.get(3)?;
        let pusi = flags & 0x40 != 0;
        if !pusi || control & 0x10 == 0 {
            continue;
        }
        let mut at = 4usize;
        if control & 0x20 != 0 {
            at = at.saturating_add(1 + usize::from(*packet.get(4)?));
        }
        let Some(pes) = packet.get(at..) else {
            continue;
        };
        let (Some(prefix), Some(&stream_id), Some(&pts_flags)) =
            (pes.get(..3), pes.get(3), pes.get(7))
        else {
            continue;
        };
        if prefix != [0, 0, 1] || !(0xE0..=0xEF).contains(&stream_id) || pts_flags & 0x80 == 0 {
            continue;
        }
        let b = pes.get(9..14)?;
        let (b0, b1, b2, b3, b4) = (
            u64::from(*b.first()?),
            u64::from(*b.get(1)?),
            u64::from(*b.get(2)?),
            u64::from(*b.get(3)?),
            u64::from(*b.get(4)?),
        );
        let pts = ((b0 >> 1) & 0x07) << 30 | b1 << 22 | (b2 >> 1) << 15 | b3 << 7 | (b4 >> 1);
        return Some(pts);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(v: i64) -> MediaTime {
        MediaTime::from_nanos(v * 1_000_000)
    }

    fn cue(slot: u8, start: i64, end: i64, lines: &[&str]) -> SubtitleCue {
        SubtitleCue {
            slot,
            start: ms(start),
            end: ms(end),
            lines: lines.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn vtt_header_escaping_and_relative_times() {
        let a = cue(0, 2_500, 3_000, &["Tom & <Jerry>", "a --> b"]);
        let text = render_vtt(&[&a], ms(2_000), ms(4_000), 900_000);
        assert_eq!(
            text,
            "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:900000,LOCAL:00:00:00.000\n\n\
             00:00:00.500 --> 00:00:01.000\nTom &amp; &lt;Jerry&gt;\na --&gt; b\n"
        );
    }

    #[test]
    fn vtt_clips_cues_to_the_segment() {
        let before = cue(0, 1_000, 2_500, &["spans in"]);
        let after = cue(0, 3_500, 9_000, &["spans out"]);
        let text = render_vtt(&[&before, &after], ms(2_000), ms(4_000), 0);
        assert!(
            text.contains("00:00:00.000 --> 00:00:00.500\nspans in\n"),
            "{text}"
        );
        assert!(
            text.contains("00:00:01.500 --> 00:00:02.000\nspans out\n"),
            "{text}"
        );
        // Hours render too.
        assert_eq!(vtt_time(3_723_004_000_000), "01:02:03.004");
    }

    #[test]
    fn empty_segment_is_a_valid_header_only_file() {
        let text = render_vtt(&[], ms(0), ms(2_000), 126_000);
        assert_eq!(
            text,
            "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:126000,LOCAL:00:00:00.000\n"
        );
    }

    #[test]
    fn overlap_selection_and_retirement() {
        let stats = SubtitleStats::default();
        let mut held = SlotCues::default();
        held.push(cue(0, 0, 1_000, &["a"]), &stats);
        held.push(cue(0, 1_500, 2_500, &["b"]), &stats);
        held.push(cue(0, 3_000, 4_000, &["c"]), &stats);
        let seg: Vec<_> = held
            .overlapping(ms(1_000), ms(2_000))
            .iter()
            .map(|c| c.lines[0].clone())
            .collect();
        assert_eq!(seg, ["b"]);
        held.retire_before(ms(2_000));
        assert_eq!(held.len(), 2, "b straddles the edge and stays");
    }

    #[test]
    fn an_open_cue_spans_segments_until_its_close_replaces_it() {
        let stats = SubtitleStats::default();
        let mut open = cue(0, 500, 0, &["long"]);
        open.end = SubtitleCue::OPEN_END;
        let mut held = SlotCues::default();
        held.push(open.clone(), &stats);
        // Segment [0, 1000) closes with the cue still open: clipped to it.
        let text = render_vtt(&held.overlapping(ms(0), ms(1_000)), ms(0), ms(1_000), 0);
        assert!(
            text.contains("00:00:00.500 --> 00:00:01.000\nlong\n"),
            "{text}"
        );
        held.retire_before(ms(1_000));
        assert_eq!(held.len(), 1, "an open cue is kept");
        // Its close arrives: the closed cue replaces the open one.
        held.push(cue(0, 500, 1_200, &["long"]), &stats);
        assert_eq!(held.len(), 1);
        let text = render_vtt(
            &held.overlapping(ms(1_000), ms(2_000)),
            ms(1_000),
            ms(2_000),
            0,
        );
        assert!(
            text.contains("00:00:00.000 --> 00:00:00.200\nlong\n"),
            "{text}"
        );
        // A lost close: the next cue closes the open one at its start.
        held = SlotCues::default();
        held.push(open, &stats);
        held.push(cue(0, 1_500, 1_800, &["next"]), &stats);
        let all = held.overlapping(ms(0), ms(3_000));
        assert_eq!(all[0].end, ms(1_500));
    }

    #[test]
    fn held_cues_are_bounded_dropping_the_oldest_ended() {
        let stats = SubtitleStats::default();
        let mut held = SlotCues::default();
        for i in 0..70 {
            held.push(cue(0, i * 10, i * 10 + 5, &["x"]), &stats);
        }
        assert_eq!(held.len(), MAX_HELD_CUES_PER_SLOT);
        assert_eq!(stats.held_dropped(), 6);
        // The six that ended first are gone.
        assert_eq!(held.overlapping(ms(0), ms(60)).len(), 0);
    }

    #[test]
    fn cue_text_is_bounded() {
        let stats = SubtitleStats::default();
        let mut held = SlotCues::default();
        held.push(cue(0, 0, 10, &["1", "2", "3", "4", "5"]), &stats);
        let long = "é".repeat(400); // 800 bytes
        held.push(cue(0, 20, 30, &[long.as_str()]), &stats);
        assert_eq!(stats.truncated(), 2);
        let all = held.overlapping(ms(0), ms(100));
        assert_eq!(all[0].lines.len(), MAX_CUE_LINES);
        assert!(all[1].lines[0].len() <= MAX_CUE_TEXT_BYTES);
        assert!(all[1].lines[0].chars().all(|c| c == 'é'));
    }

    #[test]
    fn a_full_feed_drops_and_counts_without_blocking() {
        let (feed, rx) = subtitle_feed_with_capacity(2);
        assert!(feed.offer(cue(0, 0, 1, &["a"])));
        assert!(feed.offer(cue(0, 1, 2, &["b"])));
        assert!(!feed.offer(cue(0, 2, 3, &["c"])));
        assert_eq!(feed.stats().feed_dropped(), 1);
        assert_eq!(rx.drain().len(), 2);
        assert_eq!(rx.drain().len(), 0);
        drop(rx);
        assert!(!feed.offer(cue(0, 3, 4, &["d"])));
        assert_eq!(feed.stats().feed_dropped(), 2);
    }

    #[test]
    fn first_video_pts_is_read_from_the_pes_header() {
        // One TS packet: PID 0x100, PUSI, payload only, PES video with PTS.
        let pts: u64 = 0x1_2345_6789 & ((1 << 33) - 1);
        let mut p = vec![0x47, 0x41, 0x00, 0x10];
        p.extend_from_slice(&[0, 0, 1, 0xE0, 0, 0, 0x80, 0x80, 5]);
        p.push(0x21 | u8::try_from((pts >> 29) & 0x0E).unwrap());
        p.push(u8::try_from((pts >> 22) & 0xFF).unwrap());
        p.push(0x01 | u8::try_from((pts >> 14) & 0xFE).unwrap());
        p.push(u8::try_from((pts >> 7) & 0xFF).unwrap());
        p.push(0x01 | u8::try_from((pts << 1) & 0xFE).unwrap());
        p.resize(188, 0xFF);
        assert_eq!(first_video_pts_90k(&p), Some(pts));
        assert_eq!(first_video_pts_90k(&[]), None);
        assert_eq!(pts_90k_of(MediaTime::from_nanos(1_000_000_000)), 90_000);
    }

    #[test]
    fn oversized_text_is_projected_compactly_with_fresh_allocations() {
        // 100 lines of ~2.6 KiB each: ~256 KiB.
        let big: Vec<String> = (0..100)
            .map(|i| format!("{i}:{}", "é".repeat(1_300)))
            .collect();
        let bounded = bound_cue_lines(&big);
        assert!(bounded.truncated);
        assert!(bounded.lines.len() <= MAX_CUE_LINES);
        assert!(bounded.lines.capacity() <= MAX_CUE_LINES);
        let bytes: usize = bounded.lines.iter().map(|l| l.len() + 1).sum::<usize>() - 1;
        assert!(bytes <= MAX_CUE_TEXT_BYTES, "{bytes}");
        for line in &bounded.lines {
            assert!(line.capacity() <= MAX_CUE_TEXT_BYTES, "{}", line.capacity());
            assert!(line.capacity() == line.len(), "exact-size copy");
        }
        // Small text passes untouched; blank lines go.
        let small = bound_cue_lines(&["a", " ", "b"]);
        assert_eq!(small.lines, ["a", "b"]);
        assert!(!small.truncated);
    }

    #[test]
    fn the_feed_boundary_bounds_an_oversized_payload_and_the_held_copy() {
        let (feed, rx) = subtitle_feed_with_capacity(4);
        let mut huge = String::with_capacity(256 * 1024);
        huge.push_str(&"x".repeat(256 * 1024));
        let mut lines = Vec::with_capacity(100);
        lines.push(huge);
        lines.extend((0..99).map(|i| format!("line {i}")));
        assert!(feed.offer(SubtitleCue {
            slot: 0,
            start: ms(0),
            end: SubtitleCue::OPEN_END,
            lines,
        }));
        assert_eq!(feed.stats().truncated(), 1);
        let queued = rx.drain();
        assert_eq!(queued.len(), 1);
        assert!(queued[0].lines.capacity() <= MAX_CUE_LINES);
        assert!(queued[0]
            .lines
            .iter()
            .all(|l| l.capacity() <= MAX_CUE_TEXT_BYTES));
        // A held cue never keeps a large source allocation either.
        let stats = SubtitleStats::default();
        let mut held = SlotCues::default();
        let mut wide = String::with_capacity(256 * 1024);
        wide.push_str("short");
        held.push(
            SubtitleCue {
                slot: 0,
                start: ms(0),
                end: ms(10),
                lines: vec![wide],
            },
            &stats,
        );
        let all = held.overlapping(ms(0), ms(10));
        assert_eq!(all[0].lines, ["short"]);
        assert!(all[0].lines[0].capacity() <= MAX_CUE_TEXT_BYTES);
        assert_eq!(stats.truncated(), 0, "re-projected, not cut");
    }

    #[test]
    fn try_offer_hands_a_refused_cue_back_without_counting() {
        let (feed, rx) = subtitle_feed_with_capacity(1);
        assert!(feed.try_offer(cue(0, 0, 1, &["a"])).is_ok());
        let back = feed.try_offer(cue(0, 1, 2, &["b"])).unwrap_err();
        assert_eq!(back.lines, ["b"]);
        assert_eq!(feed.stats().feed_dropped(), 0);
        assert_eq!(rx.drain().len(), 1);
        assert!(feed.try_offer(back).is_ok());
    }

    #[test]
    fn rendition_names_are_made_unique_deterministically() {
        let r = |name: &str| SubtitleRendition {
            name: name.to_owned(),
            language: None,
        };
        let names = unique_rendition_names(&[r("en"), r("en"), r("de"), r("en (2)"), r("en")]);
        assert_eq!(names, ["en", "en (2)", "de", "en (2) (4)", "en (5)"]);
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len());
    }
}
