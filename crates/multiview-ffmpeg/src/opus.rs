//! Opus packet decode + the single program Opus rendition encode
//! (ADR-T014 §5, ADR-0049, ADR-P006).
//!
//! * [`OpusDecoder`] — WHIP audio de-embed: raw Opus packets (the RFC 7587 RTP
//!   payload — one packet per push, **no container, no extradata**) decode to
//!   48 kHz **stereo interleaved-`f32`** [`AudioSamplesF32`] blocks, the shape
//!   the ADR-T013 rebase seam and `AudioStore` consume. libav's native `opus`
//!   decoder handles the extradata-free case with its built-in default header
//!   (≤2 channels, mapping family 0) — exactly the RTP configuration.
//! * [`OpusEncoder`] — the **one** Opus encode per program (encode-once
//!   extends to audio, ADR-E004/ADR-0049): 48 kHz, 20 ms frames, stereo, at a
//!   "CBR-ish" constrained-VBR target (~96–128 kbps; [`PROGRAM_OPUS_BIT_RATE`]
//!   is the ADR-P006 default). Prefers the `libopus` encoder and falls back to
//!   libav's native `opus` (experimental — opened with `strict=experimental`)
//!   when a linked `FFmpeg` lacks libopus. Emits the crate's
//!   [`EncodedPacket`]s tagged [`StreamKind::Audio`](crate::packet::StreamKind)
//!   so the fan-out routes them like any program audio.
//!
//! ## Timing (invariant #3)
//! Decoder block PTS is **input time** (packet PTS rescaled through the
//! declared clock only); the ADR-T013 seam owns wrap/anchor/re-anchor.
//! Encoder packet PTS comes from an internal **sample counter**
//! (`audio_pts = Σ samples`), the audio analogue of the output tick — raw
//! input time never reaches the encoder.
//!
//! ## Bounded memory (CLAUDE.md §7 rule 5)
//! The encoder's staging buffer drains to **less than one 20 ms frame** after
//! every push (a push loops full frames straight into libav), so steady-state
//! buffering is `< frame_samples × 2` floats regardless of caller block size.

use ffmpeg::format::sample::Type as SampleType;
use ffmpeg::format::Sample;
use ffmpeg::ChannelLayout;
use ffmpeg_next as ffmpeg;

use multiview_core::time::{rescale, Rational};

use crate::audio_file::{interleave_fltp, AudioSamplesF32};
use crate::decode::ensure_initialized;
use crate::encode::{AudioEncodeTarget, AudioEncoder};
use crate::encode_options::CodecOptions;
use crate::error::{FfmpegError, Result};
use crate::packet::EncodedPacket;
use crate::resample::{ResampleSpec, Resampler};

/// The Opus codec clock and the canonical program-audio rate (RFC 7587 fixes
/// the RTP clock at 48 kHz; ADR-R005 fixes the program bus there too).
pub const OPUS_SAMPLE_RATE: u32 = 48_000;

/// Program/preview Opus channel count: one stereo pair (single-track audio
/// capability on the WebRTC outputs, ADR-0049).
pub const OPUS_CHANNELS: u16 = 2;

/// Samples per 20 ms Opus frame at 48 kHz — the ADR-0049/ADR-P006 frame size.
/// The encoder asserts the opened codec agrees rather than trusting this
/// constant ([`OpusEncoder::frame_samples`] reports the live value).
pub const OPUS_FRAME_SAMPLES: usize = 960;

/// The ADR-P006 program-rendition bitrate (bits/sec): ~96 kbps constrained
/// VBR, inside the ADR-0049 96–128 kbps band.
pub const PROGRAM_OPUS_BIT_RATE: usize = 96_000;

/// A packet-fed Opus decoder yielding 48 kHz stereo interleaved-`f32` blocks.
///
/// `Send + !Sync`: owns a libav decoder context (and a lazily-built
/// [`Resampler`] for non-stereo/non-`FLTP` outputs) that must not be shared
/// across threads unsynchronized (CLAUDE.md §7).
pub struct OpusDecoder {
    decoder: ffmpeg::decoder::Audio,
    time_base: Rational,
    /// Built lazily from the first decoded frame that is not already
    /// stereo-`FLTP`-48 kHz (a mono publisher, upmixed to the canonical stereo
    /// pair); rebuilt on a mid-stream source change.
    resampler: Option<Resampler>,
    /// The `(format, channels, rate)` the current resampler was built for.
    source_spec: Option<(Sample, u16, u32)>,
}

