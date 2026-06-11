//! The unified **caption cue** model — the single shape every caption decoder
//! produces, regardless of which libav decoder (teletext, DVB-sub, CEA-608/708,
//! WebVTT, SubRip, mov_text, ASS) created it.
//!
//! This module is **pure** and **always compiled** (no FFI, no native deps): it
//! depends only on [`multiview_core::time`]. The feature-gated [`CaptionDecoder`]
//! (the `ffmpeg` feature, see [`crate::caption_decode`]) emits these types; the
//! per-tile cue store in `multiview-input` and the overlay burn-in renderer consume
//! them. Keeping the model native-dep-free is the efficiency rule of
//! [`docs/io/captions.md`](../docs/io/captions.md) §7: the cue shape, its expiry
//! window, and the markup-stripping logic are unit-tested with no FFmpeg.
//!
//! [`CaptionDecoder`]: crate::caption_decode::CaptionDecoder
//!
//! # The two cue shapes
//!
//! Per [`docs/io/captions.md`](../docs/io/captions.md) §1 every caption decodes
//! to **one of two shapes** on the source's normalised nanosecond timeline:
//!
//! * [`CaptionCue::Text`] — markup-stripped display lines (top→bottom), with an
//!   optional [`CueRegion`] so 608/708 roll-up/pop-on placement survives.
//!   Teletext, CEA-608/708, WebVTT, SubRip, ASS (text path) and mov_text all
//!   land here.
//! * [`CaptionCue::Bitmap`] — a premultiplied-RGBA image plus its placement
//!   [`CueRect`] relative to the source frame. DVB subtitle lands here.
//!
//! Times are absolute [`MediaTime`] (ns) on the *source's normalised* timeline
//! (invariant #3). `end_ns > start_ns` is enforced at construction
//! ([`CaptionCue::try_text`] / [`CaptionCue::try_bitmap`]); an open-ended cue is
//! closed by the caller before it reaches here.

use multiview_core::time::MediaTime;
use serde::{Deserialize, Serialize};

/// Why a [`CaptionCue`] could not be constructed.
///
/// The decoder rebases libav display times onto the ns timeline and then builds
/// a cue; a degenerate window or an empty bitmap is reported rather than silently
/// dropped or (forbidden on the data plane) panicked.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CueError {
    /// The cue window was empty or inverted (`end_ns <= start_ns`).
    #[error("cue window {start_ns}..{end_ns} is empty or inverted (end must exceed start)")]
    EmptyWindow {
        /// The proposed start of the window, in nanoseconds.
        start_ns: i64,
        /// The proposed end of the window, in nanoseconds.
        end_ns: i64,
    },

    /// A text cue carried no displayable lines after markup stripping.
    #[error("text cue has no displayable lines")]
    EmptyText,

    /// A bitmap cue carried no pixels (zero width or height, or a buffer whose
    /// length does not match `width * height * 4`).
    #[error(
        "bitmap cue is empty or its RGBA buffer ({len} bytes) does not match {width}x{height}"
    )]
    InvalidBitmap {
        /// Declared width in pixels.
        width: u32,
        /// Declared height in pixels.
        height: u32,
        /// Actual RGBA byte-buffer length.
        len: usize,
    },
}

/// Where a text cue sits, when the source carried placement (CEA-608/708
/// roll-up/pop-on, `WebVTT` cue settings). Anchors follow the ASS `\an` numpad
/// convention so a 608 `{\an7}` (top-left) survives into the renderer.
///
/// Coordinates are **normalised** to `0.0..=1.0` of the source frame so the
/// region scales with whatever tile the renderer places it in; the renderer maps
/// them to the tile's display rectangle.
// `x`/`y` are `f32`, so this (and everything embedding it) carries `PartialEq`
// but not `Eq`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CueRegion {
    /// Vertical/horizontal anchor (numpad 1..=9; 7 = top-left, 5 = centre,
    /// 2 = bottom-centre, …). [`CueAnchor::default`] is bottom-centre, the
    /// conventional caption position.
    pub anchor: CueAnchor,
    /// Horizontal position of the anchor, `0.0` (left) .. `1.0` (right), if the
    /// source specified one. [`None`] means "use the anchor's default column".
    pub x: Option<f32>,
    /// Vertical position of the anchor, `0.0` (top) .. `1.0` (bottom), if the
    /// source specified one. [`None`] means "use the anchor's default row".
    pub y: Option<f32>,
}

