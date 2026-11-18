//! Pure-Rust parsing of an HLS **`WebVTT` subtitle rendition** (issue #526).
//!
//! The isolated caption reader ([`crate::captions`]) polls the rendition's media
//! playlist and fetches its `.vtt` segments itself instead of handing the URL to
//! libav, because:
//!
//! * a minimal libav build (the deployed `--disable-everything` one) may lack the
//!   `webvtt` demuxer/decoder, which made every live rendition fail with
//!   "Invalid data found when processing input" and publish no cue;
//! * libav (7.x) ignores the segment's `X-TIMESTAMP-MAP` header, so it cannot put
//!   a cue on the MPEG-TS clock the video rides (RFC 8216 §3.5).
//!
//! This module is the parsing half: no I/O, no clock, every input bounded. The
//! reader loop and the mapping onto the video's timeline live in
//! [`crate::captions`].
//!
//! ## Timing
//!
//! A cue at local time `t` in a segment whose header maps `LOCAL:l` to
//! `MPEGTS:m` sits at MPEG-TS time `m/90000 s + (t - l)`. A segment with no map
//! maps local `0` to MPEG-TS `0` (RFC 8216 §3.5), so its cue times already are
//! MPEG-TS times (the pluto.tv rendition of #526 does exactly that:
//! `13:26:15.143` is the video's raw PTS `48375.143 s`). [`VttCue::raw_start`]
//! carries that MPEG-TS instant in nanoseconds (not reduced modulo `2^33`).

use multiview_core::time::MediaTime;

/// Most segments one media playlist may list (a live window is a handful).
pub const MAX_SEGMENTS: usize = 4096;
/// Most cues kept from one segment (a 5 s Pluto segment carries ~25).
pub const MAX_CUES_PER_SEGMENT: usize = 512;
/// Most text lines kept per cue.
pub const MAX_LINES_PER_CUE: usize = 16;
/// Most bytes kept per cue line.
pub const MAX_LINE_BYTES: usize = 512;
/// Most lines of a document the parsers scan before giving up on the rest.
const MAX_DOC_LINES: usize = 64 * 1024;

/// Nanoseconds per MPEG-TS 90 kHz tick numerator.
const NS_PER_SEC: i128 = 1_000_000_000;
/// The MPEG-TS clock rate.
const MPEGTS_HZ: i128 = 90_000;

/// One media segment of a rendition playlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// The segment's media sequence number (`EXT-X-MEDIA-SEQUENCE` + index).
    pub sequence: u64,
    /// The segment URI as written (resolve against the playlist URL).
    pub uri: String,
    /// Its discontinuity epoch: `EXT-X-DISCONTINUITY-SEQUENCE` plus the
    /// `EXT-X-DISCONTINUITY` tags before it. Cue times of different epochs
    /// sit on unrelated clocks (an ad break restarts the MPEG-TS clock).
    pub discontinuity: u64,
    /// Its `#EXTINF` duration in nanoseconds (0 when absent or malformed;
    /// at most one hour).
    pub duration_ns: i64,
}

/// A parsed rendition media playlist.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RenditionPlaylist {
    /// The listed segments, in order.
    pub segments: Vec<Segment>,
    /// `EXT-X-TARGETDURATION` in seconds (0 when absent).
    pub target_duration_s: u64,
    /// Whether `EXT-X-ENDLIST` was seen (a finished/VOD playlist).
    pub ended: bool,
}

impl RenditionPlaylist {
    /// Parse a media playlist. Returns `None` when it is not a playlist
    /// (`#EXTM3U` missing). At most [`MAX_SEGMENTS`] segments are kept; the
    /// excess (the oldest are kept first) is ignored.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let mut lines = text.lines().take(MAX_DOC_LINES).map(str::trim);
        if lines.by_ref().find(|l| !l.is_empty()) != Some("#EXTM3U") {
            return None;
        }
        let mut out = Self::default();
        let mut sequence: u64 = 0;
        let mut discontinuity: u64 = 0;
        let mut duration_ns: i64 = 0;
        for line in lines {
            if line.is_empty() {
                continue;
            }
            if let Some(value) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
                sequence = value.trim().parse().unwrap_or(0);
            } else if let Some(value) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
                out.target_duration_s = value.trim().parse().unwrap_or(0);
            } else if let Some(value) = line.strip_prefix("#EXT-X-DISCONTINUITY-SEQUENCE:") {
                discontinuity = value.trim().parse().unwrap_or(0);
            } else if line == "#EXT-X-DISCONTINUITY" {
                discontinuity = discontinuity.saturating_add(1);
            } else if let Some(value) = line.strip_prefix("#EXTINF:") {
                duration_ns = parse_extinf_ns(value);
            } else if line == "#EXT-X-ENDLIST" {
                out.ended = true;
            } else if line.starts_with('#') {
                // Other tags (EXTINF, DISCONTINUITY, KEY, PDT, vendor tags).
            } else {
                if out.segments.len() < MAX_SEGMENTS {
                    out.segments.push(Segment {
                        sequence,
                        uri: line.to_owned(),
                        discontinuity,
                        duration_ns,
                    });
                }
                sequence = sequence.saturating_add(1);
                duration_ns = 0;
            }
        }
        Some(out)
    }
}

