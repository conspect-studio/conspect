//! GP-4 — the one-time pre-baked slate baker (the `ffmpeg` feature; ADR-0030 §4
//! "Pre-bake-once slate").
//!
//! ADR-0030's **guarded passthrough** is packet-copy while the input is healthy
//! and splices a **pre-baked** slate into the copied elementary stream on input
//! loss. Because the slate is baked **once**, failover costs **zero** live
//! encode and holds **zero** of the gated NVENC concurrent-session ceiling — the
//! slate is replayed as inert bytes through the encoder-less
//! [`PacketMuxSink`](crate::sink::PacketMuxSink) (GP-7 does that splice; GP-4
//! only "produces the bytes once").
//!
//! This module is exactly that baker. Given the input's probed coded parameters
//! (codec, geometry, the output cadence and GOP), [`SlateBaker::bake_slate`]
//! encodes **one** short **IDR-led, closed-GOP, B-free** loop of black or
//! SMPTE-bars video — and, when an audio spec is supplied, an integer number of
//! AAC access units of 1 kHz tone or silence with **>= 2 leading silence AUs**
//! (ADR-0030 §4: the coded-domain AAC IMDCT/TDAC seam transient is
//! uncancellable, so the audio side opens with silence) — and returns the coded
//! packets as a shared [`Arc<[EncodedPacket]>`]. After the bake the encoder is
//! **dropped** (no held session).
//!
//! ## Encode-once + bounded memory (invariant #7, ADR-0030 §4)
//!
//! The loop is exactly **one closed GOP** ([`SlateVideoSpec::gop`] frames); a
//! downstream splice replays it by integer offset, never re-encoding, so the
//! cached slate is **O(1) in outage length**. The bake is quality-shaped
//! (CRF/CQ-style, via the codec's default quality path — never a CBR-padded
//! bake), so even a 1080p slate is a few hundred KB and well under 5 MB.
//!
//! ## Reuse, not new FFI
//!
//! The encode runs entirely through the crate's existing
//! [`ProgramEncoder`](crate::sink::ProgramEncoder) — the same single-encoder
//! `encode-once` producer the streaming path uses — over `multiview-ffmpeg`'s
//! safe wrappers. This module writes **no** `unsafe` and performs **no** FFI; it
//! only fills NV12 pixel planes (via the safe `Video::data_mut` plane slices) and
//! synthesizes audio samples. The crate stays `forbid(unsafe_code)`.
//!
//! ## Licensing (LGPL-clean default)
//!
//! The default slate codec is `mpeg2video` (an LGPL software codec already in
//! `FFmpeg`) and the audio codec is the native libav `aac` (LGPL) — never
//! x264/x265. [`SlateVideoCodec::H264`] is reserved for the separate
//! `gpl-codecs` feature (libx264) and is never reachable through `ffmpeg` alone.

use std::sync::Arc;

use ffmpeg_next::format::Pixel;
use ffmpeg_next::util::frame::Video;
use multiview_core::color::ColorInfo;
use multiview_core::frame::FrameMeta;
use multiview_core::pixel::PixelFormat;
use multiview_core::time::{MediaTime, Rational};
use multiview_ffmpeg::{DecodedVideoFrame, EncodedPacket, StreamKind};

use crate::error::{Error, Result};
use crate::sink::{AudioEncodeConfig, EncodeConfig, ProgramEncoder};

/// The picture the slate displays on input loss — the program-level half of the
/// configurable failover-slate policy (ADR-0030 §4). The engine maps a
/// `multiview_config::FailoverSlate` onto this so a passthrough / transcode
/// program honours the **same** `on_loss` choice as a layout tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SlateKind {
    /// A full-frame black raster (limited-range luma `Y = 16`, neutral chroma).
    Black,
    /// SMPTE-style vertical colour bars (the operator's "SMPTE-bars" failover).
    SmpteBars,
    /// The **signal-lost** card — a distinct, recognisable "NO SIGNAL"
    /// placeholder (the engine's `NO_SIGNAL` slate, mapped from
    /// `multiview_config::FailoverSlate::NoSignal`).
    NoSignal,
}

