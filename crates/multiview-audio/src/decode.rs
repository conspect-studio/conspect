//! Real audio decode/resample, wired behind the off-by-default `ffmpeg`
//! feature.
//!
//! This module turns a coded audio file into the in-memory [`AudioBlock`]s that
//! the pure-Rust [mixer](crate::mixer) and [meter](crate::loudness) consume. It
//! is the only part of `multiview-audio` that depends on libav — and it does so
//! **without touching it directly**: all demux/decode/resample lives in
//! [`multiview_ffmpeg`], which exposes a pure-Rust seam
//! ([`multiview_ffmpeg::AudioFileDecoder`] yielding
//! [`multiview_ffmpeg::AudioSamplesF32`]) with no libav type in its signature.
//! `multiview-audio` stays `unsafe_code = forbid` and never names a binding type
//! (CLAUDE.md §7).
//!
//! Each decoded source is resampled to the canonical internal representation —
//! 48 kHz, 32-bit-float, the layout the caller selects — matching the engine's
//! mix target (ADR-R005, streaming-gotchas §5: "per-input resample to 48k fltp
//! BEFORE mixing"). Sample rate and channel count are therefore *fixed* by the
//! decoder, independent of the source file.
//!
//! ## Timestamps are input time (invariants #1/#3)
//! [`DecodedBlock::pts`] is the source PTS rebased through the stream
//! time-base only — still input time. The engine rebases cross-source and the
//! output clock re-stamps from the tick counter; nothing here is fed to a muxer.

use multiview_core::time::MediaTime;
use multiview_ffmpeg::{AudioFileDecoder as FfDecoder, AudioSamplesF32};

use crate::error::{AudioError, Result};
use crate::format::{AudioBlock, AudioFormat, ChannelLayout};
use crate::loudness::LoudnessMeter;

/// A decoded + resampled audio block plus its input-time presentation stamp.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct DecodedBlock {
    /// The interleaved 32-bit-float PCM, in the decoder's canonical format.
    pub block: AudioBlock,
    /// Presentation time on the internal nanosecond timeline (input time).
    pub pts: MediaTime,
}

/// A file-backed audio decoder yielding canonical-format [`AudioBlock`]s.
///
/// Wraps the pure [`multiview_ffmpeg::AudioFileDecoder`] seam: every block is
/// resampled to 48 kHz / the chosen [`ChannelLayout`] / 32-bit float, so it can
/// be fed straight into a [`Mixer`](crate::mixer::Mixer) or [`LoudnessMeter`] of
/// the same [`format`](AudioFileDecoder::format).
pub struct AudioFileDecoder {
    inner: FfDecoder,
    format: AudioFormat,
}

impl AudioFileDecoder {
    /// Open `path`, select its best audio stream, and prepare to decode it to
    /// the canonical 48 kHz / `layout` / 32-bit-float representation.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::Decode`] if the container cannot be opened, has no
    /// audio stream, or no decoder can be built for it.
    pub fn open(path: impl AsRef<std::path::Path>, layout: ChannelLayout) -> Result<Self> {
        let channels = u16::try_from(layout.channel_count())
            .map_err(|_| AudioError::InvalidFormat("channel count exceeds u16"))?;
        let inner = FfDecoder::open(path.as_ref(), AudioFormat::CANONICAL_RATE, channels)
            .map_err(|source| AudioError::Decode(source.to_string()))?;
        Ok(Self {
            inner,
            format: AudioFormat::new(AudioFormat::CANONICAL_RATE, layout),
        })
    }

    /// The canonical format every block this decoder yields is in
    /// (48 kHz / the chosen layout).
    #[must_use]
    pub const fn format(&self) -> AudioFormat {
        self.format
    }

    /// Decode and resample the next block, or `Ok(None)` at end-of-stream.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::Decode`] on a libav decode/resample failure, or
    /// [`AudioError::RaggedBlock`] if a decoded block is not a whole number of
    /// frames (a resampler invariant violation — never expected in practice).
    pub fn next_block(&mut self) -> Result<Option<AudioBlock>> {
        match self.next_decoded()? {
            Some(decoded) => Ok(Some(decoded.block)),
            None => Ok(None),
        }
    }

    /// Like [`next_block`](Self::next_block) but also returns the block's
    /// input-time PTS.
    ///
    /// # Errors
    ///
    /// As [`next_block`](Self::next_block).
    pub fn next_decoded(&mut self) -> Result<Option<DecodedBlock>> {
        let Some(samples) = self
            .inner
            .next_block()
            .map_err(|source| AudioError::Decode(source.to_string()))?
        else {
            return Ok(None);
        };
        let block = self.block_from_samples(&samples)?;
        Ok(Some(DecodedBlock {
            block,
            pts: MediaTime::from_nanos(samples.pts_nanos),
        }))
    }

    /// Wrap a pure [`AudioSamplesF32`] into an [`AudioBlock`] of this decoder's
    /// canonical format.
    fn block_from_samples(&self, samples: &AudioSamplesF32) -> Result<AudioBlock> {
        AudioBlock::from_interleaved(self.format, samples.interleaved.clone())
    }
}

/// Decode an entire audio file into one [`LoudnessMeter`] and return it,
/// fully fed (the caller reads M/S/I/LRA/true-peak off the returned meter).
///
/// The file is resampled to 48 kHz / `layout` / 32-bit float before metering,
/// matching the pure-Rust in-memory path exactly, so a decoded clip and an
/// identical in-memory signal meter to the same loudness.
///
/// # Errors
///
/// Returns [`AudioError::Decode`] on an open/decode/resample failure, or
/// [`AudioError::InvalidFormat`] if a meter cannot be built for the format.
pub fn meter_file(
    path: impl AsRef<std::path::Path>,
    layout: ChannelLayout,
) -> Result<LoudnessMeter> {
    let mut decoder = AudioFileDecoder::open(path, layout)?;
    let mut meter = LoudnessMeter::new(decoder.format())?;
    while let Some(block) = decoder.next_block()? {
        meter.push_interleaved(block.interleaved())?;
    }
    Ok(meter)
}