/// Parse an `#EXTINF` value (`<seconds>[,title]`) into nanoseconds: decimal
/// seconds, at most one hour, millisecond precision; 0 when malformed.
fn parse_extinf_ns(value: &str) -> i64 {
    let seconds = value.split(',').next().unwrap_or("").trim();
    let (whole, frac) = seconds.split_once('.').unwrap_or((seconds, ""));
    if whole.is_empty()
        || whole.len() > 4
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !frac.bytes().all(|b| b.is_ascii_digit())
    {
        return 0;
    }
    let whole: i64 = whole.parse().unwrap_or(0);
    // The first three fraction digits, right-padded: "1" -> 100 ms.
    let padded: String = frac.chars().chain("000".chars()).take(3).collect();
    let millis: i64 = padded.parse().unwrap_or(0);
    whole
        .min(3600)
        .saturating_mul(1_000_000_000)
        .saturating_add(millis.saturating_mul(1_000_000))
}

/// One `WebVTT` cue of a rendition segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VttCue {
    /// The cue start as written in the segment (its LOCAL timeline).
    pub local_start: MediaTime,
    /// The cue end as written in the segment.
    pub local_end: MediaTime,
    /// The cue start on the MPEG-TS clock, in ns (`X-TIMESTAMP-MAP` applied;
    /// equal to [`Self::local_start`] when the segment carries no map).
    pub raw_start: i64,
    /// The cue end on the MPEG-TS clock, in ns.
    pub raw_end: i64,
    /// Display lines, markup stripped, bounded, non-empty.
    pub lines: Vec<String>,
}

/// A parsed `X-TIMESTAMP-MAP`: MPEG-TS minus LOCAL, in nanoseconds.
fn parse_timestamp_map(value: &str) -> Option<i64> {
    let mut local = None;
    let mut mpegts = None;
    for part in value.split(',') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix("LOCAL:") {
            local = parse_vtt_time(v.trim());
        } else if let Some(v) = part.strip_prefix("MPEGTS:") {
            mpegts = v
                .trim()
                .parse::<i64>()
                .ok()
                .filter(|m| (0..(1_i64 << 33)).contains(m));
        }
    }
    let mpegts_ns = i128::from(mpegts?) * NS_PER_SEC / MPEGTS_HZ;
    let offset = mpegts_ns - i128::from(local?.as_nanos());
    i64::try_from(offset).ok()
}

/// Parse a `WebVTT` timestamp `[HH:]MM:SS.mmm` into a [`MediaTime`].
fn parse_vtt_time(field: &str) -> Option<MediaTime> {
    let (hms, frac) = field.split_once('.')?;
    if frac.len() != 3 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let millis: i64 = frac.parse().ok()?;
    let parts: Vec<&str> = hms.split(':').collect();
    if !(2..=3).contains(&parts.len()) {
        return None;
    }
    let mut secs: i64 = 0;
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() || part.len() > 10 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let value: i64 = part.parse().ok()?;
        // Minutes and seconds are two digits, below 60.
        if index > 0 && (part.len() != 2 || value >= 60) {
            return None;
        }
        secs = secs.checked_mul(60)?.checked_add(value)?;
    }
    let ns = secs
        .checked_mul(1_000)?
        .checked_add(millis)?
        .checked_mul(1_000_000)?;
    Some(MediaTime::from_nanos(ns))
}

/// Strip `<...>` tags and decode the common entities of one cue text line,
/// keeping at most [`MAX_LINE_BYTES`] bytes (on a char boundary).
fn clean_line(line: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for ch in line.chars() {
        match ch {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if in_tag => {}
            _ => {
                if out.len().saturating_add(ch.len_utf8()) > MAX_LINE_BYTES {
                    break;
                }
                out.push(ch);
            }
        }
    }
    let out = out
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&");
    out.trim().to_owned()
}

