//! Generic stream decoders that yield frames mapped onto [`multiview_core`] types.
//!
//! Where [`crate::decode::VideoDecoder`] is the minimal first-frame spike, the
//! decoders here are the pipeline-facing primitives: a [`StreamVideoDecoder`]
//! that pumps caller-supplied packets and yields NV12 host frames described by
//! [`multiview_core::frame::FrameMeta`], and a [`StreamAudioDecoder`] that yields
//! raw audio frames. Both pair with [`crate::demux::Demuxer`] (which supplies
//! the packets) and own their decoder context (`Send + !Sync`, freed in `Drop`
//! by `ffmpeg_next`).
//!
//! ## NV12-throughout (invariant #5)
//! Software decoders typically emit planar `YUV420P`. [`StreamVideoDecoder`]
//! transparently converts that to NV12 via an internal [`crate::scale::Scaler`]
//! (libswscale), so every frame leaving this layer is on the canonical NV12
//! timeline. A frame already in NV12/P010 passes through untouched.
//!
//! ## Timestamps are input time (invariants #1/#3)
//! [`DecodedVideoFrame::meta`]'s `pts` is the frame's raw stream PTS rebased to
//! nanoseconds **through the stream time-base only** — it is still *input* time.
//! The engine applies cross-source rebasing and the output clock re-stamps from
//! the tick counter; nothing here is fed to a muxer.

use ffmpeg::format::Pixel;
use ffmpeg::util::frame::{Audio, Video};
use ffmpeg_next as ffmpeg;

use multiview_core::color::ColorInfo;
use multiview_core::frame::FrameMeta;
use multiview_core::pixel::PixelFormat;
use multiview_core::time::{rescale, MediaTime, Rational};

use crate::convert::{color_from_ff, from_ff_rational};
use crate::error::{FfmpegError, Result};
use crate::scale::{ScaleSpec, Scaler};

/// One decoded video frame: the NV12 (or P010) host pixels plus its pure
/// [`FrameMeta`] description.
pub struct DecodedVideoFrame {
    /// The decoded pixels as a host [`Video`] frame in NV12 (or P010 for 10-bit
    /// sources). Borrow planes via [`Video::data`]/[`Video::stride`].
    pub frame: Video,
    /// The pure-Rust metadata describing this frame. Its `pts` is already
    /// rescaled to the canonical nanosecond timeline.
    pub meta: FrameMeta,
    /// The **raw** best-effort presentation timestamp in the source stream's own
    /// time-base ticks (pre-rescale), or `None` when the decoder emitted no
    /// usable timestamp. This is the unwrap-domain value a downstream
    /// [`PtsNormalizer`](multiview_input::normalize::PtsNormalizer) needs to
    /// detect a 33-bit MPEG-TS / 32-bit RTP wrap; `meta.pts` has already been
    /// rescaled and cannot reveal the wrap. Sampled, never used to pace.
    pub raw_pts: Option<i64>,
    /// The embedded CEA-608/708 caption bytes (`AV_FRAME_DATA_A53_CC` side data)
    /// this frame carried, or [`None`] for the common no-caption frame.
    ///
    /// A53 captions are not a separate stream: they ride as side data on the
    /// decoded **video** frames (captions.md §2/§4). The side data does **not**
    /// survive a libswscale pixel-format conversion, so it is extracted here off
    /// the **raw** decoded frame, *before* [`Self::frame`] is converted to NV12 —
    /// this is the only place the runtime can reach it. A downstream embedded-CC
    /// [`CaptionDecoder`](crate::CaptionDecoder) feeds these bytes to `cc_dec`
    /// anchored at [`Self::raw_pts`]. Sampled, never paced (invariants #2/#10).
    pub a53_cc: Option<Vec<u8>>,
}

