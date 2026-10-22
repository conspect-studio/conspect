//! A self-contained file → interleaved-`f32` audio decode primitive (the
//! `ffmpeg` feature).
//!
//! [`AudioFileDecoder`] composes the crate's existing safe wrappers — a
//! [`Demuxer`] feeding a [`StreamAudioDecoder`] whose frames are converted to
//! 32-bit-float planar at a caller-chosen sample rate and channel count by a
//! [`Resampler`] — and yields
//! **pure-Rust** [`AudioSamplesF32`] blocks: interleaved `f32` plus the rate,
//! channel count, and an input-time PTS in nanoseconds. **No libav type appears
//! in this module's public API**, so a pure-Rust crate (e.g. `multiview-audio`)
//! can drive a real decode without ever touching the binding or `unsafe`.
//!
//! ## Why this seam exists
//! The lower-level [`Resampler`] / [`StreamAudioDecoder`] operate on
//! `ffmpeg_next::frame::Audio`. Extracting samples from such a frame requires
//! reading its planes — knowledge that must live in the one crate allowed raw
//! libav access (CLAUDE.md §7). This module is that boundary: it owns the frame
//! and exposes only plain numbers.
//!
//! ## Timestamps are *input* time (invariants #1/#3)
//! [`AudioSamplesF32::pts_nanos`] is the source PTS rebased through the stream
//! time-base only — still input time. The engine rebases cross-source and the
//! output clock re-stamps from the tick counter; nothing here is fed to a muxer.
//!
//! ## Licensing
//! Decode uses whatever LGPL software decoder the linked `FFmpeg` provides for
//! the input codec; nothing here pulls a GPL encoder.

use std::path::Path;

use ffmpeg::format::sample::Type as SampleType;
use ffmpeg::format::Sample;
use ffmpeg::util::frame::Audio;
use ffmpeg::ChannelLayout;
use ffmpeg_next as ffmpeg;

use multiview_core::time::{MediaTime, Rational};

use crate::convert::MediaKind;
use crate::decode_stream::StreamAudioDecoder;
use crate::demux::Demuxer;
use crate::error::{FfmpegError, Result};
use crate::resample::{ResampleSpec, Resampler};

/// One decoded + resampled audio block as pure interleaved `f32`.
///
/// Frame-major interleave: for stereo, `[l0, r0, l1, r1, …]`. The length is
/// always `frames * channels`.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct AudioSamplesF32 {
    /// Interleaved 32-bit-float samples, length `frames * channels`.
    pub interleaved: Vec<f32>,
    /// Sample rate in Hz (the resample target).
    pub rate: u32,
    /// Channel count (the resample target).
    pub channels: u16,
    /// Presentation time on the internal nanosecond timeline (input time).
    pub pts_nanos: i64,
}

impl AudioSamplesF32 {
    /// Number of audio frames (samples per channel) in this block.
    #[must_use]
    pub fn frame_count(&self) -> usize {
        self.interleaved
            .len()
            .checked_div(usize::from(self.channels.max(1)))
            .unwrap_or(0)
    }
}

/// A file-backed audio decoder yielding interleaved-`f32` blocks resampled to a
/// fixed target rate and channel count.
///
/// `Send + !Sync`: it owns libav contexts (a [`Demuxer`], a
/// [`StreamAudioDecoder`], and a lazily-built [`Resampler`]) that must not be
/// shared across threads unsynchronized (CLAUDE.md §7).
pub struct AudioFileDecoder {
    demuxer: Demuxer,
    decoder: StreamAudioDecoder,
    stream_index: usize,
    target_rate: u32,
    target_channels: u16,
    target_layout: ChannelLayout,
    /// Built from the first decoded frame (whose format/layout/rate are then
    /// known); rebuilt if a mid-stream source change is observed.
    resampler: Option<Resampler>,
    /// The source `(format, layout-channels, rate)` the current resampler was
    /// built for, used to detect a mid-stream change.
    source_spec: Option<(Sample, u16, u32)>,
    /// Set once the demuxer is drained and the decoder flushed.
    finished: bool,
    /// Pending input packets have all been sent; only draining remains.
    eof_sent: bool,
}

