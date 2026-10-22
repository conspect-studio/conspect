//! Safe encoder wrappers (the `ffmpeg` feature).
//!
//! [`VideoEncoder`] / [`AudioEncoder`] configure a codec from a target
//! description, then run the send-frame/receive-packet loop. Each owns its
//! codec context (`Send + !Sync`, freed in `Drop` by `ffmpeg_next`).
//!
//! ## Licensing (LGPL-clean default)
//! These wrappers configure whatever codec id the caller names; the **crate**
//! never defaults to a GPL encoder. Tests and the default pipeline use LGPL
//! software codecs already in `FFmpeg` (`mpeg2video`, `ffv1`, `mjpeg`); the GPL
//! `x264`/`x265` path is reserved for the separate `gpl-codecs` feature and is
//! never reachable through `ffmpeg` alone.
//!
//! ## Timestamps (invariants #1/#3)
//! The encoder's `time_base` is the caller's exact output cadence-derived
//! rational. Callers set each frame's PTS from the **tick counter** before
//! sending — raw input PTS is never forwarded. The receive side reports packet
//! PTS/DTS in encoder time-base for the muxer to rescale into stream time-base.

use ffmpeg::format::sample::Type as SampleType;
use ffmpeg::format::{Pixel, Sample};
use ffmpeg::util::frame::{Audio, Video};
use ffmpeg::{codec, encoder, ChannelLayout, Dictionary};
use ffmpeg_next as ffmpeg;

use multiview_core::time::Rational;

use crate::convert::to_ff_rational;
use crate::decode::ensure_initialized;
use crate::encode_options::CodecOptions;
use crate::error::{FfmpegError, Result};

/// Build a libav option dictionary from a validated [`CodecOptions`] set.
///
/// The pairs were NUL-validated at construction, so `Dictionary::set` (which
/// builds C strings) is safe for every entry.
fn to_dictionary(options: &CodecOptions) -> Dictionary<'static> {
    let mut dict = Dictionary::new();
    for (key, value) in options.as_pairs() {
        dict.set(key, value);
    }
    dict
}

/// Target description for a [`VideoEncoder`].
#[derive(Debug, Clone)]
pub struct VideoEncodeTarget {
    /// libav short codec name (e.g. `"mpeg2video"`, `"ffv1"`, `"mjpeg"`). Must
    /// be an LGPL software codec for the default build.
    pub codec_name: String,
    /// Output frame width.
    pub width: u32,
    /// Output frame height.
    pub height: u32,
    /// Input/output pixel format fed to the encoder.
    pub format: Pixel,
    /// Encoder time-base — the exact output cadence reciprocal
    /// (e.g. `1001/60000` for 59.94 fps). Never a float fps.
    pub time_base: Rational,
    /// Target bitrate in bits/sec (`0` lets the codec choose / use quality).
    pub bit_rate: usize,
    /// Keyframe interval in frames (GOP size); `0` lets the codec choose.
    pub gop: u32,
    /// Optional CUDA device ordinal (e.g. `Some("1")`) to pin a `*_nvenc`
    /// encoder onto, so encode lands on the admission-selected GPU instead of
    /// libav's default CUDA device (ordinal 0) — the NVENC device-affinity seam
    /// (Tier-2 P1a). `None` keeps the default-device behaviour, and the pin is
    /// **inert** for any non-`*_nvenc` codec (the bind is gated by the codec-name
    /// suffix in [`VideoEncoder::new`]). A bind failure (no GPU / no such
    /// ordinal) degrades gracefully to a default-device open, never a panic.
    pub cuda_device: Option<String>,
}

/// A configured-and-opened video encoder.
pub struct VideoEncoder {
    encoder: encoder::video::Encoder,
    time_base: Rational,
    /// Armed by [`Self::force_next_keyframe`] (the ADR-0049 force-keyframe
    /// seam): the next frame sent is encoded as an intra/IDR picture. One-shot —
    /// cleared by the send that consumes it.
    force_keyframe: bool,
    /// The CUDA device context a `*_nvenc` encoder is bound to, when the
    /// device-affinity pin (Tier-2 P1a) is live. Held for the encoder's lifetime
    /// so the device outlives every `send_frame` — exactly as
    /// [`StreamVideoDecoder`](crate::decode_stream::StreamVideoDecoder) holds its
    /// decode-side `hw_device`. Freed synchronously in `Drop` on the encode
    /// thread (CLAUDE.md §7), never an async destructor. [`None`] on the default
    /// path (no pin requested, a non-NVENC codec, or a bind that gracefully fell
    /// back to the default device).
    hw_device: Option<crate::hwframe::HwDeviceContext>,
}