/// A video decoder that consumes caller-supplied packets and yields NV12 host
/// frames described by [`FrameMeta`].
pub struct StreamVideoDecoder {
    decoder: ffmpeg::decoder::Video,
    time_base: Rational,
    /// Lazily-built converter to NV12 (only when the decoder's output format is
    /// not already a canonical working format). Keyed implicitly by the source
    /// geometry/format it was built for; rebuilt on a mid-stream change.
    to_nv12: Option<Scaler>,
    /// Last emitted presentation time (ns). Feeds the genpts fallback so a frame
    /// that carries no usable timestamp still advances the timeline (invariant
    /// #3) — otherwise every such frame would map to 0 and a downstream PTS
    /// pacer would release the whole stream at once.
    last_pts_ns: Option<i64>,
    /// Inter-frame step (ns) used only by the genpts fallback in
    /// [`Self::next_pts`]. Defaults to the NTSC nominal (`1/29.97`); set from the
    /// stream's declared cadence via [`Self::with_declared_fps`] so 25/24 fps
    /// sources advance at their true rate rather than an NTSC-shaped guess.
    fallback_step_ns: i64,
    /// The concrete NVDEC `*_cuvid` decoder this was opened with (e.g.
    /// `"h264_cuvid"`), or [`None`] when it runs the software decoder. Lets the
    /// caller report which decode path is live (telemetry / the hardware
    /// validation check).
    hw_decoder_name: Option<&'static str>,
    /// The CUDA device context the cuvid decoder is bound to, when the hardware
    /// path is live. Held for the decoder's lifetime so the device outlives every
    /// decode + any [`transfer_hwframe_to_host`](crate::hwframe::transfer_hwframe_to_host)
    /// download. Freed synchronously in `Drop` on the decode thread (CLAUDE.md
    /// §7), never an async destructor. [`None`] on the software path.
    hw_device: Option<crate::hwframe::HwDeviceContext>,
}

impl StreamVideoDecoder {
    /// Default genpts fallback inter-frame step (NTSC nominal `1/29.97`), used
    /// only when the stream declares no usable frame rate. A real cadence is
    /// derived from the declared fps via [`Self::with_declared_fps`].
    const DEFAULT_FALLBACK_STEP_NS: i64 = 33_366_667;

    /// Build a **software** decoder from a [`Demuxer`](crate::demux::Demuxer)
    /// stream's parameters and time-base.
    ///
    /// This always opens the libav software decoder for the stream's codec; it is
    /// the universal fallback the hardware path degrades to. To prefer NVDEC where
    /// available, use [`Self::new_preferring_hw`].
    ///
    /// # Errors
    /// Returns [`FfmpegError::OpenDecoder`] if a decoder cannot be built.
    pub fn new(parameters: ffmpeg::codec::Parameters, time_base: Rational) -> Result<Self> {
        let ctx = ffmpeg::codec::context::Context::from_parameters(parameters)
            .map_err(FfmpegError::OpenDecoder)?;
        let decoder = ctx.decoder().video().map_err(FfmpegError::OpenDecoder)?;
        Ok(Self {
            decoder,
            time_base,
            to_nv12: None,
            last_pts_ns: None,
            fallback_step_ns: Self::DEFAULT_FALLBACK_STEP_NS,
            hw_decoder_name: None,
            hw_device: None,
        })
    }

    /// Wrap an already-opened libav video decoder (crate-internal): the
    /// receive path — NV12 conversion, color detection, A53 extraction, genpts
    /// fallback — for decoders that are **not** built from container
    /// parameters, e.g. the packet-fed WHIP H.264 decoder
    /// ([`H264PacketDecoder`](crate::packet_decode::H264PacketDecoder),
    /// ADR-T014) whose geometry comes from the in-band SPS.
    pub(crate) fn from_parts(decoder: ffmpeg::decoder::Video, time_base: Rational) -> Self {
        Self {
            decoder,
            time_base,
            to_nv12: None,
            last_pts_ns: None,
            fallback_step_ns: Self::DEFAULT_FALLBACK_STEP_NS,
            hw_decoder_name: None,
            hw_device: None,
        }
    }