/// The audio the slate plays on input loss.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SlateAudio {
    /// A 1 kHz reference tone (the operator's "1 kHz tone" failover).
    Tone1k,
    /// Digital silence.
    Silence,
}

/// The codec the slate is baked in. Must match the probed input codec so the
/// spliced slate is parameter-compatible with the copied elementary stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SlateVideoCodec {
    /// MPEG-2 video — the LGPL-clean default (`mpeg2video`), B-free by default.
    Mpeg2Video,
    /// H.264 / AVC via `libx264`. **GPL** — reachable only when the crate's
    /// `gpl-codecs` feature is enabled; never in the default LGPL-clean build.
    H264,
}

impl SlateVideoCodec {
    /// The libav short codec name the encoder is opened from.
    const fn codec_name(self) -> &'static str {
        match self {
            Self::Mpeg2Video => "mpeg2video",
            Self::H264 => "libx264",
        }
    }
}

/// The probed video parameters the slate must match (ADR-0030 §4 pre-bake).
///
/// These come from the input's coded parameters via the GP-2 probe (codec,
/// width, height) plus the program's output cadence and GOP. The slate is baked
/// **once** in exactly these parameters so a downstream splice is
/// parameter-compatible with the copied elementary stream.
#[derive(Debug, Clone)]
pub struct SlateVideoSpec {
    /// The codec to bake in (matches the probed input codec).
    pub codec: SlateVideoCodec,
    /// Slate width in pixels (matches the probed input width).
    pub width: u32,
    /// Slate height in pixels (matches the probed input height).
    pub height: u32,
    /// The output cadence (frames per second) as an exact rational — never a
    /// float fps (invariant #3).
    pub cadence: Rational,
    /// The GOP length in frames. The slate is exactly **one** closed GOP, so
    /// this is also the number of frames baked. Must be non-zero.
    pub gop: u32,
}

/// The probed audio parameters the optional slate audio must match.
#[derive(Debug, Clone)]
pub struct SlateAudioSpec {
    /// Sample rate in Hz (matches the program audio rate; e.g. 48 000).
    pub sample_rate: u32,
    /// Channel count (`1` = mono, `2` = stereo).
    pub channels: u16,
    /// What to play: 1 kHz tone or silence.
    pub audio: SlateAudio,
}

/// A complete slate specification: what to show, in which parameters, with what
/// optional audio.
#[derive(Debug, Clone)]
pub struct SlateSpec {
    /// The picture to display.
    pub kind: SlateKind,
    /// The probed video parameters to bake in.
    pub video: SlateVideoSpec,
    /// The optional audio to bake alongside the video. `None` bakes a
    /// video-only slate (no audio packets).
    pub audio: Option<SlateAudioSpec>,
}

/// The baked-in coded parameters of a [`BakedSlate`].
///
/// A downstream splice (GP-7) checks these against the live input's params
/// before choosing the matched-slate-splice rung; they are exactly the requested
/// [`SlateVideoSpec`] values, snapshotted so the value type is self-describing.
#[derive(Debug, Clone)]
pub struct BakedParams {
    /// The failover **picture** that was baked ([`SlateKind`]) — the program-level
    /// `on_loss` choice this slate displays on input loss. Recorded so a splice
    /// (and the per-program robustness badge) knows what the program shows.
    pub kind: SlateKind,
    /// The codec the slate was baked in.
    pub codec: SlateVideoCodec,
    /// The baked slate width in pixels.
    pub width: u32,
    /// The baked slate height in pixels.
    pub height: u32,
    /// The output cadence the slate was baked at (exact rational).
    pub cadence: Rational,
    /// The GOP length (= the number of frames in the closed-GOP loop).
    pub gop: u32,
}

/// The product of a bake: the slate's coded packets plus its baked parameters.
///
/// A pure value type — it does **not** wire into any sink here (GP-7 does the
/// splice). The coded packets are held behind [`Arc`] so a passthrough can share
/// the one bake across its replay ring without copying (memory is O(1) in outage
/// length — ADR-0030 §4).
#[derive(Debug, Clone)]
pub struct BakedSlate {
    /// The slate's coded **video** packets: one closed, IDR-led GOP. The first
    /// packet is the closed-GOP keyframe (the splice re-anchor point).
    video: Arc<[EncodedPacket]>,
    /// The slate's coded **audio** access units, or `None` for a video-only
    /// slate. When present, opens with **>= 2 leading silence AUs**.
    audio: Option<Arc<[EncodedPacket]>>,
    /// The baked-in coded parameters (for the downstream splice compatibility
    /// check).
    params: BakedParams,
}