impl VideoEncoder {
    /// Configure and open a video encoder for `target` (no extra `AVOption`s).
    ///
    /// # Errors
    /// * [`FfmpegError::CodecNotFound`] — the named codec is not in this build.
    /// * [`FfmpegError::Rational`] — the time-base does not fit an `AVRational`.
    /// * [`FfmpegError::OpenEncoder`] — libav rejected the configuration.
    pub fn new(target: &VideoEncodeTarget) -> Result<Self> {
        Self::new_with_options(target, &CodecOptions::new())
    }

    /// Configure and open a video encoder for `target`, applying a validated
    /// [`CodecOptions`] set at `avcodec_open2` — the seam the fixed preview
    /// profiles ([`preview_h264_options`](crate::encode_options::preview_h264_options)
    /// / [`preview_vp8_options`](crate::encode_options::preview_vp8_options),
    /// ADR-P006) open through.
    ///
    /// Per libav semantics, an option key the chosen encoder does not declare
    /// is left unconsumed and ignored (never an open failure) — which is why
    /// the preview helpers only emit family-specific keys for recognized
    /// encoder families.
    ///
    /// # Errors
    /// * [`FfmpegError::CodecNotFound`] — the named codec is not in this build.
    /// * [`FfmpegError::Rational`] — the time-base does not fit an `AVRational`.
    /// * [`FfmpegError::OpenEncoder`] — libav rejected the configuration.
    pub fn new_with_options(target: &VideoEncodeTarget, options: &CodecOptions) -> Result<Self> {
        ensure_initialized()?;
        // Leak-safe: `codec_name` is matched against a static set of LGPL codecs
        // for the typed `CodecNotFound` message without allocating per-call.
        let codec = encoder::find_by_name(&target.codec_name)
            .ok_or_else(|| FfmpegError::CodecNotFound(static_codec_name(&target.codec_name)))?;

        let mut ctx = codec::context::Context::new_with_codec(codec)
            .encoder()
            .video()
            .map_err(FfmpegError::OpenEncoder)?;

        let tb = to_ff_rational(target.time_base)?;
        ctx.set_width(target.width);
        ctx.set_height(target.height);
        ctx.set_format(target.format);
        ctx.set_time_base(tb);
        if target.bit_rate > 0 {
            ctx.set_bit_rate(target.bit_rate);
        }
        if target.gop > 0 {
            ctx.set_gop(target.gop);
        }

        // NVENC device-affinity pin (Tier-2 P1a): when an ordinal is requested
        // AND the codec is a `*_nvenc` encoder, bind a CUDA device context for
        // that ordinal onto the not-yet-opened encoder so encode lands on the
        // admission-selected GPU instead of defaulting to ordinal 0. The bind is
        // gated by the `_nvenc` suffix so naming an ordinal on a software codec
        // is inert. A create/attach failure (no GPU, no such ordinal, OOM) is
        // logged once and degraded to a default-device open — mirroring the
        // decode-side HW->default graceful degradation, never a panic
        // (`hw_device` then stays `None`).
        let hw_device = match target.cuda_device.as_deref() {
            Some(ordinal) if target.codec_name.ends_with("_nvenc") => {
                match Self::bind_nvenc_device(&mut ctx, ordinal) {
                    Ok(device) => Some(device),
                    Err(err) => {
                        tracing::warn!(
                            codec = %target.codec_name,
                            ordinal,
                            error = %err,
                            "NVENC device pin unavailable; opening encoder on the default CUDA device"
                        );
                        None
                    }
                }
            }
            _ => None,
        };

        let encoder = if options.is_empty() {
            ctx.open_as(codec)
        } else {
            ctx.open_as_with(codec, to_dictionary(options))
        }
        .map_err(FfmpegError::OpenEncoder)?;
        Ok(Self {
            encoder,
            time_base: target.time_base,
            force_keyframe: false,
            hw_device,
        })
    }