impl OpusDecoder {
    /// Open libav's native `opus` decoder in the RTP shape: no extradata, no
    /// container parameters — the decoder's built-in default header applies
    /// (48 kHz, ≤2 channels, mapping family 0, RFC 7587's payload config).
    ///
    /// `time_base` is the clock the pushed raw PTS values tick on (the RTP
    /// audio clock is `1/48_000`); it only rescales the blocks' convenience
    /// nanosecond PTS — wrap-aware rebasing is the ADR-T013 seam's job.
    ///
    /// # Errors
    /// * [`FfmpegError::Init`] — global libav init failed.
    /// * [`FfmpegError::CodecNotFound`] — the linked `FFmpeg` has no Opus
    ///   decoder.
    /// * [`FfmpegError::OpenDecoder`] — the decoder could not be opened.
    pub fn new(time_base: Rational) -> Result<Self> {
        ensure_initialized()?;
        let codec = ffmpeg::decoder::find(ffmpeg::codec::Id::OPUS)
            .ok_or(FfmpegError::CodecNotFound("opus"))?;
        let ctx = ffmpeg::codec::context::Context::new_with_codec(codec);
        let decoder = ctx.decoder().audio().map_err(FfmpegError::OpenDecoder)?;
        Ok(Self {
            decoder,
            time_base,
            resampler: None,
            source_spec: None,
        })
    }

    /// Push one raw Opus packet (one RTP payload) with its raw PTS in
    /// `time_base` ticks. Non-blocking; an empty push is a no-op. Drain with
    /// [`receive_block`](Self::receive_block).
    ///
    /// # Errors
    /// Returns [`FfmpegError::Decode`] on a libav send error.
    pub fn push(&mut self, packet: &[u8], raw_pts: Option<i64>) -> Result<()> {
        if packet.is_empty() {
            return Ok(());
        }
        let mut pkt = ffmpeg::codec::packet::Packet::copy(packet);
        pkt.set_pts(raw_pts);
        self.decoder.send_packet(&pkt).map_err(FfmpegError::Decode)
    }

    /// Pull the next decoded block as 48 kHz stereo interleaved `f32`, or
    /// `Ok(None)` when the decoder needs more input / is fully drained.
    ///
    /// # Errors
    /// * [`FfmpegError::Decode`] — a real libav decode error.
    /// * [`FfmpegError::Convert`] — libswresample failed.
    /// * [`FfmpegError::FrameMismatch`] — an impossible frame shape surfaced.
    pub fn receive_block(&mut self) -> Result<Option<AudioSamplesF32>> {
        let mut decoded = ffmpeg::util::frame::Audio::empty();
        match self.decoder.receive_frame(&mut decoded) {
            Ok(()) => {}
            Err(
                ffmpeg::Error::Other {
                    errno: ffmpeg::util::error::EAGAIN,
                }
                | ffmpeg::Error::Eof,
            ) => return Ok(None),
            Err(other) => return Err(FfmpegError::Decode(other)),
        }

        let pts_nanos = decoded
            .pts()
            .map_or(0, |ticks| rescale(ticks, self.time_base, NANOS_TB));

        // Fast path: the native opus decoder emits FLTP at 48 kHz; a stereo
        // stream needs no resample, only the planar->interleaved copy.
        let interleaved = if decoded.format() == Sample::F32(SampleType::Planar)
            && decoded.channels() == OPUS_CHANNELS
            && decoded.rate() == OPUS_SAMPLE_RATE
        {
            interleave_fltp(&decoded, OPUS_CHANNELS)?
        } else {
            // Mono (or any other shape): resample/upmix to the canonical
            // stereo FLTP pair, then interleave.
            self.ensure_resampler(&decoded)?;
            let resampler = self
                .resampler
                .as_mut()
                .ok_or(FfmpegError::FrameMismatch("resampler unexpectedly absent"))?;
            let converted = resampler.run(&decoded)?;
            interleave_fltp(&converted, OPUS_CHANNELS)?
        };

        Ok(Some(AudioSamplesF32 {
            interleaved,
            rate: OPUS_SAMPLE_RATE,
            channels: OPUS_CHANNELS,
            pts_nanos,
        }))
    }

    /// Signal end-of-stream (session teardown) so buffered frames drain.
    ///
    /// # Errors
    /// Returns [`FfmpegError::Decode`] on a libav error.
    pub fn send_eof(&mut self) -> Result<()> {
        self.decoder.send_eof().map_err(FfmpegError::Decode)
    }

