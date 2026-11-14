//! Per-output **format** and **slots** (ADR-0110, output-routing §12.3, slice B #342).
//!
//! An output may declare the picture format it carries (`[outputs.format]`:
//! size, rate, scan, colour, pixel format, aspect and the fit used to bring a
//! picture to that size) and its media slots (`[[outputs.slots.video]]`,
//! `[[outputs.slots.audio]]`). Both are optional: absent means exactly today's
//! behaviour (the Program canvas, one video slot with the output's `codec`, one
//! program audio slot).
//!
//! Slots live under `slots.video` / `slots.audio` rather than §12.3's
//! `[[outputs.video]]` / `[[outputs.audio]]`: `[outputs.audio]` is already the
//! ADR-R005 audio *selection* table, and TOML cannot make one key both a table
//! and an array of tables.
//!
//! Validation ([`validate_output_format`]) refuses with a typed message prefix:
//! [`SLOT_MEDIUM_MISMATCH`] (more slots than the medium carries, a codec on a
//! raw output, a video slot on an audio-only output) and
//! [`SLOT_FORMAT_UNSUPPORTED`] (a codec the medium does not carry, a rate,
//! scan, colour, pixel format or aspect the engine cannot produce yet, an
//! invalid size, or a format on a kind slice B does not convert for).
//!
//! Slice F (#345) adds subtitle slots (`[[outputs.slots.subtitle]]`): a
//! format (`webvtt` | `cea608` | `dvb`), a language, a menu name and the
//! source whose captions the slot carries (its patch). HLS carries up to
//! [`MAX_SUBTITLE_SLOTS`] `WebVTT` renditions; every other format/medium pair is
//! refused with the same two prefixes.
//!
//! Every type is `#[non_exhaustive]`: slice C adds per-output rates, slice D
//! passthrough.

use serde::{Deserialize, Serialize};

use crate::audio::{OutputAudio, OutputAudioMode, PROGRAM_TRACK};
use crate::error::ConfigError;
use crate::schema::{Canvas, Fps, Output};

/// Message prefix of a slot the output's medium cannot carry.
pub const SLOT_MEDIUM_MISMATCH: &str = "slot-medium-mismatch";
/// Message prefix of a slot format the engine cannot produce (yet).
pub const SLOT_FORMAT_UNSUPPORTED: &str = "slot-format-unsupported";

/// Smallest accepted format edge, in pixels.
pub const MIN_FORMAT_EDGE: u32 = 16;
/// Largest accepted format edge, in pixels.
pub const MAX_FORMAT_EDGE: u32 = 8192;
/// Largest accepted format area (UHD), in pixels.
pub const MAX_FORMAT_AREA: u64 = 3840 * 2160;

/// Largest accepted video slot bit rate, in kbit/s (1 Gbit/s).
pub const MAX_SLOT_BITRATE_KBPS: u32 = 1_000_000;

/// Most subtitle slots an output may declare (slice F, #345).
pub const MAX_SUBTITLE_SLOTS: usize = 8;
/// Longest accepted subtitle slot language tag, in characters.
pub const MAX_SUBTITLE_LANGUAGE_LEN: usize = 35;
/// Longest accepted subtitle slot menu name, in characters.
pub const MAX_SUBTITLE_NAME_LEN: usize = 64;

/// The audio grid every audio slot is encoded on (ADR-0108 §5.2): stereo.
pub const SLOT_AUDIO_CHANNELS: u16 = 2;
/// The audio grid every audio slot is encoded on: 48 kHz.
pub const SLOT_AUDIO_SAMPLE_RATE: u32 = 48_000;
/// The audio grid every audio slot is encoded on: AAC.
pub const SLOT_AUDIO_CODEC: &str = "aac";

/// Field order of an output picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Scan {
    /// Progressive frames (the only scan produced in slice B).
    #[default]
    Progressive,
    /// Interlaced, top field first.
    Tff,
    /// Interlaced, bottom field first.
    Bff,
}

impl Scan {
    /// Whether this is the default (progressive) scan.
    #[must_use]
    pub const fn is_progressive(&self) -> bool {
        matches!(self, Self::Progressive)
    }
}

