//! Audio format primitives: channel layout, [`AudioFormat`], and the in-memory
//! [`AudioBlock`] of planar/interleaved PCM that the routing model operates on.
//!
//! The canonical internal representation is 48 kHz, 32-bit float, matching the
//! engine's resample target (ADR-R005). Samples are nominally in `[-1.0, 1.0]`.
use serde::{Deserialize, Serialize};

use crate::error::{AudioError, Result};

/// A speaker/channel layout.
///
/// Only the layouts Multiview's program bus and discrete-track model need are
/// enumerated; the channel weighting for loudness follows ITU-R BS.1770
/// (front/centre unity, surround +1.5 dB, LFE excluded).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind")]
#[non_exhaustive]
pub enum ChannelLayout {
    /// Single channel.
    Mono,
    /// Two channels: L, R.
    Stereo,
    /// Six channels: L, R, C, LFE, Ls, Rs (the BS.1770 5.1 ordering).
    FivePointOne,
}

impl ChannelLayout {
    /// Number of channels in this layout.
    #[must_use]
    pub const fn channel_count(self) -> usize {
        match self {
            Self::Mono => 1,
            Self::Stereo => 2,
            Self::FivePointOne => 6,
        }
    }
}

/// A PCM stream description: sample rate and channel layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AudioFormat {
    sample_rate: u32,
    layout: ChannelLayout,
}

impl AudioFormat {
    /// The canonical internal sample rate (Hz).
    pub const CANONICAL_RATE: u32 = 48_000;

    /// Construct a format from a sample rate and channel layout.
    #[must_use]
    pub const fn new(sample_rate: u32, layout: ChannelLayout) -> Self {
        Self {
            sample_rate,
            layout,
        }
    }

    /// Sample rate in Hz.
    #[must_use]
    pub const fn sample_rate(self) -> u32 {
        self.sample_rate
    }

    /// Channel layout.
    #[must_use]
    pub const fn channel_layout(self) -> ChannelLayout {
        self.layout
    }

    /// Number of channels.
    #[must_use]
    pub const fn channel_count(self) -> usize {
        self.layout.channel_count()
    }

    /// Whether this format is usable for metering/mixing (non-zero rate and
    /// channels).
    #[must_use]
    pub const fn is_valid(self) -> bool {
        self.sample_rate != 0 && self.layout.channel_count() != 0
    }
}

/// A block of interleaved 32-bit-float PCM with a known [`AudioFormat`].
///
/// Interleaving is frame-major: for stereo, `[l0, r0, l1, r1, ...]`. The block
/// is the unit the [mixer](crate::mixer) and [meter](crate::loudness) consume.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioBlock {
    format: AudioFormat,
    /// Interleaved samples, length == `frame_count * channel_count`.
    samples: Vec<f32>,
}

impl AudioBlock {
    /// Build a block from interleaved samples.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::RaggedBlock`] if `samples.len()` is not a whole
    /// multiple of the layout's channel count.
    pub fn from_interleaved(format: AudioFormat, samples: Vec<f32>) -> Result<Self> {
        let channels = format.channel_count();
        if channels == 0 || samples.len() % channels != 0 {
            return Err(AudioError::RaggedBlock {
                samples: samples.len(),
                channels,
            });
        }
        Ok(Self { format, samples })
    }

    /// A block of `frames` frames of silence in the given format.
    #[must_use]
    pub fn silence(format: AudioFormat, frames: usize) -> Self {
        let channels = format.channel_count();
        Self {
            format,
            samples: vec![0.0; frames.saturating_mul(channels)],
        }
    }

    /// This block's format.
    #[must_use]
    pub const fn format(&self) -> AudioFormat {
        self.format
    }

    /// The interleaved samples.
    #[must_use]
    pub fn interleaved(&self) -> &[f32] {
        &self.samples
    }

    /// Number of frames (samples per channel).
    #[must_use]
    pub fn frame_count(&self) -> usize {
        self.samples
            .len()
            .checked_div(self.format.channel_count())
            .unwrap_or(0)
    }
}