    /// Build (or rebuild on a source change) the to-stereo-48 kHz resampler.
    fn ensure_resampler(&mut self, decoded: &ffmpeg::util::frame::Audio) -> Result<()> {
        let spec = (decoded.format(), decoded.channels(), decoded.rate());
        if self.source_spec == Some(spec) && self.resampler.is_some() {
            return Ok(());
        }
        let src_layout = if decoded.channel_layout().is_empty() {
            ChannelLayout::default(i32::from(decoded.channels().max(1)))
        } else {
            decoded.channel_layout()
        };
        let src = ResampleSpec::new(decoded.format(), src_layout, decoded.rate().max(1));
        let dst = ResampleSpec::new(
            Sample::F32(SampleType::Planar),
            ChannelLayout::STEREO,
            OPUS_SAMPLE_RATE,
        );
        self.resampler = Some(Resampler::new(src, dst)?);
        self.source_spec = Some(spec);
        Ok(())
    }
}

/// The nanosecond timeline rational shared by the decode paths.
const NANOS_TB: Rational = Rational::new(1, 1_000_000_000);

/// How the opened encoder wants its `f32` samples laid out.
enum PcmLayout {
    /// `libopus`: packed/interleaved `FLT`.
    Packed,
    /// Native `opus`: planar `FLTP` (de-interleaved into scratch planes).
    Planar,
}

/// The single program Opus rendition encoder (ADR-0049): 48 kHz / 20 ms /
/// stereo, `libopus` preferred with the native-`opus` fallback, fed
/// interleaved `f32` program audio and emitting audio-tagged
/// [`EncodedPacket`]s.
///
/// `Send + !Sync`: owns the underlying [`AudioEncoder`] context.
pub struct OpusEncoder {
    encoder: AudioEncoder,
    encoder_name: &'static str,
    layout: PcmLayout,
    /// Interleaved staging: drains below one frame (`frame_samples * 2`
    /// floats) on every push — bounded by construction.
    pending: Vec<f32>,
    /// De-interleave scratch planes (planar fallback only); reused per frame
    /// so the steady state allocates nothing.
    scratch_left: Vec<f32>,
    scratch_right: Vec<f32>,
    /// Samples per coded frame the opened encoder requires (960 = 20 ms).
    frame_samples: usize,
    /// Running sample counter — the next frame's PTS in `1/48_000`
    /// (invariant #3: a counter, never input time).
    next_pts: i64,
}

impl OpusEncoder {
    /// Open the program Opus encoder at `bit_rate` bits/sec (see
    /// [`PROGRAM_OPUS_BIT_RATE`]): `libopus` (packed `FLT`, 20 ms frames,
    /// constrained VBR — the "CBR-ish" ADR-0049 shape) when the linked
    /// `FFmpeg` has it, else libav's native `opus` encoder (planar `FLTP`,
    /// experimental — opened with `strict=experimental`).
    ///
    /// # Errors
    /// * [`FfmpegError::CodecNotFound`] — the linked `FFmpeg` has no Opus
    ///   encoder at all.
    /// * [`FfmpegError::OpenEncoder`] — libav rejected the configuration.
    pub fn new(bit_rate: usize) -> Result<Self> {
        ensure_initialized()?;
        let (encoder_name, layout, format, options) =
            if ffmpeg::encoder::find_by_name("libopus").is_some() {
                (
                    "libopus",
                    PcmLayout::Packed,
                    Sample::F32(SampleType::Packed),
                    CodecOptions::new()
                        .try_set("vbr", "constrained")
                        .and_then(|o| o.try_set("frame_duration", "20"))
                        .and_then(|o| o.try_set("application", "audio")),
                )
            } else {
                (
                    "opus",
                    PcmLayout::Planar,
                    Sample::F32(SampleType::Planar),
                    CodecOptions::new().try_set("strict", "experimental"),
                )
            };
        // The literals above carry no NUL; an error here is impossible, but it
        // is propagated typed rather than unwrapped (no panic paths).
        let options = options.map_err(|_| FfmpegError::FrameMismatch("opus option literals"))?;

        let target = AudioEncodeTarget {
            codec_name: encoder_name.to_owned(),
            format,
            channel_layout: ChannelLayout::STEREO,
            sample_rate: OPUS_SAMPLE_RATE,
            bit_rate,
        };
        let encoder = AudioEncoder::new_with_options(&target, &options)?;
        let frame_samples = usize::try_from(encoder.frame_size())
            .ok()
            .filter(|&n| n > 0)
            .unwrap_or(OPUS_FRAME_SAMPLES);
        Ok(Self {
            encoder,
            encoder_name,
            layout,
            pending: Vec::with_capacity(frame_samples.saturating_mul(4)),
            scratch_left: vec![0.0; frame_samples],
            scratch_right: vec![0.0; frame_samples],
            frame_samples,
            next_pts: 0,
        })
    }