    /// Build a decoder for `parameters`, **preferring NVDEC hardware decode**
    /// (`*_cuvid`) when `want_hw` is set, the build compiled the `cuda` feature,
    /// and the linked libav offers a cuvid wrapper for the stream's codec — else
    /// (or on any hardware-open failure) the software decoder.
    ///
    /// This is the run-path constructor: it moves 4K H.264/HEVC decode off the
    /// CPU onto the NVDEC ASIC. The selection is
    /// [`select_decoder_for_id`](crate::hwdecode::select_decoder_for_id); opening
    /// the GPU decoder needs a working CUDA device, so a driver/library mismatch
    /// or a GPU-free box falls back to software **gracefully** — the source keeps
    /// running, the output never falters (invariants #1/#2). Decoded CUDA surfaces
    /// are downloaded to host NV12 in [`Self::receive_frame`] (the budgeted
    /// CPU↔GPU copy), so downstream consumers are unchanged (invariant #5).
    ///
    /// The boolean of the returned tuple reports whether the hardware path opened,
    /// so the caller can log it once and surface it as telemetry.
    ///
    /// `cuda_device` is the CUDA enumeration ordinal (e.g. `Some("1")`) the NVDEC
    /// decoder is bound to — the **load-aware admission pick** (ADR-0035 Tier-1 /
    /// the GPU-placement principle: *one chosen device hosts the whole pipeline*).
    /// It is threaded straight to [`HwDeviceContext::create`](crate::hwframe::HwDeviceContext::create)
    /// so decode opens on the SAME physical GPU the wgpu compositor was pinned to
    /// (affinity — no cross-GPU split). `None` selects libav's default CUDA device
    /// (ordinal 0), which is exactly today's behaviour and the lockstep partner of
    /// the compositor's `None` (when admission names no device, neither stage
    /// pins one).
    ///
    /// # Errors
    /// Returns [`FfmpegError::OpenDecoder`] only if **even the software** decoder
    /// cannot be built; a hardware-open failure never surfaces as an error (it is
    /// absorbed by the software fallback).
    pub fn new_preferring_hw(
        parameters: ffmpeg::codec::Parameters,
        time_base: Rational,
        want_hw: bool,
        cuda_device: Option<&str>,
    ) -> Result<(Self, bool)> {
        if let Some(name) = crate::hwdecode::select_decoder_for_id(parameters.id(), want_hw) {
            match Self::try_open_cuvid(parameters.clone(), time_base, name, cuda_device) {
                Ok(decoder) => return Ok((decoder, true)),
                Err(err) => {
                    // Graceful degradation: a driver/library mismatch or no usable
                    // GPU. Log ONCE (rate-limited libav bridge aside, this is a
                    // single per-open warning) and fall through to software so the
                    // tile keeps running rather than crashing the output.
                    tracing::warn!(
                        decoder = name,
                        error = %err,
                        "NVDEC hardware decode unavailable; falling back to software decode"
                    );
                }
            }
        }
        let decoder = Self::new(parameters, time_base)?;
        Ok((decoder, false))
    }

    /// Try to open the named NVDEC `*_cuvid` decoder bound to a CUDA device
    /// context. On success the decoder runs on the GPU and emits host NV12 (the
    /// cuvid wrapper downloads internally) or CUDA surfaces (downloaded in
    /// [`Self::receive_frame`]). Any failure here is recoverable — the caller
    /// degrades to software.
    ///
    /// `cuda_device` selects which physical GPU by its CUDA enumeration ordinal
    /// (e.g. `Some("1")`) — the load-aware admission pick, so decode lands on the
    /// SAME GPU as the compositor (affinity). `None` uses libav's default CUDA
    /// device (ordinal 0).
    fn try_open_cuvid(
        parameters: ffmpeg::codec::Parameters,
        time_base: Rational,
        name: &'static str,
        cuda_device: Option<&str>,
    ) -> Result<Self> {
        let codec =
            ffmpeg::decoder::find_by_name(name).ok_or(FfmpegError::CodecNotFound("cuvid"))?;
        let mut ctx = ffmpeg::codec::context::Context::from_parameters(parameters)
            .map_err(FfmpegError::OpenDecoder)?;

        // A CUDA device context the cuvid decoder runs on, pinned to the
        // admission-chosen ordinal (`cuda_device`) so decode co-locates with the
        // compositor on one GPU (affinity). Requires a working driver/GPU at run
        // time; on a GPU-free box this returns a typed error and the caller falls
        // back to software (never a panic).
        let device = crate::hwframe::HwDeviceContext::create(
            crate::hwdecode::HwDeviceKind::Cuda,
            cuda_device,
        )?;
        // Must be set BEFORE the decoder is opened — libav reads `hw_device_ctx`
        // only during `avcodec_open2`.
        device.attach_to_decoder(&mut ctx)?;

        // Open as the cuvid codec specifically (not the context's own software
        // codec id), then take the video decoder.
        let opened = ctx
            .decoder()
            .open_as(codec)
            .map_err(FfmpegError::OpenDecoder)?;
        let decoder = opened.video().map_err(FfmpegError::OpenDecoder)?;

        Ok(Self {
            decoder,
            time_base,
            to_nv12: None,
            last_pts_ns: None,
            fallback_step_ns: Self::DEFAULT_FALLBACK_STEP_NS,
            hw_decoder_name: Some(name),
            hw_device: Some(device),
        })
    }