impl AudioFileDecoder {
    /// Open `path`, select its best audio stream, and prepare to decode it to
    /// interleaved `f32` at `target_rate` Hz / `target_channels` channels.
    ///
    /// # Errors
    /// * [`FfmpegError::OpenInput`] — the container could not be opened/probed.
    /// * [`FfmpegError::StreamNotFound`] — no audio stream is present.
    /// * [`FfmpegError::OpenDecoder`] — a decoder could not be built.
    /// * [`FfmpegError::FrameMismatch`] — `target_channels` is zero, or the
    ///   stream parameters could not be resolved.
    pub fn open(path: &Path, target_rate: u32, target_channels: u16) -> Result<Self> {
        if target_rate == 0 || target_channels == 0 {
            return Err(FfmpegError::FrameMismatch(
                "target rate and channel count must be non-zero",
            ));
        }
        let demuxer = Demuxer::open(path)?;
        let stream_index = demuxer
            .best_stream(MediaKind::Audio)
            .ok_or(FfmpegError::StreamNotFound("audio"))?;

        // Find the matching stream's parameters + time-base to build the decoder.
        let (parameters, time_base) = Self::stream_params(path, stream_index)?;
        let decoder = StreamAudioDecoder::new(parameters, time_base)?;

        let target_layout = ChannelLayout::default(i32::from(target_channels));

        Ok(Self {
            demuxer,
            decoder,
            stream_index,
            target_rate,
            target_channels,
            target_layout,
            resampler: None,
            source_spec: None,
            finished: false,
            eof_sent: false,
        })
    }

    /// Re-open the container briefly to read the chosen stream's codec
    /// parameters and time-base (the [`Demuxer`] does not expose the raw
    /// `Parameters` needed to construct a decoder).
    fn stream_params(
        path: &Path,
        stream_index: usize,
    ) -> Result<(ffmpeg::codec::Parameters, Rational)> {
        let input = ffmpeg::format::input(&path).map_err(|source| FfmpegError::OpenInput {
            path: path.display().to_string(),
            source,
        })?;
        let stream = input
            .streams()
            .nth(stream_index)
            .ok_or(FfmpegError::StreamNotFound("audio"))?;
        let time_base = crate::convert::from_ff_rational(stream.time_base());
        Ok((stream.parameters(), time_base))
    }

    /// The target sample rate in Hz.
    #[must_use]
    pub fn rate(&self) -> u32 {
        self.target_rate
    }

    /// The target channel count.
    #[must_use]
    pub fn channels(&self) -> u16 {
        self.target_channels
    }

    /// Decode and resample the next available block, or `Ok(None)` at
    /// end-of-stream.
    ///
    /// Pumps coded packets into the decoder until a frame emerges (handling
    /// `EAGAIN`), then flushes at end-of-input. Each decoded frame is resampled
    /// to the target `FLTP`/rate/layout and interleaved to `f32`.
    ///
    /// # Errors
    /// * [`FfmpegError::Decode`] — a real libav decode error.
    /// * [`FfmpegError::Convert`] — libswresample failed.
    /// * [`FfmpegError::FrameMismatch`] — the resampled frame was not the
    ///   expected planar-`f32` shape.
    pub fn next_block(&mut self) -> Result<Option<AudioSamplesF32>> {
        loop {
            if self.finished {
                return Ok(None);
            }

            // Try to pull an already-buffered frame first.
            if let Some(frame) = self.decoder.receive_frame()? {
                return self.convert(frame.frame, frame.pts).map(Some);
            }

            if self.eof_sent {
                // Decoder fully drained after EOF.
                self.finished = true;
                return Ok(None);
            }

            // Need more input: feed the next packet for our stream, or signal
            // EOF when the demuxer is exhausted.
            if let Some(pkt) = self.demuxer.read_packet_for(self.stream_index)? {
                self.decoder.send_packet(&pkt.packet)?;
            } else {
                self.decoder.send_eof()?;
                self.eof_sent = true;
            }
        }
    }