    /// Arm the force-keyframe seam (ADR-0049): the **next** frame sent through
    /// [`Self::send_frame`] is encoded as an intra picture
    /// (`AV_PICTURE_TYPE_I`; with `forced-idr=1` on NVENC/x264 that is a true
    /// IDR). One-shot — the flag clears on the send that consumes it.
    ///
    /// This is the encoder end of the viewer-join / PLI demand path; the
    /// caller (the cli gate) owns the ≥2 s rate-limit and coalescing, so
    /// arming is deliberately unconditional here.
    pub fn force_next_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    /// Create a CUDA device context for `ordinal` and attach it to the
    /// not-yet-opened `ctx` (NVENC device-affinity pin, Tier-2 P1a). On success
    /// the returned handle MUST outlive the encoder (stored in `hw_device`); the
    /// attach takes a separate `av_buffer_ref` libav frees with the encoder.
    /// Requires a working driver/GPU at run time; on a GPU-free box / a missing
    /// ordinal this returns a typed error and the caller degrades to the default
    /// device (never a panic).
    fn bind_nvenc_device(
        ctx: &mut encoder::video::Video,
        ordinal: &str,
    ) -> Result<crate::hwframe::HwDeviceContext> {
        let device = crate::hwframe::HwDeviceContext::create(
            crate::hwdecode::HwDeviceKind::Cuda,
            Some(ordinal),
        )?;
        // Must be set BEFORE the encoder is opened — libav reads `hw_device_ctx`
        // only during `avcodec_open2` (`open_as`).
        device.attach_to_encoder(ctx)?;
        Ok(device)
    }

    /// The encoder time-base (exact rational).
    #[must_use]
    pub const fn time_base(&self) -> Rational {
        self.time_base
    }

    /// Whether this encoder is pinned to a specific CUDA device (Tier-2 P1a) —
    /// true exactly when a `*_nvenc` device-affinity bind succeeded and its CUDA
    /// device context is held for the encoder's lifetime. `false` on the default
    /// path (no pin requested, a non-NVENC codec, or a bind that gracefully fell
    /// back to the default device). The mirror of
    /// [`StreamVideoDecoder::is_hardware`](crate::decode_stream::StreamVideoDecoder::is_hardware)
    /// on the encode side; used by telemetry / the hardware-validation check.
    #[must_use]
    pub const fn is_device_pinned(&self) -> bool {
        self.hw_device.is_some()
    }

    /// Borrow the opened encoder's codec context — used to register a matching
    /// stream on a [`Muxer`](crate::mux::Muxer) (which copies codec parameters
    /// from it).
    #[must_use]
    pub fn as_codec_context(&self) -> &codec::Context {
        self.encoder.as_ref()
    }

    /// Snapshot the encoder's codec parameters into an owned, owner-less
    /// `AVCodecParameters` (`avcodec_parameters_from_context`). Used to build a
    /// [`StreamCodecParameters`](crate::packet::StreamCodecParameters) that
    /// crosses threads to a mux-only sink without the encoder instance
    /// (encode-once-mux-many, invariant #7).
    #[must_use]
    pub(crate) fn codec_parameters(&self) -> codec::Parameters {
        codec::Parameters::from(&self.encoder)
    }

    /// Send one frame, whose PTS the caller has already set from the tick
    /// counter (encoder time-base). Drain packets with [`Self::receive_packet`].
    ///
    /// If [`Self::force_next_keyframe`] armed the seam, this send consumes it:
    /// the frame is deep-copied once (geometry-bounded, and rate-bounded by the
    /// caller's ≥2 s force floor — never a steady-state cost), stamped
    /// `AV_PICTURE_TYPE_I`, and encoded as a keyframe.
    ///
    /// # Errors
    /// Returns [`FfmpegError::Encode`] on a libav send error.
    pub fn send_frame(&mut self, frame: &Video) -> Result<()> {
        if self.force_keyframe {
            self.force_keyframe = false;
            // `Video::clone` copies the pixel planes AND the frame props (PTS
            // included) via av_frame_copy + av_frame_copy_props, so the keyed
            // copy is the same frame with only the picture type forced.
            let mut keyed = frame.clone();
            keyed.set_kind(ffmpeg::picture::Type::I);
            return self.encoder.send_frame(&keyed).map_err(FfmpegError::Encode);
        }
        self.encoder.send_frame(frame).map_err(FfmpegError::Encode)
    }

    /// Flush the encoder (signal EOF) so buffered packets can be drained.
    ///
    /// # Errors
    /// Returns [`FfmpegError::Encode`] on a libav error.
    pub fn send_eof(&mut self) -> Result<()> {
        self.encoder.send_eof().map_err(FfmpegError::Encode)
    }