/// How a picture is brought to an output's size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OutputFitMode {
    /// The whole picture inside the output, letter/pillar-boxed at the anchor.
    #[default]
    Contain,
    /// The output filled, the picture cropped at the anchor ("zoom and clip").
    Cover,
    /// The whole picture stretched onto the whole output.
    Fill,
}

/// Where a `contain` box sits or a `cover` crop is taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Anchor {
    /// Centred on both axes.
    #[default]
    Center,
    /// Top edge, centred horizontally.
    Top,
    /// Bottom edge, centred horizontally.
    Bottom,
    /// Left edge, centred vertically.
    Left,
    /// Right edge, centred vertically.
    Right,
    /// Top-left corner.
    TopLeft,
    /// Top-right corner.
    TopRight,
    /// Bottom-left corner.
    BottomLeft,
    /// Bottom-right corner.
    BottomRight,
}

impl Anchor {
    /// Every anchor, in declaration order.
    pub const ALL: [Self; 9] = [
        Self::Center,
        Self::Top,
        Self::Bottom,
        Self::Left,
        Self::Right,
        Self::TopLeft,
        Self::TopRight,
        Self::BottomLeft,
        Self::BottomRight,
    ];

    /// The anchor's position on each axis as `(x, y)`, each `0` (start),
    /// `1` (middle) or `2` (end) — halves of the free space.
    #[must_use]
    pub const fn halves(self) -> (u8, u8) {
        match self {
            Self::Center => (1, 1),
            Self::Top => (1, 0),
            Self::Bottom => (1, 2),
            Self::Left => (0, 1),
            Self::Right => (2, 1),
            Self::TopLeft => (0, 0),
            Self::TopRight => (2, 0),
            Self::BottomLeft => (0, 2),
            Self::BottomRight => (2, 2),
        }
    }
}

/// The fit of an output format: a mode plus the anchor it is applied at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct OutputFit {
    /// contain (default) | cover | fill.
    #[serde(default)]
    pub mode: OutputFitMode,
    /// The anchor (default centre). Irrelevant for `fill`.
    #[serde(default)]
    pub anchor: Anchor,
}

impl OutputFit {
    /// A fit with `mode` at `anchor`.
    #[must_use]
    pub const fn new(mode: OutputFitMode, anchor: Anchor) -> Self {
        Self { mode, anchor }
    }
}

/// The picture format an output carries (`[outputs.format]`). Absent ⇒ the
/// Program canvas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct OutputFormat {
    /// Width in pixels (even, 16..=8192).
    pub width: u32,
    /// Height in pixels (even, 16..=8192).
    pub height: u32,
    /// Frame rate; absent ⇒ the canvas rate. Other rates are slice C (#343).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<Fps>,
    /// Scan; only progressive is produced in slice B.
    #[serde(default, skip_serializing_if = "Scan::is_progressive")]
    pub scan: Scan,
    /// Colour profile name; absent ⇒ the canvas profile (#310 gates others).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Pixel format; absent ⇒ the canvas pixel format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pixel_format: Option<String>,
    /// Display aspect `"w:h"`; absent ⇒ square pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aspect: Option<String>,
    /// How a picture is brought to this size.
    #[serde(default)]
    pub fit: OutputFit,
}

impl OutputFormat {
    /// A progressive format of `width × height` with `fit`, every other field
    /// following the canvas.
    #[must_use]
    pub const fn new(width: u32, height: u32, fit: OutputFit) -> Self {
        Self {
            width,
            height,
            fps: None,
            scan: Scan::Progressive,
            color: None,
            pixel_format: None,
            aspect: None,
            fit,
        }
    }

    /// Whether this format is the canvas picture itself (size equal; every
    /// other field validated equal to the canvas). The fit is then irrelevant.
    #[must_use]
    pub const fn is_identity_for(&self, canvas: &Canvas) -> bool {
        self.width == canvas.width && self.height == canvas.height
    }
}

/// One video slot (`[[outputs.slots.video]]`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct VideoSlot {
    /// Encoder codec; absent ⇒ the output's `codec`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    /// Video bit rate in kbit/s; absent ⇒ today's 4000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitrate_kbps: Option<u32>,
}