    /// Resample one decoded frame to the target FLTP/rate/layout and interleave
    /// it to `f32`.
    ///
    /// Takes the frame by value so it can normalize an unspecified-order channel
    /// layout in place (see [`normalize_layout`]) before the resampler sees it.
    fn convert(&mut self, mut decoded: Audio, pts: MediaTime) -> Result<AudioSamplesF32> {
        // Give the frame a concrete native layout if the demuxer left it
        // unspecified, so both the resampler's input definition and the frame
        // libswresample receives agree (else it errors "Input changed").
        let normalized = normalize_layout(decoded.channel_layout(), decoded.channels());
        decoded.set_channel_layout(normalized);

        self.ensure_resampler(&decoded)?;
        let resampler = self
            .resampler
            .as_mut()
            .ok_or(FfmpegError::FrameMismatch("resampler unexpectedly absent"))?;
        let converted = resampler.run(&decoded)?;
        let interleaved = interleave_fltp(&converted, self.target_channels)?;
        Ok(AudioSamplesF32 {
            interleaved,
            rate: self.target_rate,
            channels: self.target_channels,
            pts_nanos: pts.as_nanos(),
        })
    }

    /// Build (or rebuild on a source change) the resampler from a decoded
    /// frame's actual format/layout/rate to the fixed `FLTP` target.
    ///
    /// The frame's channel layout has already been normalized by
    /// [`Self::convert`].
    fn ensure_resampler(&mut self, decoded: &Audio) -> Result<()> {
        let src_format = decoded.format();
        let src_channels = decoded.channels();
        let src_rate = decoded.rate();
        let spec = (src_format, src_channels, src_rate);
        if self.source_spec == Some(spec) && self.resampler.is_some() {
            return Ok(());
        }

        let src_layout = normalize_layout(decoded.channel_layout(), src_channels);
        let src = ResampleSpec::new(src_format, src_layout, src_rate.max(1));
        let dst = ResampleSpec::new(
            Sample::F32(SampleType::Planar),
            self.target_layout,
            self.target_rate,
        );
        self.resampler = Some(Resampler::new(src, dst)?);
        self.source_spec = Some(spec);
        Ok(())
    }
}

/// Replace an unspecified-order channel layout with the default native layout
/// for `channels`, leaving a concrete layout untouched. Gives libswresample a
/// stable input definition for sources (WAV/PCM) that omit the layout.
fn normalize_layout(layout: ChannelLayout, channels: u16) -> ChannelLayout {
    if layout.is_empty() || layout.channels() <= 0 {
        ChannelLayout::default(i32::from(channels.max(1)))
    } else {
        layout
    }
}

/// Interleave a planar-`f32` (`FLTP`) audio frame into a single `Vec<f32>`,
/// frame-major. Validates the frame really is planar `f32` with the expected
/// channel count first, so the underlying plane access cannot trip libav's
/// out-of-bounds / wrong-type guards (no panic on the decode path, CLAUDE.md §7).
///
/// Crate-internal: shared with the packet-fed [`OpusDecoder`](crate::opus::OpusDecoder)
/// (ADR-T014 §5), which emits the same [`AudioSamplesF32`] block shape.
///
/// # Errors
/// Returns [`FfmpegError::FrameMismatch`] if the frame is not planar `f32` or
/// its plane count disagrees with `channels`.
pub(crate) fn interleave_fltp(frame: &Audio, channels: u16) -> Result<Vec<f32>> {
    if frame.format() != Sample::F32(SampleType::Planar) {
        return Err(FfmpegError::FrameMismatch(
            "resampled frame is not planar f32 (FLTP)",
        ));
    }
    let ch = usize::from(channels);
    // For planar audio `planes() == channels`; guard so each `plane()` index is
    // valid and the per-plane element type (`f32`) check inside libav passes.
    if frame.planes() != ch || ch == 0 {
        return Err(FfmpegError::FrameMismatch(
            "planar audio plane count does not match the channel count",
        ));
    }
    let samples = frame.samples();
    let mut out = vec![0.0f32; samples.saturating_mul(ch)];
    for c in 0..ch {
        let plane: &[f32] = frame.plane(c);
        for (i, &v) in plane.iter().enumerate().take(samples) {
            // dst index = i * ch + c, always < out.len() by construction.
            if let Some(slot) = out.get_mut(i.saturating_mul(ch).saturating_add(c)) {
                *slot = v;
            }
        }
    }
    Ok(out)
}

// `AudioFileDecoder` owns libav contexts that must not be shared across threads
// without synchronization; like the other decoders here it is `Send` (it may
// move to a decode thread) but intentionally `!Sync`. The wrapped
// `ffmpeg_next` contexts are already `Send` and not `Sync`.