/// Parse one `WebVTT` segment into its cues, applying the segment's
/// `X-TIMESTAMP-MAP`. Returns `None` when the text is not `WebVTT` (no
/// `WEBVTT` signature). Malformed cue blocks are skipped; at most
/// [`MAX_CUES_PER_SEGMENT`] cues are kept.
#[must_use]
pub fn parse_segment(text: &str) -> Option<Vec<VttCue>> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.lines().take(MAX_DOC_LINES);
    let header = lines.next()?;
    if !header.starts_with("WEBVTT") {
        return None;
    }
    // Header lines up to the first blank line carry the timestamp map.
    let mut offset: i64 = 0;
    for line in lines.by_ref() {
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("X-TIMESTAMP-MAP=") {
            // An unparseable map is ignored (the cue keeps its local time),
            // as libav's demuxer does.
            offset = parse_timestamp_map(value).unwrap_or(0);
        }
    }
    let mut cues = Vec::new();
    let mut block: Vec<&str> = Vec::new();
    for line in lines {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() {
            finish_block(&mut block, &mut cues, offset);
        } else if block.len() < MAX_LINES_PER_CUE.saturating_add(2) {
            block.push(line);
        }
    }
    finish_block(&mut block, &mut cues, offset);
    Some(cues)
}

/// Turn the collected `block` into a cue (while under the cap) and clear it.
fn finish_block(block: &mut Vec<&str>, cues: &mut Vec<VttCue>, offset: i64) {
    if cues.len() < MAX_CUES_PER_SEGMENT {
        if let Some(cue) = parse_block(block, offset) {
            cues.push(cue);
        }
    }
    block.clear();
}

/// Parse one cue block (optional identifier, timing line, text lines).
fn parse_block(block: &[&str], offset: i64) -> Option<VttCue> {
    let timing_at = block.iter().position(|l| l.contains("-->"))?;
    // A NOTE/STYLE/REGION block never has a timing line at index 0..=1 with
    // a leading keyword; a cue identifier may precede the timing line.
    if timing_at > 1 {
        return None;
    }
    let timing = block.get(timing_at)?;
    let (start, rest) = timing.split_once("-->")?;
    let start = parse_vtt_time(start.trim())?;
    let end = parse_vtt_time(rest.split_whitespace().next()?)?;
    if end.as_nanos() <= start.as_nanos() {
        return None;
    }
    let lines: Vec<String> = block
        .iter()
        .skip(timing_at.saturating_add(1))
        .take(MAX_LINES_PER_CUE)
        .map(|l| clean_line(l))
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(VttCue {
        local_start: start,
        local_end: end,
        raw_start: start.as_nanos().saturating_add(offset),
        raw_end: end.as_nanos().saturating_add(offset),
        lines,
    })
}

/// The 33-bit MPEG-TS wrap period in nanoseconds (`2^33 / 90 kHz`), rounded.
pub const MPEGTS_WRAP_NS: i64 = 95_443_717_688_889;