/// The numpad-style anchor of a [`CueRegion`], matching the ASS `\an` codes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CueAnchor {
    /// `\an1` — bottom-left.
    BottomLeft,
    /// `\an2` — bottom-centre (the caption default).
    #[default]
    BottomCenter,
    /// `\an3` — bottom-right.
    BottomRight,
    /// `\an4` — middle-left.
    MiddleLeft,
    /// `\an5` — middle-centre.
    MiddleCenter,
    /// `\an6` — middle-right.
    MiddleRight,
    /// `\an7` — top-left.
    TopLeft,
    /// `\an8` — top-centre.
    TopCenter,
    /// `\an9` — top-right.
    TopRight,
}

impl CueAnchor {
    /// Map an ASS `\an` numpad code (1..=9) to an anchor, or [`None`] if out of
    /// range.
    #[must_use]
    pub const fn from_an(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::BottomLeft,
            2 => Self::BottomCenter,
            3 => Self::BottomRight,
            4 => Self::MiddleLeft,
            5 => Self::MiddleCenter,
            6 => Self::MiddleRight,
            7 => Self::TopLeft,
            8 => Self::TopCenter,
            9 => Self::TopRight,
            _ => return None,
        })
    }
}

/// A premultiplied-RGBA bitmap cue's placement relative to the source frame.
///
/// Coordinates are in **source pixels** (the geometry the DVB-sub decoder reports
/// for the rect); the renderer rebases them to the tile's display rectangle so a
/// bitmap cue scales with its tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CueRect {
    /// Left edge in source pixels.
    pub x: u32,
    /// Top edge in source pixels.
    pub y: u32,
    /// Width in source pixels (equals the image width).
    pub width: u32,
    /// Height in source pixels (equals the image height).
    pub height: u32,
}

impl CueRect {
    /// Construct a [`CueRect`] from its source-pixel `x`/`y` origin and
    /// `width`/`height`. (`CueRect` is `#[non_exhaustive]`, so this is the way to
    /// build one outside this crate.)
    #[must_use]
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

/// The displayable payload of a [`CaptionCue::Text`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CueText {
    /// Display lines, top to bottom, already markup-stripped and non-empty.
    pub lines: Vec<String>,
    /// Source-carried placement, if any (608/708, `WebVTT` settings).
    pub region: Option<CueRegion>,
}

/// The pixel payload of a [`CaptionCue::Bitmap`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CueBitmap {
    /// Premultiplied-RGBA pixels, row-major, `width * height * 4` bytes, tightly
    /// packed (no row padding — the decoder copies into a tight buffer).
    pub rgba: Vec<u8>,
    /// Placement rect relative to the source frame.
    pub rect: CueRect,
}

/// One caption cue on the source's normalised nanosecond timeline.
///
/// An adjacently-tagged enum (`#[serde(tag = "kind")]`, never `untagged`) so it
/// round-trips TOML/JSON, and `#[non_exhaustive]` so a future cue shape can be
/// added without breaking consumers (CLAUDE.md §5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum CaptionCue {
    /// A text cue: markup-stripped display lines plus optional placement.
    Text {
        /// When the cue becomes visible (inclusive).
        start: MediaTime,
        /// When the cue stops being visible (exclusive).
        end: MediaTime,
        /// The displayable text and placement.
        text: CueText,
    },
    /// A bitmap cue: premultiplied-RGBA image plus its placement rect.
    Bitmap {
        /// When the cue becomes visible (inclusive).
        start: MediaTime,
        /// When the cue stops being visible (exclusive).
        end: MediaTime,
        /// The pixels and placement.
        bitmap: CueBitmap,
    },
}

impl CaptionCue {
    /// Build a text cue, validating the window and that at least one line remains.
    ///
    /// # Errors
    /// * [`CueError::EmptyWindow`] if `end <= start`.
    /// * [`CueError::EmptyText`] if `lines` is empty.
    pub fn try_text(
        start: MediaTime,
        end: MediaTime,
        lines: Vec<String>,
        region: Option<CueRegion>,
    ) -> Result<Self, CueError> {
        check_window(start, end)?;
        if lines.is_empty() {
            return Err(CueError::EmptyText);
        }
        Ok(Self::Text {
            start,
            end,
            text: CueText { lines, region },
        })
    }

