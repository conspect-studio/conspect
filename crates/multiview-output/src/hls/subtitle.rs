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
//!   cues [`SlotCues::publish`] chose, on the rendition's shared `WebVTT`
//!   timeline (RFC 8216 §3.5: a known cue carries its total display interval;
//!   a cue published while open becomes immutable, non-overlapping pieces).
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
/// Most source bytes [`bound_cue_lines`] examines (whitespace included), so
/// projecting a hostile cue costs a fixed amount of work per tick.
pub const MAX_INSPECTED_BYTES: usize = 4096;
/// Most source lines [`bound_cue_lines`] examines (blank lines included).
pub const MAX_INSPECTED_LINES: usize = 16;

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
    /// Source bytes examined (≤ [`MAX_INSPECTED_BYTES`]).
    inspected_bytes: usize,
    /// Source lines examined, blank ones included (≤ [`MAX_INSPECTED_LINES`]).
    inspected_lines: usize,
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

    /// Source bytes the projection examined (its work bound).
    #[must_use]
    pub const fn inspected_bytes(&self) -> usize {
        self.inspected_bytes
    }

    /// Source lines the projection examined, blank ones included.
    #[must_use]
    pub const fn inspected_lines(&self) -> usize {
        self.inspected_lines
    }
}

/// Project `lines` onto a compact, bounded copy: blank lines dropped, at most
/// [`MAX_CUE_LINES`] lines and [`MAX_CUE_TEXT_BYTES`] bytes (one byte per
/// separating newline), cut on a UTF-8 char boundary. Only the kept bytes are
/// copied (never the source allocation). The **work** is bounded too: at most
/// [`MAX_INSPECTED_LINES`] source lines and [`MAX_INSPECTED_BYTES`] source
/// bytes are examined (blank lines and whitespace included; the budget is
/// checked before each next line, and whitespace is classified only within
/// the inspected prefix), whatever the source size.
#[must_use]
pub fn bound_cue_lines<S: AsRef<str>>(lines: &[S]) -> BoundedLines {
    let mut kept: Vec<&str> = Vec::with_capacity(MAX_CUE_LINES);
    let mut truncated = false;
    let mut budget = MAX_CUE_TEXT_BYTES;
    let mut inspected_bytes = 0usize;
    let mut inspected_lines = 0usize;
    for line in lines.iter().map(AsRef::as_ref) {
        // The inspection budget is checked BEFORE looking at the next line
        // (INV2-1/ABU2-1): blank lines and whitespace consume it too, so the
        // work is bounded whatever the source holds.
        if kept.len() == MAX_CUE_LINES
            || budget == 0
            || inspected_lines == MAX_INSPECTED_LINES
            || inspected_bytes == MAX_INSPECTED_BYTES
        {
            truncated = true;
            break;
        }
        inspected_lines += 1;
        // Only a bounded, UTF-8-safe prefix of the line is ever examined.
        let mut look = line.len().min(MAX_INSPECTED_BYTES - inspected_bytes);
        while !line.is_char_boundary(look) {
            look -= 1;
        }
        let prefix = &line[..look];
        inspected_bytes += prefix.len();
        if prefix.trim().is_empty() {
            if prefix.len() < line.len() {
                // Content (if any) lies beyond what may be inspected.
                truncated = true;
            }
            continue;
        }
        let mut at = prefix.len().min(budget);
        while !prefix.is_char_boundary(at) {
            at -= 1;
        }
        if at < line.len() {
            truncated = true;
        }
        let part = &prefix[..at];
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
        inspected_bytes,
        inspected_lines,
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
/// boundary is written into every segment it covers: with its total display
/// interval when its end was known at its first publication, else as one
/// contiguous piece per segment (see [`SlotCues::publish`]).
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

/// One held cue plus whether a piece of it was already published while it
/// was still open (its end unknown).
#[derive(Debug)]
struct Held {
    cue: SubtitleCue,
    /// A segment carried a piece of this cue while it was open: from then on
    /// it is published as contiguous, non-overlapping per-segment pieces,
    /// never as one total interval (MED2-1).
    rolling: bool,
}

/// The cues one slot holds between segment closes, bounded.
#[derive(Debug, Default)]
pub struct SlotCues {
    cues: VecDeque<Held>,
}

impl SlotCues {
    /// Hold `cue` (its text bounded); beyond [`MAX_HELD_CUES_PER_SLOT`] the
    /// cue that ends first is dropped and counted. A cue with the start of a
    /// held open cue replaces it (its close; it inherits whether pieces of it
    /// were already published); any other held open cue is closed at this
    /// cue's start (a lost close never shows a cue forever past its
    /// successor).
    pub fn push(&mut self, mut cue: SubtitleCue, stats: &SubtitleStats) {
        let mut rolling = false;
        self.cues.retain(|h| {
            let replaced = h.cue.is_open() && h.cue.start == cue.start;
            rolling |= replaced && h.rolling;
            !replaced
        });
        if cue.end <= cue.start {
            // An empty close still retires its open cue.
            return;
        }
        for held in &mut self.cues {
            if held.cue.is_open() && held.cue.start < cue.start {
                held.cue.end = cue.start;
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
        self.cues.push_back(Held { cue, rolling });
        while self.cues.len() > MAX_HELD_CUES_PER_SLOT {
            let oldest = self
                .cues
                .iter()
                .enumerate()
                .min_by_key(|(_, h)| h.cue.end)
                .map(|(i, _)| i);
            if let Some(i) = oldest {
                self.cues.remove(i);
                stats.held_dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// The cues overlapping `[start, end)`, in start order (as held).
    #[must_use]
    pub fn overlapping(&self, start: MediaTime, end: MediaTime) -> Vec<&SubtitleCue> {
        let mut out: Vec<&SubtitleCue> = self
            .cues
            .iter()
            .map(|h| &h.cue)
            .filter(|c| c.start < end && c.end > start)
            .collect();
        out.sort_by_key(|c| (c.start, c.end));
        out
    }

    /// The cue intervals segment `[start, end)` publishes, in start order.
    /// Published segments are immutable and clients merge the cues of all
    /// loaded segments (hls.js de-duplicates only identical start/end/text),
    /// so every published interval must stay true forever and copies may
    /// never overlap:
    ///
    /// - a cue whose end is known and that was never published open carries
    ///   its **total** interval in every segment it overlaps (RFC 8216 §3.5;
    ///   identical copies, de-duplicated by the client);
    /// - a cue still open (end unknown) carries the piece
    ///   `[max(cue start, start), end)` and is marked rolling; from then on
    ///   every segment carries its piece `[max(cue start, start),
    ///   min(cue end, end))` — contiguous, non-overlapping pieces whose union
    ///   is the cue's display time, through its close.
    pub fn publish(&mut self, start: MediaTime, end: MediaTime) -> Vec<SubtitleCue> {
        let mut out = Vec::new();
        for held in &mut self.cues {
            let cue = &held.cue;
            if !(cue.start < end && cue.end > start) {
                continue;
            }
            if cue.is_open() {
                held.rolling = true;
            }
            let piece = if held.rolling {
                SubtitleCue {
                    slot: cue.slot,
                    start: cue.start.max(start),
                    end: cue.end.min(end),
                    lines: cue.lines.clone(),
                }
            } else {
                cue.clone()
            };
            out.push(piece);
        }
        out.sort_by_key(|c| (c.start, c.end));
        out
    }

    /// Forget every cue that ended at or before `edge` (already written).
    pub fn retire_before(&mut self, edge: MediaTime) {
        self.cues.retain(|h| h.cue.end > edge);
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

/// Render one `.vtt` segment for `[seg_start, seg_end)` of a rendition whose
/// shared `WebVTT` timeline starts at `base` (LOCAL zero = output instant
/// `base`, the rendition's first segment start; every segment of the
/// rendition uses it, so a cue has identical timestamps in every segment it
/// appears in).
///
/// The header maps the segment start: `X-TIMESTAMP-MAP=MPEGTS:<m>,LOCAL:<l>`
/// with `l = seg_start - base` cut to the millisecond and `m` = `mpegts` (the
/// 90 kHz PTS of `seg_start`) moved back by the sub-millisecond remainder,
/// modulo 2^33 — so the pair is exact and a 33-bit PTS wrap between segments
/// stays consistent (players resolve `MPEGTS` modulo 2^33).
///
/// Every cue given follows with the interval it carries, on that timeline
/// ([`SlotCues::publish`] decides it: a cue's total interval, RFC 8216 §3.5,
/// or a contiguous per-segment piece of a cue published while open). A cue
/// still marked open is written as the segment's piece `[max(start,
/// seg_start), seg_end)`. A cue that started before `base` (before this
/// rendition's first segment) starts at LOCAL zero. No cue ⇒ a header-only
/// (valid) file.
#[must_use]
pub fn render_vtt(
    cues: &[&SubtitleCue],
    seg_start: MediaTime,
    seg_end: MediaTime,
    base: MediaTime,
    mpegts: u64,
) -> String {
    let local = |at: MediaTime| at.as_nanos().saturating_sub(base.as_nanos()).max(0);
    let seg_local = local(seg_start);
    let remainder_ns = seg_local % 1_000_000;
    let shift = u64::try_from(multiview_core::time::rescale(
        remainder_ns,
        multiview_core::time::Rational::new(1, 1_000_000_000),
        multiview_core::time::Rational::new(1, 90_000),
    ))
    .unwrap_or(0);
    let wrap = 1u64 << 33;
    let mpegts = (mpegts % wrap + wrap - shift % wrap) % wrap;
    let mut out = String::new();
    let _ = writeln!(out, "WEBVTT");
    let _ = writeln!(
        out,
        "X-TIMESTAMP-MAP=MPEGTS:{mpegts},LOCAL:{}",
        vtt_time(seg_local - remainder_ns)
    );
    for cue in cues {
        // An open cue (end unknown) is only ever the segment's piece: it never
        // claims time before the segment (see `SlotCues::publish`).
        let (start, end) = if cue.is_open() {
            (local(cue.start.max(seg_start)), local(seg_end))
        } else {
            (local(cue.start), local(cue.end))
        };
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
        let text = render_vtt(&[&a], ms(2_000), ms(4_000), ms(2_000), 900_000);
        assert_eq!(
            text,
            "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:900000,LOCAL:00:00:00.000\n\n\
             00:00:00.500 --> 00:00:01.000\nTom &amp; &lt;Jerry&gt;\na --&gt; b\n"
        );
    }

    #[test]
    fn vtt_cues_carry_their_total_interval_on_the_shared_timeline() {
        // RFC 8216 §3.5: a cue overlapping the segment carries its TOTAL
        // display interval (never clipped), on the rendition's timeline
        // (LOCAL zero = the rendition's first segment start, here 0).
        let before = cue(0, 1_000, 2_500, &["spans in"]);
        let after = cue(0, 3_500, 9_000, &["spans out"]);
        let text = render_vtt(&[&before, &after], ms(2_000), ms(4_000), ms(0), 180_000);
        assert!(
            text.starts_with("WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:180000,LOCAL:00:00:02.000\n"),
            "{text}"
        );
        assert!(
            text.contains("00:00:01.000 --> 00:00:02.500\nspans in\n"),
            "{text}"
        );
        assert!(
            text.contains("00:00:03.500 --> 00:00:09.000\nspans out\n"),
            "{text}"
        );
        // Hours render too.
        assert_eq!(vtt_time(3_723_004_000_000), "01:02:03.004");
    }

    #[test]
    fn empty_segment_is_a_valid_header_only_file() {
        let text = render_vtt(&[], ms(0), ms(2_000), ms(0), 126_000);
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
        // Segment [0, 1000) closes with the cue still open (end unknown): it
        // is ended at the segment end ...
        let text = render_vtt(
            &held.overlapping(ms(0), ms(1_000)),
            ms(0),
            ms(1_000),
            ms(0),
            0,
        );
        assert!(
            text.contains("00:00:00.500 --> 00:00:01.000\nlong\n"),
            "{text}"
        );
        held.retire_before(ms(1_000));
        assert_eq!(held.len(), 1, "an open cue is kept");
        // ... and re-emitted with the same start, extended, while still open.
        let text = render_vtt(
            &held.overlapping(ms(1_000), ms(2_000)),
            ms(1_000),
            ms(2_000),
            ms(0),
            90_000,
        );
        assert!(
            text.contains("00:00:00.500 --> 00:00:02.000\nlong\n"),
            "{text}"
        );
        // Its close arrives: the closed cue replaces the open one and carries
        // its total interval (same start as in every earlier segment).
        held.push(cue(0, 500, 1_200, &["long"]), &stats);
        assert_eq!(held.len(), 1);
        let text = render_vtt(
            &held.overlapping(ms(1_000), ms(2_000)),
            ms(1_000),
            ms(2_000),
            ms(0),
            90_000,
        );
        assert!(
            text.contains("00:00:00.500 --> 00:00:01.200\nlong\n"),
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
    fn a_closed_cue_spanning_segments_has_identical_timestamps_in_each() {
        let long = cue(0, 1_500, 5_200, &["spans three"]);
        let base = ms(1_000);
        let mut lines = Vec::new();
        for (i, seg) in [(1_000, 3_000), (3_000, 5_000), (5_000, 7_000)]
            .into_iter()
            .enumerate()
        {
            let mpegts = 90_000 + 180_000 * u64::try_from(i).unwrap();
            let text = render_vtt(&[&long], ms(seg.0), ms(seg.1), base, mpegts);
            let local = vtt_time((seg.0 - 1_000) * 1_000_000);
            assert!(
                text.starts_with(&format!(
                    "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:{mpegts},LOCAL:{local}\n"
                )),
                "{text}"
            );
            let timing = text
                .lines()
                .find(|l| l.contains("-->"))
                .unwrap_or_else(|| panic!("{text}"))
                .to_owned();
            lines.push(timing);
        }
        assert_eq!(lines[0], "00:00:00.500 --> 00:00:04.200");
        assert!(lines.windows(2).all(|w| w[0] == w[1]), "{lines:?}");
    }

    #[test]
    fn the_map_stays_consistent_across_a_33_bit_pts_wrap() {
        let wrap = 1u64 << 33;
        // Segment A maps LOCAL 1.000 to just below the wrap; segment B (1 s
        // later) carries a wrapped PTS. Both maps describe one timeline:
        // MPEGTS - LOCAL*90k is the same modulo 2^33.
        let a_pts = wrap - 45_000;
        let b_pts = (a_pts + 90_000) % wrap;
        let header = |text: &str| -> (u64, i64) {
            let line = text.lines().nth(1).unwrap();
            let rest = line.strip_prefix("X-TIMESTAMP-MAP=MPEGTS:").unwrap();
            let (m, l) = rest.split_once(",LOCAL:").unwrap();
            let parts: Vec<i64> = l.split([':', '.']).map(|p| p.parse().unwrap()).collect();
            let ms = ((parts[0] * 60 + parts[1]) * 60 + parts[2]) * 1000 + parts[3];
            (m.parse().unwrap(), ms)
        };
        let c = cue(0, 1_500, 2_500, &["across the wrap"]);
        let a = render_vtt(&[&c], ms(2_000), ms(3_000), ms(1_000), a_pts);
        let b = render_vtt(&[&c], ms(3_000), ms(4_000), ms(1_000), b_pts);
        let (am, al) = header(&a);
        let (bm, bl) = header(&b);
        assert!(bm < am, "segment B's PTS wrapped: {am} {bm}");
        let origin = |m: u64, l: i64| {
            let l90 = u64::try_from(l).unwrap() * 90;
            (m + wrap - l90 % wrap) % wrap
        };
        assert_eq!(origin(am, al), origin(bm, bl));
        assert!(a.contains("00:00:00.500 --> 00:00:01.500\nacross the wrap\n"));
        assert!(b.contains("00:00:00.500 --> 00:00:01.500\nacross the wrap\n"));
        // A sub-millisecond segment start moves MPEGTS back by the remainder
        // (modulo 2^33), so the pair stays exact; at PTS 0 it wraps.
        let odd = render_vtt(
            &[],
            MediaTime::from_nanos(1_000_500_000),
            ms(2_000),
            ms(0),
            45,
        );
        assert!(
            odd.contains(&format!("MPEGTS:{},LOCAL:00:00:01.000", 0)),
            "{odd}"
        );
        let wrapped = render_vtt(
            &[],
            MediaTime::from_nanos(1_000_500_000),
            ms(2_000),
            ms(0),
            0,
        );
        assert!(
            wrapped.contains(&format!("MPEGTS:{},LOCAL:00:00:01.000", wrap - 45)),
            "{wrapped}"
        );
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

    /// Parse the cues of one rendered segment (on the shared timeline: every
    /// segment of a rendition maps the same origin), as a client does.
    fn client_cues(text: &str) -> Vec<(i64, i64, String)> {
        let time = |t: &str| -> i64 {
            let p: Vec<i64> = t.split([':', '.']).map(|x| x.parse().unwrap()).collect();
            ((p[0] * 60 + p[1]) * 60 + p[2]) * 1000 + p[3]
        };
        let mut out = Vec::new();
        let mut it = text.lines();
        while let Some(line) = it.next() {
            if let Some((a, b)) = line.split_once(" --> ") {
                out.push((time(a), time(b), it.next().unwrap().to_owned()));
            }
        }
        out
    }

    /// Run `events` (cue pushes keyed by the segment index before whose
    /// publication they arrive) through 1-second segments, load every
    /// segment's cues together de-duplicated by (start, end, text) as hls.js
    /// does, and return how many copies are active at each 10 ms instant.
    fn client_active_counts(events: &[(usize, SubtitleCue)], segments: usize) -> Vec<usize> {
        let stats = SubtitleStats::default();
        let mut held = SlotCues::default();
        let mut loaded: Vec<(i64, i64, String)> = Vec::new();
        for seg in 0..segments {
            for (_, cue) in events.iter().filter(|(at, _)| *at == seg) {
                held.push(cue.clone(), &stats);
            }
            let (start, end) = (
                ms(i64::try_from(seg).unwrap() * 1_000),
                ms(i64::try_from(seg + 1).unwrap() * 1_000),
            );
            let pieces = held.publish(start, end);
            let text = render_vtt(&pieces.iter().collect::<Vec<_>>(), start, end, ms(0), 0);
            for c in client_cues(&text) {
                if !loaded.contains(&c) {
                    loaded.push(c);
                }
            }
            held.retire_before(end);
        }
        (0..i64::try_from(segments).unwrap() * 100)
            .map(|t| {
                let at = t * 10;
                loaded
                    .iter()
                    .filter(|(a, b, _)| *a <= at && at < *b)
                    .count()
            })
            .collect()
    }

    #[test]
    fn a_client_loading_every_segment_sees_one_copy_of_a_rolling_cue() {
        // Open at 0.5 s (known at segment 0), still open through segments 0
        // and 1, closed at 2.4 s before segment 2 is published.
        let mut open = cue(0, 500, 0, &["rolling"]);
        open.end = SubtitleCue::OPEN_END;
        let events = [(0, open), (2, cue(0, 500, 2_400, &["rolling"]))];
        let counts = client_active_counts(&events, 4);
        for (i, n) in counts.iter().enumerate() {
            let at = i64::try_from(i).unwrap() * 10;
            let expected = usize::from((500..2_400).contains(&at));
            assert_eq!(*n, expected, "at {at} ms: {counts:?}");
        }
    }

    #[test]
    fn a_client_loading_every_segment_sees_one_copy_of_a_known_cue() {
        // Its end is known before its first publication: the total interval
        // in every segment it overlaps, identical copies de-duplicated.
        let events = [(0, cue(0, 500, 2_400, &["known"]))];
        let counts = client_active_counts(&events, 4);
        for (i, n) in counts.iter().enumerate() {
            let at = i64::try_from(i).unwrap() * 10;
            let expected = usize::from((500..2_400).contains(&at));
            assert_eq!(*n, expected, "at {at} ms: {counts:?}");
        }
        let stats = SubtitleStats::default();
        let mut held = SlotCues::default();
        held.push(cue(0, 500, 2_400, &["known"]), &stats);
        assert_eq!(
            held.publish(ms(1_000), ms(2_000))[0].start,
            ms(500),
            "total"
        );
    }

    #[test]
    fn projection_work_is_bounded_for_whitespace_heavy_text() {
        // 256 KiB of leading whitespace before one character.
        let padded = format!("{}x", " ".repeat(256 * 1024));
        let b = bound_cue_lines(&[padded.as_str()]);
        assert!(
            b.inspected_bytes() <= MAX_INSPECTED_BYTES,
            "{}",
            b.inspected_bytes()
        );
        assert!(b.is_empty() && b.truncated());
        // 100k blank lines before real text.
        let mut blanks = vec![String::new(); 100_000];
        blanks.push("late".to_owned());
        let b = bound_cue_lines(&blanks);
        assert!(
            b.inspected_lines() <= MAX_INSPECTED_LINES,
            "{}",
            b.inspected_lines()
        );
        assert!(b.is_empty() && b.truncated());
        // 1000 whitespace-only lines of 4 KiB each.
        let wide = vec![" ".repeat(4096); 1_000];
        let b = bound_cue_lines(&wide);
        assert!(b.inspected_bytes() <= MAX_INSPECTED_BYTES);
        assert!(b.inspected_lines() <= MAX_INSPECTED_LINES);
        // Huge whitespace suffixes: kept text is cut, inspection bounded.
        let suffixed: Vec<String> = (0..50)
            .map(|i| format!("line {i}{}", " ".repeat(100_000)))
            .collect();
        let b = bound_cue_lines(&suffixed);
        assert!(b.inspected_bytes() <= MAX_INSPECTED_BYTES);
        assert!(b.inspected_lines() <= MAX_INSPECTED_LINES);
        assert!(b.truncated());
        assert!(b.lines()[0].starts_with("line 0"));
        // Output budget exhausted, then a huge line: not scanned.
        let mut tail: Vec<String> = (0..4).map(|i| format!("l{i}")).collect();
        tail.push(" ".repeat(1 << 20));
        let b = bound_cue_lines(&tail);
        assert_eq!(b.lines(), ["l0", "l1", "l2", "l3"]);
        assert_eq!(b.inspected_lines(), 4);
        assert!(b.truncated());
        // Normal cues are unaffected.
        let b = bound_cue_lines(&["  Hello ", "", "world"]);
        assert_eq!(b.lines(), ["  Hello ", "world"]);
        assert!(!b.truncated());
        assert_eq!(b.inspected_lines(), 3);
    }
}