/// Project a raw MPEG-TS instant `raw_ns` onto the video's normalized
/// timeline, given the video anchor `(anchor_raw_ns, pts0)`: the anchor frame's raw
/// container PTS (in `[0, 2^33)` ticks, as ns) and its normalized PTS. The
/// occurrence of `raw_ns` modulo the 33-bit wrap nearest the anchor is used.
#[must_use]
pub fn project_raw(raw_ns: i64, anchor_raw_ns: i64, pts0: MediaTime) -> MediaTime {
    let wrap = i128::from(MPEGTS_WRAP_NS);
    let mut delta = (i128::from(raw_ns) - i128::from(anchor_raw_ns)).rem_euclid(wrap);
    if delta > wrap / 2 {
        delta -= wrap;
    }
    let ns = i128::from(pts0.as_nanos()).saturating_add(delta);
    MediaTime::from_nanos(i64::try_from(ns).unwrap_or(if ns < 0 { i64::MIN } else { i64::MAX }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
    use super::*;

    const S: i64 = 1_000_000_000;

    #[test]
    fn a_timestamp_map_moves_cues_onto_the_mpegts_clock() {
        let cues = parse_segment(
            "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:900000,LOCAL:00:00:00.000\n\n\
             00:00:01.000 --> 00:00:03.000\nhello\n",
        )
        .unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].local_start, MediaTime::from_nanos(S));
        assert_eq!(cues[0].raw_start, 11 * S, "MPEGTS 900000 = 10 s");
        assert_eq!(cues[0].raw_end, 13 * S);
        assert_eq!(cues[0].lines, vec!["hello".to_owned()]);
    }

    #[test]
    fn a_nonzero_local_anchor_and_reversed_order_are_honoured() {
        let cues = parse_segment(
            "WEBVTT\nX-TIMESTAMP-MAP=LOCAL:00:00:10.000,MPEGTS:180000\n\n\
             00:00:10.500 --> 00:00:11.000\nx\n",
        )
        .unwrap();
        // LOCAL 10 s maps to 2 s: cue 10.5 s -> 2.5 s.
        assert_eq!(cues[0].raw_start, 2 * S + S / 2);
    }

    #[test]
    fn no_map_keeps_local_times_as_mpegts_times() {
        let cues = parse_segment(
            "WEBVTT\n\n13:26:15.143 --> 13:26:15.229 position:10% line:58% size:80% align:start\n\
             AT MIDNIGHT ON NOVEMBER 19TH    \nAND LASTS UNTIL THE PLAYER      \n",
        )
        .unwrap();
        assert_eq!(
            cues[0].raw_start,
            (13 * 3600 + 26 * 60 + 15) * S + 143_000_000
        );
        assert_eq!(cues[0].lines.len(), 2);
        assert_eq!(cues[0].lines[0], "AT MIDNIGHT ON NOVEMBER 19TH");
    }

    #[test]
    fn markup_is_stripped_and_malformed_blocks_are_skipped() {
        let cues = parse_segment(
            "WEBVTT\n\nNOTE a comment\n\n1\n00:01.000 --> 00:02.000\n<i>it&amp;s</i>\n\n\
             00:03.000 --> 00:02.000\nbackwards\n\nbad --> worse\nx\n\n\
             00:04.000 --> 00:05.000\n\n",
        )
        .unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].lines, vec!["it&s".to_owned()]);
    }

    #[test]
    fn a_non_webvtt_body_is_refused() {
        assert!(parse_segment("\u{0}garbage").is_none());
        assert!(parse_segment("").is_none());
    }

    #[test]
    fn hostile_segments_stay_bounded() {
        let mut doc = String::from("WEBVTT\n\n");
        for i in 0..(MAX_CUES_PER_SEGMENT + 100) {
            let _ = std::fmt::Write::write_fmt(
                &mut doc,
                format_args!("00:00:01.000 --> 00:00:02.000\n{i}\n"),
            );
            for _ in 0..24 {
                doc.push_str(&"y".repeat(600));
                doc.push('\n');
            }
            doc.push('\n');
        }
        let cues = parse_segment(&doc).unwrap();
        assert_eq!(cues.len(), MAX_CUES_PER_SEGMENT);
        for cue in &cues {
            assert!(cue.lines.len() <= MAX_LINES_PER_CUE);
            assert!(cue.lines.iter().all(|l| l.len() <= MAX_LINE_BYTES));
        }
    }

    #[test]
    fn the_media_playlist_lists_sequences_and_end() {
        let playlist = RenditionPlaylist::parse(
            "#EXTM3U\n#EXT-X-TARGETDURATION:5\n#EXT-X-DISCONTINUITY-SEQUENCE:283\n\
             #EXT-X-MEDIA-SEQUENCE:6658\n\
             #EXTINF:5,\na.vtt\n#EXT-X-DISCONTINUITY\n#EXTINF:5,\nb.vtt\n",
        )
        .unwrap();
        assert_eq!(playlist.target_duration_s, 5);
        assert!(!playlist.ended);
        assert_eq!(
            playlist.segments,
            vec![
                Segment {
                    sequence: 6658,
                    uri: "a.vtt".to_owned(),
                    discontinuity: 283,
                    duration_ns: 5_000_000_000,
                },
                Segment {
                    sequence: 6659,
                    uri: "b.vtt".to_owned(),
                    discontinuity: 284,
                    duration_ns: 5_000_000_000,
                },
            ]
        );
        assert!(RenditionPlaylist::parse("WEBVTT\n").is_none());
        assert!(
            RenditionPlaylist::parse("#EXTM3U\n#EXT-X-ENDLIST\n")
                .unwrap()
                .ended
        );
    }

    #[test]
    fn extinf_durations_parse_bounded() {
        assert_eq!(parse_extinf_ns("5,"), 5 * S);
        assert_eq!(parse_extinf_ns("4.004,title"), 4 * S + 4_000_000);
        assert_eq!(parse_extinf_ns("0.1,"), S / 10);
        assert_eq!(parse_extinf_ns("99999,"), 0);
        assert_eq!(parse_extinf_ns("-1,"), 0);
        assert_eq!(parse_extinf_ns("abc"), 0);
    }

    #[test]
    fn projection_uses_the_anchor_and_the_nearest_wrap() {
        let pts0 = MediaTime::from_nanos(5 * S);
        assert_eq!(project_raw(48_376 * S, 48_375 * S, pts0).as_nanos(), 6 * S);
        // A cue just past the 33-bit wrap of an anchor just before it.
        let raw0 = MPEGTS_WRAP_NS - S;
        assert_eq!(project_raw(S, raw0, pts0).as_nanos(), 7 * S);
        // And the reverse: an anchor just after the wrap, a cue just before.
        assert_eq!(project_raw(MPEGTS_WRAP_NS - S, S, pts0).as_nanos(), 3 * S);
    }
}