/// One audio slot (`[[outputs.slots.audio]]`). Slot 0 is the program mix;
/// slot `i > 0` carries a declared discrete track.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct AudioSlot {
    /// The discrete track (an `[audio]` route `target_track`); absent on slot 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track: Option<String>,
    /// Language tag of this rendition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Channels; when given must be 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<u16>,
    /// Sample rate; when given must be 48000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    /// Codec; when given must be `aac`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
}

/// The format a subtitle slot is carried as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SubtitleFormat {
    /// `WebVTT` renditions (HLS `TYPE=SUBTITLES`). The only format built in F1.
    #[default]
    Webvtt,
    /// CEA-608 in A/53 SEI (TS/RTMP); not built yet (follow-up).
    Cea608,
    /// DVB subtitles (TS); not built yet (follow-up).
    Dvb,
}

impl SubtitleFormat {
    /// The config token of this format.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Webvtt => "webvtt",
            Self::Cea608 => "cea608",
            Self::Dvb => "dvb",
        }
    }
}

/// One subtitle slot (`[[outputs.slots.subtitle]]`, slice F #345).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct SubtitleSlot {
    /// Carried format (default `webvtt`).
    #[serde(default)]
    pub format: SubtitleFormat,
    /// Language tag (BCP-47-ish: 1..=35 of `[A-Za-z0-9-]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Menu label (≤ 64 printable characters, no `"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The source whose captions this slot carries (its patch); absent ⇒
    /// unpatched (the rendition stays empty).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl SubtitleSlot {
    /// A slot of `format` with no language, name or source.
    #[must_use]
    pub const fn new(format: SubtitleFormat) -> Self {
        Self {
            format,
            language: None,
            name: None,
            source: None,
        }
    }

    /// The player-menu label of slot `index` (0-based): the explicit `name`,
    /// else the `language`, else `Subtitles <index + 1>`. Labels of one
    /// output's slots must be distinct (RFC 8216 §4.3.4.1.1; validated).
    #[must_use]
    pub fn effective_name(&self, index: usize) -> String {
        self.name
            .clone()
            .or_else(|| self.language.clone())
            .unwrap_or_else(|| format!("Subtitles {}", index + 1))
    }
}

/// An output's media slots. An empty list means "absent" (today's single slot).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct OutputSlots {
    /// Video slots (at most one in slice B).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub video: Vec<VideoSlot>,
    /// Audio slots: slot 0 the program, slot `i > 0` a discrete track.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audio: Vec<AudioSlot>,
    /// Subtitle slots (slice F); empty ⇒ none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subtitle: Vec<SubtitleSlot>,
}

/// A video codec family a medium carries. [`CodecFamily::of`] is THE codec
/// token table: the engine resolves encoder tokens through it too, so a token
/// config accepts is a token the engine can open (and vice versa).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CodecFamily {
    /// H.264 / AVC.
    H264,
    /// H.265 / HEVC.
    Hevc,
    /// MPEG-2 video.
    Mpeg2,
    /// FFV1 (lossless; no medium carries it as a slot codec).
    Ffv1,
    /// Motion JPEG (no medium carries it as a slot codec).
    Mjpeg,
}

impl CodecFamily {
    /// The family of an encoder/codec token (case-insensitive); `None` for a
    /// token the engine cannot encode.
    #[must_use]
    pub fn of(token: &str) -> Option<Self> {
        match token.trim().to_ascii_lowercase().as_str() {
            "h264" | "avc" | "libx264" | "h264_nvenc" => Some(Self::H264),
            "h265" | "hevc" | "libx265" | "hevc_nvenc" => Some(Self::Hevc),
            "mpeg2video" => Some(Self::Mpeg2),
            "ffv1" => Some(Self::Ffv1),
            "mjpeg" => Some(Self::Mjpeg),
            _ => None,
        }
    }
}