    /// Build a bitmap cue, validating the window and that the RGBA buffer matches
    /// the rect geometry.
    ///
    /// # Errors
    /// * [`CueError::EmptyWindow`] if `end <= start`.
    /// * [`CueError::InvalidBitmap`] if the rect is degenerate or the buffer
    ///   length is not exactly `width * height * 4`.
    pub fn try_bitmap(
        start: MediaTime,
        end: MediaTime,
        rgba: Vec<u8>,
        rect: CueRect,
    ) -> Result<Self, CueError> {
        check_window(start, end)?;
        let expected = usize::try_from(rect.width)
            .ok()
            .zip(usize::try_from(rect.height).ok())
            .and_then(|(w, h)| w.checked_mul(h))
            .and_then(|px| px.checked_mul(4));
        if rect.width == 0 || rect.height == 0 || expected != Some(rgba.len()) {
            return Err(CueError::InvalidBitmap {
                width: rect.width,
                height: rect.height,
                len: rgba.len(),
            });
        }
        Ok(Self::Bitmap {
            start,
            end,
            bitmap: CueBitmap { rgba, rect },
        })
    }

    /// When this cue becomes visible (inclusive).
    #[must_use]
    pub const fn start(&self) -> MediaTime {
        match self {
            Self::Text { start, .. } | Self::Bitmap { start, .. } => *start,
        }
    }

    /// When this cue stops being visible (exclusive).
    #[must_use]
    pub const fn end(&self) -> MediaTime {
        match self {
            Self::Text { end, .. } | Self::Bitmap { end, .. } => *end,
        }
    }

    /// Whether the cue is visible at media time `now` (`start <= now < end`).
    #[must_use]
    pub fn is_active_at(&self, now: MediaTime) -> bool {
        now.as_nanos() >= self.start().as_nanos() && now.as_nanos() < self.end().as_nanos()
    }
}

/// Validate a `[start, end)` window: `end` must be strictly after `start`.
fn check_window(start: MediaTime, end: MediaTime) -> Result<(), CueError> {
    if end.as_nanos() <= start.as_nanos() {
        return Err(CueError::EmptyWindow {
            start_ns: start.as_nanos(),
            end_ns: end.as_nanos(),
        });
    }
    Ok(())
}

/// Strip an ASS/SSA dialogue event line down to its plain display lines and an
/// optional [`CueRegion`].
///
/// `cc_dec` (CEA-608/708), `libzvbi_teletextdec`, and the `ass` decoder all emit
/// their text as ASS dialogue: optional override blocks in `{...}` (e.g.
/// `{\an7}{\pos(38,182)}`) followed by text where `\N` (and `\n`) are hard line
/// breaks. This returns the markup-stripped lines (top→bottom, empties removed)
/// plus any `\an` anchor / `\pos` placement found in the override blocks.
///
/// The input is either a full ASS event line (eight comma-separated fields,
/// the ninth being the text) or just the text field; both are handled.
#[must_use]
pub fn strip_ass_event(event: &str) -> (Vec<String>, Option<CueRegion>) {
    let text = ass_text_field(event);
    let mut anchor: Option<CueAnchor> = None;
    let mut pos: Option<(f32, f32)> = None;

    // Walk the string, pulling out `{...}` override blocks (parsing \anN / \posX,Y
    // from them) and treating `\N` / `\n` as hard breaks elsewhere.
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '{' => {
                // Collect until the closing brace (or end of string).
                let mut block = String::new();
                for inner in chars.by_ref() {
                    if inner == '}' {
                        break;
                    }
                    block.push(inner);
                }
                if let Some(a) = parse_ass_anchor(&block) {
                    anchor = Some(a);
                }
                if let Some(p) = parse_ass_pos(&block) {
                    pos = Some(p);
                }
            }
            '\\' => match chars.peek() {
                Some('N' | 'n') => {
                    chars.next();
                    lines.push(std::mem::take(&mut current));
                }
                _ => current.push('\\'),
            },
            _ => current.push(ch),
        }
    }
    lines.push(current);

    let lines: Vec<String> = lines
        .into_iter()
        .map(|l| l.trim().to_owned())
        .filter(|l| !l.is_empty())
        .collect();

    let region = build_region(anchor, pos);
    (lines, region)
}