    /// Pull the next encoded packet, or `Ok(None)` on `EAGAIN`/`EOF`.
    ///
    /// The returned packet's PTS/DTS are in encoder time-base; the muxer
    /// rescales them into stream time-base.
    ///
    /// # Errors
    /// Returns [`FfmpegError::Encode`] for a real libav error.
    pub fn receive_packet(&mut self) -> Result<Option<codec::packet::Packet>> {
        receive_packet(&mut self.encoder)
    }
}

/// Target description for an [`AudioEncoder`].
#[derive(Debug, Clone)]
pub struct AudioEncodeTarget {
    /// libav short codec name (e.g. `"flac"`, `"pcm_s16le"`). LGPL only for the
    /// default build.
    pub codec_name: String,
    /// Sample format fed to the encoder.
    pub format: Sample,
    /// Channel layout.
    pub channel_layout: ChannelLayout,
    /// Sample rate in Hz (also the natural time-base reciprocal).
    pub sample_rate: u32,
    /// Target bitrate in bits/sec (`0` lets the codec choose).
    pub bit_rate: usize,
}

/// A configured-and-opened audio encoder.
pub struct AudioEncoder {
    encoder: encoder::audio::Encoder,
    time_base: Rational,
    frame_size: u32,
    /// The sample format the encoder accepts (used to build input frames).
    format: Sample,
    /// The channel layout the encoder was opened with.
    channel_layout: ChannelLayout,
    /// The sample rate in Hz (stamped onto each built frame).
    sample_rate: u32,
}

// SAFETY: every field is owned and carries no thread-affine interior state: the
// libav `encoder::audio::Encoder` owns its `AVCodecContext` outright (the same
// ownership `VideoEncoder` relies on for its auto-`Send`), and `ChannelLayout`
// is an owned value whose only non-`Send` member is a `*mut AVChannelCustom`
// that libav populates as plain owned heap data, never shared. Moving the whole
// encoder between threads is therefore sound (the cli's bake consumer owns the
// `ProgramEncoder` on a single thread and drives it serially). It is
// deliberately NOT `Sync` (no `unsafe impl Sync`): a libav encoder context must
// be externally synchronized for shared access, and `encoder::audio::Encoder`
// is `!Sync` by default, so leaving `Sync` underived enforces single-thread
// access — matching `VideoEncoder`, `Scaler`, and `HwDeviceContext`.
#[allow(unsafe_code)]
unsafe impl Send for AudioEncoder {}

impl AudioEncoder {
    /// Configure and open an audio encoder for `target` (no extra `AVOption`s).
    ///
    /// # Errors
    /// * [`FfmpegError::CodecNotFound`] — the named codec is not in this build.
    /// * [`FfmpegError::OpenEncoder`] — libav rejected the configuration.
    pub fn new(target: &AudioEncodeTarget) -> Result<Self> {
        Self::new_with_options(target, &CodecOptions::new())
    }

    /// Configure and open an audio encoder for `target`, applying a validated
    /// [`CodecOptions`] set at `avcodec_open2` — how the program Opus
    /// rendition ([`OpusEncoder`](crate::opus::OpusEncoder), ADR-0049) sets
    /// `vbr=constrained`/`frame_duration` on `libopus`, and
    /// `strict=experimental` when falling back to libav's native `opus`.
    ///
    /// # Errors
    /// * [`FfmpegError::CodecNotFound`] — the named codec is not in this build.
    /// * [`FfmpegError::OpenEncoder`] — libav rejected the configuration.
    pub fn new_with_options(target: &AudioEncodeTarget, options: &CodecOptions) -> Result<Self> {
        ensure_initialized()?;
        let codec = encoder::find_by_name(&target.codec_name)
            .ok_or_else(|| FfmpegError::CodecNotFound(static_codec_name(&target.codec_name)))?;

        let mut ctx = codec::context::Context::new_with_codec(codec)
            .encoder()
            .audio()
            .map_err(FfmpegError::OpenEncoder)?;

        // Audio time-base is 1/sample_rate.
        let time_base = Rational::new(1, i64::from(target.sample_rate));
        let tb = to_ff_rational(time_base)?;
        ctx.set_rate(i32::try_from(target.sample_rate).unwrap_or(i32::MAX));
        ctx.set_format(target.format);
        ctx.set_channel_layout(target.channel_layout);
        ctx.set_time_base(tb);
        if target.bit_rate > 0 {
            ctx.set_bit_rate(target.bit_rate);
        }

        let encoder = if options.is_empty() {
            ctx.open_as(codec)
        } else {
            ctx.open_as_with(codec, to_dictionary(options))
        }
        .map_err(FfmpegError::OpenEncoder)?;
        let frame_size = encoder.frame_size();
        Ok(Self {
            encoder,
            time_base,
            frame_size,
            format: target.format,
            channel_layout: target.channel_layout,
            sample_rate: target.sample_rate,
        })
    }