impl BakedSlate {
    /// The slate's coded video packets (one IDR-led closed GOP).
    #[must_use]
    pub fn video(&self) -> &Arc<[EncodedPacket]> {
        &self.video
    }

    /// The slate's coded audio access units, or `None` for a video-only slate.
    #[must_use]
    pub fn audio(&self) -> Option<&Arc<[EncodedPacket]>> {
        self.audio.as_ref()
    }

    /// The baked-in coded parameters.
    #[must_use]
    pub fn params(&self) -> &BakedParams {
        &self.params
    }
}

/// The one-time slate baker (ADR-0030 §4 pre-bake-once).
///
/// Stateless — [`bake_slate`](Self::bake_slate) is the only entry point; this
/// type is just the namespace for it.
#[derive(Debug, Clone, Copy)]
pub struct SlateBaker;

impl SlateBaker {
    /// Bake `spec` **once** into a [`BakedSlate`], releasing the encoder before
    /// returning (no held session).
    ///
    /// Encodes exactly one closed GOP ([`SlateVideoSpec::gop`] frames) of the
    /// requested picture, in the requested codec/geometry/cadence, with the
    /// optional audio. The encoder is dropped when this returns.
    ///
    /// # Errors
    /// Returns [`Error::Output`] if the spec is invalid (zero geometry or GOP),
    /// the encoder cannot be opened for the requested codec (e.g.
    /// [`SlateVideoCodec::H264`] without the `gpl-codecs` feature / `libx264`
    /// present), or the encode fails.
    pub fn bake_slate(spec: &SlateSpec) -> Result<BakedSlate> {
        let video = &spec.video;
        if video.width == 0 || video.height == 0 {
            return Err(Error::Output("slate has a zero dimension".to_owned()));
        }
        if video.gop == 0 {
            return Err(Error::Output("slate GOP must be non-zero".to_owned()));
        }

        let mut encoder = ProgramEncoder::new(&encode_config(spec))?;

        // Bake exactly one closed GOP. The encoder opens the stream on its IDR
        // and re-keys every `gop` frames, so a single-GOP loop is IDR-led; a
        // downstream splice replays it by offset (ADR-0030 §4).
        let mut video_packets: Vec<EncodedPacket> = Vec::new();
        let mut audio_packets: Vec<EncodedPacket> = Vec::new();
        for _ in 0..video.gop {
            let frame = slate_frame(spec.kind, video);
            video_packets.extend(encoder.encode_frame(frame)?);
        }

        // Bake the optional audio (>= 2 leading silence AUs, then the chosen
        // content) BEFORE flushing. `encode_audio` emits packets as the FIFO
        // fills, so collect them here as well as the flush remainder.
        if let Some(audio_spec) = &spec.audio {
            audio_packets.extend(bake_audio(&mut encoder, video, audio_spec)?);
        }

        // Flush: trailing video packets + the audio FIFO remainder/EOF. The
        // encoder is dropped at the end of this scope — no held session.
        let tail = encoder.finish()?;
        for packet in tail {
            // `finish` interleaves the two streams; route by kind.
            match packet.kind() {
                StreamKind::Audio => audio_packets.push(packet),
                _ => video_packets.push(packet),
            }
        }
        drop(encoder);

        let audio = spec.audio.as_ref().map(|_| Arc::from(audio_packets));

        Ok(BakedSlate {
            video: Arc::from(video_packets),
            audio,
            params: BakedParams {
                kind: spec.kind,
                codec: video.codec,
                width: video.width,
                height: video.height,
                cadence: video.cadence,
                gop: video.gop,
            },
        })
    }
}

/// The minimum number of leading silence AAC access units the audio slate opens
/// with (ADR-0030 §4: the coded-domain AAC seam transient is uncancellable, so
/// the audio side starts in silence regardless of the chosen content).
const LEADING_SILENCE_AUS: usize = 2;