    /// The concrete NVDEC `*_cuvid` decoder name this is running on (e.g.
    /// `"h264_cuvid"`), or [`None`] when it runs the software decoder.
    #[must_use]
    pub fn hw_decoder_name(&self) -> Option<&'static str> {
        self.hw_decoder_name
    }

    /// Whether this decoder is running the NVDEC hardware path.
    ///
    /// True exactly when a cuvid decoder opened **and** its CUDA device context
    /// is held for the decoder's lifetime (so every decode + download stays valid
    /// until `Drop`).
    #[must_use]
    pub fn is_hardware(&self) -> bool {
        self.hw_decoder_name.is_some() && self.hw_device.is_some()
    }

    /// Set the genpts fallback inter-frame step from the stream's declared frame
    /// rate (e.g. the demuxer's `avg_frame_rate`). A non-positive or zero rate is
    /// ignored and the NTSC default retained — the cadence is never fabricated
    /// (invariant #3: real timing only, never a float-fps guess). Chainable after
    /// [`Self::new`].
    #[must_use]
    pub fn with_declared_fps(mut self, fps: Option<Rational>) -> Self {
        if let Some(step) = fps.and_then(fallback_step_ns_from_fps) {
            self.fallback_step_ns = step;
        }
        self
    }

    /// Send one coded packet to the decoder.
    ///
    /// # Errors
    /// Returns [`FfmpegError::Decode`] on a libav send error.
    pub fn send_packet(&mut self, packet: &ffmpeg::codec::packet::Packet) -> Result<()> {
        self.decoder
            .send_packet(packet)
            .map_err(FfmpegError::Decode)
    }

    /// Signal end-of-stream so buffered frames can be drained.
    ///
    /// # Errors
    /// Returns [`FfmpegError::Decode`] on a libav error.
    pub fn send_eof(&mut self) -> Result<()> {
        self.decoder.send_eof().map_err(FfmpegError::Decode)
    }

    /// Flush the decoder's buffered state (`avcodec_flush_buffers`), discarding
    /// every buffered/reordered frame so decoding can resume cleanly from a new
    /// position.
    ///
    /// **Call this after every [`Demuxer::seek`](crate::demux::Demuxer::seek)**
    /// (e.g. the media-player loop/vamp wrap): a seek without a flush leaves the
    /// decoder holding pictures decoded *before* the seek — for codecs that
    /// reorder (B-frames) those stale frames would surface after the wrap as
    /// out-of-order garbage. Flushing drops them so the first frame after the
    /// seek is the seek target's frame.
    ///
    /// It is safe to call between packets (including on a fresh or already-drained
    /// decoder, where it is a harmless no-op), and the decoder stays usable for
    /// the next [`send_packet`](Self::send_packet). The genpts fallback origin
    /// ([`next_pts`](Self::next_pts)'s `last_pts_ns`) is reset so a frame that
    /// carries no usable timestamp after the seek re-bases from the new position
    /// rather than from the pre-seek timeline (invariant #3); the NV12 converter
    /// is retained because a seek does not change the source geometry, and
    /// [`ensure_nv12`](Self::ensure_nv12) rebuilds it anyway on any geometry
    /// change.
    ///
    /// # Safety / FFI invariant
    /// The flush runs through `ffmpeg_next`'s safe
    /// [`Decoder::flush`](ffmpeg_next::decoder::decoder::Opened) wrapper, which
    /// owns the single `unsafe` `avcodec_flush_buffers` call (this crate adds no
    /// raw FFI here — the decode path is zero-`unsafe`). The invariant that makes
    /// it sound: the `AVCodecContext` is **owned** by this decoder and accessed
    /// only through `&mut self` (the type is `Send + !Sync`, so libav's
    /// single-threaded-access requirement holds); flushing merely drops buffered
    /// frames and resets internal decode state, which is valid between packets.
    ///
    /// # Errors
    /// Returns [`Result::Ok`] — the underlying libav flush is infallible. The
    /// `Result` return matches the rest of this decoder's API
    /// ([`send_packet`](Self::send_packet)/[`send_eof`](Self::send_eof)) and
    /// leaves room for a future fallible flush without a breaking change.
    pub fn flush(&mut self) -> Result<()> {
        self.decoder.flush();
        // A seek is a deliberate timeline discontinuity: drop the genpts origin
        // so a post-seek frame with no PTS re-bases from the new position.
        self.last_pts_ns = None;
        Ok(())
    }

    /// Pull the next decoded frame, converting to NV12 if needed.
    ///
    /// Returns `Ok(None)` when the decoder needs more input (`EAGAIN`) or is
    /// fully drained (`EOF`).
    ///
    /// # Errors
    /// Returns [`FfmpegError::Decode`] for a real libav error, or
    /// [`FfmpegError::Convert`] if the NV12 conversion fails.
    pub fn receive_frame(&mut self) -> Result<Option<DecodedVideoFrame>> {
        let mut decoded = Video::empty();
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

        // If NVDEC handed back a CUDA device surface, download it to a host frame
        // first — the budgeted CPU↔GPU copy (core-engine §8.1). The `*_cuvid`
        // wrappers usually emit host NV12 directly (internal download), in which
        // case this is a no-op; the predicate keeps the loop correct either way.
        // A transfer failure is recoverable: the frame is dropped (ride last-good)
        // rather than crashing the output (invariants #1/#2).
        let decoded = if crate::hwframe::is_cuda_frame(&decoded) {
            let mut host = Video::empty();
            crate::hwframe::transfer_hwframe_to_host(&decoded, &mut host)?;
            host
        } else {
            decoded
        };

        let color = color_from_ff(
            decoded.color_space(),
            decoded.color_primaries(),
            decoded.color_transfer_characteristic(),
            decoded.color_range(),
        );
        // Prefer the decoder's best-effort timestamp: a bare `.pts()` is
        // frequently absent after decoding (mpeg2/H.264 with B-frames), which
        // would map every frame to 0 and defeat downstream PTS pacing.
        let raw_pts = decoded.timestamp().or_else(|| decoded.pts());
        // Pull the embedded CEA-608/708 A53 caption bytes off the RAW decoded
        // frame, before NV12 conversion strips its side data (captions.md §2/§4);
        // most frames carry none (a cheap `None`). The downstream embedded-CC
        // decoder feeds these to `cc_dec`. Sampled, never paced.
        let a53_cc = crate::caption_decode::extract_a53_cc(&decoded);
        let nv12 = self.ensure_nv12(decoded)?;

        // After `ensure_nv12` the frame is NV12 (8-bit) or P010LE (10-bit); any
        // other format is impossible, so it defaults to NV12-shaped metadata
        // rather than panicking on the hot path (CLAUDE.md §7).
        let multiview_format = match nv12.format() {
            Pixel::P010LE => PixelFormat::P010,
            _ => PixelFormat::Nv12,
        };

        let pts = self.next_pts(raw_pts);
        let meta = FrameMeta {
            pts,
            width: nv12.width(),
            height: nv12.height(),
            format: multiview_format,
            color,
        };
        Ok(Some(DecodedVideoFrame {
            frame: nv12,
            meta,
            raw_pts,
            a53_cc,
        }))
    }

    /// Convert a decoded frame to NV12 if it is not already a canonical working
    /// format, rebuilding the converter when the source geometry/format changes.
    fn ensure_nv12(&mut self, decoded: Video) -> Result<Video> {
        match decoded.format() {
            // Already on the NV12-throughout timeline (or its 10-bit sibling).
            Pixel::NV12 | Pixel::P010LE => Ok(decoded),
            src_fmt => {
                let src = ScaleSpec::new(src_fmt, decoded.width(), decoded.height());
                let dst = ScaleSpec::new(Pixel::NV12, decoded.width(), decoded.height());
                let rebuild = match &self.to_nv12 {
                    Some(s) => s.source() != src || s.destination() != dst,
                    None => true,
                };
                if rebuild {
                    self.to_nv12 = Some(Scaler::new(src, dst)?);
                }
                let scaler = self.to_nv12.as_mut().ok_or(FfmpegError::FrameMismatch(
                    "NV12 scaler unexpectedly absent",
                ))?;
                scaler.run(&decoded)
            }
        }
    }

    /// Rebase a raw stream PTS into the internal nanosecond timeline using the
    /// stream time-base, with a genpts fallback (invariant #3): a frame with no
    /// usable timestamp advances by one nominal frame from the last emitted PTS
    /// rather than collapsing to 0, so a downstream PTS-to-wall-clock pacer keeps
    /// pacing instead of releasing the whole stream at once.
    fn next_pts(&mut self, raw: Option<i64>) -> MediaTime {
        let ns = match raw {
            Some(ticks) => rescale(ticks, self.time_base, Rational::new(1, 1_000_000_000)),
            None => match self.last_pts_ns {
                Some(last) => last.saturating_add(self.fallback_step_ns),
                None => 0,
            },
        };
        self.last_pts_ns = Some(ns);
        MediaTime::from_nanos(ns)
    }
}