    /// The channel count this encoder was opened with.
    #[must_use]
    pub fn channels(&self) -> usize {
        usize::try_from(self.channel_layout.channels().max(0)).unwrap_or(0)
    }

    /// The encoder time-base (`1/sample_rate`).
    #[must_use]
    pub const fn time_base(&self) -> Rational {
        self.time_base
    }

    /// The encoder's required samples-per-frame, or `0` if it accepts any
    /// (variable) frame size.
    #[must_use]
    pub const fn frame_size(&self) -> u32 {
        self.frame_size
    }

    /// Borrow the opened encoder's codec context — used to register a matching
    /// stream on a [`Muxer`](crate::mux::Muxer).
    #[must_use]
    pub fn as_codec_context(&self) -> &codec::Context {
        self.encoder.as_ref()
    }

    /// Snapshot the encoder's codec parameters into an owned, owner-less
    /// `AVCodecParameters`. See
    /// [`VideoEncoder::codec_parameters`](crate::encode::VideoEncoder::codec_parameters).
    #[must_use]
    pub(crate) fn codec_parameters(&self) -> codec::Parameters {
        codec::Parameters::from(&self.encoder)
    }

    /// Send one audio frame (PTS already set by the caller).
    ///
    /// # Errors
    /// Returns [`FfmpegError::Encode`] on a libav send error.
    pub fn send_frame(&mut self, frame: &Audio) -> Result<()> {
        self.encoder.send_frame(frame).map_err(FfmpegError::Encode)
    }

    /// Build and send one planar-`f32` audio frame from `planes` (one slice per
    /// channel, each at least `samples` long), stamped at `pts` in the encoder
    /// time-base (`1/sample_rate`). This owns the libav frame construction so a
    /// caller (the program-bus encode path) never names a raw frame type.
    ///
    /// `pts` must come from a sample counter (`audio_pts = Σ samples`), the audio
    /// analogue of the output tick (invariant #3) — never a raw input PTS.
    ///
    /// # Errors
    /// * [`FfmpegError::FrameMismatch`] — the encoder is not planar `f32`, the
    ///   plane count does not match the channel layout, or a plane is shorter
    ///   than `samples`.
    /// * [`FfmpegError::Encode`] — a libav send error.
    pub fn send_planar_f32(&mut self, planes: &[&[f32]], samples: usize, pts: i64) -> Result<()> {
        if self.format != Sample::F32(SampleType::Planar) {
            return Err(FfmpegError::FrameMismatch(
                "send_planar_f32 requires a planar-f32 encoder",
            ));
        }
        if planes.len() != self.channels() {
            return Err(FfmpegError::FrameMismatch(
                "planar audio block channel count does not match the encoder layout",
            ));
        }

        let mut frame = Audio::new(self.format, samples, self.channel_layout);
        frame.set_rate(self.sample_rate);
        for (channel, plane) in planes.iter().enumerate() {
            // `.get(..samples)` (not `[..samples]`) so a short plane is a typed
            // FrameMismatch, never a slice-index panic. The frame's plane is
            // allocated to exactly `samples`, so `copy_from_slice` lengths match.
            let src = plane.get(..samples).ok_or(FfmpegError::FrameMismatch(
                "planar audio plane shorter than the requested sample count",
            ))?;
            frame.plane_mut::<f32>(channel).copy_from_slice(src);
        }
        frame.set_pts(Some(pts));
        self.send_frame(&frame)
    }