/// What an output's medium carries in slots (§12.3 capability table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct SlotCapability {
    /// Most video slots (0 for an audio-only output).
    pub video_max: usize,
    /// Most audio slots; `None` ⇒ unlimited.
    pub audio_max: Option<usize>,
    /// The video codec families an encoded video slot may name; empty for a
    /// raw (unencoded) video output.
    pub video_codecs: &'static [CodecFamily],
    /// Whether the video slot is raw frames (no encoder: NDI, display).
    pub raw_video: bool,
    /// Whether slice B converts the Program to a non-canvas format for it.
    pub format_convertible: bool,
    /// Most subtitle slots (0 ⇒ the medium carries no subtitles).
    pub subtitle_max: usize,
    /// The subtitle formats this medium carries (built or not).
    pub subtitle_medium_formats: &'static [SubtitleFormat],
    /// The subtitle formats the engine builds for this medium (a subset of
    /// `subtitle_medium_formats`).
    pub subtitle_formats: &'static [SubtitleFormat],
}

const H264_HEVC: &[CodecFamily] = &[CodecFamily::H264, CodecFamily::Hevc];
const H264_HEVC_MPEG2: &[CodecFamily] = &[CodecFamily::H264, CodecFamily::Hevc, CodecFamily::Mpeg2];
const H264_ONLY: &[CodecFamily] = &[CodecFamily::H264];
const WEBVTT: &[SubtitleFormat] = &[SubtitleFormat::Webvtt];
const TS_SUBTITLES: &[SubtitleFormat] = &[SubtitleFormat::Cea608, SubtitleFormat::Dvb];
const NO_SUBTITLES: &[SubtitleFormat] = &[];

impl Output {
    /// The output's declared picture format, if any (absent ⇒ the canvas).
    #[must_use]
    pub const fn format(&self) -> Option<&OutputFormat> {
        match self {
            Output::RtspServer { format, .. }
            | Output::LlHls { format, .. }
            | Output::Hls { format, .. }
            | Output::Ndi { format, .. }
            | Output::Rtmp { format, .. }
            | Output::Srt { format, .. }
            | Output::Rist { format, .. }
            | Output::Webrtc { format, .. }
            | Output::WhipPush { format, .. }
            | Output::Aes67 { format, .. }
            | Output::Display { format, .. } => format.as_ref(),
        }
    }

    /// Mutable access to the declared picture format.
    pub fn format_mut(&mut self) -> &mut Option<OutputFormat> {
        match self {
            Output::RtspServer { format, .. }
            | Output::LlHls { format, .. }
            | Output::Hls { format, .. }
            | Output::Ndi { format, .. }
            | Output::Rtmp { format, .. }
            | Output::Srt { format, .. }
            | Output::Rist { format, .. }
            | Output::Webrtc { format, .. }
            | Output::WhipPush { format, .. }
            | Output::Aes67 { format, .. }
            | Output::Display { format, .. } => format,
        }
    }

    /// The output's declared slots, if any (absent ⇒ today's single slots).
    #[must_use]
    pub const fn slots(&self) -> Option<&OutputSlots> {
        match self {
            Output::RtspServer { slots, .. }
            | Output::LlHls { slots, .. }
            | Output::Hls { slots, .. }
            | Output::Ndi { slots, .. }
            | Output::Rtmp { slots, .. }
            | Output::Srt { slots, .. }
            | Output::Rist { slots, .. }
            | Output::Webrtc { slots, .. }
            | Output::WhipPush { slots, .. }
            | Output::Aes67 { slots, .. }
            | Output::Display { slots, .. } => slots.as_ref(),
        }
    }

    /// Mutable access to the declared slots.
    pub fn slots_mut(&mut self) -> &mut Option<OutputSlots> {
        match self {
            Output::RtspServer { slots, .. }
            | Output::LlHls { slots, .. }
            | Output::Hls { slots, .. }
            | Output::Ndi { slots, .. }
            | Output::Rtmp { slots, .. }
            | Output::Srt { slots, .. }
            | Output::Rist { slots, .. }
            | Output::Webrtc { slots, .. }
            | Output::WhipPush { slots, .. }
            | Output::Aes67 { slots, .. }
            | Output::Display { slots, .. } => slots,
        }
    }