/// Derive the genpts fallback inter-frame step (ns) from a declared frame rate
/// (`num/den` fps). Returns `None` for a non-positive or zero rate so the caller
/// keeps its default — the cadence is never fabricated (invariant #3).
///
/// `period_ns = 1e9 / fps = 1e9 * den / num`, computed via the exact-rational
/// [`rescale`] so NTSC fractional rates stay exact (`30000/1001 → 33_366_667`).
fn fallback_step_ns_from_fps(fps: Rational) -> Option<i64> {
    if fps.num <= 0 || fps.den <= 0 {
        return None;
    }
    let step = rescale(
        1_000_000_000,
        Rational::new(fps.den, fps.num),
        Rational::new(1, 1),
    );
    (step > 0).then_some(step)
}

/// A decoded audio frame plus a minimal description.
pub struct DecodedAudioFrame {
    /// The decoded audio samples.
    pub frame: Audio,
    /// Presentation time on the internal nanosecond timeline (input time).
    pub pts: MediaTime,
}

/// An audio decoder consuming caller-supplied packets and yielding raw audio
/// frames (no resample here — that is the audio subsystem's job).
pub struct StreamAudioDecoder {
    decoder: ffmpeg::decoder::Audio,
    time_base: Rational,
}

impl StreamAudioDecoder {
    /// Build an audio decoder from a stream's parameters and time-base.
    ///
    /// # Errors
    /// Returns [`FfmpegError::OpenDecoder`] if a decoder cannot be built.
    pub fn new(parameters: ffmpeg::codec::Parameters, time_base: Rational) -> Result<Self> {
        let ctx = ffmpeg::codec::context::Context::from_parameters(parameters)
            .map_err(FfmpegError::OpenDecoder)?;
        let decoder = ctx.decoder().audio().map_err(FfmpegError::OpenDecoder)?;
        Ok(Self { decoder, time_base })
    }

