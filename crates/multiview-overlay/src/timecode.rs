//! Timecode model: embedded (ATC / RP-188 / VITC / LTC) vs generated.
//!
//! Multiview can display either a timecode **embedded** in a source — carried as
//! ancillary data per SMPTE ST 12-1/-2/-3 / RP 188 (ATC), in the vertical
//! interval (VITC), or as a separate linear audio track (LTC) — or a timecode
//! **generated** from the output clock. [`TimecodeModel`] holds both and exposes
//! which is being shown ([`TimecodeModel::displayed`]), preferring an embedded
//! source when present so the on-tile readout follows the source's own house
//! clock.
//!
//! A [`Timecode`] is the SMPTE `HH:MM:SS:FF` value with a drop-frame flag.
//! Drop-frame numbering (29.97 fps) skips frame labels `;00` and `;01` at the
//! start of every minute that is not a multiple of ten — the standard NTSC
//! drop-frame algorithm — so the displayed timecode stays aligned with
//! wall-clock time. All conversions are exact integer arithmetic; frame rates
//! are carried as exact [`Rational`]s ([`TcRate::cadence`]), never as a float
//! fps (invariant #3).

use core::fmt;

use multiview_core::time::Rational;
use serde::{Deserialize, Serialize};

/// A timecode frame rate family.
///
/// Carries an exact [`Rational`] cadence; the drop-frame variant labels frames
/// per the NTSC drop-frame rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TcRate {
    /// 25 fps (PAL), non-drop.
    Fps25,
    /// 30 fps (integer), non-drop.
    Fps30,
    /// 24 fps (film), non-drop.
    Fps24,
    /// 29.97 fps NTSC, drop-frame labelling.
    Fps2997Drop,
}

impl TcRate {
    /// The exact rational cadence (frames per second) — never a float fps.
    #[must_use]
    pub const fn cadence(self) -> Rational {
        match self {
            Self::Fps25 => Rational::FPS_25,
            Self::Fps30 => Rational::FPS_30,
            Self::Fps24 => Rational::new(24, 1),
            Self::Fps2997Drop => Rational::FPS_29_97,
        }
    }

    /// The nominal whole frames-per-second used for the `FF` field counting
    /// (e.g. 30 for 29.97 drop-frame).
    #[must_use]
    pub const fn nominal_frames(self) -> u32 {
        match self {
            Self::Fps24 => 24,
            Self::Fps25 => 25,
            Self::Fps30 | Self::Fps2997Drop => 30,
        }
    }

    /// Whether this rate uses drop-frame labelling.
    #[must_use]
    pub const fn is_drop_frame(self) -> bool {
        matches!(self, Self::Fps2997Drop)
    }
}

/// A SMPTE `HH:MM:SS:FF` timecode value with a drop-frame flag.
///
/// Fields are stored as given; build a normalised value from a running frame
/// count with [`Timecode::from_frame_count`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timecode {
    /// Hours field (`0..24`).
    pub hours: u8,
    /// Minutes field (`0..60`).
    pub minutes: u8,
    /// Seconds field (`0..60`).
    pub seconds: u8,
    /// Frames field (`0..nominal_frames`).
    pub frames: u8,
    /// Whether this timecode is drop-frame (`;` separator before frames).
    pub drop_frame: bool,
}

impl Timecode {
    /// Construct a timecode from explicit fields.
    #[must_use]
    pub const fn new(hours: u8, minutes: u8, seconds: u8, frames: u8, drop_frame: bool) -> Self {
        Self {
            hours,
            minutes,
            seconds,
            frames,
            drop_frame,
        }
    }

    /// Build the displayed timecode from a running `frame_count` at `rate`.
    ///
    /// For [`TcRate::Fps2997Drop`] the standard NTSC drop-frame algorithm is
    /// applied so the labelled timecode tracks wall-clock time. Non-drop rates
    /// decompose the count directly.
    #[must_use]
    pub fn from_frame_count(frame_count: u64, rate: TcRate) -> Self {
        let fps = u64::from(rate.nominal_frames());
        if rate.is_drop_frame() {
            Self::from_drop_frame_count(frame_count)
        } else {
            Self::decompose(frame_count, fps, false)
        }
    }

    /// Decompose a frame count into fields at `fps` (no drop-frame).
    fn decompose(frame_count: u64, fps: u64, drop_frame: bool) -> Self {
        let frames = frame_count % fps;
        let total_seconds = frame_count / fps;
        let seconds = total_seconds % 60;
        let total_minutes = total_seconds / 60;
        let minutes = total_minutes % 60;
        let hours = (total_minutes / 60) % 24;
        Self {
            hours: clamp_u8(hours),
            minutes: clamp_u8(minutes),
            seconds: clamp_u8(seconds),
            frames: clamp_u8(frames),
            drop_frame,
        }
    }