    /// Video slot 0, when declared.
    #[must_use]
    pub fn video_slot0(&self) -> Option<&VideoSlot> {
        self.slots().and_then(|s| s.video.first())
    }

    /// What this output's medium carries in slots (§12.3 table).
    #[must_use]
    pub const fn slot_capability(&self) -> SlotCapability {
        match self {
            Output::Hls { .. } => SlotCapability {
                video_max: 1,
                audio_max: None,
                video_codecs: H264_HEVC,
                raw_video: false,
                format_convertible: true,
                subtitle_max: MAX_SUBTITLE_SLOTS,
                subtitle_medium_formats: WEBVTT,
                subtitle_formats: WEBVTT,
            },
            // LL-HLS carries WebVTT in fMP4/CMAF parts: not built yet.
            Output::LlHls { .. } => SlotCapability {
                video_max: 1,
                audio_max: None,
                video_codecs: H264_HEVC,
                raw_video: false,
                format_convertible: true,
                subtitle_max: MAX_SUBTITLE_SLOTS,
                subtitle_medium_formats: WEBVTT,
                subtitle_formats: NO_SUBTITLES,
            },
            // MPEG-TS (and RTMP) carry CEA-608 (A/53 SEI) / DVB subtitles: not built yet.
            Output::Srt { .. } | Output::Rist { .. } => SlotCapability {
                video_max: 1,
                audio_max: None,
                video_codecs: H264_HEVC_MPEG2,
                raw_video: false,
                format_convertible: true,
                subtitle_max: MAX_SUBTITLE_SLOTS,
                subtitle_medium_formats: TS_SUBTITLES,
                subtitle_formats: NO_SUBTITLES,
            },
            Output::Rtmp { multitrack, .. } => SlotCapability {
                video_max: 1,
                audio_max: if *multitrack { None } else { Some(1) },
                video_codecs: if *multitrack { H264_HEVC } else { H264_ONLY },
                raw_video: false,
                format_convertible: true,
                subtitle_max: MAX_SUBTITLE_SLOTS,
                subtitle_medium_formats: TS_SUBTITLES,
                subtitle_formats: NO_SUBTITLES,
            },
            // The RTSP server is not a packet sink yet (the engine skips it), so
            // there is nothing to convert for.
            Output::RtspServer { .. } => SlotCapability {
                video_max: 1,
                audio_max: None,
                video_codecs: H264_HEVC,
                raw_video: false,
                format_convertible: false,
                subtitle_max: 0,
                subtitle_medium_formats: NO_SUBTITLES,
                subtitle_formats: NO_SUBTITLES,
            },
            Output::Webrtc { .. } | Output::WhipPush { .. } => SlotCapability {
                video_max: 1,
                audio_max: Some(1),
                video_codecs: H264_ONLY,
                raw_video: false,
                format_convertible: false,
                subtitle_max: 0,
                subtitle_medium_formats: NO_SUBTITLES,
                subtitle_formats: NO_SUBTITLES,
            },
            Output::Ndi { .. } | Output::Display { .. } => SlotCapability {
                video_max: 1,
                audio_max: Some(1),
                video_codecs: &[],
                raw_video: true,
                format_convertible: false,
                subtitle_max: 0,
                subtitle_medium_formats: NO_SUBTITLES,
                subtitle_formats: NO_SUBTITLES,
            },
            Output::Aes67 { .. } => SlotCapability {
                video_max: 0,
                audio_max: Some(1),
                video_codecs: &[],
                raw_video: false,
                format_convertible: false,
                subtitle_max: 0,
                subtitle_medium_formats: NO_SUBTITLES,
                subtitle_formats: NO_SUBTITLES,
            },
        }
    }

    /// The audio selection this output carries: the one implied by
    /// `slots.audio` when declared (slot 0 = program, slot `i > 0` = its
    /// track), otherwise the explicit [`Output::audio`] selection.
    #[must_use]
    pub fn effective_audio(&self) -> Option<OutputAudio> {
        match self.slots().map(|s| s.audio.as_slice()) {
            Some(slots) if !slots.is_empty() => {
                let mut tracks = Vec::with_capacity(slots.len());
                tracks.push(PROGRAM_TRACK.to_owned());
                tracks.extend(slots.iter().skip(1).filter_map(|s| s.track.clone()));
                Some(OutputAudio {
                    mode: OutputAudioMode::Tracks,
                    tracks,
                })
            }
            _ => self.audio().cloned(),
        }
    }
}