/// Combine a parsed anchor and `\pos` into a [`CueRegion`], if either is present.
fn build_region(anchor: Option<CueAnchor>, pos: Option<(f32, f32)>) -> Option<CueRegion> {
    if anchor.is_none() && pos.is_none() {
        return None;
    }
    let (x, y) = match pos {
        // `\pos` is in script (typically 384x288 PAL teletext / 608) coordinates;
        // the decoder normalises against the known reference height when it has
        // it. Here we only carry the raw value through as a normalised hint when
        // it already looks normalised (0..=1); otherwise leave it for the decoder
        // to scale. We keep it conservative and only forward already-normalised
        // values so we never emit a bogus >1.0 coordinate.
        Some((px, py)) if (0.0..=1.0).contains(&px) && (0.0..=1.0).contains(&py) => {
            (Some(px), Some(py))
        }
        _ => (None, None),
    };
    Some(CueRegion {
        anchor: anchor.unwrap_or_default(),
        x,
        y,
    })
}

/// Extract the text field of an ASS event line. The libav subtitle decoders
/// (`cc_dec`, the `ass` decoder, …) emit the eight-field event form
/// `ReadOrder,Layer,Style,Name,MarginL,MarginR,MarginV,Effect` before the text —
/// `cc_dec` emits exactly that prefix (`0,0,Default,,0,0,0,,`). The text begins
/// after the **8th** comma, so any commas *inside* the text (e.g. `\pos(38,182)`)
/// stay part of it. If the input has fewer than eight commas it is treated as
/// already being the text field.
fn ass_text_field(event: &str) -> &str {
    let trimmed = event
        .strip_prefix("Dialogue:")
        .unwrap_or(event)
        .trim_start();
    match trimmed.match_indices(',').nth(7) {
        Some((idx, _)) => trimmed.get(idx.saturating_add(1)..).unwrap_or(trimmed),
        None => trimmed,
    }
}

/// Parse an `\anN` anchor code out of an ASS override block (e.g. `\an7\pos(..)`).
fn parse_ass_anchor(block: &str) -> Option<CueAnchor> {
    let idx = block.find("\\an")?;
    let rest = block.get(idx.saturating_add(3)..)?;
    let digit = rest.chars().next()?;
    let code = digit.to_digit(10)?;
    u8::try_from(code).ok().and_then(CueAnchor::from_an)
}