    /// The NTSC drop-frame algorithm for 29.97 fps.
    ///
    /// Two frame labels are dropped at the start of every minute except every
    /// tenth minute: 18 dropped per 10-minute block. We convert the real frame
    /// count to a label count by adding back the dropped labels, then decompose
    /// at 30 fps.
    fn from_drop_frame_count(frame_count: u64) -> Self {
        const FPS: u64 = 30;
        const FRAMES_PER_MINUTE: u64 = FPS * 60; // 1800 (label space, no drops)
        const FRAMES_PER_10MIN: u64 = FRAMES_PER_MINUTE * 10 - 18; // 17982 (real)
        const DROP: u64 = 2;

        let ten_min_blocks = frame_count / FRAMES_PER_10MIN;
        let remainder = frame_count % FRAMES_PER_10MIN;
        // Within a 10-minute block, the first minute has no drop; each later
        // minute dropped 2 labels.
        let minute_in_block = if remainder < FRAMES_PER_MINUTE {
            0
        } else {
            (remainder - FRAMES_PER_MINUTE) / (FRAMES_PER_MINUTE - DROP) + 1
        };
        // Total dropped labels up to this point.
        let dropped = ten_min_blocks * 18 + minute_in_block * DROP;
        let label_count = frame_count + dropped;
        Self::decompose(label_count, FPS, true)
    }

    /// Convert the timecode fields back to a running frame count at `rate`.
    ///
    /// Inverse of [`Timecode::from_frame_count`] for in-range values.
    #[must_use]
    pub fn to_frame_count(self, rate: TcRate) -> u64 {
        let fps = u64::from(rate.nominal_frames());
        let total_minutes = u64::from(self.hours) * 60 + u64::from(self.minutes);
        let label_count =
            ((total_minutes * 60) + u64::from(self.seconds)) * fps + u64::from(self.frames);
        if rate.is_drop_frame() {
            // Remove the dropped labels: 2 per minute except every tenth minute.
            let dropped = DROP_PER_MINUTE * (total_minutes - total_minutes / 10);
            label_count.saturating_sub(dropped)
        } else {
            label_count
        }
    }
}

/// Labels dropped per affected minute in 29.97 drop-frame.
const DROP_PER_MINUTE: u64 = 2;

impl fmt::Display for Timecode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sep = if self.drop_frame { ';' } else { ':' };
        write!(
            f,
            "{:02}:{:02}:{:02}{}{:02}",
            self.hours, self.minutes, self.seconds, sep, self.frames
        )
    }
}

/// Where a displayed timecode came from.
///
/// Serialised tagged (the variant name); never `untagged`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TcSource {
    /// Linear timecode on a separate audio track.
    Ltc,
    /// Vertical-interval timecode in the picture.
    Vitc,
    /// Ancillary timecode per SMPTE RP 188 / ST 12-2 (ATC).
    AtcRp188,
    /// Generated from the output clock (no embedded source).
    Generated,
}

impl TcSource {
    /// A short text label naming the source (accessibility: the readout names
    /// its origin in text, not by colour alone).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Ltc => "LTC",
            Self::Vitc => "VITC",
            Self::AtcRp188 => "ATC",
            Self::Generated => "GEN",
        }
    }

    /// Whether this is an embedded (source-carried) timecode rather than one
    /// generated locally.
    #[must_use]
    pub const fn is_embedded(self) -> bool {
        !matches!(self, Self::Generated)
    }
}

/// A timecode overlay model: a generated timecode and, optionally, an embedded
/// source timecode that takes display precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimecodeModel {
    /// The locally generated timecode (from the output clock).
    pub generated: Timecode,
    /// The embedded source timecode and its [`TcSource`], if one is present.
    pub embedded: Option<(TcSource, Timecode)>,
}

impl TimecodeModel {
    /// Construct a model with only a generated timecode.
    #[must_use]
    pub const fn new(generated: Timecode) -> Self {
        Self {
            generated,
            embedded: None,
        }
    }

    /// Attach an embedded source timecode (takes display precedence).
    #[must_use]
    pub const fn with_embedded(mut self, source: TcSource, tc: Timecode) -> Self {
        self.embedded = Some((source, tc));
        self
    }

    /// The timecode to display and its source: the embedded source when present,
    /// otherwise the generated one ([`TcSource::Generated`]).
    #[must_use]
    pub fn displayed(&self) -> (TcSource, Timecode) {
        match self.embedded {
            Some((source, tc)) => (source, tc),
            None => (TcSource::Generated, self.generated),
        }
    }
}

/// Clamp a `u64` field into a `u8` (fields are provably in range; this is the
/// total, panic-free conversion).
fn clamp_u8(value: u64) -> u8 {
    u8::try_from(value).unwrap_or(u8::MAX)
}