    /// Build and send one **packed** (interleaved) `f32` **stereo** audio frame
    /// from `interleaved` (`[l0, r0, l1, r1, …]`, at least `samples * 2` long),
    /// stamped at `pts` in the encoder time-base (`1/sample_rate`). The packed
    /// sibling of [`Self::send_planar_f32`] for encoders whose only `f32` input
    /// is interleaved `FLT` (`libopus`).
    ///
    /// `pts` must come from a sample counter (`audio_pts = Σ samples`), the
    /// audio analogue of the output tick (invariant #3) — never a raw input PTS.
    ///
    /// # Errors
    /// * [`FfmpegError::FrameMismatch`] — the encoder is not packed `f32`
    ///   stereo, or `interleaved` is shorter than `samples * 2`.
    /// * [`FfmpegError::Encode`] — a libav send error.
    pub fn send_interleaved_f32(
        &mut self,
        interleaved: &[f32],
        samples: usize,
        pts: i64,
    ) -> Result<()> {
        if self.format != Sample::F32(SampleType::Packed) {
            return Err(FfmpegError::FrameMismatch(
                "send_interleaved_f32 requires a packed-f32 encoder",
            ));
        }
        if self.channels() != 2 {
            return Err(FfmpegError::FrameMismatch(
                "send_interleaved_f32 supports stereo encoders only",
            ));
        }
        let needed = samples.saturating_mul(2);
        let src = interleaved.get(..needed).ok_or(FfmpegError::FrameMismatch(
            "interleaved audio block shorter than the requested sample count",
        ))?;

        let mut frame = Audio::new(self.format, samples, self.channel_layout);
        frame.set_rate(self.sample_rate);
        // Packed stereo f32 exposes one plane of `(f32, f32)` pairs, exactly
        // `samples` long — the format/channel checks above keep the typed plane
        // access in-bounds (no panic on the encode path, CLAUDE.md §7).
        for (dst, pair) in frame
            .plane_mut::<(f32, f32)>(0)
            .iter_mut()
            .zip(src.chunks_exact(2))
        {
            if let &[left, right] = pair {
                *dst = (left, right);
            }
        }
        frame.set_pts(Some(pts));
        self.send_frame(&frame)
    }

    /// Flush the encoder (signal EOF).
    ///
    /// # Errors
    /// Returns [`FfmpegError::Encode`] on a libav error.
    pub fn send_eof(&mut self) -> Result<()> {
        self.encoder.send_eof().map_err(FfmpegError::Encode)
    }

    /// Pull the next encoded packet, or `Ok(None)` on `EAGAIN`/`EOF`.
    ///
    /// # Errors
    /// Returns [`FfmpegError::Encode`] for a real libav error.
    pub fn receive_packet(&mut self) -> Result<Option<codec::packet::Packet>> {
        receive_packet(&mut self.encoder)
    }
}

/// Shared receive-packet drain logic for both encoder kinds.
fn receive_packet<E>(encoder: &mut E) -> Result<Option<codec::packet::Packet>>
where
    E: ReceivePacket,
{
    let mut packet = codec::packet::Packet::empty();
    match encoder.recv(&mut packet) {
        Ok(()) => Ok(Some(packet)),
        Err(
            ffmpeg::Error::Other {
                errno: ffmpeg::util::error::EAGAIN,
            }
            | ffmpeg::Error::Eof,
        ) => Ok(None),
        Err(other) => Err(FfmpegError::Encode(other)),
    }
}

/// Internal abstraction over the two encoder types' `receive_packet`.
trait ReceivePacket {
    fn recv(
        &mut self,
        packet: &mut codec::packet::Packet,
    ) -> std::result::Result<(), ffmpeg::Error>;
}

impl ReceivePacket for encoder::video::Encoder {
    fn recv(
        &mut self,
        packet: &mut codec::packet::Packet,
    ) -> std::result::Result<(), ffmpeg::Error> {
        self.receive_packet(packet)
    }
}

impl ReceivePacket for encoder::audio::Encoder {
    fn recv(
        &mut self,
        packet: &mut codec::packet::Packet,
    ) -> std::result::Result<(), ffmpeg::Error> {
        self.receive_packet(packet)
    }
}

/// Map a runtime codec name to a `'static` string for the typed
/// [`FfmpegError::CodecNotFound`] message, covering the LGPL test/default
/// codecs; an unrecognized name falls back to a generic label.
fn static_codec_name(name: &str) -> &'static str {
    match name {
        "mpeg2video" => "mpeg2video",
        "ffv1" => "ffv1",
        "mjpeg" => "mjpeg",
        "rawvideo" => "rawvideo",
        "libvpx" => "libvpx",
        "flac" => "flac",
        "pcm_s16le" => "pcm_s16le",
        "aac" => "aac",
        "libopus" => "libopus",
        "opus" => "opus",
        "mp2" => "mp2",
        _ => "<encoder>",
    }
}