    /// The decoder's sample rate in Hz.
    #[must_use]
    pub fn sample_rate(&self) -> u32 {
        self.decoder.rate()
    }

    /// The decoder's channel count.
    #[must_use]
    pub fn channels(&self) -> u16 {
        self.decoder.channels()
    }

    /// Send one coded packet to the decoder.
    ///
    /// # Errors
    /// Returns [`FfmpegError::Decode`] on a libav send error.
    pub fn send_packet(&mut self, packet: &ffmpeg::codec::packet::Packet) -> Result<()> {
        self.decoder
            .send_packet(packet)
            .map_err(FfmpegError::Decode)
    }

    /// Signal end-of-stream so buffered frames can be drained.
    ///
    /// # Errors
    /// Returns [`FfmpegError::Decode`] on a libav error.
    pub fn send_eof(&mut self) -> Result<()> {
        self.decoder.send_eof().map_err(FfmpegError::Decode)
    }

    /// Pull the next decoded audio frame, or `Ok(None)` on `EAGAIN`/`EOF`.
    ///
    /// # Errors
    /// Returns [`FfmpegError::Decode`] for a real libav error.
    pub fn receive_frame(&mut self) -> Result<Option<DecodedAudioFrame>> {
        let mut decoded = Audio::empty();
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
        let pts = match decoded.pts() {
            Some(ticks) => MediaTime::from_nanos(rescale(
                ticks,
                self.time_base,
                Rational::new(1, 1_000_000_000),
            )),
            None => MediaTime::ZERO,
        };
        Ok(Some(DecodedAudioFrame {
            frame: decoded,
            pts,
        }))
    }
}