/// AAC's fixed access-unit length in samples (per channel).
const AAC_FRAME_SAMPLES: usize = 1024;

/// Build the [`EncodeConfig`] the slate's [`ProgramEncoder`] is opened from.
///
/// LGPL software codecs (mpeg2video) want planar `yuv420p`; the NV12 slate frames
/// are converted NV12 -> yuv420p by the encoder's converter, exactly as the live
/// pipeline does. `bit_rate: 0` selects the codec's default quality path (no
/// CBR padding), keeping the cached slate small (ADR-0030 §4 CRF/CQ rule).
fn encode_config(spec: &SlateSpec) -> EncodeConfig {
    EncodeConfig {
        codec_name: spec.video.codec.codec_name().to_owned(),
        width: spec.video.width,
        height: spec.video.height,
        format: Pixel::YUV420P,
        cadence: spec.video.cadence,
        gop: spec.video.gop,
        bit_rate: 0,
        audio: spec.audio.as_ref().map(|a| {
            // Native libav AAC (LGPL); `bit_rate: 0` lets the encoder choose.
            AudioEncodeConfig::aac(a.sample_rate, a.channels, 0)
        }),
        // The slate is a CPU-generated failover card — never device-pinned.
        cuda_ordinal: None,
    }
}

/// Build one NV12 slate frame (`kind`) at the spec geometry.
///
/// The compositor's canonical working format is NV12 (invariant #5), so the
/// slate is generated as NV12 and the encoder's converter handles NV12 ->
/// encoder format — matching the live path. The frame's metadata PTS is left at
/// zero: [`ProgramEncoder`] re-stamps every frame's PTS from its own tick
/// counter (`out_pts = f(tick)`, invariant #3), so the input meta PTS is unused.
fn slate_frame(kind: SlateKind, video: &SlateVideoSpec) -> DecodedVideoFrame {
    let mut frame = Video::new(Pixel::NV12, video.width, video.height);
    match kind {
        SlateKind::Black => fill_black(&mut frame),
        SlateKind::SmpteBars => fill_bars(&mut frame, video.width),
        SlateKind::NoSignal => fill_nosignal(&mut frame),
    }
    let meta = FrameMeta {
        pts: MediaTime::ZERO,
        width: video.width,
        height: video.height,
        format: PixelFormat::Nv12,
        color: ColorInfo::default(),
    };
    DecodedVideoFrame {
        frame,
        meta,
        raw_pts: None,
        // A synthetic slate card carries no embedded (A53) captions.
        a53_cc: None,
    }
}

/// Limited-range black: luma `Y = 16` over the whole Y plane, neutral chroma
/// (`U` = `V` = 128) over the interleaved NV12 `CbCr` plane.
fn fill_black(frame: &mut Video) {
    const Y_BLACK: u8 = 16;
    const C_NEUTRAL: u8 = 128;
    let y_plane = frame.data_mut(0);
    y_plane.fill(Y_BLACK);
    let uv_plane = frame.data_mut(1);
    uv_plane.fill(C_NEUTRAL);
}

/// The **signal-lost** card: a flat dark-blue field — the classic recognisable
/// "NO SIGNAL" placeholder, distinct from both `Black` and the colour `Bars`.
///
/// A flat field keeps the bake tiny (well under the < 5 MB budget) while its
/// non-neutral chroma (`Cb` raised, `Cr` lowered for blue) makes the coded bytes
/// differ from a pure-black bake — so the program-level `on_loss` choice
/// provably changes *what* is shown. The exact code values are a deep, broadcast
/// "no-signal blue" (limited-range NV12).
fn fill_nosignal(frame: &mut Video) {
    /// Luma of the dark "no-signal blue" field (limited range).
    const Y_BLUE: u8 = 41;
    /// `Cb` for blue (raised well above neutral 128).
    const CB_BLUE: u8 = 200;
    /// `Cr` for blue (lowered below neutral 128).
    const CR_BLUE: u8 = 110;
    let y_plane = frame.data_mut(0);
    y_plane.fill(Y_BLUE);
    let uv_plane = frame.data_mut(1);
    // Interleaved NV12 chroma: even bytes are Cb, odd bytes are Cr.
    for pair in uv_plane.chunks_exact_mut(2) {
        if let [cb, cr] = pair {
            *cb = CB_BLUE;
            *cr = CR_BLUE;
        }
    }
}