    /// The concrete libav encoder this opened with (`"libopus"` or `"opus"`).
    #[must_use]
    pub const fn encoder_name(&self) -> &'static str {
        self.encoder_name
    }

    /// Samples per coded frame (960 at the 20 ms / 48 kHz profile).
    #[must_use]
    pub const fn frame_samples(&self) -> usize {
        self.frame_samples
    }

    /// Push a block of interleaved stereo `f32` program audio
    /// (`[l0, r0, l1, r1, …]`, any length). Full 20 ms frames are sent to the
    /// codec immediately (the staging buffer stays below one frame); drain
    /// coded packets with [`receive_packet`](Self::receive_packet).
    ///
    /// # Errors
    /// * [`FfmpegError::FrameMismatch`] — `samples` has an odd length (a torn
    ///   stereo pair is a wiring bug upstream).
    /// * [`FfmpegError::Encode`] — a libav send error.
    pub fn push_interleaved_f32(&mut self, samples: &[f32]) -> Result<()> {
        if samples.len() % 2 != 0 {
            return Err(FfmpegError::FrameMismatch(
                "interleaved stereo block must have an even sample count",
            ));
        }
        self.pending.extend_from_slice(samples);
        self.drain_full_frames()
    }

    /// Flush: silence-pad any final partial frame to a full 20 ms frame (so no
    /// pushed audio is lost) and signal EOF. Drain remaining packets with
    /// [`receive_packet`](Self::receive_packet).
    ///
    /// # Errors
    /// Returns [`FfmpegError::Encode`] on a libav error.
    pub fn finish(&mut self) -> Result<()> {
        if !self.pending.is_empty() {
            self.pending
                .resize(self.frame_samples.saturating_mul(2), 0.0);
            self.drain_full_frames()?;
        }
        self.encoder.send_eof()
    }

    /// Pull the next coded packet, tagged [`StreamKind::Audio`](crate::packet::StreamKind),
    /// or `Ok(None)` when the codec needs more input / is fully drained.
    ///
    /// # Errors
    /// Returns [`FfmpegError::Encode`] for a real libav error.
    pub fn receive_packet(&mut self) -> Result<Option<EncodedPacket>> {
        Ok(self
            .encoder
            .receive_packet()?
            .map(EncodedPacket::from_audio_packet))
    }

    /// Send every complete frame currently staged, stamping each from the
    /// running sample counter. Allocation-free in steady state: frames are
    /// read in place from the staging buffer (one `drain` compaction at the
    /// end), and the planar path reuses its scratch planes.
    fn drain_full_frames(&mut self) -> Result<()> {
        let per_frame = self.frame_samples.saturating_mul(2);
        if per_frame == 0 {
            return Ok(());
        }
        let mut offset = 0_usize;
        while let Some(frame) = self.pending.get(offset..offset.saturating_add(per_frame)) {
            let pts = self.next_pts;
            self.next_pts = self
                .next_pts
                .saturating_add(i64::try_from(self.frame_samples).unwrap_or(0));
            match self.layout {
                PcmLayout::Packed => {
                    self.encoder
                        .send_interleaved_f32(frame, self.frame_samples, pts)?;
                }
                PcmLayout::Planar => {
                    for ((pair, l), r) in frame
                        .chunks_exact(2)
                        .zip(self.scratch_left.iter_mut())
                        .zip(self.scratch_right.iter_mut())
                    {
                        if let &[left, right] = pair {
                            *l = left;
                            *r = right;
                        }
                    }
                    let planes: [&[f32]; 2] = [&self.scratch_left, &self.scratch_right];
                    self.encoder
                        .send_planar_f32(&planes, self.frame_samples, pts)?;
                }
            }
            offset = offset.saturating_add(per_frame);
        }
        if offset > 0 {
            self.pending.drain(..offset);
        }
        Ok(())
    }
}