fn mismatch(output: &Output, detail: &str) -> ConfigError {
    ConfigError::Validation(format!(
        "{SLOT_MEDIUM_MISMATCH}: output {:?}: {detail}",
        output.id()
    ))
}

fn unsupported(output: &Output, detail: &str) -> ConfigError {
    ConfigError::Validation(format!(
        "{SLOT_FORMAT_UNSUPPORTED}: output {:?}: {detail}",
        output.id()
    ))
}

const fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

fn rationals_equal(a: Fps, b: Fps) -> bool {
    let (a, b) = (a.rational(), b.rational());
    i128::from(a.num) * i128::from(b.den) == i128::from(b.num) * i128::from(a.den)
}

/// Validate one output's `format` and `slots` against the canvas, its medium's
/// [`SlotCapability`] and the document's declared discrete tracks
/// (`discrete_tracks`, the program bus excluded).
///
/// # Errors
///
/// [`ConfigError::Validation`] prefixed [`SLOT_MEDIUM_MISMATCH`] or
/// [`SLOT_FORMAT_UNSUPPORTED`] (or an unprefixed audio-slot reference error).
pub fn validate_output_format(
    output: &Output,
    canvas: &Canvas,
    discrete_tracks: &[&str],
) -> Result<(), ConfigError> {
    let cap = output.slot_capability();
    if let Some(slots) = output.slots() {
        validate_video_slots(output, cap, &slots.video)?;
        validate_audio_slots(output, cap, &slots.audio, discrete_tracks)?;
        validate_subtitle_slots(output, cap, &slots.subtitle)?;
    }
    if let Some(format) = output.format() {
        validate_format(output, cap, format, canvas)?;
    }
    Ok(())
}

fn validate_video_slots(
    output: &Output,
    cap: SlotCapability,
    video: &[VideoSlot],
) -> Result<(), ConfigError> {
    if video.len() > cap.video_max {
        return Err(mismatch(
            output,
            &format!(
                "declares {} video slot(s); this medium carries {}",
                video.len(),
                cap.video_max
            ),
        ));
    }
    for (i, slot) in video.iter().enumerate() {
        if cap.raw_video && (slot.codec.is_some() || slot.bitrate_kbps.is_some()) {
            return Err(mismatch(
                output,
                &format!("video slot {i} names a codec or bit rate on a raw (unencoded) output"),
            ));
        }
        if let Some(codec) = &slot.codec {
            let carried = CodecFamily::of(codec).is_some_and(|f| cap.video_codecs.contains(&f));
            if !carried {
                return Err(unsupported(
                    output,
                    &format!("video slot {i} codec {codec:?} is not carried by this medium"),
                ));
            }
        }
        if slot
            .bitrate_kbps
            .is_some_and(|kbps| kbps == 0 || kbps > MAX_SLOT_BITRATE_KBPS)
        {
            return Err(unsupported(
                output,
                &format!("video slot {i} bitrate_kbps must be 1..={MAX_SLOT_BITRATE_KBPS}"),
            ));
        }
    }
    Ok(())
}