/// Convenience: the [`multiview_core`] [`ColorInfo`] of a decoded frame after the
/// untagged-default policy is applied for its geometry.
///
/// Useful when a caller wants the *resolved* color (matrix/primaries inferred
/// from size) rather than the raw, possibly-`Unspecified` tags.
#[must_use]
pub fn resolved_color(meta: &FrameMeta) -> ColorInfo {
    meta.color.resolve_defaults(meta.width, meta.height)
}

/// The stream time-base helper used to convert this decoder's raw PTS values;
/// exposed for callers that want to rebase packets the same way.
#[must_use]
pub fn nanos_from_ticks(ticks: i64, time_base: ffmpeg::Rational) -> i64 {
    rescale(
        ticks,
        from_ff_rational(time_base),
        Rational::new(1, 1_000_000_000),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_step_matches_declared_cadence() {
        // PAL 25 fps → 40.000 ms; the bug was every source advancing at the
        // NTSC-shaped ~33.367 ms regardless of its real rate.
        assert_eq!(
            fallback_step_ns_from_fps(Rational::new(25, 1)),
            Some(40_000_000)
        );
        // Film 24 fps → 41.666… ms (rounded half away from zero).
        assert_eq!(
            fallback_step_ns_from_fps(Rational::new(24, 1)),
            Some(41_666_667)
        );
        // 50 fps → 20 ms.
        assert_eq!(
            fallback_step_ns_from_fps(Rational::new(50, 1)),
            Some(20_000_000)
        );
    }

    #[test]
    fn fallback_step_keeps_ntsc_rates_exact() {
        // 29.97 (30000/1001) reproduces the historical default constant exactly.
        assert_eq!(
            fallback_step_ns_from_fps(Rational::FPS_29_97),
            Some(StreamVideoDecoder::DEFAULT_FALLBACK_STEP_NS)
        );
        // 23.976 film (24000/1001) → ~41.708 ms.
        assert_eq!(
            fallback_step_ns_from_fps(Rational::FPS_23_976),
            Some(41_708_333)
        );
        // 59.94 (60000/1001) → ~16.683 ms.
        assert_eq!(
            fallback_step_ns_from_fps(Rational::FPS_59_94),
            Some(16_683_333)
        );
    }

    #[test]
    fn fallback_step_rejects_unusable_rates_without_fabricating() {
        // Unknown rate (libav reports 0/0 or 0/1) and malformed rationals are
        // ignored so the caller keeps its default rather than dividing by zero
        // or inventing a cadence.
        assert_eq!(fallback_step_ns_from_fps(Rational::new(0, 1)), None);
        assert_eq!(fallback_step_ns_from_fps(Rational::new(0, 0)), None);
        assert_eq!(fallback_step_ns_from_fps(Rational::new(25, 0)), None);
        assert_eq!(fallback_step_ns_from_fps(Rational::new(-25, 1)), None);
    }
}