/// Eight equal-width SMPTE-style vertical colour bars (white, yellow, cyan,
/// green, magenta, red, blue, black) written into the NV12 planes.
///
/// Bar boundaries align to even columns (NV12 chroma is 2:1 horizontally) so the
/// shared `CbCr` sample for a `2`-pixel pair is unambiguous. `width` is the
/// visible width; the Y/UV planes may be wider (stride padding), which
/// `data_mut` covers and the row loops respect via the plane stride.
fn fill_bars(frame: &mut Video, width: u32) {
    /// One bar's BT.601-ish limited-range YCbCr triple.
    const BARS: [(u8, u8, u8); 8] = [
        (235, 128, 128), // white
        (210, 16, 146),  // yellow
        (170, 166, 16),  // cyan
        (145, 54, 34),   // green
        (106, 202, 222), // magenta
        (81, 90, 240),   // red
        (41, 240, 110),  // blue
        (16, 128, 128),  // black
    ];

    let visible_w = usize::try_from(width).unwrap_or(0);
    // Bar boundaries on even columns; a zero/odd width degrades gracefully.
    let bar_w = ((visible_w / BARS.len()).max(1) & !1_usize).max(1);
    // Pick a bar's triple for a luma column (clamped to the last bar). `.get`
    // keeps the lookup panic-free even past the eighth bar at the right edge.
    let bar_at = |luma_col: usize| -> (u8, u8, u8) {
        let idx = (luma_col / bar_w).min(BARS.len() - 1);
        BARS.get(idx).copied().unwrap_or((16, 128, 128))
    };

    // Y plane (full resolution). Rows are derived from the plane length and its
    // stride so the loop respects any stride padding without naming a row count.
    let y_stride = frame.stride(0);
    let y_plane = frame.data_mut(0);
    let y_rows = y_plane.len().checked_div(y_stride).unwrap_or(0);
    for row in 0..y_rows {
        let base = row * y_stride;
        for col in 0..visible_w {
            if let Some(px) = y_plane.get_mut(base + col) {
                *px = bar_at(col).0;
            }
        }
    }

    // CbCr plane (NV12: half width/height, interleaved Cb,Cr per 2x2 block).
    let c_stride = frame.stride(1);
    let c_w = visible_w.div_ceil(2);
    let uv_plane = frame.data_mut(1);
    let c_rows = uv_plane.len().checked_div(c_stride).unwrap_or(0);
    for row in 0..c_rows {
        let base = row * c_stride;
        for cx in 0..c_w {
            // The chroma sample at `cx` covers luma columns `2*cx`/`2*cx+1`.
            let (_, chroma_b, chroma_r) = bar_at(cx * 2);
            let cb_off = base + cx * 2;
            if let Some(cb) = uv_plane.get_mut(cb_off) {
                *cb = chroma_b;
            }
            if let Some(cr) = uv_plane.get_mut(cb_off + 1) {
                *cr = chroma_r;
            }
        }
    }
}