fn validate_audio_slots(
    output: &Output,
    cap: SlotCapability,
    audio: &[AudioSlot],
    discrete_tracks: &[&str],
) -> Result<(), ConfigError> {
    if audio.is_empty() {
        return Ok(());
    }
    if let Some(max) = cap.audio_max {
        if audio.len() > max {
            return Err(mismatch(
                output,
                &format!(
                    "declares {} audio slot(s); this medium carries {max}",
                    audio.len()
                ),
            ));
        }
    }
    if output.audio().is_some() {
        return Err(ConfigError::Validation(format!(
            "output {:?} declares both an `audio` selection and `slots.audio`; declare one",
            output.id()
        )));
    }
    let mut seen: Vec<&str> = Vec::with_capacity(audio.len());
    for (i, slot) in audio.iter().enumerate() {
        match (i, slot.track.as_deref()) {
            (0, Some(track)) => {
                return Err(ConfigError::Validation(format!(
                    "output {:?}: audio slot 0 is the program mix and names no track (got {track:?})",
                    output.id()
                )));
            }
            (0, None) => {}
            (_, None) => {
                return Err(ConfigError::Validation(format!(
                    "output {:?}: audio slot {i} must name a declared discrete track",
                    output.id()
                )));
            }
            (_, Some(track)) => {
                if !discrete_tracks.contains(&track) {
                    return Err(ConfigError::Validation(format!(
                        "output {:?}: audio slot {i} names undeclared track {track:?}",
                        output.id()
                    )));
                }
                if seen.contains(&track) {
                    return Err(ConfigError::Validation(format!(
                        "output {:?}: audio slot {i} repeats track {track:?}",
                        output.id()
                    )));
                }
                seen.push(track);
            }
        }
        if slot.channels.is_some_and(|c| c != SLOT_AUDIO_CHANNELS) {
            return Err(unsupported(
                output,
                &format!("audio slot {i} channels must be {SLOT_AUDIO_CHANNELS}"),
            ));
        }
        if slot
            .sample_rate
            .is_some_and(|r| r != SLOT_AUDIO_SAMPLE_RATE)
        {
            return Err(unsupported(
                output,
                &format!("audio slot {i} sample_rate must be {SLOT_AUDIO_SAMPLE_RATE}"),
            ));
        }
        if slot
            .codec
            .as_deref()
            .is_some_and(|c| !c.eq_ignore_ascii_case(SLOT_AUDIO_CODEC))
        {
            return Err(unsupported(
                output,
                &format!("audio slot {i} codec must be {SLOT_AUDIO_CODEC:?}"),
            ));
        }
    }
    Ok(())
}

fn validate_subtitle_slots(
    output: &Output,
    cap: SlotCapability,
    subtitle: &[SubtitleSlot],
) -> Result<(), ConfigError> {
    for (i, slot) in subtitle.iter().enumerate() {
        let format = slot.format.as_str();
        if !cap.subtitle_medium_formats.contains(&slot.format) {
            return Err(mismatch(
                output,
                &format!("subtitle slot {i} format {format:?} is not carried by this medium"),
            ));
        }
        if !cap.subtitle_formats.contains(&slot.format) {
            return Err(unsupported(
                output,
                &format!(
                    "subtitle slot {i} format {format:?} is not built yet for this medium \
                     (follow-up)"
                ),
            ));
        }
        if let Some(language) = &slot.language {
            let ok = (1..=MAX_SUBTITLE_LANGUAGE_LEN).contains(&language.len())
                && language
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-');
            if !ok {
                return Err(unsupported(
                    output,
                    &format!(
                        "subtitle slot {i} language {language:?} must be \
                         1..={MAX_SUBTITLE_LANGUAGE_LEN} of [A-Za-z0-9-]"
                    ),
                ));
            }
        }
        if let Some(name) = &slot.name {
            let ok = name.chars().count() <= MAX_SUBTITLE_NAME_LEN
                && !name.chars().any(|c| c.is_control() || c == '"');
            if !ok {
                return Err(unsupported(
                    output,
                    &format!(
                        "subtitle slot {i} name must be at most {MAX_SUBTITLE_NAME_LEN} \
                         characters, no control characters and no '\"'"
                    ),
                ));
            }
        }
    }
    // RFC 8216 §4.3.4.1.1: every member of the master's `subs` group needs a
    // different NAME, so the effective labels (explicit name, else language,
    // else the default) must be distinct.
    for (i, slot) in subtitle.iter().enumerate() {
        let name = slot.effective_name(i);
        if let Some(first) = subtitle
            .iter()
            .take(i)
            .enumerate()
            .find_map(|(j, other)| (other.effective_name(j) == name).then_some(j))
        {
            return Err(unsupported(
                output,
                &format!(
                    "subtitle slot {i} name {name:?} repeats slot {first}'s (the menu \
                     label is the name, else the language); give each slot a distinct name"
                ),
            ));
        }
    }
    if subtitle.len() > cap.subtitle_max {
        return Err(mismatch(
            output,
            &format!(
                "declares {} subtitle slot(s); this medium carries {}",
                subtitle.len(),
                cap.subtitle_max
            ),
        ));
    }
    Ok(())
}