/// Parse a `\pos(x,y)` out of an ASS override block.
fn parse_ass_pos(block: &str) -> Option<(f32, f32)> {
    let idx = block.find("\\pos(")?;
    let rest = block.get(idx.saturating_add(5)..)?;
    let close = rest.find(')')?;
    let inside = rest.get(..close)?;
    let (xs, ys) = inside.split_once(',')?;
    let x: f32 = xs.trim().parse().ok()?;
    let y: f32 = ys.trim().parse().ok()?;
    Some((x, y))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ns(n: i64) -> MediaTime {
        MediaTime::from_nanos(n)
    }

    #[test]
    fn text_cue_rejects_inverted_window() {
        let err = CaptionCue::try_text(ns(2_000), ns(1_000), vec!["x".into()], None)
            .expect_err("inverted window must be rejected");
        assert_eq!(
            err,
            CueError::EmptyWindow {
                start_ns: 2_000,
                end_ns: 1_000
            }
        );
    }

    #[test]
    fn text_cue_rejects_equal_window() {
        let err = CaptionCue::try_text(ns(1_000), ns(1_000), vec!["x".into()], None)
            .expect_err("zero-length window must be rejected");
        assert!(matches!(err, CueError::EmptyWindow { .. }));
    }

    #[test]
    fn text_cue_rejects_empty_lines() {
        let err = CaptionCue::try_text(ns(0), ns(1_000), Vec::new(), None)
            .expect_err("empty text must be rejected");
        assert_eq!(err, CueError::EmptyText);
    }

    #[test]
    fn text_cue_active_window_is_half_open() {
        let cue =
            CaptionCue::try_text(ns(1_000), ns(2_000), vec!["HELLO".into()], None).expect("valid");
        assert!(!cue.is_active_at(ns(999)));
        assert!(cue.is_active_at(ns(1_000)), "start is inclusive");
        assert!(cue.is_active_at(ns(1_999)));
        assert!(!cue.is_active_at(ns(2_000)), "end is exclusive");
        assert_eq!(cue.start(), ns(1_000));
        assert_eq!(cue.end(), ns(2_000));
    }

    #[test]
    fn bitmap_cue_rejects_buffer_geometry_mismatch() {
        let rect = CueRect {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        };
        // 2x2 RGBA must be 16 bytes; 12 is wrong.
        let err = CaptionCue::try_bitmap(ns(0), ns(10), vec![0; 12], rect)
            .expect_err("short buffer must be rejected");
        assert_eq!(
            err,
            CueError::InvalidBitmap {
                width: 2,
                height: 2,
                len: 12
            }
        );
    }

    #[test]
    fn bitmap_cue_rejects_zero_dimensions() {
        let rect = CueRect {
            x: 0,
            y: 0,
            width: 0,
            height: 4,
        };
        let err = CaptionCue::try_bitmap(ns(0), ns(10), Vec::new(), rect)
            .expect_err("zero width must be rejected");
        assert!(matches!(err, CueError::InvalidBitmap { .. }));
    }

    #[test]
    fn bitmap_cue_accepts_matching_geometry() {
        let rect = CueRect {
            x: 5,
            y: 7,
            width: 2,
            height: 2,
        };
        let cue = CaptionCue::try_bitmap(ns(0), ns(10), vec![1; 16], rect).expect("valid bitmap");
        match cue {
            CaptionCue::Bitmap { bitmap, .. } => {
                assert_eq!(bitmap.rect.x, 5);
                assert_eq!(bitmap.rect.y, 7);
                assert_eq!(bitmap.rgba.len(), 16);
            }
            CaptionCue::Text { .. } => panic!("expected a bitmap cue"),
        }
    }

    #[test]
    fn strip_ass_event_handles_full_dialogue_prefix_and_overrides() {
        // Exactly the shape cc_dec emits for a CC1 pop-on caption.
        let (lines, region) = strip_ass_event("0,0,Default,,0,0,0,,{\\an7}{\\pos(38,182)}HI");
        assert_eq!(lines, vec!["HI".to_owned()]);
        let region = region.expect("an7 carries an anchor");
        assert_eq!(region.anchor, CueAnchor::TopLeft);
        // 38/182 are script coordinates (>1), so they are not forwarded as a
        // normalised position — only the anchor survives.
        assert_eq!(region.x, None);
        assert_eq!(region.y, None);
    }

    #[test]
    fn strip_ass_event_splits_hard_line_breaks() {
        let (lines, _) = strip_ass_event("{\\an2}LINE ONE\\NLINE TWO");
        assert_eq!(lines, vec!["LINE ONE".to_owned(), "LINE TWO".to_owned()]);
    }

    #[test]
    fn strip_ass_event_drops_empty_lines() {
        let (lines, _) = strip_ass_event("\\N\\NONLY\\N");
        assert_eq!(lines, vec!["ONLY".to_owned()]);
    }

    #[test]
    fn strip_ass_event_without_dialogue_prefix_is_plain_text() {
        let (lines, region) = strip_ass_event("just text");
        assert_eq!(lines, vec!["just text".to_owned()]);
        assert!(region.is_none());
    }

    #[test]
    fn strip_ass_event_forwards_normalised_pos() {
        let (_, region) = strip_ass_event("{\\an5\\pos(0.5,0.9)}centred");
        let region = region.expect("region present");
        assert_eq!(region.anchor, CueAnchor::MiddleCenter);
        assert_eq!(region.x, Some(0.5));
        assert_eq!(region.y, Some(0.9));
    }

    #[test]
    fn anchor_from_an_covers_the_numpad_and_rejects_out_of_range() {
        assert_eq!(CueAnchor::from_an(7), Some(CueAnchor::TopLeft));
        assert_eq!(CueAnchor::from_an(2), Some(CueAnchor::BottomCenter));
        assert_eq!(CueAnchor::from_an(0), None);
        assert_eq!(CueAnchor::from_an(10), None);
    }

    #[test]
    fn cue_round_trips_through_json_tagged() {
        let cue =
            CaptionCue::try_text(ns(0), ns(1_000), vec!["HELLO".into()], None).expect("valid");
        let json = serde_json::to_string(&cue).expect("serialize");
        assert!(
            json.contains("\"kind\":\"text\""),
            "adjacently tagged: {json}"
        );
        let back: CaptionCue = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, cue);
    }
}