/// Bake the slate audio: `>= 2` leading silence AUs, then enough AUs of the
/// chosen content to cover the slate's loop duration (one GOP), all integer AAC
/// access units. The encoder rebuffers our blocks into 1024-sample frames and
/// stamps each from its sample counter (the audio peer of `out_pts = f(tick)`).
/// Returns the audio packets `encode_audio` emitted as the FIFO filled (the
/// flush remainder is collected by the caller).
fn bake_audio(
    encoder: &mut ProgramEncoder,
    video: &SlateVideoSpec,
    audio: &SlateAudioSpec,
) -> Result<Vec<EncodedPacket>> {
    let channels = usize::from(audio.channels.max(1));
    let mut packets: Vec<EncodedPacket> = Vec::new();

    // Always lead with silence (ADR-0030 §4 audio seam).
    for _ in 0..LEADING_SILENCE_AUS {
        packets.extend(feed_audio_block(
            encoder,
            channels,
            AAC_FRAME_SAMPLES,
            |_, _| 0.0,
        )?);
    }

    // Then the chosen content, sized to cover the one-GOP loop so the audio loop
    // length is an integer number of AUs (>= the video loop duration). At least
    // one content AU is always emitted.
    let loop_samples = loop_audio_samples(video, audio.sample_rate);
    let content_aus = loop_samples.div_ceil(AAC_FRAME_SAMPLES).max(1);
    for au in 0..content_aus {
        let block = match audio.audio {
            SlateAudio::Silence => {
                feed_audio_block(encoder, channels, AAC_FRAME_SAMPLES, |_, _| 0.0)?
            }
            SlateAudio::Tone1k => {
                let base = au.saturating_mul(AAC_FRAME_SAMPLES);
                feed_audio_block(encoder, channels, AAC_FRAME_SAMPLES, |_, n| {
                    tone_1k_sample(base.saturating_add(n), audio.sample_rate)
                })?
            }
        };
        packets.extend(block);
    }
    Ok(packets)
}

/// The number of audio samples (per channel) that span one slate loop (one GOP
/// of video). `gop / cadence` seconds at `sample_rate` Hz, evaluated in exact
/// integer arithmetic over the rational cadence (no float fps).
fn loop_audio_samples(video: &SlateVideoSpec, sample_rate: u32) -> usize {
    // seconds = gop * cadence.den / cadence.num ; samples = seconds * rate.
    let num = i128::from(video.gop)
        .saturating_mul(i128::from(video.cadence.den))
        .saturating_mul(i128::from(sample_rate));
    let den = i128::from(video.cadence.num).max(1);
    let samples = num / den;
    usize::try_from(samples.max(0)).unwrap_or(AAC_FRAME_SAMPLES)
}

/// A 1 kHz sine sample at sample index `n` for `sample_rate`, at ~ −20 dBFS to
/// avoid inter-sample-peak clipping on the AAC encode. Computed in `f64` over
/// lint-clean `From`-converted integers (the per-AU index is tiny, so the
/// `usize -> u32` clamp never bites); the result is in `[-0.1, 0.1]`.
fn tone_1k_sample(n: usize, sample_rate: u32) -> f32 {
    use std::f64::consts::TAU;
    const FREQ_HZ: f64 = 1_000.0;
    const AMPLITUDE: f64 = 0.1; // ~ −20 dBFS
    let rate = f64::from(sample_rate.max(1));
    let index = f64::from(u32::try_from(n).unwrap_or(u32::MAX));
    let phase = TAU * FREQ_HZ * index / rate;
    narrow_to_f32(AMPLITUDE * phase.sin())
}

/// Narrow a small, finite `f64` (a `[-1.0, 1.0]`-domain audio sample) to `f32`.
///
/// `as_conversions` is denied workspace-wide and there is no `From`/`TryFrom`
/// `f64 -> f32`, so this is the single contained narrowing point. The input is
/// pre-clamped to the finite `f32` range, so the conversion is lossy in
/// precision only — never UB, an overflow, or a surprising infinity.
#[allow(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "single contained f64->f32 audio-sample narrow; input clamped to finite f32 range, lossy precision only"
)]
fn narrow_to_f32(value: f64) -> f32 {
    value.clamp(f64::from(f32::MIN), f64::from(f32::MAX)) as f32
}

/// Feed one planar-f32 audio block of `samples` per channel into the encoder,
/// each sample produced by `gen(channel, sample_index)`. Returns the audio
/// packets the encoder emitted for this block (zero or more as the FIFO fills).
fn feed_audio_block(
    encoder: &mut ProgramEncoder,
    channels: usize,
    samples: usize,
    gen: impl Fn(usize, usize) -> f32,
) -> Result<Vec<EncodedPacket>> {
    // One contiguous plane per channel (planar f32, the layout the program bus
    // and the AAC encoder use).
    let planes: Vec<Vec<f32>> = (0..channels)
        .map(|ch| (0..samples).map(|n| gen(ch, n)).collect())
        .collect();
    let refs: Vec<&[f32]> = planes.iter().map(Vec::as_slice).collect();
    encoder.encode_audio(&refs, samples)
}