/// Every subtitle slot `source` of `output` must name a declared source
/// (`source_ids`). Called from document validation, where the sources are
/// known.
///
/// # Errors
///
/// [`ConfigError::Validation`] naming the slot and the unknown source.
pub fn validate_subtitle_sources(output: &Output, source_ids: &[&str]) -> Result<(), ConfigError> {
    let Some(slots) = output.slots() else {
        return Ok(());
    };
    for (i, slot) in slots.subtitle.iter().enumerate() {
        if let Some(source) = &slot.source {
            if !source_ids.contains(&source.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "output {:?}: subtitle slot {i} names unknown source {source:?}",
                    output.id()
                )));
            }
        }
    }
    Ok(())
}

fn validate_format(
    output: &Output,
    cap: SlotCapability,
    format: &OutputFormat,
    canvas: &Canvas,
) -> Result<(), ConfigError> {
    let (w, h) = (format.width, format.height);
    let edge_ok = |v: u32| v % 2 == 0 && (MIN_FORMAT_EDGE..=MAX_FORMAT_EDGE).contains(&v);
    if !edge_ok(w) || !edge_ok(h) || u64::from(w) * u64::from(h) > MAX_FORMAT_AREA {
        return Err(unsupported(
            output,
            &format!(
                "format {w}x{h}: each edge must be even and {MIN_FORMAT_EDGE}..={MAX_FORMAT_EDGE}, \
                 area at most {MAX_FORMAT_AREA}"
            ),
        ));
    }
    if let Some(fps) = format.fps {
        if !rationals_equal(fps, canvas.fps) {
            return Err(unsupported(
                output,
                &format!(
                    "format fps {fps} differs from the Program's {} (per-output rates are slice C, #343)",
                    canvas.fps
                ),
            ));
        }
    }
    if !format.scan.is_progressive() {
        return Err(unsupported(
            output,
            "interlaced scan (tff/bff) is not produced yet",
        ));
    }
    if let Some(color) = &format.color {
        if *color != canvas.color.profile {
            return Err(unsupported(
                output,
                &format!(
                    "format color {color:?} differs from the canvas {:?} (colour convert is gated on #310)",
                    canvas.color.profile
                ),
            ));
        }
    }
    if let Some(pf) = &format.pixel_format {
        if !pf.eq_ignore_ascii_case(&canvas.pixel_format) {
            return Err(unsupported(
                output,
                &format!(
                    "format pixel_format {pf:?} differs from the canvas {:?}",
                    canvas.pixel_format
                ),
            ));
        }
    }
    if let Some(aspect) = &format.aspect {
        let parsed = aspect
            .split_once(':')
            .and_then(|(a, b)| Some((a.trim().parse::<u64>().ok()?, b.trim().parse::<u64>().ok()?)))
            .filter(|(a, b)| *a > 0 && *b > 0);
        let g = gcd(u64::from(w), u64::from(h));
        let square = (u64::from(w) / g, u64::from(h) / g);
        // u128: an authored `a:b` of two u64s cannot overflow the product.
        let ok = parsed.is_some_and(|(a, b)| {
            u128::from(a) * u128::from(square.1) == u128::from(b) * u128::from(square.0)
        });
        if !ok {
            return Err(unsupported(
                output,
                &format!(
                    "format aspect {aspect:?} must equal {}:{} (square pixels only)",
                    square.0, square.1
                ),
            ));
        }
    }
    if !format.is_identity_for(canvas) && !cap.format_convertible {
        return Err(unsupported(
            output,
            &format!(
                "format {w}x{h} differs from the canvas {}x{} and this output kind is not \
                 converted in slice B",
                canvas.width, canvas.height
            ),
        ));
    }
    Ok(())
}
