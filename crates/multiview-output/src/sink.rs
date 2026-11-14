//! Real encode-once-mux-many output sinks (the `ffmpeg` feature).
//!
//! This module turns a stream of composited frames into encoded output, two
//! ways, sharing **one** encoder per rendition (invariant #7,
//! encode-once-mux-many):
//!
//! - [`FileSink`] — encode the canvas once and mux every packet into a single
//!   container file.
//! - [`SegmentSink`] — encode the canvas once and split the *same* packet
//!   stream into GOP-aligned segments (one self-contained MPEG-TS file each),
//!   feeding each completed segment into the pure-Rust
//!   [`MediaPlaylist`](crate::hls::MediaPlaylist) so the playlist references
//!   exactly the segments written.
//!
//! The encoder and muxer are built and driven entirely through
//! [`multiview_ffmpeg`]'s **safe** wrappers
//! ([`VideoEncoder`](multiview_ffmpeg::VideoEncoder) /
//! [`Muxer`](multiview_ffmpeg::Muxer)); this crate never touches libav directly and
//! stays `unsafe_code = forbid`. It never even *names* a raw libav packet type —
//! encoded packets flow from `receive_packet` straight into `write_packet` with
//! their type inferred.
//!
//! ## Timestamps (invariants #1/#3)
//!
//! Frames are presented to the encoder with a PTS computed **from the output
//! tick counter** — `out_pts = tick` in the encoder time-base (the reciprocal
//! of the output cadence). Raw input PTS is never forwarded to the encoder or
//! muxer; the source's per-frame timestamps are deliberately overwritten here.
//!
//! ## Licensing (LGPL-clean default)
//!
//! [`EncodeConfig::codec_name`] must name an LGPL software codec
//! (`"mpeg2video"`, `"ffv1"`, `"mjpeg"`, `"rawvideo"`). The GPL `x264`/`x265`
//! encoders are reserved for the separate `gpl-codecs` feature and are never
//! reachable through `ffmpeg` alone. Tests and the default path use
//! `mpeg2video`.

use std::path::{Path, PathBuf};

use ffmpeg_next::format::sample::Type as SampleType;
use ffmpeg_next::format::{Pixel, Sample};
use ffmpeg_next::util::frame::Video;
use ffmpeg_next::ChannelLayout;
use multiview_core::time::{rescale, MediaTime, Rational};
use multiview_ffmpeg::{
    program_h264_options, AudioEncodeTarget, AudioEncoder, DecodedVideoFrame, EncodedPacket, Muxer,
    ScaleSpec, Scaler, StreamCodecParameters, StreamKind, VideoEncodeTarget, VideoEncoder,
};

use crate::epoch::SharedEpoch;
use crate::error::{Error, Result};
use crate::hls::{
    media_segment_references, media_segment_uris, AudioRendition, BackgroundFile, LivePlaylist,
    MasterPlaylist, MediaPlaylist, Segment, SegmentType, SubtitleFeedReceiver, SubtitleMedia,
    SubtitleRendition, SubtitleStats, VariantStream,
};
use crate::metadata::{DisplayMatrix, MetadataScope, MuxMetadata};

mod push;
pub use push::{is_srt_listener_url, PushRunReport};

/// Nanoseconds in one second (the internal timeline unit, invariant #3).
const NANOS_PER_SEC: i64 = 1_000_000_000;

/// Test-only seam: counts how many *seed* encoders the segment sink builds
/// across a run. The seed encoder exists solely to copy codec parameters onto a
/// freshly-opened segment muxer; under encode-once-mux-many (invariant #7) the
/// codec is fixed for the whole run, so this must be built **once** regardless
/// of how many segments are produced. The unit tests assert exactly one build
/// per run; production code only ever increments it.
#[cfg(test)]
static SEED_ENCODER_BUILDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Record that one seed encoder was built (test-only instrumentation).
#[cfg(test)]
fn note_seed_encoder_built() {
    SEED_ENCODER_BUILDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// No-op in non-test builds: the seed-encoder counter is test-only.
#[cfg(not(test))]
const fn note_seed_encoder_built() {}

/// Test-only seam: counts how many segment muxers the segment sink *finalizes*
/// (writes the trailer for) across a run. The finalize-on-error fix must finish
/// the currently-open segment before propagating a mid-run error, so on the
/// error path this must equal the number of segments that were opened — not one
/// fewer (the bug left the last open segment un-finalized). MPEG-TS has no
/// load-bearing trailer, so this counter is the faithful structural signal the
/// fix is observable through.
#[cfg(test)]
static SEGMENT_FINALIZES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Record that one segment muxer was finalized (test-only instrumentation).
#[cfg(test)]
fn note_segment_finalized() {
    SEGMENT_FINALIZES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// No-op in non-test builds: the segment-finalize counter is test-only.
#[cfg(not(test))]
const fn note_segment_finalized() {}

/// A source of composited frames to encode.
///
/// The engine implements this over the compositor's program output; tests
/// implement it over decoded test frames. Each call yields the next frame to
/// present, or `Ok(None)` when the program is exhausted (a finite test run; a
/// live engine never ends).
///
/// The yielded frame's own timestamps are ignored: the sink re-stamps each
/// frame's PTS from the output tick counter (invariant #3) before encoding.
pub trait VideoFrameSource {
    /// Pull the next composited frame, or `Ok(None)` at end of program.
    ///
    /// # Errors
    /// Returns an [`Error`] if the underlying source failed to produce a frame.
    fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>>;
}

/// A source of already-encoded packets to mux (the packet-fed twin of
/// [`VideoFrameSource`]).
///
/// Encode-once-mux-many (invariant #7, ADR-0026): the canvas is encoded **once**
/// upstream and the *same* coded packets are fanned to N mux-only
/// [`PacketMuxSink`]s, each driving its own muxer off the engine hot path. Each
/// sink pulls owned packet copies from one of these. `Ok(None)` signals
/// end-of-program, at which point the sink finalizes its container/trailer.
///
/// The yielded packet's timestamps were already re-stamped from the output tick
/// counter before encode (invariant #3); this trait never re-stamps — it only
/// carries packets to the muxer, which performs the mechanical encoder→stream
/// time-base rescale.
pub trait PacketSource {
    /// Pull the next encoded packet, or `Ok(None)` at end of program.
    ///
    /// # Errors
    /// Returns an [`Error`] if the upstream packet feed failed.
    fn next_packet(&mut self) -> Result<Option<EncodedPacket>>;

    /// Whether the packet most recently returned by [`Self::next_packet`]
    /// opens a window whose decoder configuration changed (a routing move onto
    /// a RAP with different parameter sets). Reported at most once per change;
    /// a segmenting muxer asks only at a video keyframe and marks the segment
    /// that starts there discontinuous. Sources without the notion keep the
    /// default `false`.
    fn take_decoder_config_change(&mut self) -> bool {
        false
    }

    /// The new decoder configuration's video stream parameters, when the
    /// change [`Self::take_decoder_config_change`] just reported came with
    /// them (#342 B3, a live format reset): a segment muxer rebuilt at the
    /// change is seeded from them instead of the run's original seed.
    fn take_decoder_config_params(&mut self) -> Option<StreamCodecParameters> {
        None
    }
}

/// One muxer stream's registration: the `Send` codec-params snapshot it is
/// seeded from plus the encoder time-base its packets rescale out of.
///
/// A mux-only sink registers each `MuxStream` (the program video, and — under
/// AUD-4 — the optional program audio) **before** writing the container header;
/// the stream layout is then pinned for the session (ADR-R005 §3.3). The
/// kind-tagged packets a [`PacketSource`] yields route to the matching stream
/// (video packets to the video stream, audio packets to the audio stream).
#[derive(Clone, Copy)]
pub struct MuxStream<'a> {
    params: &'a StreamCodecParameters,
    time_base: Rational,
}

impl<'a> MuxStream<'a> {
    /// A stream registered from `params`, whose packets rescale out of
    /// `time_base` (the encoder time-base the packets were stamped in).
    #[must_use]
    pub fn new(params: &'a StreamCodecParameters, time_base: Rational) -> Self {
        Self { params, time_base }
    }
}

/// One discrete audio track a mux sink carries (#152): the track's index in the
/// run's declared track list ([`StreamKind::AudioTrack`]) plus the metadata the
/// muxer advertises for its PID (`language` = ISO-639, `title`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackSelection {
    /// Index into the run's declared discrete tracks.
    pub index: u16,
    /// ISO-639 language tag for the stream (PMT language descriptor).
    pub language: Option<String>,
    /// Human title for the stream.
    pub title: Option<String>,
}

/// Which audio a mux sink carries (#151): the program bus and/or a set of
/// discrete tracks. The default (program only, no tracks) is byte-identical to
/// the pre-#152 path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioSelection {
    /// Whether the mixed program audio stream is registered.
    pub program: bool,
    /// The discrete tracks registered after it, in this order.
    pub tracks: Vec<TrackSelection>,
}

impl Default for AudioSelection {
    fn default() -> Self {
        Self {
            program: true,
            tracks: Vec::new(),
        }
    }
}

/// The discrete-track streams registered on one muxer: `(track index, muxer
/// stream index)` pairs.
type TrackIndices = Vec<(u16, usize)>;

/// Register the selected discrete tracks on `muxer` (after video/program) and
/// tag each stream's language/title, before the header.
fn add_track_streams(
    muxer: &mut Muxer,
    selection: &AudioSelection,
    track_streams: &[MuxStream<'_>],
) -> Result<TrackIndices> {
    let mut out = Vec::with_capacity(selection.tracks.len());
    for track in &selection.tracks {
        let Some(stream) = track_streams.get(usize::from(track.index)) else {
            // A selection naming a track the encoder did not open is skipped
            // (never mis-routed); the run keeps its other streams.
            continue;
        };
        let index = muxer
            .add_stream_from_parameters(stream.params, stream.time_base)
            .map_err(ff)?;
        if let Some(language) = &track.language {
            muxer
                .set_stream_metadata(index, "language", language)
                .map_err(ff)?;
        }
        if let Some(title) = &track.title {
            muxer
                .set_stream_metadata(index, "title", title)
                .map_err(ff)?;
        }
        out.push((track.index, index));
    }
    Ok(out)
}

/// The muxer stream a discrete-track packet writes to, or `None` when this sink
/// does not carry that track (the packet is dropped for this sink).
fn track_stream_index(tracks: &TrackIndices, track: u16) -> Option<usize> {
    tracks.iter().find(|(t, _)| *t == track).map(|(_, i)| *i)
}

/// Resolve the muxer stream index a kind-tagged packet writes to.
///
/// An audio packet arriving at a sink with no registered audio stream is a
/// wiring bug (the fan-out produced audio for a video-only mux), surfaced as an
/// error rather than silently mis-routed onto the video stream.
fn stream_index_for(
    kind: StreamKind,
    video_index: usize,
    audio_index: Option<usize>,
) -> Result<usize> {
    match kind {
        StreamKind::Video => Ok(video_index),
        StreamKind::Audio => audio_index.ok_or_else(|| {
            Error::Output(
                "audio packet routed to a video-only mux sink (no audio stream registered)"
                    .to_owned(),
            )
        }),
        // `StreamKind` is `#[non_exhaustive]`: a future elementary-stream kind
        // this sink registers no stream for is an error, not a mis-route.
        _ => Err(Error::Output(
            "unsupported stream kind for this mux sink".to_owned(),
        )),
    }
}

/// Apply OUTMETA metadata + the optional display matrix to `muxer` before its
/// header (shared by [`PacketMuxSink`] and the resilient push writers).
fn apply_mux_metadata(
    muxer: &mut Muxer,
    metadata: &MuxMetadata,
    display_matrix: Option<DisplayMatrix>,
    video_index: usize,
) -> Result<()> {
    for entry in metadata.entries() {
        match entry.scope {
            MetadataScope::Format => muxer
                .set_format_metadata(&entry.key, &entry.value)
                .map_err(ff)?,
            MetadataScope::Stream { index } => muxer
                .set_stream_metadata(index, &entry.key, &entry.value)
                .map_err(ff)?,
        }
    }
    if let Some(matrix) = display_matrix {
        muxer
            .set_stream_display_matrix(video_index, matrix)
            .map_err(ff)?;
    }
    Ok(())
}

/// Map a `multiview-ffmpeg` error onto this crate's output error taxonomy.
// reason: takes the error by value so it can be used directly as
// `.map_err(ff)` (which hands ownership of the error); the body only needs a
// reference, hence the lint, but a `&` signature would force a closure at every
// call site.
#[allow(clippy::needless_pass_by_value)]
fn ff(err: multiview_ffmpeg::FfmpegError) -> Error {
    Error::Output(err.to_string())
}

/// Configuration for the single per-rendition encoder shared by both sinks.
///
/// `cadence` is the output clock's frames-per-second as an exact rational
/// (e.g. [`Rational::FPS_30`] or `60000/1001`) — never a float fps (invariant
/// #3). The encoder time-base is its reciprocal.
#[derive(Debug, Clone)]
pub struct EncodeConfig {
    /// LGPL software codec short name (`"mpeg2video"`, `"ffv1"`, `"mjpeg"`,
    /// `"rawvideo"`). GPL encoders are out of scope for the default build.
    pub codec_name: String,
    /// Canvas width in pixels.
    pub width: u32,
    /// Canvas height in pixels.
    pub height: u32,
    /// The pixel format fed to the encoder. The pipeline carries frames as NV12
    /// (invariant #5); this is the format the chosen codec accepts, and frames
    /// are converted NV12 -> this immediately before encoding. LGPL software
    /// codecs (`mpeg2video`, `mjpeg`, `ffv1`) want planar `yuv420p`.
    pub format: Pixel,
    /// Output cadence (frames per second) as an exact rational.
    pub cadence: Rational,
    /// Keyframe interval in frames (GOP size). For GOP-aligned segmenting this
    /// is also the per-segment frame count. Must be non-zero.
    pub gop: u32,
    /// Target bitrate in bits/sec (`0` lets the codec choose).
    pub bit_rate: usize,
    /// Optional program-audio encoder muxed alongside the video (AUD-4). `None`
    /// is video-only output: the muxer registers a single video stream and every
    /// existing video-only sink/test is unchanged. `Some` adds a second (audio)
    /// elementary stream carrying the mixed program bus.
    pub audio: Option<AudioEncodeConfig>,
    /// Optional CUDA device ordinal (e.g. `Some("1")`) to pin a `*_nvenc` encoder
    /// onto, threaded into the `multiview-ffmpeg` [`VideoEncodeTarget`] so encode
    /// lands on the admission-selected GPU instead of libav's default CUDA device
    /// (ordinal 0) — the NVENC device-affinity seam (Tier-2 P1a). `None` is the
    /// default-device behaviour, and the pin is **inert** for any non-`*_nvenc`
    /// codec (the encoder gates the bind on the codec-name suffix). A bind
    /// failure degrades gracefully to a default-device open, never a panic.
    pub cuda_ordinal: Option<String>,
}

/// Configuration for the optional program-audio encoder (AUD-4).
///
/// The program bus (`multiview_audio::program::ProgramBus`) mixes per-input
/// audio into one stream at a fixed rate; this names the codec it is encoded
/// with before being muxed as the output's second elementary stream. Kept
/// backend-agnostic (a short codec name + rate/channels/bitrate); [`audio_target`]
/// translates it into the [`AudioEncodeTarget`] the encoder is opened from.
///
/// [`audio_target`]: AudioEncodeConfig::audio_target
#[derive(Debug, Clone)]
pub struct AudioEncodeConfig {
    /// libav short codec name (`"aac"`, `"flac"`). LGPL-clean for the default
    /// build (AAC's native libav encoder is LGPL — no external `libfdk`).
    pub codec_name: String,
    /// Sample rate in Hz. The program bus runs at 48 kHz.
    pub sample_rate: u32,
    /// Channel count (`1` = mono, `2` = stereo); selects the encoder's layout.
    pub channels: u16,
    /// Target bitrate in bits/sec (`0` lets the codec choose).
    pub bit_rate: usize,
}

impl AudioEncodeConfig {
    /// The default LGPL-clean program-audio codec: native libav `aac`.
    #[must_use]
    pub fn aac(sample_rate: u32, channels: u16, bit_rate: usize) -> Self {
        Self {
            codec_name: "aac".to_owned(),
            sample_rate,
            channels,
            bit_rate,
        }
    }

    /// The channel layout for this configuration's channel count. Mono and
    /// stereo are named explicitly (the only layouts the program bus mixes
    /// today); any other count falls back to libav's default layout for it.
    #[must_use]
    fn channel_layout(&self) -> ChannelLayout {
        match self.channels {
            1 => ChannelLayout::MONO,
            2 => ChannelLayout::STEREO,
            other => ChannelLayout::default(i32::from(other)),
        }
    }

    /// Translate into the `multiview-ffmpeg` [`AudioEncodeTarget`] the
    /// [`AudioEncoder`](multiview_ffmpeg::AudioEncoder) is opened from. The
    /// program bus is planar float (`fltp`), the format AAC and FLAC accept, so
    /// the encoder is fed the bus samples without an extra resample.
    #[must_use]
    pub fn audio_target(&self) -> AudioEncodeTarget {
        AudioEncodeTarget {
            codec_name: self.codec_name.clone(),
            format: Sample::F32(SampleType::Planar),
            channel_layout: self.channel_layout(),
            sample_rate: self.sample_rate,
            bit_rate: self.bit_rate,
        }
    }
}

impl EncodeConfig {
    /// A sensible LGPL-clean default for tests/examples: `mpeg2video` fed
    /// `yuv420p`, 30 fps, the given geometry, a one-second GOP. Video-only
    /// (`audio: None`) so existing streaming-encode tests are unchanged.
    #[must_use]
    pub fn mpeg2(width: u32, height: u32) -> Self {
        Self {
            codec_name: "mpeg2video".to_owned(),
            width,
            height,
            format: Pixel::YUV420P,
            cadence: Rational::FPS_30,
            gop: 30,
            bit_rate: 2_000_000,
            audio: None,
            // No device pin by default — behaviour is unchanged from before the
            // NVENC affinity seam (Tier-2 P1a); the pin is opt-in.
            cuda_ordinal: None,
        }
    }

    /// The encoder time-base: the reciprocal of the cadence (seconds per tick),
    /// so a frame at tick `n` carries PTS `n`.
    #[must_use]
    pub fn time_base(&self) -> Rational {
        Rational::new(self.cadence.den, self.cadence.num)
    }

    /// Open the program video encoder for this configuration.
    ///
    /// Every program encode opens through here so they all carry the same
    /// pinned stream-shaping options ([`program_h264_options`]) instead of
    /// inheriting libav's version-dependent encoder defaults.
    fn open_video_encoder(&self) -> Result<VideoEncoder> {
        VideoEncoder::new_with_options(&self.target(), &program_h264_options(&self.codec_name))
            .map_err(ff)
    }

    /// Build the `multiview-ffmpeg` encode target for this configuration.
    fn target(&self) -> VideoEncodeTarget {
        VideoEncodeTarget {
            codec_name: self.codec_name.clone(),
            width: self.width,
            height: self.height,
            format: self.format,
            time_base: self.time_base(),
            bit_rate: self.bit_rate,
            gop: self.gop,
            // NVENC device-affinity pin (Tier-2 P1a): inert for software codecs;
            // the encoder gates the bind on the `*_nvenc` codec-name suffix.
            cuda_device: self.cuda_ordinal.clone(),
        }
    }

    /// Validate the configuration before opening an encoder.
    fn validate(&self) -> Result<()> {
        if self.width == 0 || self.height == 0 {
            return Err(Error::Output(
                "encode canvas has a zero dimension".to_owned(),
            ));
        }
        if self.gop == 0 {
            return Err(Error::Output("encode GOP size must be non-zero".to_owned()));
        }
        if self.cadence.num <= 0 || self.cadence.den <= 0 {
            return Err(Error::Output("output cadence must be positive".to_owned()));
        }
        Ok(())
    }
}

/// Lazily-built NV12 -> encoder-format converter, reused across frames.
///
/// The compositor's program output is NV12 (invariant #5), but LGPL software
/// encoders want planar `yuv420p`; this performs that one conversion immediately
/// before `send_frame`, via `multiview-ffmpeg`'s safe [`Scaler`] (libswscale). The
/// scaler is built on the first frame and rebuilt only if the geometry/format
/// changes. When the source frame already matches the encoder format, the frame
/// is passed through with only its PTS re-stamped.
struct FrameConverter {
    dst: Pixel,
    scaler: Option<Scaler>,
}

impl FrameConverter {
    fn new(dst: Pixel) -> Self {
        Self { dst, scaler: None }
    }

    /// Convert `frame` to the encoder format if needed and stamp it with `pts`
    /// (from the output tick counter — invariants #1/#3).
    fn prepare(&mut self, frame: Video, pts: i64) -> Result<Video> {
        let mut out = if frame.format() == self.dst {
            frame
        } else {
            let src = ScaleSpec::new(frame.format(), frame.width(), frame.height());
            let dst = ScaleSpec::new(self.dst, frame.width(), frame.height());
            let rebuild = match &self.scaler {
                Some(s) => s.source() != src || s.destination() != dst,
                None => true,
            };
            if rebuild {
                self.scaler = Some(Scaler::new(src, dst).map_err(ff)?);
            }
            let scaler = self
                .scaler
                .as_mut()
                .ok_or_else(|| Error::Output("frame converter unexpectedly absent".to_owned()))?;
            scaler.run(&frame).map_err(ff)?
        };
        // Re-stamp from the tick counter; the source's input PTS is discarded.
        out.set_pts(Some(pts));
        Ok(out)
    }
}

/// The single **encode-once** producer (invariant #7, ADR-E003/E004, ADR-0026).
///
/// Owns ONE [`VideoEncoder`] plus the NV12 → codec-format `FrameConverter` and
/// the output tick counter, turning each baked NV12 canvas frame into the coded
/// [`EncodedPacket`]s it produced, with every PTS re-stamped from the tick
/// (`out_pts = f(tick)`, inv #3 — never the input PTS). The cli's bake consumer
/// owns one of these: it feeds each baked frame in and fans the produced
/// packets — as independently-owned copies — to N mux-only [`PacketMuxSink`]s,
/// so the canvas is encoded exactly once and the *same* coded packets feed every
/// transport (file / HLS / push), never a per-output re-encode.
///
/// It holds no muxer and no channel and never blocks on a client: it is a pure
/// frame-in → packets-out transform driven by the off-hot-path consumer, so it
/// can neither stall the output clock (inv #1) nor be back-pressured by a slow
/// sink (inv #10). The codec is fixed for the run, so the
/// [`StreamCodecParameters`] each muxer seeds its stream from is snapshotted once
/// at construction.
/// The optional program-audio encode state owned by a [`ProgramEncoder`]
/// (AUD-4): one AAC encoder plus the per-channel sample FIFO that rebuffers the
/// program bus's variable-size blocks (~1600 samples/tick at 48 kHz/30 fps) into
/// the encoder's fixed `frame_size` (1024) frames, and the running audio sample
/// counter that stamps each frame's PTS.
struct AudioState {
    encoder: AudioEncoder,
    /// Codec-params snapshot a mux sink registers its audio stream from.
    params: StreamCodecParameters,
    /// The audio encoder time-base (`1/sample_rate`).
    time_base: Rational,
    /// Samples per encoder frame (1024 for AAC); the FIFO emits in this unit.
    frame_size: usize,
    /// One pending-sample queue per channel; full `frame_size` chunks are popped
    /// off the front and encoded, the remainder carried to the next block.
    fifo: Vec<Vec<f32>>,
    /// `audio_pts = Σ samples emitted` — the audio analogue of `out_pts = tick`
    /// (invariant #3); every encoded frame is stamped from this, never input PTS.
    audio_pts: i64,
    /// The stream kind every produced packet is tagged with: the program
    /// ([`StreamKind::Audio`]) or a discrete track ([`StreamKind::AudioTrack`]).
    kind: StreamKind,
}

/// One independently admitted video rendition. Shares the baked canvas with
/// sibling renditions; it never opens or duplicates a program-audio encoder.
pub struct VideoRenditionEncoder {
    encoder: VideoEncoder,
    converter: FrameConverter,
    params: StreamCodecParameters,
    time_base: Rational,
    tick: i64,
    finished: bool,
    no_reorder: bool,
}

impl VideoRenditionEncoder {
    /// Open one video-only encoder with the exact requested configuration.
    ///
    /// # Errors
    /// Returns an error if the video encoder is unavailable or invalid.
    pub fn new(config: &EncodeConfig) -> Result<Self> {
        config.validate()?;
        let pairs: &[(&str, &str)] =
            // `forced-idr=1` (ADR-0108 §4.2): a forced keyframe (a route move,
            // `force_next_keyframe`) is a true IDR. It changes nothing for a
            // stream that never forces one. No family is opened with
            // `AV_CODEC_FLAG_GLOBAL_HEADER`, so libavcodec repeats the parameter
            // sets in band at every IDR (x264 `b_repeat_headers`, x265
            // `repeat-headers`, NVENC `repeatSPSPPS`).
            // libx265 defaults to an open GOP, which turns a forced I picture
            // and every periodic keyframe into a CRA: never a decoder-ready RAP
            // for a joined or moved consumer. A closed GOP makes them IDRs.
            if config.codec_name == "libx264" {
                &[("tune", "zerolatency"), ("bf", "0"), ("forced-idr", "1")]
            } else if config.codec_name == "libx265" {
                &[
                    ("tune", "zerolatency"),
                    ("bf", "0"),
                    ("forced-idr", "1"),
                    ("x265-params", "open-gop=0"),
                ]
            } else if config.codec_name.ends_with("_nvenc") {
                &[
                    ("tune", "ull"),
                    ("zerolatency", "1"),
                    ("delay", "0"),
                    ("bf", "0"),
                    ("forced-idr", "1"),
                ]
            } else {
                &[("bf", "0")]
            };
        let options = multiview_ffmpeg::CodecOptions::from_pairs(pairs)
            .map_err(|e| Error::Output(e.to_string()))?;
        let encoder = VideoEncoder::new_with_options(&config.target(), &options).map_err(ff)?;
        let params = StreamCodecParameters::from_encoder(&encoder);
        let time_base = encoder.time_base();
        let no_reorder = encoder.no_reorder_proven();
        Ok(Self {
            encoder,
            converter: FrameConverter::new(config.format),
            params,
            time_base,
            tick: 0,
            finished: false,
            no_reorder,
        })
    }

    /// Start this encoder at absolute output tick `t0` instead of 0 (ADR-0108
    /// §4.3, an encoder opened live): the first frame it accepts is `t0`, and
    /// it is a keyframe. PTS stays the absolute tick index, so a subscriber
    /// moved from an encoder that started at 0 sees a one-tick step.
    #[must_use]
    pub fn starting_at(mut self, t0: u64) -> Self {
        self.start_at(t0);
        self
    }

    /// In-place [`Self::starting_at`], for an encoder already owned by a worker.
    pub fn start_at(&mut self, t0: u64) {
        self.tick = i64::try_from(t0).unwrap_or(i64::MAX);
        self.encoder.force_next_keyframe();
    }

    /// The next tick this encoder accepts.
    #[must_use]
    pub fn next_tick(&self) -> u64 {
        u64::try_from(self.tick).unwrap_or(0)
    }

    /// Make the next encoded frame a keyframe (an IDR with `forced-idr=1`).
    /// One-shot; the caller owns the rate limit (ADR-0108 §4.2: one forced IDR
    /// per 2 s per encoded bus).
    pub fn force_next_keyframe(&mut self) {
        self.encoder.force_next_keyframe();
    }

    /// True only when the opened encoder is proven never to reorder (no B
    /// frames configured). NOT a zero-delay claim: a delayed-but-ordered
    /// encoder (e.g. mpeg2video's fixed one-frame delay) still proves this
    /// true, and its drained packets are re-stamped `DTS := PTS` (see
    /// [`Self::drain`]); the emission order is pinned empirically by tests.
    #[must_use]
    pub const fn no_reorder_proven(&self) -> bool {
        self.no_reorder
    }

    /// Codec parameters for all bindings to this rendition.
    #[must_use]
    pub fn codec_params(&self) -> &StreamCodecParameters {
        &self.params
    }

    /// The output cadence reciprocal used for encoder timestamps.
    #[must_use]
    pub fn time_base(&self) -> Rational {
        self.time_base
    }

    /// Encode one immutable baked canvas at the absolute output tick.
    ///
    /// # Errors
    /// Returns an error for an invalid frame or an encoder failure.
    pub fn encode_frame_at(
        &mut self,
        frame: DecodedVideoFrame,
        tick: u64,
    ) -> Result<Vec<EncodedPacket>> {
        if self.finished {
            return Err(Error::Output(
                "video rendition encoder is finished".to_owned(),
            ));
        }
        let pts = i64::try_from(tick)
            .map_err(|_| Error::Output("video tick exceeds i64 PTS".to_owned()))?;
        if pts != self.tick {
            return Err(Error::Output(format!(
                "video tick discontinuity: expected {}, got {pts}",
                self.tick
            )));
        }
        let prepared = self.converter.prepare(frame.frame, pts)?;
        self.encoder.send_frame(&prepared).map_err(ff)?;
        self.tick = pts.saturating_add(1);
        self.drain()
    }

    /// Flush any delayed coded frames after the last input tick.
    ///
    /// # Errors
    /// Returns an error if encoder EOF or packet receive fails.
    pub fn finish(&mut self) -> Result<Vec<EncodedPacket>> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;
        self.encoder.send_eof().map_err(ff)?;
        self.drain()
    }

    fn drain(&mut self) -> Result<Vec<EncodedPacket>> {
        let mut packets = Vec::new();
        while let Some(packet) = self.encoder.receive_packet().map_err(ff)? {
            packets.push(EncodedPacket::from_packet(packet));
        }
        // A proven no-reorder encoder emits packets in frame order, so this
        // packet's PTS is its frame's tick and decode order equals
        // presentation order: re-stamp DTS := PTS (the output-clock rule —
        // timestamps come from the tick counter, never from a floating encoder
        // artifact). This cancels the one-frame DTS lag of delayed-but-ordered
        // encoders (e.g. mpeg2video's fixed `delay = 1`, whose first packet
        // would otherwise carry DTS one tick below zero and fall outside the
        // dispatcher's tick-window protocol). Reordering encoders keep
        // libav's DTS untouched: their decode order genuinely differs from
        // presentation order. The per-codec emission order this relies on is
        // PINNED EMPIRICALLY by the emission-order tests below, not by doc.
        if self.no_reorder {
            for packet in &mut packets {
                packet.set_dts_to_pts();
            }
        }
        Ok(packets)
    }
}

/// One shared program-audio rendition, independent of all video keys.
pub struct AudioRenditionEncoder {
    audio: AudioState,
    tracks: Vec<AudioState>,
    config: AudioEncodeConfig,
    finished: bool,
}

impl AudioRenditionEncoder {
    /// Open the program audio exactly once, independent of video rendition count.
    ///
    /// # Errors
    /// Returns an error when its audio codec cannot open.
    pub fn new(config: &AudioEncodeConfig) -> Result<Self> {
        Ok(Self {
            audio: AudioState::open(config, StreamKind::Audio)?,
            tracks: Vec::new(),
            config: config.clone(),
            finished: false,
        })
    }

    /// Open a program-audio encoder whose first frame starts at absolute
    /// sample `start` of the run's one sample counter (ADR-0108 §5.2 Rule 1:
    /// one global AAC frame grid). `start` must be a multiple of the opened
    /// encoder's frame size, so this bus emits frames on the same boundaries
    /// and with the same PTS as every other bus (today's program encoder
    /// starts at 0, also on the grid). The caller feeds PCM from `start` on.
    ///
    /// # Errors
    /// The codec cannot open, `start` is negative, or `start` is off the grid.
    pub fn starting_at_sample(config: &AudioEncodeConfig, start: i64) -> Result<Self> {
        let mut encoder = Self::new(config)?;
        let frame = i64::try_from(encoder.audio.frame_size).unwrap_or(i64::MAX);
        if start < 0 || frame <= 0 || start % frame != 0 {
            return Err(Error::Output(format!(
                "audio start sample {start} is not on the {frame}-sample frame grid"
            )));
        }
        encoder.audio.audio_pts = start;
        Ok(encoder)
    }

    /// The absolute sample index the next fed sample will carry (the encoded
    /// frames' PTS run plus the samples still buffered for the next frame).
    #[must_use]
    pub fn next_sample(&self) -> i64 {
        let pending = self.audio.fifo.first().map_or(0, Vec::len);
        self.audio
            .audio_pts
            .saturating_add(i64::try_from(pending).unwrap_or(i64::MAX))
    }

    /// Open separately identified discrete tracks with the shared audio format.
    ///
    /// # Errors
    /// Returns an error when the requested track codec cannot open.
    pub fn open_tracks(&mut self, count: u16) -> Result<()> {
        if self.finished || !self.tracks.is_empty() || self.audio.audio_pts != 0 {
            return Err(Error::Output(
                "audio tracks must open before encoding".to_owned(),
            ));
        }
        for index in 0..count {
            self.tracks.push(AudioState::open(
                &self.config,
                StreamKind::AudioTrack(index),
            )?);
        }
        Ok(())
    }

    /// The settings this encoder opened with (a private per-output bus opens
    /// with exactly these, so its `AudioSpecificConfig` can match, §5.2).
    #[must_use]
    pub const fn config(&self) -> &AudioEncodeConfig {
        &self.config
    }

    /// Program-audio encoder frame size in exact samples (AAC is typically 1024).
    #[must_use]
    pub fn frame_samples(&self) -> Option<u32> {
        u32::try_from(self.audio.frame_size).ok()
    }

    /// Per-track sample rate and frame size from the actual opened encoder.
    #[must_use]
    pub fn watermark_streams(&self) -> Vec<(u32, u32)> {
        std::iter::once(&self.audio)
            .chain(self.tracks.iter())
            .filter_map(|state| {
                Some((
                    u32::try_from(state.frame_size).ok()?,
                    u32::try_from(state.time_base.den).ok()?,
                ))
            })
            .collect()
    }

    /// Program-audio stream parameters and sample-clock time base.
    #[must_use]
    pub fn codec_params(&self) -> &StreamCodecParameters {
        &self.audio.params
    }

    /// Program-audio sample-clock time base.
    #[must_use]
    pub fn time_base(&self) -> Rational {
        self.audio.time_base
    }

    /// Discrete tracks in stable stream-index order.
    #[must_use]
    pub fn track_streams(&self) -> Vec<(StreamCodecParameters, Rational)> {
        self.tracks
            .iter()
            .map(|track| (track.params.clone(), track.time_base))
            .collect()
    }

    /// Encode a mixed program block once for all video renditions.
    ///
    /// # Errors
    /// Returns an error after finish or if encoder input is invalid.
    pub fn encode_interleaved(
        &mut self,
        samples: &[f32],
        frames: usize,
    ) -> Result<Vec<EncodedPacket>> {
        if self.finished {
            return Err(Error::Output(
                "audio rendition encoder is finished".to_owned(),
            ));
        }
        self.audio.encode_interleaved(samples, frames)
    }

    /// Encode one discrete track from a source block or matching silence.
    ///
    /// # Errors
    /// Returns an error after finish, for an unknown track, or encoder failure.
    pub fn encode_track_interleaved(
        &mut self,
        index: usize,
        samples: Option<&[f32]>,
        frames: usize,
    ) -> Result<Vec<EncodedPacket>> {
        if self.finished {
            return Err(Error::Output(
                "audio rendition encoder is finished".to_owned(),
            ));
        }
        let track = self
            .tracks
            .get_mut(index)
            .ok_or_else(|| Error::Output(format!("unknown discrete audio track {index}")))?;
        let needed = frames.saturating_mul(track.fifo.len());
        if let Some(samples) = samples.filter(|samples| samples.len() >= needed) {
            track.encode_interleaved(samples, frames)
        } else {
            track.encode_interleaved(&vec![0.0; needed], frames)
        }
    }

    /// Flush every audio track and the program once.
    ///
    /// # Errors
    /// Returns an error if an audio encoder cannot flush.
    pub fn finish(&mut self) -> Result<Vec<EncodedPacket>> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;
        let mut packets = self.audio.finish()?;
        for track in &mut self.tracks {
            packets.extend(track.finish()?);
        }
        Ok(packets)
    }
}

/// The single **encode-once** program producer.
pub struct ProgramEncoder {
    /// The single video encoder (the one encode of invariant #7).
    encoder: VideoEncoder,
    /// NV12 → encoder-format conversion (libswscale), built lazily on first use.
    converter: FrameConverter,
    /// The codec-params snapshot every [`PacketMuxSink`] seeds its stream from
    /// (taken once: the codec is fixed across the encode).
    params: StreamCodecParameters,
    /// The encoder time-base packets are stamped in (reciprocal of the cadence).
    time_base: Rational,
    /// The optional program-audio encoder + rebuffer (AUD-4). `None` for a
    /// video-only run, so the existing single-stream path is unchanged.
    audio: Option<AudioState>,
    /// The program-audio encode settings, kept so discrete tracks (#152) are
    /// opened with exactly the program's codec/rate/layout/bitrate.
    audio_cfg: Option<AudioEncodeConfig>,
    /// One encoder per declared discrete audio track (#152), index = the
    /// [`StreamKind::AudioTrack`] index. Each is fed exactly the program's
    /// per-tick sample budget, so its sample counter — and therefore its PTS —
    /// is identical to the program audio's (locked to vision). Empty when no
    /// tracks are declared (the program-only path is unchanged).
    tracks: Vec<AudioState>,
    /// The monotonic output tick; each [`encode_frame`](Self::encode_frame)
    /// stamps the frame's PTS with it, then advances it by one.
    tick: i64,
    /// Set by [`finish`](Self::finish): the encoder has been flushed, so further
    /// [`encode_frame`](Self::encode_frame) calls are rejected.
    finished: bool,
}

impl ProgramEncoder {
    /// Open the single encoder for `config` (validated first), snapshotting the
    /// codec parameters and time-base every mux sink will seed its stream from.
    ///
    /// # Errors
    /// Returns [`Error::Output`] if the configuration is invalid (zero geometry /
    /// GOP) or the encoder cannot be opened for the named codec.
    pub fn new(config: &EncodeConfig) -> Result<Self> {
        config.validate()?;
        let encoder = config.open_video_encoder()?;
        let params = StreamCodecParameters::from_encoder(&encoder);
        let time_base = encoder.time_base();
        let audio = match &config.audio {
            Some(audio_cfg) => Some(AudioState::open(audio_cfg, StreamKind::Audio)?),
            None => None,
        };
        Ok(Self {
            encoder,
            converter: FrameConverter::new(config.format),
            params,
            time_base,
            audio,
            audio_cfg: config.audio.clone(),
            tracks: Vec::new(),
            tick: 0,
            finished: false,
        })
    }

    /// Open `count` discrete audio-track encoders (#152) with the program
    /// audio's exact settings. Call before the first encode; a later call
    /// replaces nothing (it is rejected) so every track's sample counter starts
    /// at the program's. A no-op for `count == 0`.
    ///
    /// # Errors
    /// [`Error::Output`] if the run carries no program audio, encoding already
    /// started, tracks were already opened, or an encoder cannot be opened.
    pub fn open_audio_tracks(&mut self, count: u16) -> Result<()> {
        if count == 0 {
            return Ok(());
        }
        let cfg = self.audio_cfg.clone().ok_or_else(|| {
            Error::Output("discrete audio tracks need program audio enabled".to_owned())
        })?;
        let started = self.tick != 0 || self.audio.as_ref().is_some_and(|a| a.audio_pts != 0);
        if started || !self.tracks.is_empty() {
            return Err(Error::Output(
                "discrete audio tracks must be opened before encoding starts".to_owned(),
            ));
        }
        for index in 0..count {
            self.tracks
                .push(AudioState::open(&cfg, StreamKind::AudioTrack(index))?);
        }
        Ok(())
    }

    /// The `(codec params, time-base)` of each discrete track, in track-index
    /// order (empty when none are open).
    #[must_use]
    pub fn audio_track_streams(&self) -> Vec<(StreamCodecParameters, Rational)> {
        self.tracks
            .iter()
            .map(|t| (t.params.clone(), t.time_base))
            .collect()
    }

    /// The number of open discrete audio-track encoders.
    #[must_use]
    pub fn audio_track_count(&self) -> usize {
        self.tracks.len()
    }

    /// Encode one tick of discrete track `index` (interleaved f32, `frames`
    /// frames). `None` (the source contributed nothing this tick, or a block of
    /// the wrong shape) encodes `frames` of silence, so the track's sample clock
    /// always advances exactly with the program's.
    ///
    /// # Errors
    /// [`Error::Output`] after [`finish`](Self::finish), for an unknown track, or
    /// if the encode fails.
    pub fn encode_track_interleaved(
        &mut self,
        index: usize,
        interleaved: Option<&[f32]>,
        frames: usize,
    ) -> Result<Vec<EncodedPacket>> {
        if self.finished {
            return Err(Error::Output(
                "ProgramEncoder::encode_track_interleaved called after finish".to_owned(),
            ));
        }
        let track = self
            .tracks
            .get_mut(index)
            .ok_or_else(|| Error::Output(format!("unknown discrete audio track {index}")))?;
        let channels = track.fifo.len();
        let needed = frames.saturating_mul(channels);
        match interleaved {
            Some(samples) if samples.len() >= needed => track.encode_interleaved(samples, frames),
            _ => {
                let silence = vec![0.0_f32; needed];
                track.encode_interleaved(&silence, frames)
            }
        }
    }

    /// The program-audio codec parameters a mux sink registers its audio stream
    /// from (AUD-4), or `None` for a video-only run.
    #[must_use]
    pub fn audio_codec_params(&self) -> Option<&StreamCodecParameters> {
        self.audio.as_ref().map(|a| &a.params)
    }

    /// The program-audio encoder time-base (`1/sample_rate`), or `None` for a
    /// video-only run.
    #[must_use]
    pub fn audio_time_base(&self) -> Option<Rational> {
        self.audio.as_ref().map(|a| a.time_base)
    }

    /// Encode one program-bus audio block: `planes` is one planar-f32 slice per
    /// channel, each carrying `samples` samples. The samples are appended to the
    /// per-channel FIFO and every full `frame_size` chunk is encoded into
    /// [`StreamKind::Audio`]-tagged packets, each stamped from the running sample
    /// counter (the audio analogue of `out_pts = f(tick)`, inv #3). A partial
    /// remainder is carried to the next call. A no-op (empty) for a video-only
    /// run, so the consumer can call it unconditionally.
    ///
    /// # Errors
    /// Returns [`Error::Output`] if the channel count mismatches the encoder, a
    /// plane is shorter than `samples`, or the encode fails.
    pub fn encode_audio(
        &mut self,
        planes: &[&[f32]],
        samples: usize,
    ) -> Result<Vec<EncodedPacket>> {
        if self.finished {
            return Err(Error::Output(
                "ProgramEncoder::encode_audio called after finish".to_owned(),
            ));
        }
        match self.audio.as_mut() {
            Some(state) => state.encode(planes, samples),
            None => Ok(Vec::new()),
        }
    }

    /// Encode one program-bus audio block given as **interleaved** f32 samples
    /// (`[L, R, L, R, …]` for stereo) carrying `frames` frames — the layout the
    /// [`AudioBlock`](multiview_audio::AudioBlock) program bus produces. The block
    /// is de-interleaved into the per-channel FIFO and encoded exactly like
    /// [`encode_audio`](Self::encode_audio). A no-op for a video-only run.
    ///
    /// # Errors
    /// Returns [`Error::Output`] if called after [`finish`](Self::finish), the
    /// block is shorter than `frames × channels`, or the encode fails.
    pub fn encode_audio_interleaved(
        &mut self,
        interleaved: &[f32],
        frames: usize,
    ) -> Result<Vec<EncodedPacket>> {
        if self.finished {
            return Err(Error::Output(
                "ProgramEncoder::encode_audio_interleaved called after finish".to_owned(),
            ));
        }
        match self.audio.as_mut() {
            Some(state) => state.encode_interleaved(interleaved, frames),
            None => Ok(Vec::new()),
        }
    }

    /// The codec parameters each [`PacketMuxSink`] seeds its muxer stream from.
    /// Snapshotted once at [`new`](Self::new) — the codec is fixed across the
    /// encode (invariant #7), so this is cloned per sink, never rebuilt.
    #[must_use]
    pub fn codec_params(&self) -> &StreamCodecParameters {
        &self.params
    }

    /// The encoder time-base the produced packets are stamped in (the reciprocal
    /// of the output cadence), to be passed to [`PacketMuxSink::run`].
    #[must_use]
    pub fn time_base(&self) -> Rational {
        self.time_base
    }

    /// Encode one baked canvas `frame`, returning the coded packets it produced
    /// this call (zero or more — the encoder may buffer a frame before emitting).
    /// The frame is converted to the encoder format if needed and its PTS is
    /// re-stamped from the internal tick counter, which then advances by one
    /// (`out_pts = f(tick)`, inv #3) — the input PTS is discarded.
    ///
    /// # Errors
    /// Returns [`Error::Output`] if called after [`finish`](Self::finish), or if
    /// the format conversion or the encode fails.
    pub fn encode_frame(&mut self, frame: DecodedVideoFrame) -> Result<Vec<EncodedPacket>> {
        let tick = u64::try_from(self.tick).unwrap_or(0);
        self.encode_frame_at(frame, tick)
    }

    /// Encode one baked canvas `frame` stamped at the absolute output tick
    /// `tick` (`out_pts = f(tick)`, inv #3).
    ///
    /// A live run sheds frames under overload, so the consumer only sees the
    /// surviving ticks. Stamping from the tick index (not a count of encoded
    /// frames) keeps video on the same clock the program audio is driven by
    /// (`ProgramBus::tick_to(tick + 1)`): a shed tick leaves a PTS gap, never an
    /// accumulating A/V offset. A `tick` at or before the previous one is stamped
    /// one past it, so PTS stays strictly increasing.
    ///
    /// # Errors
    /// As [`encode_frame`](Self::encode_frame).
    pub fn encode_frame_at(
        &mut self,
        frame: DecodedVideoFrame,
        tick: u64,
    ) -> Result<Vec<EncodedPacket>> {
        if self.finished {
            return Err(Error::Output(
                "ProgramEncoder::encode_frame called after finish".to_owned(),
            ));
        }
        let pts = i64::try_from(tick).unwrap_or(i64::MAX).max(self.tick);
        let prepared = self.converter.prepare(frame.frame, pts)?;
        self.encoder.send_frame(&prepared).map_err(ff)?;
        self.tick = pts.saturating_add(1);
        self.drain()
    }

    /// Flush the encoder (send EOF) and return its trailing packets. Idempotent:
    /// a second call yields no packets. After this, [`encode_frame`](Self::encode_frame)
    /// is rejected.
    ///
    /// # Errors
    /// Returns [`Error::Output`] if signalling EOF or a trailing receive fails.
    pub fn finish(&mut self) -> Result<Vec<EncodedPacket>> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;
        // Flush audio first (drain its FIFO remainder + EOF), then video; the
        // caller routes each tagged packet to its stream, so order is moot.
        let mut out = match self.audio.as_mut() {
            Some(state) => state.finish()?,
            None => Vec::new(),
        };
        for track in &mut self.tracks {
            out.append(&mut track.finish()?);
        }
        self.encoder.send_eof().map_err(ff)?;
        out.append(&mut self.drain()?);
        Ok(out)
    }

    /// Drain every packet currently available from the encoder into owned
    /// [`EncodedPacket`]s (each independently muxable by a downstream sink).
    fn drain(&mut self) -> Result<Vec<EncodedPacket>> {
        let mut out = Vec::new();
        while let Some(packet) = self.encoder.receive_packet().map_err(ff)? {
            out.push(EncodedPacket::from_packet(packet));
        }
        Ok(out)
    }
}

impl AudioState {
    /// Open the AAC encoder for `config` and seed an empty per-channel FIFO.
    fn open(config: &AudioEncodeConfig, kind: StreamKind) -> Result<Self> {
        let encoder = AudioEncoder::new(&config.audio_target()).map_err(ff)?;
        let params = StreamCodecParameters::from_audio_encoder(&encoder);
        let time_base = encoder.time_base();
        // The encoder's required samples-per-frame; 0 means it accepts any size,
        // in which case we still batch in a sensible fixed unit.
        let frame_size = match encoder.frame_size() {
            0 => 1024,
            n => usize::try_from(n).unwrap_or(1024),
        };
        let channels = encoder.channels();
        Ok(Self {
            encoder,
            params,
            time_base,
            frame_size,
            fifo: vec![Vec::new(); channels],
            audio_pts: 0,
            kind,
        })
    }

    /// Append a block to the FIFO and encode every full `frame_size` chunk.
    fn encode(&mut self, planes: &[&[f32]], samples: usize) -> Result<Vec<EncodedPacket>> {
        if planes.len() != self.fifo.len() {
            return Err(Error::Output(
                "program audio block channel count does not match the encoder layout".to_owned(),
            ));
        }
        for (queue, plane) in self.fifo.iter_mut().zip(planes.iter()) {
            let src = plane.get(..samples).ok_or_else(|| {
                Error::Output("program audio plane shorter than the sample count".to_owned())
            })?;
            queue.extend_from_slice(src);
        }
        self.drain_full_frames()
    }

    /// De-interleave `interleaved` (`frames × channels` f32, `[L, R, L, R, …]`)
    /// into the per-channel FIFO and encode every full frame.
    fn encode_interleaved(
        &mut self,
        interleaved: &[f32],
        frames: usize,
    ) -> Result<Vec<EncodedPacket>> {
        let channels = self.fifo.len();
        if channels == 0 {
            return Ok(Vec::new());
        }
        let needed = frames
            .checked_mul(channels)
            .ok_or_else(|| Error::Output("interleaved audio frame count overflow".to_owned()))?;
        if interleaved.len() < needed {
            return Err(Error::Output(
                "interleaved program audio shorter than frames × channels".to_owned(),
            ));
        }
        for frame in 0..frames {
            let base = frame * channels;
            for (channel, queue) in self.fifo.iter_mut().enumerate() {
                // `.get` (not index) keeps this panic-free even on a short block.
                let sample = interleaved.get(base + channel).copied().unwrap_or(0.0);
                queue.push(sample);
            }
        }
        self.drain_full_frames()
    }

    /// Encode every complete `frame_size` chunk currently buffered across all
    /// channels, returning the audio-tagged packets produced.
    fn drain_full_frames(&mut self) -> Result<Vec<EncodedPacket>> {
        let mut out = Vec::new();
        while self.fifo.iter().all(|queue| queue.len() >= self.frame_size) {
            self.encode_one_frame(self.frame_size, &mut out)?;
        }
        Ok(out)
    }

    /// Pop `count` samples (≤ a frame) off the front of each channel queue, send
    /// them as one frame stamped from the sample counter, and collect packets.
    fn encode_one_frame(&mut self, count: usize, out: &mut Vec<EncodedPacket>) -> Result<()> {
        // Pop `count` from the front of every channel into contiguous buffers.
        let mut frames: Vec<Vec<f32>> = Vec::with_capacity(self.fifo.len());
        for queue in &mut self.fifo {
            let remainder = queue.split_off(count.min(queue.len()));
            let frame = std::mem::replace(queue, remainder);
            frames.push(frame);
        }
        let slices: Vec<&[f32]> = frames.iter().map(Vec::as_slice).collect();
        self.encoder
            .send_planar_f32(&slices, count, self.audio_pts)
            .map_err(ff)?;
        self.audio_pts = self
            .audio_pts
            .saturating_add(i64::try_from(count).unwrap_or(0));
        while let Some(packet) = self.encoder.receive_packet().map_err(ff)? {
            out.push(EncodedPacket::with_kind(packet, self.kind));
        }
        Ok(())
    }

    /// Flush the audio encoder: send any partial remainder as a final frame
    /// (AAC permits a short last frame), signal EOF, and drain trailing packets.
    fn finish(&mut self) -> Result<Vec<EncodedPacket>> {
        let mut out = Vec::new();
        let remainder = self.fifo.first().map_or(0, Vec::len);
        if remainder > 0 {
            self.encode_one_frame(remainder, &mut out)?;
        }
        self.encoder.send_eof().map_err(ff)?;
        while let Some(packet) = self.encoder.receive_packet().map_err(ff)? {
            out.push(EncodedPacket::with_kind(packet, self.kind));
        }
        Ok(out)
    }
}

/// Counters describing one encode run.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EncodeStats {
    /// Total encoded packets produced.
    pub packets: u64,
    /// How many of those packets were keyframes.
    pub keyframes: u64,
}

/// A sink that encodes the canvas once and muxes every packet into a single
/// container file (`.mkv`, `.mp4`, `.ts`, …, inferred from the extension).
pub struct FileSink {
    config: EncodeConfig,
    path: PathBuf,
}

impl FileSink {
    /// Create a file sink that will write the encoded program to `path`.
    #[must_use]
    pub fn new(config: EncodeConfig, path: impl Into<PathBuf>) -> Self {
        Self {
            config,
            path: path.into(),
        }
    }

    /// Encode the entire `source` to the configured container file and return
    /// the encode statistics.
    ///
    /// One encoder, one muxer stream: composite/decode once, encode once, mux
    /// every packet (invariant #7).
    ///
    /// # Errors
    /// Returns [`Error::Output`] if the encoder/muxer fails or the source errors.
    pub fn run<S: VideoFrameSource>(&self, source: &mut S) -> Result<EncodeStats> {
        self.config.validate()?;
        let mut encoder = self.config.open_video_encoder()?;
        let time_base = encoder.time_base();
        let mut muxer = Muxer::create(&self.path).map_err(ff)?;
        let stream_index = muxer
            .add_stream(encoder.as_codec_context(), time_base)
            .map_err(ff)?;
        muxer.write_header().map_err(ff)?;
        // Composite/decode once, encode once, mux every packet to the single
        // container stream (invariant #7).
        let driven = drive_to_single_muxer(
            &mut encoder,
            &mut muxer,
            stream_index,
            self.config.format,
            source,
        );
        // Finalize-on-error: always write the trailer (best-effort) before
        // returning, so even a mid-run source/encoder error leaves a structurally
        // valid container (e.g. an MP4 with its moov atom) rather than a file a
        // player cannot open. `Muxer::finish` is idempotent.
        finalize_or_propagate(&mut muxer, driven)
    }

    /// The path this sink writes to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Finalize a single-stream muxer after its drive loop, preserving error
/// priority. On success the trailer is written and a finish failure is surfaced;
/// on a drive failure the trailer is still written **best-effort** (so the
/// container is structurally valid) but the original drive error is the one
/// returned — a finish failure on an already-failing run is intentionally
/// dropped (the muxer is finalized as far as libav could manage).
///
/// `Muxer::finish` is idempotent, so calling it here is safe even though the
/// success path already finalized in earlier revisions.
fn finalize_or_propagate(muxer: &mut Muxer, driven: Result<EncodeStats>) -> Result<EncodeStats> {
    match driven {
        Ok(stats) => {
            muxer.finish().map_err(ff)?;
            Ok(stats)
        }
        Err(err) => {
            // Best-effort finalize; the drive error wins. Match (not `let _ =`)
            // to make the deliberate drop explicit rather than a silent discard.
            match muxer.finish() {
                Ok(()) | Err(_) => {}
            }
            Err(err)
        }
    }
}

/// Finalize a segmentation run after its drive loop, preserving error priority
/// the same way [`finalize_or_propagate`] does for a single muxer.
///
/// On a drive failure the currently-open segment is finalized **best-effort**
/// (so its container is structurally closed) and the drive error propagates — no
/// playlist is built. On success the final segment is closed and the populated,
/// finished HLS [`MediaPlaylist`] is returned alongside the segment paths and
/// stats.
///
/// Shared by the encoder-fed [`SegmentSink`] and the packet-fed
/// [`PacketMuxSink`] segment flavour so both finalize identically.
fn finish_segments(
    mut state: SegmentState<'_>,
    driven: Result<()>,
    time_base: Rational,
) -> Result<SegmentResult> {
    if let Err(err) = driven {
        state.finalize_open_segment_best_effort();
        // Land the last committed manifest(s) before the publishers are dropped
        // (#253): a failed run keeps its previous playlist valid on disk.
        state.flush_published_best_effort(std::time::Duration::from_secs(2));
        return Err(err);
    }
    let frame_ns = rescale(1, time_base, Rational::new(1, NANOS_PER_SEC));
    let mut playlist = MediaPlaylist::new(SegmentType::MpegTs);
    if let Err(err) = state.finish(&mut playlist, frame_ns) {
        // A failed finalize (e.g. the main playlist's final publish) must not
        // drop the renditions' pending manifests either (#253).
        state.flush_published_best_effort(std::time::Duration::from_secs(2));
        return Err(err);
    }
    // TARGETDURATION must be >= the longest EXTINF, as an integer.
    playlist.recompute_target_duration();
    playlist.set_finished(true);
    let run_stats = state.stats;
    Ok(SegmentResult {
        segments: state.into_segment_paths(),
        playlist,
        stats: run_stats,
    })
}

/// Drive the shared encode-once-mux-many loop: pull frames from `source`,
/// convert NV12 -> the encoder format, re-stamp each PTS from the tick counter
/// (invariants #1/#3), encode once, and write every packet to `muxer` on the
/// single registered `stream_index`. Flushes the encoder at end-of-source. The
/// caller writes the muxer header before and the trailer after.
///
/// Both [`FileSink`] and [`PushSink`] share this so a container file and a
/// live push are the *same* one-encode stream fanned to different muxers
/// (invariant #7) — never a per-output re-encode.
fn drive_to_single_muxer<S: VideoFrameSource>(
    encoder: &mut VideoEncoder,
    muxer: &mut Muxer,
    stream_index: usize,
    format: Pixel,
    source: &mut S,
) -> Result<EncodeStats> {
    let mut tick: i64 = 0;
    let mut stats = EncodeStats::default();
    let mut converter = FrameConverter::new(format);
    while let Some(frame) = source.next_frame()? {
        // Convert NV12 -> encoder format and re-stamp PTS from the tick
        // counter: out_pts = f(tick) (inv #1/#3).
        let prepared = converter.prepare(frame.frame, tick)?;
        encoder.send_frame(&prepared).map_err(ff)?;
        // `packet`'s type is inferred — this crate never names a libav type.
        while let Some(packet) = encoder.receive_packet().map_err(ff)? {
            record(&mut stats, packet.is_key());
            muxer.write_packet(stream_index, packet).map_err(ff)?;
        }
        tick = tick.saturating_add(1);
    }
    encoder.send_eof().map_err(ff)?;
    while let Some(packet) = encoder.receive_packet().map_err(ff)? {
        record(&mut stats, packet.is_key());
        muxer.write_packet(stream_index, packet).map_err(ff)?;
    }
    Ok(stats)
}

/// Drive a single-container mux from a [`PacketSource`]: pull each pre-encoded
/// packet, route it to the matching stream by its [`StreamKind`] (video packets
/// to `video_index`, audio packets to `audio_index`), and write its own owned
/// copy to `muxer` (no encode — the canvas was encoded once upstream,
/// invariant #7). The caller registers both streams + writes the header before,
/// and the trailer after. `audio_index` is `None` for a video-only sink.
///
/// The packet-fed twin of [`drive_to_single_muxer`], shared by the
/// [`PacketMuxSink`] file and push flavours.
/// The stream routing of one single-container muxer.
struct SingleRoutes<'r> {
    video: usize,
    audio: Option<usize>,
    /// The sink deselected the program bus: program-audio packets are dropped
    /// for it (not an error).
    drop_program_audio: bool,
    tracks: &'r TrackIndices,
}

/// The muxer stream `packet` writes to under `routes`, or `None` when this sink
/// does not carry it (an unselected discrete track / deselected program audio).
fn route_packet(routes: &SingleRoutes<'_>, packet: &EncodedPacket) -> Result<Option<usize>> {
    match packet.kind() {
        StreamKind::AudioTrack(track) => Ok(track_stream_index(routes.tracks, track)),
        StreamKind::Audio if routes.drop_program_audio => Ok(None),
        kind => stream_index_for(kind, routes.video, routes.audio).map(Some),
    }
}

fn drive_packets_to_single_muxer<P: PacketSource>(
    muxer: &mut Muxer,
    routes: &SingleRoutes<'_>,
    source: &mut P,
) -> Result<EncodeStats> {
    let mut stats = EncodeStats::default();
    while let Some(packet) = source.next_packet()? {
        let Some(index) = route_packet(routes, &packet)? else {
            continue;
        };
        // Keyframe stats track the VIDEO GOP only; AAC packets are all flagged
        // key, which would otherwise inflate the count.
        let is_key = matches!(packet.kind(), StreamKind::Video) && packet.is_keyframe();
        record(&mut stats, is_key);
        // Each muxer gets its OWN owned packet, so write_packet's in-place
        // set_stream + rescale_ts is sound even when the same packet fans to
        // many muxers (invariant #7).
        muxer
            .write_packet(index, packet.into_owned_packet())
            .map_err(ff)?;
    }
    Ok(stats)
}

/// Result of a [`SegmentSink`] run: the segments written and the playlist that
/// references them.
#[derive(Debug)]
pub struct SegmentResult {
    /// Absolute paths of the segment files written, in order.
    pub segments: Vec<PathBuf>,
    /// The media playlist referencing those segments (already populated).
    pub playlist: MediaPlaylist,
    /// Encode statistics for the run.
    pub stats: EncodeStats,
}

/// A sink that encodes the canvas once and splits the *same* packet stream into
/// GOP-aligned MPEG-TS segments, building the HLS media playlist that
/// references them (invariant #7: one encode, many segments).
///
/// Each segment is a self-contained MPEG-TS file that begins on a keyframe, so a
/// player can decode any segment independently. Segments are written into
/// `dir`; the populated playlist is returned for the caller to write alongside
/// them.
pub struct SegmentSink {
    config: EncodeConfig,
    dir: PathBuf,
    prefix: String,
}

impl SegmentSink {
    /// Create a segment sink writing `prefix{n}.ts` segments into `dir`.
    #[must_use]
    pub fn new(config: EncodeConfig, dir: impl Into<PathBuf>, prefix: impl Into<String>) -> Self {
        Self {
            config,
            dir: dir.into(),
            prefix: prefix.into(),
        }
    }

    /// Encode `source` once, segmenting the packet stream at keyframe
    /// boundaries, writing each segment as an MPEG-TS file and recording it in
    /// the returned [`MediaPlaylist`].
    ///
    /// A new segment begins whenever a keyframe packet is produced (the encoder
    /// is configured with a fixed GOP, so this is deterministic and
    /// GOP-aligned). The first packet is always a keyframe.
    ///
    /// # Errors
    /// Returns [`Error::Output`] if the encoder/muxer fails, the source errors,
    /// or the encoder produces a non-keyframe before any keyframe (a degenerate
    /// configuration).
    pub fn run<S: VideoFrameSource>(&self, source: &mut S) -> Result<SegmentResult> {
        self.config.validate()?;
        // The encode-once-mux-many seed (invariant #7): a single opened encoder
        // whose codec parameters every segment muxer's stream is copied from. It
        // is separate from the `drive` encoder below only because the drive
        // encoder is borrowed mutably to produce packets while the seed is
        // borrowed immutably into the segmentation state; both are the SAME fixed
        // codec, built once for the whole run (never per segment).
        let seed = self.config.open_video_encoder()?;
        note_seed_encoder_built();
        let mut encoder = self.config.open_video_encoder()?;
        let time_base = encoder.time_base();

        let mut converter = FrameConverter::new(self.config.format);
        let mut state = SegmentState::new(
            StreamSeed::Encoder(&seed),
            time_base,
            // The encoder-fed SegmentSink is video-only; the audio path runs
            // through the packet-fed PacketMuxSink (AUD-4).
            None,
            &self.dir,
            &self.prefix,
        );

        // Drive the encode/segment loop, capturing any mid-run failure so the
        // currently-open segment can be finalized best-effort before the error
        // propagates (finalize-on-error).
        let driven = state.drive(&mut encoder, &mut converter, source);
        finish_segments(state, driven, time_base)
    }

    /// The directory segments are written into.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

/// A live push transport: the container/muxer libav uses to stream the encoded
/// program to a remote peer over the matching protocol.
///
/// The protocol fixes the on-the-wire container the same way a file extension
/// fixes a file's: RTMP carries FLV, MPEG-TS-over-{SRT,UDP,RTP} carries an
/// MPEG-TS, and RTSP its own framing. The selected libav muxer name is an
/// implementation detail surfaced by [`PushProtocol::muxer_name`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PushProtocol {
    /// RTMP push (`rtmp://…`) — FLV-framed, the common ingest protocol.
    Rtmp,
    /// SRT push (`srt://…`) — an MPEG-TS payload over the SRT transport.
    Srt,
    /// RIST push (`rist://…`) — an MPEG-TS payload over the RIST transport
    /// (VSF TR-06; the open-standard sibling of SRT, ADR-0095). Fanned the
    /// **same** encoded packets as every other push sink (invariant #7).
    Rist,
    /// RTSP announce/record (`rtsp://…`).
    Rtsp,
    /// Raw MPEG-TS over UDP (`udp://…`).
    UdpTs,
}

impl PushProtocol {
    /// The libav output-muxer short-name this protocol streams through.
    #[must_use]
    pub const fn muxer_name(self) -> &'static str {
        match self {
            Self::Rtmp => "flv",
            // SRT, RIST, and plain UDP all carry an MPEG-TS payload; the URL
            // scheme selects the transport, the muxer is the container.
            Self::Srt | Self::Rist | Self::UdpTs => "mpegts",
            Self::Rtsp => "rtsp",
        }
    }
}

/// A sink that encodes the canvas once and pushes the *same* packet stream to a
/// remote peer over a live transport (RTMP / SRT / RTSP / MPEG-TS-over-UDP).
///
/// This is the egress twin of [`FileSink`]: identical encode-once-mux-many drive
/// loop (invariant #7), but the muxer targets a network URL instead of a file.
/// Opening the muxer **connects** to the peer, so [`PushSink::run`] only succeeds
/// when a peer is listening; with no peer it surfaces the libav connect error as
/// [`Error::Output`] rather than blocking or panicking. (CI has no peer, so the
/// run path is exercised only against a local listener; construction and muxer
/// selection are always testable.)
pub struct PushSink {
    config: EncodeConfig,
    protocol: PushProtocol,
    url: String,
}

impl PushSink {
    /// Create a push sink that will stream the encoded program to `url` using
    /// `protocol`.
    #[must_use]
    pub fn new(config: EncodeConfig, protocol: PushProtocol, url: impl Into<String>) -> Self {
        Self {
            config,
            protocol,
            url: url.into(),
        }
    }

    /// The libav muxer name this sink streams through (derived from the
    /// protocol).
    #[must_use]
    pub fn muxer_name(&self) -> &'static str {
        self.protocol.muxer_name()
    }

    /// The push protocol.
    #[must_use]
    pub const fn protocol(&self) -> PushProtocol {
        self.protocol
    }

    /// The destination URL.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Encode `source` once and push every packet to the remote peer.
    ///
    /// Opens a libav muxer on the URL (which connects to the peer), forcing the
    /// protocol's container muxer, then runs the shared encode-once-mux-many
    /// loop and writes the trailer.
    ///
    /// # Errors
    /// Returns [`Error::Output`] if no peer is reachable (the connect fails), or
    /// if the encoder/muxer/source errors. A push never blocks the caller
    /// waiting for a peer beyond libav's own connect.
    pub fn run<S: VideoFrameSource>(&self, source: &mut S) -> Result<EncodeStats> {
        self.config.validate()?;
        let mut encoder = self.config.open_video_encoder()?;
        let time_base = encoder.time_base();
        // The URL is passed as a path; libav's avio resolves the scheme
        // (rtmp://, srt://, …) and the forced muxer name fixes the container.
        let mut muxer =
            Muxer::create_as(Path::new(&self.url), self.protocol.muxer_name()).map_err(ff)?;
        let stream_index = muxer
            .add_stream(encoder.as_codec_context(), time_base)
            .map_err(ff)?;
        muxer.write_header().map_err(ff)?;
        let driven = drive_to_single_muxer(
            &mut encoder,
            &mut muxer,
            stream_index,
            self.config.format,
            source,
        );
        // Finalize-on-error (see `FileSink::run`): always write the trailer
        // best-effort so a mid-run failure still leaves the transport's container
        // properly closed. `Muxer::finish` is idempotent.
        finalize_or_propagate(&mut muxer, driven)
    }
}

/// What a [`PacketMuxSink::run`] produced.
///
/// A single-container sink yields just the [`EncodeStats`] (packets muxed);
/// a segmented sink yields the full [`SegmentResult`] (segment paths, playlist,
/// and stats).
///
/// Closed (not `#[non_exhaustive]`): a packet mux is inherently either
/// single-container or segmented, so callers `match` both arms exhaustively.
#[derive(Debug)]
pub enum PacketMuxOutcome {
    /// A file/push (single-container) sink finished: the muxed-packet stats.
    Single(EncodeStats),
    /// A segmented (HLS) sink finished: segments + playlist + stats.
    ///
    /// Boxed because [`SegmentResult`] is much larger than [`EncodeStats`];
    /// keeping it behind an indirection keeps the enum small.
    Segment(Box<SegmentResult>),
}

/// Where a single-container [`PacketMuxSink`] writes its one muxer.
enum SingleTarget {
    /// A container file; the muxer is inferred from the path extension.
    File(PathBuf),
    /// A live push URL with a forced container muxer name (FLV / MPEG-TS / …).
    Push {
        url: String,
        muxer_name: &'static str,
    },
}

impl SingleTarget {
    /// Open the muxer for this target.
    fn open(&self) -> Result<Muxer> {
        match self {
            Self::File(path) => Muxer::create(path),
            Self::Push { url, muxer_name } => Muxer::create_as(Path::new(url), muxer_name),
        }
        .map_err(ff)
    }
}

/// A **mux-only** sink fed pre-encoded packets — the encode-once-mux-many egress
/// (invariant #7, ADR-0026).
///
/// Unlike [`FileSink`]/[`SegmentSink`]/[`PushSink`] (which build their own
/// encoder), a `PacketMuxSink` holds **no encoder**: the canvas is encoded once
/// upstream and the *same* coded packets are fanned, as owned copies, to N of
/// these. Each builds its muxer stream from a [`StreamCodecParameters`] snapshot
/// (so no encoder instance is needed on the sink thread), writes the header,
/// muxes every packet pulled from a [`PacketSource`], and finalizes the trailer
/// on stop **and** on error (the same finalize-on-error contract the encoder-fed
/// sinks honour). Two flavours:
///
/// * **single-container** ([`PacketMuxSink::file`] / [`PacketMuxSink::push`]):
///   one muxer, every packet to its one stream.
/// * **segmented** ([`PacketMuxSink::segment`]): rotates the MPEG-TS segment
///   muxer on a keyframe-flagged packet (the GOP boundary, decided by
///   [`EncodedPacket::is_keyframe`] — never an encoder GOP counter) and emits the
///   HLS media playlist exactly as [`SegmentSink`] does.
///
/// These sinks run off the engine hot path; a slow one paces only its own
/// consumer, never the engine (invariants #1/#10).
pub struct PacketMuxSink {
    kind: PacketMuxKind,
    /// Per-output container/stream metadata (OUTMETA, ADR-0088): the validated
    /// `(scope, key, value)` dictionary entries applied to the muxer **before**
    /// `write_header` (TS SDT `service_name`/`service_provider`, PMT `language`,
    /// container `title`/`comment`, RTMP `onMetaData` keys, …). Empty ⇒ no
    /// metadata to apply (every existing call site is unchanged). For the
    /// segmented HLS paths the same metadata is re-applied to **each** segment
    /// muxer (the codec/stream layout is fixed for the run, invariant #7).
    metadata: MuxMetadata,
    /// The tag-path display-rotation matrix (OUTMETA, ADR-0089 mechanism *a*)
    /// for the **video** stream (index 0), written into the container's `tkhd` /
    /// `DISPLAYMATRIX` side data before `write_header`. `None` ⇒ no tag (the
    /// orientation is identity, or it took the pixels path upstream in the
    /// compositor). MPEG-TS carries no rotation tag, so this is only ever set on
    /// a tag-capable container (validated at config time).
    display_matrix: Option<DisplayMatrix>,
    /// Which audio this sink carries (#151/#152). Default: program only.
    audio_selection: AudioSelection,
    /// The rendition-master writer (production: fenced atomic publish). A test
    /// seam to inject a failing master write; see `with_master_writer`.
    master_writer: Option<crate::hls::ManifestWriter>,
    /// `WebVTT` subtitle renditions (slice F #345) and the cue feed they are
    /// filled from; empty ⇒ none (the master is unchanged).
    subtitles: Vec<SubtitleRendition>,
    subtitle_feed: Option<SubtitleFeedReceiver>,
}

/// The two `PacketMuxSink` flavours (private; the public surface is the
/// [`PacketMuxSink::file`]/[`push`](PacketMuxSink::push)/[`segment`](PacketMuxSink::segment)
/// constructors + [`run`](PacketMuxSink::run)).
enum PacketMuxKind {
    /// Single-container mux (file or live push).
    Single(SingleTarget),
    /// GOP-segmented MPEG-TS mux with an HLS playlist (`dir`, segment `prefix`).
    ///
    /// **Batch** flavour: every segment is accumulated and the playlist is
    /// rendered once at finalize (the historical model). `live` is `None`.
    Segment { dir: PathBuf, prefix: String },
    /// GOP-segmented MPEG-TS mux driven as a **rolling live** playlist (HLS-0/1,
    /// ADR-0032): on each closed segment the windowed `.m3u8` is atomically
    /// re-published and the evicted `.ts` pruned, so an infinite live run writes
    /// (and bounds) the playlist + segments instead of only at a finalize that
    /// never comes.
    SegmentLive {
        dir: PathBuf,
        prefix: String,
        /// Where the rolling `.m3u8` is published on every close.
        playlist_path: PathBuf,
        /// The bounded segment window (playlist length + on-disk segment count).
        window: usize,
        /// The shared outbound presentation epoch (ADR-M010, DEV-C1): the
        /// rolling playlist stamps each closed segment's
        /// `EXT-X-PROGRAM-DATE-TIME` from `epoch.wall_at(segment first PTS)`.
        /// An empty cell (sampler not anchored yet) stamps nothing.
        epoch: SharedEpoch,
    },
}

impl PacketMuxSink {
    /// A single-container sink that muxes every packet into the container file
    /// at `path` (`.mkv`, `.mp4`, `.ts`, … inferred from the extension).
    #[must_use]
    pub fn file(path: impl Into<PathBuf>) -> Self {
        Self {
            kind: PacketMuxKind::Single(SingleTarget::File(path.into())),
            metadata: MuxMetadata::new(),
            display_matrix: None,
            audio_selection: AudioSelection::default(),
            master_writer: None,
            subtitles: Vec::new(),
            subtitle_feed: None,
        }
    }

    /// A single-container sink that pushes every packet to `url` over `protocol`
    /// (the protocol fixes the on-the-wire container muxer).
    #[must_use]
    pub fn push(protocol: PushProtocol, url: impl Into<String>) -> Self {
        Self {
            kind: PacketMuxKind::Single(SingleTarget::Push {
                url: url.into(),
                muxer_name: protocol.muxer_name(),
            }),
            metadata: MuxMetadata::new(),
            display_matrix: None,
            audio_selection: AudioSelection::default(),
            master_writer: None,
            subtitles: Vec::new(),
            subtitle_feed: None,
        }
    }

    /// A segmented (HLS) sink writing `prefix{n}.ts` MPEG-TS segments into `dir`
    /// and producing the referencing media playlist.
    ///
    /// **Batch** flavour: the playlist is built and returned at finalize. Suitable
    /// for finite (VOD) runs; for an infinite live run use
    /// [`segment_live`](Self::segment_live), which publishes a rolling playlist and
    /// bounds the segment window on disk.
    #[must_use]
    pub fn segment(dir: impl Into<PathBuf>, prefix: impl Into<String>) -> Self {
        Self {
            kind: PacketMuxKind::Segment {
                dir: dir.into(),
                prefix: prefix.into(),
            },
            metadata: MuxMetadata::new(),
            display_matrix: None,
            audio_selection: AudioSelection::default(),
            master_writer: None,
            subtitles: Vec::new(),
            subtitle_feed: None,
        }
    }

    /// Replace the rendition-master writer (test seam: inject a failing or slow
    /// master publish). Production uses the fenced atomic writer.
    #[doc(hidden)]
    #[must_use]
    pub fn with_master_writer(mut self, writer: crate::hls::ManifestWriter) -> Self {
        self.master_writer = Some(writer);
        self
    }

    /// A **rolling live** segmented (HLS) sink (HLS-0/1, ADR-0032): like
    /// [`segment`](Self::segment), but on **each** closed segment it atomically
    /// re-publishes a windowed `.m3u8` to `playlist_path` and prunes the segment
    /// that ages out of `window` from disk — so an infinite live run writes (and
    /// bounds) both the playlist and the segment set instead of accumulating
    /// forever and only rendering the playlist at a finalize that never arrives.
    ///
    /// The rendered manifest omits `#EXT-X-ENDLIST` while the run is live; it is
    /// added once when [`run`](Self::run)/[`run_av`](Self::run_av) finalizes on a
    /// clean stop. The segment muxing is atomic (mux to `<seg>.tmp`, rename after
    /// `Muxer::finish`).
    #[must_use]
    pub fn segment_live(
        dir: impl Into<PathBuf>,
        prefix: impl Into<String>,
        playlist_path: impl Into<PathBuf>,
        window: usize,
        epoch: SharedEpoch,
    ) -> Self {
        Self {
            kind: PacketMuxKind::SegmentLive {
                dir: dir.into(),
                prefix: prefix.into(),
                playlist_path: playlist_path.into(),
                window,
                epoch,
            },
            metadata: MuxMetadata::new(),
            display_matrix: None,
            audio_selection: AudioSelection::default(),
            master_writer: None,
            subtitles: Vec::new(),
            subtitle_feed: None,
        }
    }

    /// Attach OUTMETA per-output metadata + an optional tag-path display-rotation
    /// matrix (ADR-0088 / ADR-0089) to this sink. The metadata entries are
    /// applied to the output container/streams and the display matrix to the
    /// video stream — both **before** `write_header` — in every mux path
    /// (single, and each segment of the HLS paths). A builder so existing call
    /// sites stay unchanged.
    #[must_use]
    pub fn with_output_metadata(
        mut self,
        metadata: MuxMetadata,
        display_matrix: Option<DisplayMatrix>,
    ) -> Self {
        self.metadata = metadata;
        self.display_matrix = display_matrix;
        self
    }

    /// Attach `WebVTT` subtitle renditions (slice F #345, segment-live sink
    /// only): one `subs<i>-g<gen>.m3u8` media playlist per rendition, one
    /// `.vtt` per video segment (same index, duration and media sequence),
    /// listed in the master's `"subs"` group. Cues arrive on `feed` (bounded,
    /// drop-and-count on the engine side) and are drained at each segment
    /// close. A builder; without it the sink is unchanged.
    #[must_use]
    pub fn with_subtitles(
        mut self,
        renditions: Vec<SubtitleRendition>,
        feed: SubtitleFeedReceiver,
    ) -> Self {
        self.subtitles = renditions;
        self.subtitle_feed = Some(feed);
        self
    }

    /// The subtitle drop/write counters, when subtitle renditions are attached.
    #[must_use]
    pub fn subtitle_stats(&self) -> Option<std::sync::Arc<SubtitleStats>> {
        self.subtitle_feed.as_ref().map(SubtitleFeedReceiver::stats)
    }

    /// Select which audio this sink carries (#151/#152): the program bus and/or
    /// discrete tracks. A builder; the default is the program bus only.
    #[must_use]
    pub fn with_audio_selection(mut self, selection: AudioSelection) -> Self {
        self.audio_selection = selection;
        self
    }

    /// Replace the audio selection in place (see
    /// [`with_audio_selection`](Self::with_audio_selection)).
    pub fn set_audio_selection(&mut self, selection: AudioSelection) {
        self.audio_selection = selection;
    }

    /// The audio this sink carries.
    #[must_use]
    pub fn audio_selection(&self) -> &AudioSelection {
        &self.audio_selection
    }

    /// Apply this sink's OUTMETA metadata + display matrix to a freshly-opened
    /// `muxer` whose streams are already registered, **before** `write_header`
    /// (ADR-0088 §3 / ADR-0089 §2.3). `video_index` is the video stream the
    /// display-rotation tag targets; format-scoped entries hit the container and
    /// stream-scoped entries their stream. Shared by the single-mux and the
    /// per-segment paths so every container of a run carries the same metadata.
    fn apply_metadata(&self, muxer: &mut Muxer, video_index: usize) -> Result<()> {
        apply_mux_metadata(muxer, &self.metadata, self.display_matrix, video_index)
    }

    /// Mux the whole `source` packet stream, seeding every muxer stream from
    /// `codec_params` and rescaling packets out of `time_base`.
    ///
    /// Pulls packets until [`PacketSource::next_packet`] yields `Ok(None)` (end
    /// of program → finalize), writing each packet's own owned copy to the
    /// muxer. The trailer (and, for HLS, the playlist) is finalized on a clean
    /// stop; on a mid-stream source error the open container is finalized
    /// best-effort before the error propagates (so e.g. an MP4 `moov` atom is
    /// still written).
    ///
    /// # Errors
    /// Returns [`Error::Output`] if the muxer cannot be opened (a push connect
    /// failure surfaces here, never a block/panic), the source errors, or a
    /// packet cannot be written.
    pub fn run<P: PacketSource>(
        &self,
        source: &mut P,
        codec_params: &StreamCodecParameters,
        time_base: Rational,
    ) -> Result<PacketMuxOutcome> {
        self.run_av(source, MuxStream::new(codec_params, time_base), None)
    }

    /// Mux a kind-tagged `source` carrying program **video and (optionally)
    /// audio** (AUD-4). Registers the `video` stream — and the `audio` stream if
    /// present — **before** writing the header (ADR-R005 §3.3 header-pinning),
    /// then routes each packet to its stream by [`StreamKind`].
    ///
    /// [`run`](Self::run) is the video-only convenience over this (audio `None`),
    /// so every existing single-stream sink/test is unchanged.
    ///
    /// # Errors
    /// As [`run`](Self::run); additionally returns [`Error::Output`] if an audio
    /// packet arrives with no audio stream registered (a fan-out wiring bug).
    pub fn run_av<P: PacketSource>(
        &self,
        source: &mut P,
        video: MuxStream<'_>,
        audio: Option<MuxStream<'_>>,
    ) -> Result<PacketMuxOutcome> {
        self.run_av_tracks(source, video, audio, &[])
    }

    /// [`run_av`](Self::run_av) plus the run's discrete audio tracks (#152):
    /// `track_streams[i]` seeds [`StreamKind::AudioTrack`]`(i)`. The sink's
    /// [`AudioSelection`] decides which are registered (each as its own audio
    /// PID, tagged with the route's language/title); packets of an unselected
    /// track — or program audio when the program is deselected — are dropped
    /// for this sink. With the default selection and no tracks this is exactly
    /// `run_av`.
    ///
    /// # Errors
    /// As [`run_av`](Self::run_av).
    pub fn run_av_tracks<P: PacketSource>(
        &self,
        source: &mut P,
        video: MuxStream<'_>,
        audio: Option<MuxStream<'_>>,
        track_streams: &[MuxStream<'_>],
    ) -> Result<PacketMuxOutcome> {
        let carry_program = self.audio_selection.program;
        let audio = if carry_program { audio } else { None };
        match &self.kind {
            PacketMuxKind::Single(target) => {
                let mut muxer = target.open()?;
                // Register video first, then audio, BEFORE the header — the
                // stream layout is immutable once written (ADR-R005 §3.3).
                let video_index = muxer
                    .add_stream_from_parameters(video.params, video.time_base)
                    .map_err(ff)?;
                let audio_index = match audio {
                    Some(a) => Some(
                        muxer
                            .add_stream_from_parameters(a.params, a.time_base)
                            .map_err(ff)?,
                    ),
                    None => None,
                };
                let tracks = add_track_streams(&mut muxer, &self.audio_selection, track_streams)?;
                // OUTMETA: tag the container/streams + the display-rotation
                // matrix before the header is written (ADR-0088/0089).
                self.apply_metadata(&mut muxer, video_index)?;
                muxer.write_header().map_err(ff)?;
                let routes = SingleRoutes {
                    video: video_index,
                    audio: audio_index,
                    drop_program_audio: !carry_program,
                    tracks: &tracks,
                };
                let driven = drive_packets_to_single_muxer(&mut muxer, &routes, source);
                // Finalize-on-error: always write the trailer best-effort so a
                // mid-stream failure still leaves a structurally valid container.
                finalize_or_propagate(&mut muxer, driven).map(PacketMuxOutcome::Single)
            }
            PacketMuxKind::Segment { dir, prefix } => {
                let mut state = SegmentState::new(
                    StreamSeed::Params(video.params),
                    video.time_base,
                    audio,
                    dir,
                    prefix,
                );
                state.set_meta(&self.metadata, self.display_matrix);
                state.set_tracks(&self.audio_selection, track_streams);
                let driven = state.drive_from_packets(source);
                finish_segments(state, driven, video.time_base)
                    .map(|result| PacketMuxOutcome::Segment(Box::new(result)))
            }
            PacketMuxKind::SegmentLive {
                dir,
                prefix,
                playlist_path,
                window,
                epoch,
            } => {
                let mut state = SegmentState::new(
                    StreamSeed::Params(video.params),
                    video.time_base,
                    audio,
                    dir,
                    prefix,
                );
                state.set_meta(&self.metadata, self.display_matrix);
                state.set_tracks(&self.audio_selection, track_streams);
                if let Some(writer) = self.master_writer.as_ref() {
                    state.master_writer = std::sync::Arc::clone(writer);
                }
                // The rolling-live driver: each closed segment is published into a
                // windowed `.m3u8` on disk (atomic) and the evicted `.ts` pruned,
                // PDT-stamped from the shared outbound epoch (ADR-M010).
                let mut live = LivePlaylist::new(playlist_path.clone(), *window);
                live.set_epoch_source(epoch.clone());
                // Keep every rendition's sequence and filename generation ahead
                // of its prior run, including a track absent from the old run.
                let next = next_segment_index(dir, prefix)?;
                live.set_media_sequence(next);
                if next != 0 {
                    live.set_initial_discontinuity();
                }
                // #220: manifest write + fsync + rename off the packet path.
                live.enable_background_publish()?;
                state.seg_index = next;
                state.live = Some(live);
                state.subtitle_feed = self.subtitle_feed.as_ref();
                state.set_audio_renditions(
                    playlist_path,
                    *window,
                    epoch.clone(),
                    video,
                    audio,
                    track_streams,
                    &self.subtitles,
                )?;
                let driven = state.drive_from_packets(source);
                finish_segments(state, driven, video.time_base)
                    .map(|result| PacketMuxOutcome::Segment(Box::new(result)))
            }
        }
    }
}

/// The single, run-wide source of the codec parameters each new segment muxer's
/// stream is seeded from (invariant #7: the codec is fixed across the encode, so
/// this is borrowed once and reused for every segment — never rebuilt).
///
/// Two forms, so the same segmentation state machine serves both egress paths:
/// the encoder-fed [`SegmentSink`] borrows its run's seed [`VideoEncoder`]; the
/// packet-fed [`PacketMuxSink`] borrows the [`StreamCodecParameters`] snapshot
/// the cli's single encoder produced (no encoder instance on this thread).
#[derive(Clone, Copy)]
enum StreamSeed<'a> {
    /// Seed a stream from a live encoder's codec context.
    Encoder(&'a VideoEncoder),
    /// Seed a stream from a thread-movable codec-parameters snapshot.
    Params(&'a StreamCodecParameters),
}

impl StreamSeed<'_> {
    /// Register a stream seeded from this codec source on `muxer`, returning the
    /// stream index.
    fn add_stream(&self, muxer: &mut Muxer, time_base: Rational) -> Result<usize> {
        match self {
            Self::Encoder(encoder) => muxer.add_stream(encoder.as_codec_context(), time_base),
            Self::Params(params) => muxer.add_stream_from_parameters(params, time_base),
        }
        .map_err(ff)
    }
}

/// Per-run segmentation state: the open segment, completed segments, and stats.
///
/// ## One long-lived muxer per run (continuity counters, root-caused)
/// [`current`](Self::current) holds exactly **one** [`Muxer`] for the entire
/// run (opened lazily by the first keyframe), not one muxer per segment. A
/// segment boundary calls [`Muxer::redirect_output`] to point the SAME
/// `AVFormatContext` at the next segment's file; it never calls
/// `Muxer::create_as` again. This is required because libav's `mpegtsenc`
/// keeps PAT/PMT/SDT/ES continuity counters (`cc`) as per-PID state on the
/// `AVFormatContext`: a fresh muxer per segment restarted every `cc` at 0,
/// which a player demuxing the HLS output as one continuous MPEG-TS reports as
/// a continuity-counter discontinuity at every segment cut (see the
/// [`Muxer::redirect_output`] doc for the libav-source cross-reference). The
/// discrete per-rendition audio muxers ([`AudioRenditionState::current`])
/// carry the identical one-muxer-per-rendition-for-the-run fix.
#[allow(clippy::struct_excessive_bools)]
// reason: each flag is an independent segmentation fact (recovering, a
// pending decoder-config change, a pending rebuild, a failed publication,
// two test-only fault injections); a state enum would conflate them.
struct SegmentState<'a> {
    dir: &'a Path,
    prefix: &'a str,
    time_base: Rational,
    /// The **single** run-wide codec-parameter seed. The one long-lived video
    /// muxer's stream is registered from this ONCE, at first open; the codec is
    /// fixed across the encode (invariant #7).
    seed: StreamSeed<'a>,
    /// The optional program-audio stream registered on the long-lived video
    /// muxer alongside the video (AUD-4), once at first open. `None` for the
    /// encoder-fed (video-only) [`SegmentSink`].
    audio: Option<MuxStream<'a>>,
    current: Option<OpenSegment>,
    /// Completed `(path, duration_seconds)` in order. For the batch flavour this
    /// is the playlist source built at finalize; for the live flavour each entry
    /// is also published into [`Self::live`] as it is closed (and the file
    /// renamed into place), so this remains the full ordered record of segments
    /// written during the run.
    done: Vec<(PathBuf, f64)>,
    /// PTS of the most recently written packet (the running segment edge).
    last_pts: MediaTime,
    /// A failed live muxer is discarded. Resume only at a video keyframe.
    recovering: bool,
    /// Inject one packet-write failure in the real segment driver (unit test only).
    #[cfg(test)]
    fail_packet_write_once: bool,
    /// Test seam: fail the next segment-muxer rebuild once (review V2).
    #[cfg(test)]
    fail_rebuild_once: bool,
    /// Test seam: the next rebuild of the master's video rendition muxer
    /// fails once (review R2-V1).
    #[cfg(test)]
    fail_rendition_rebuild_once: bool,
    /// Consecutive failed reopen attempts; terminal errors reach the supervisor.
    recovery_failures: u8,
    /// A close/playlist publication failure must reach the caller, not hide.
    publication_failed: bool,
    #[cfg(test)]
    fail_playlist_publish_once: Option<()>,
    stats: EncodeStats,
    /// The optional rolling **live** playlist driver (HLS-0/1, ADR-0032). `None`
    /// is the historical **batch** behaviour: segments mux in place and the
    /// playlist is rendered once at finalize (byte-identical to before this
    /// field existed). `Some` switches to rolling publish + bounded window +
    /// atomic segment muxing.
    live: Option<LivePlaylist>,
    /// Monotonic segment index for **live**-flavour filenames. Unlike
    /// `done.len()`, this never reuses an index even though older segments are
    /// pruned from disk — so a recycled name can never collide with a still-listed
    /// segment. Unused (and stays `0`) for the batch flavour, which keeps its
    /// historical `done.len()`-derived names.
    seg_index: u64,
    /// The MPEG-TS PAT/PMT `version_number` (5 bits) the segment muxer
    /// writes. Bumped (mod 32) when a decoder-configuration change starts a
    /// segment: that output's muxer is rebuilt with `tables_version`+1 so a
    /// receiver re-reads its tables (#342 B3). 0 = libav's default.
    tables_version: u8,
    /// The video stream parameters a rebuilt segment muxer is seeded from
    /// after a live format reset (#342 B3 review V2); `None` = the run's
    /// original [`Self::seed`].
    reseed: Option<StreamCodecParameters>,
    /// A flagged change whose segment has not started yet (a failed rebuild
    /// is retried at the next RAP by the recovery path, review V2): the
    /// next keyframe rebuilds the muxer and marks the discontinuity.
    change_pending: bool,
    /// The muxer was released for a rebuild that has not opened yet: the
    /// next build bumps `tables_version` exactly once.
    rebuild_pending: bool,
    /// OUTMETA (ADR-0088/0089): the per-output metadata + tag-path display
    /// matrix applied to the long-lived video muxer ONCE, before its first
    /// `write_header` (the codec/stream layout is fixed for the run, invariant
    /// #7). `None` (the default, and always for the encoder-fed `SegmentSink`)
    /// ⇒ no metadata to apply, byte-identical to before this field existed.
    meta: Option<(&'a MuxMetadata, Option<DisplayMatrix>)>,
    /// Audio-only rendition muxers mirror video GOP boundaries off the hot path.
    renditions: Option<AudioRenditions<'a>>,
    /// The discrete tracks (#152) registered on the long-lived video muxer
    /// ONCE, after the program audio, plus the per-track seeds. Empty ⇒
    /// unchanged layout.
    tracks: Option<(&'a AudioSelection, &'a [MuxStream<'a>])>,
    /// The rendition-master writer (production default; tests may inject one).
    master_writer: crate::hls::ManifestWriter,
    /// The subtitle cue feed (slice F #345), drained at each segment close.
    subtitle_feed: Option<&'a SubtitleFeedReceiver>,
}

/// One `WebVTT` subtitle rendition's state: its media playlist, its `.vtt`
/// file prefix and the cues held between segment closes (slice F #345).
struct SubtitleSlotState {
    live: LivePlaylist,
    prefix: String,
    held: crate::hls::subtitle::SlotCues,
    /// The rendition's shared `WebVTT` timeline origin (LOCAL zero): its
    /// first segment's start, set at the first segment close (MED-2).
    base: Option<MediaTime>,
}

/// One audio-only MPEG-TS muxer, opened ONCE at the run's first video keyframe
/// and redirected (never rebuilt) to a new file at every later GOP boundary —
/// same continuity-counter-preserving fix as [`OpenSegment`].
struct AudioSegment {
    muxer: Muxer,
    stream_index: usize,
    /// The **final** path the current segment is published under.
    path: PathBuf,
    /// The path the muxer is currently redirected to (the live flavour's
    /// same-directory `<seg>.tmp`, renamed onto `path` on close).
    write_path: PathBuf,
    packets: u64,
}

struct AudioRenditionState<'a> {
    kind: StreamKind,
    stream: MuxStream<'a>,
    live: LivePlaylist,
    prefix: String,
    current: Option<AudioSegment>,
}

struct AudioRenditions<'a> {
    states: Vec<AudioRenditionState<'a>>,
    video: AudioRenditionState<'a>,
    master: PathBuf,
    master_text: Option<String>,
    /// The audio entries the master lists (unchanged for the run).
    audio_entries: Vec<AudioRendition>,
    /// The video variant's media playlist URI.
    video_uri: String,
    /// After a live reset (#342 B3, review R3-1): the master re-rendered from
    /// the NEW encoder's parameters, waiting for the reset segment to be
    /// published in the media playlists before it replaces the master.
    pending_master: Option<String>,
    /// The master is static for a run: queued once (#220) to an off-packet-path
    /// publisher, never written + fsynced inline on a segment close.
    master_published: bool,
    /// The off-packet-path writer for the master (spawned on first publish).
    master_writer: Option<BackgroundFile>,
    retention_window: usize,
    /// The subtitle renditions (slice F #345): their master entries, states
    /// and the shared counters. Empty ⇒ none.
    subtitle_entries: Vec<SubtitleMedia>,
    subtitles: Vec<SubtitleSlotState>,
    subtitle_stats: std::sync::Arc<SubtitleStats>,
}

/// The one long-lived segment muxer's state: its (never rebuilt) `Muxer`, its
/// registered stream indices, the current segment's file path, and the PTS of
/// its first frame (for duration computation). A segment boundary redirects
/// [`Self::muxer`] to a new file in place — it never replaces this struct's
/// `muxer` with a fresh one.
struct OpenSegment {
    muxer: Muxer,
    stream_index: usize,
    /// The audio stream index registered on the muxer, if the run carries
    /// program audio (AUD-4); `None` for a video-only run.
    audio_index: Option<usize>,
    /// `(track index, stream index)` of each discrete track, registered once.
    track_indices: TrackIndices,
    /// The **final** path the current segment is published under (referenced
    /// by the playlist).
    path: PathBuf,
    /// The path the muxer is currently redirected to. For the **live** flavour
    /// this is a same-directory `<path>.tmp` that is renamed onto `path` only
    /// after the segment closes (HLS-1 atomic publish); for the batch flavour
    /// it equals `path` (mux in place, byte-identical to before).
    write_path: PathBuf,
    start_pts: MediaTime,
    /// The segment's filename index (the subtitle `.vtt` reuses it).
    index: u64,
}

impl<'a> SegmentState<'a> {
    /// Build the per-run segmentation state around the single run-wide codec
    /// seed (invariant #7: one fixed codec for the whole run). The caller owns
    /// the seed (a [`VideoEncoder`] or a [`StreamCodecParameters`]) and borrows
    /// it in for the duration of the run.
    fn new(
        seed: StreamSeed<'a>,
        time_base: Rational,
        audio: Option<MuxStream<'a>>,
        dir: &'a Path,
        prefix: &'a str,
    ) -> Self {
        Self {
            dir,
            prefix,
            time_base,
            seed,
            audio,
            current: None,
            done: Vec::new(),
            last_pts: MediaTime::ZERO,
            recovering: false,
            #[cfg(test)]
            fail_packet_write_once: false,
            #[cfg(test)]
            fail_rebuild_once: false,
            #[cfg(test)]
            fail_rendition_rebuild_once: false,
            recovery_failures: 0,
            publication_failed: false,
            #[cfg(test)]
            fail_playlist_publish_once: None,
            stats: EncodeStats::default(),
            live: None,
            seg_index: 0,
            tables_version: 0,
            reseed: None,
            change_pending: false,
            rebuild_pending: false,
            meta: None,
            tracks: None,
            renditions: None,
            master_writer: crate::hls::default_writer(),
            subtitle_feed: None,
        }
    }

    /// Build independent audio-only renditions from the same encoded packets.
    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    // reason: initialization joins rendition selection, video metadata, and the
    // master; keep these states together so publication order stays coherent.
    fn set_audio_renditions(
        &mut self,
        playlist_path: &Path,
        window: usize,
        epoch: SharedEpoch,
        video: MuxStream<'a>,
        audio: Option<MuxStream<'a>>,
        tracks: &'a [MuxStream<'a>],
        subtitles: &[SubtitleRendition],
    ) -> Result<()> {
        let default_selection = AudioSelection::default();
        let selection = self
            .tracks
            .map_or(&default_selection, |(selection, _)| selection);
        let mut master = MasterPlaylist::new();
        let mut states: Vec<AudioRenditionState<'a>> = Vec::new();
        let parent = playlist_path.parent().unwrap_or_else(|| Path::new("."));
        let generation = self.seg_index;
        if selection.program {
            if let Some(stream) = audio {
                let uri = format!("audio-program-g{generation}.m3u8");
                let mut live = LivePlaylist::new(parent.join(&uri), window);
                live.set_epoch_source(epoch.clone());
                live.enable_background_publish()?;
                live.set_media_sequence(self.seg_index);
                if self.seg_index != 0 {
                    live.set_initial_discontinuity();
                }
                master.push_audio(AudioRendition {
                    name: "Program".to_owned(),
                    uri,
                    language: None,
                    default: true,
                });
                states.push(AudioRenditionState {
                    kind: StreamKind::Audio,
                    stream,
                    live,
                    prefix: "audio-program-".to_owned(),
                    current: None,
                });
            }
        }
        for track in &selection.tracks {
            let Some(stream) = tracks.get(usize::from(track.index)).copied() else {
                continue;
            };
            let prefix = format!("audio-track{}-", track.index);
            let uri = format!("audio-track{}-g{generation}.m3u8", track.index);
            let mut live = LivePlaylist::new(parent.join(&uri), window);
            live.set_epoch_source(epoch.clone());
            live.enable_background_publish()?;
            live.set_media_sequence(self.seg_index);
            if self.seg_index != 0 {
                live.set_initial_discontinuity();
            }
            master.push_audio(AudioRendition {
                name: track
                    .title
                    .clone()
                    .unwrap_or_else(|| format!("Track {}", track.index)),
                uri,
                language: track.language.clone(),
                default: states.is_empty(),
            });
            states.push(AudioRenditionState {
                kind: StreamKind::AudioTrack(track.index),
                stream,
                live,
                prefix,
                current: None,
            });
        }
        // Slice F (#345): one WebVTT media playlist per subtitle slot, kept
        // aligned with the video playlist (same sequence, same segments).
        let mut subtitle_entries = Vec::with_capacity(subtitles.len());
        let mut sub_slots = Vec::with_capacity(subtitles.len());
        // RFC 8216 §4.3.4.1.1: distinct NAMEs in the group (config validates
        // this; the sink disambiguates anything unvalidated).
        let names = crate::hls::subtitle::unique_rendition_names(subtitles);
        for ((i, rendition), name) in subtitles.iter().enumerate().zip(names) {
            let uri = format!("subs{i}-g{generation}.m3u8");
            let mut subs_live = LivePlaylist::new(parent.join(&uri), window);
            subs_live.set_epoch_source(epoch.clone());
            subs_live.enable_background_publish()?;
            subs_live.set_media_sequence(self.seg_index);
            if self.seg_index != 0 {
                subs_live.set_initial_discontinuity();
            }
            subtitle_entries.push(SubtitleMedia {
                name,
                uri,
                language: rendition.language.clone(),
            });
            sub_slots.push(SubtitleSlotState {
                live: subs_live,
                prefix: format!("subs{i}-"),
                held: crate::hls::subtitle::SlotCues::default(),
                base: None,
            });
        }
        let sub_counters = self
            .subtitle_feed
            .map_or_else(std::sync::Arc::default, SubtitleFeedReceiver::stats);
        let video_uri = format!("video-g{generation}.m3u8");
        let mut video_live = LivePlaylist::new(parent.join(&video_uri), window);
        video_live.set_epoch_source(epoch);
        video_live.enable_background_publish()?;
        video_live.set_media_sequence(self.seg_index);
        if self.seg_index != 0 {
            video_live.set_initial_discontinuity();
        }
        let audio_entries = master.audio_renditions().to_vec();
        let master_text = render_master(
            &audio_entries,
            &subtitle_entries,
            &video_uri,
            video.params,
            video.time_base,
            &states,
        );
        // Publish only after the first complete media segment exists.
        self.renditions = Some(AudioRenditions {
            states,
            video: AudioRenditionState {
                kind: StreamKind::Video,
                stream: video,
                live: video_live,
                prefix: "video-".to_owned(),
                current: None,
            },
            master: parent.join("master.m3u8"),
            master_text,
            master_published: false,
            audio_entries,
            video_uri,
            pending_master: None,
            master_writer: None,
            retention_window: window,
            subtitle_entries,
            subtitles: sub_slots,
            subtitle_stats: sub_counters,
        });
        Ok(())
    }

    /// Open or redirect every audio-only rendition muxer to segment `index`'s
    /// file. The FIRST call per rendition builds its one long-lived `Muxer`
    /// (registers its stream, writes the header); every later call
    /// [`Muxer::redirect_output`]s the SAME muxer to the new file instead of
    /// rebuilding it — preserving that rendition's own PAT/PMT/SDT/ES
    /// continuity counters across segment boundaries (identical fix to the
    /// video muxer; see [`SegmentState`]'s doc).
    fn open_audio_segments(&mut self, index: u64) -> Result<()> {
        let Some(renditions) = self.renditions.as_mut() else {
            return Ok(());
        };
        let tables_version = self.tables_version;
        let reseed = self.reseed.as_ref();
        let mut rebuilt_video = false;
        #[cfg(test)]
        let fail_video_rebuild = &mut self.fail_rendition_rebuild_once;
        for state in renditions
            .states
            .iter_mut()
            .chain(std::iter::once(&mut renditions.video))
        {
            let path = self.dir.join(format!("{}{index}.ts", state.prefix));
            let write_path = segment_temp_path(&path);
            if let Some(segment) = state.current.as_mut() {
                segment.muxer.redirect_output(&write_path).map_err(ff)?;
                segment.path = path;
                segment.write_path = write_path;
                segment.packets = 0;
            } else {
                // The master's video-only rendition carries the video stream:
                // after a live reset it is (re)built like the main muxer, from
                // the NEW encoder's parameters at the run's current
                // `tables_version` (review R2-V1). Audio renditions keep their
                // own (unchanged) stream at version 0.
                let video = state.kind == StreamKind::Video;
                let version = if video { tables_version } else { 0 };
                #[cfg(test)]
                if video && version > 0 && std::mem::take(fail_video_rebuild) {
                    return Err(Error::Output(
                        "injected video rendition rebuild failure".to_owned(),
                    ));
                }
                let mut muxer = if version == 0 {
                    Muxer::create_as(&write_path, "mpegts").map_err(ff)?
                } else {
                    let version = version.to_string();
                    Muxer::create_with_options(
                        &write_path,
                        "mpegts",
                        &[("tables_version", version.as_str())],
                    )
                    .map_err(ff)?
                };
                let params = match reseed {
                    Some(params) if video => params,
                    _ => state.stream.params,
                };
                let stream_index = muxer
                    .add_stream_from_parameters(params, state.stream.time_base)
                    .map_err(ff)?;
                muxer.write_header().map_err(ff)?;
                if video && version > 0 {
                    rebuilt_video = true;
                }
                state.current = Some(AudioSegment {
                    muxer,
                    stream_index,
                    path,
                    write_path,
                    packets: 0,
                });
            }
        }
        // A live reset's rendition is rebuilt (review R3-1): the master is
        // re-rendered from the NEW encoder's parameters and replaces the
        // advertised one once this segment is in the media playlists.
        if rebuilt_video {
            if let Some(params) = reseed {
                renditions.pending_master = render_master(
                    &renditions.audio_entries,
                    &renditions.subtitle_entries,
                    &renditions.video_uri,
                    params,
                    renditions.video.stream.time_base,
                    &renditions.states,
                );
            }
        }
        Ok(())
    }

    fn write_audio_rendition(&mut self, packet: &EncodedPacket) -> Result<()> {
        let Some(renditions) = self.renditions.as_mut() else {
            return Ok(());
        };
        let Some(state) = renditions
            .states
            .iter_mut()
            .chain(std::iter::once(&mut renditions.video))
            .find(|state| state.kind == packet.kind())
        else {
            return Ok(());
        };
        if let Some(segment) = state.current.as_mut() {
            // Shared-ref header copy: the HLS segmenter never mutates coded
            // payload bytes, so the mux gets independent AVPacket metadata over
            // the SAME immutable payload the other bindings are muxing. The old
            // `to_owned_packet()` did `av_packet_ref` + `av_packet_make_writable`,
            // i.e. one full coded-payload copy per HLS write.
            segment
                .muxer
                .write_packet(
                    segment.stream_index,
                    packet.to_owned_packet_shared_ref().map_err(ff)?,
                )
                .map_err(ff)?;
            segment.packets = segment.packets.saturating_add(1);
        }
        Ok(())
    }

    /// Publish each rendition's just-finished segment (rename `<seg>.tmp` onto
    /// its final name, push it into the rendition's rolling playlist) WITHOUT
    /// finishing (trailer-writing) the muxer — it stays open and gets
    /// [`Muxer::redirect_output`]ed to the next file by the following
    /// [`SegmentState::open_audio_segments`] call. [`Self::finish_audio_segments`]
    /// is the true end-of-run (or error-path) trailer write.
    fn close_audio_segments(&mut self, duration: f64, start_pts_ns: i64) -> Result<()> {
        let Some(renditions) = self.renditions.as_mut() else {
            return Ok(());
        };
        for state in renditions
            .states
            .iter_mut()
            .chain(std::iter::once(&mut renditions.video))
        {
            let Some(segment) = state.current.as_mut() else {
                continue;
            };
            if segment.packets == 0 {
                // Windows cannot delete an open file: close it first.
                #[cfg(windows)]
                segment.muxer.release_output().map_err(ff)?;
                match std::fs::remove_file(&segment.write_path) {
                    Ok(()) | Err(_) => {}
                }
                // Never list an empty audio segment as a playable rendition.
                continue;
            }
            // Flush every buffered byte of THIS segment onto `write_path`
            // before renaming it into place — `redirect_output` (called next,
            // from `SegmentState::open_audio_segments`) also flushes, but that
            // is too late for an atomic rename of the file we are about to
            // retire.
            segment.muxer.flush().map_err(ff)?;
            // Windows cannot rename an open file: close it first (the muxer
            // stays alive and is redirected to the next segment).
            #[cfg(windows)]
            segment.muxer.release_output().map_err(ff)?;
            std::fs::rename(&segment.write_path, &segment.path)
                .map_err(|e| Error::Output(format!("publishing audio segment: {e}")))?;
            let uri = segment
                .path
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .ok_or_else(|| Error::Output("audio segment lacks filename".to_owned()))?;
            state
                .live
                .push_closed_segment(uri, segment.path.clone(), duration, start_pts_ns)?;
        }
        Ok(())
    }

    /// True end-of-run (or best-effort error-path) finalize: write the trailer
    /// on every rendition's long-lived muxer and drop it. Must run AFTER the
    /// final [`Self::close_audio_segments`] (the publish/rename for the last
    /// segment) — mirrors the video muxer's split between
    /// [`SegmentState::publish_current_segment`] (bookkeeping, muxer stays
    /// open) and the real `finish()` here.
    fn finish_audio_segments(&mut self) -> Result<()> {
        let Some(renditions) = self.renditions.as_mut() else {
            return Ok(());
        };
        for state in renditions
            .states
            .iter_mut()
            .chain(std::iter::once(&mut renditions.video))
        {
            if let Some(mut segment) = state.current.take() {
                segment.muxer.finish().map_err(ff)?;
            }
        }
        Ok(())
    }

    /// Best-effort finalize of every rendition's long-lived muxer on the
    /// mid-run error path: writes the trailer (closing the container
    /// structurally) and drops it, WITHOUT publishing/renaming the in-flight
    /// segment (it is left at its `.tmp`/direct write path, matching the
    /// video muxer's [`SegmentState::finalize_open_segment_best_effort`]).
    /// Any finish failure is swallowed — the original drive error wins.
    fn finalize_audio_segments_best_effort(&mut self) {
        let Some(renditions) = self.renditions.as_mut() else {
            return;
        };
        for state in renditions
            .states
            .iter_mut()
            .chain(std::iter::once(&mut renditions.video))
        {
            if let Some(mut segment) = state.current.take() {
                match segment.muxer.finish() {
                    Ok(()) | Err(_) => {}
                }
            }
        }
    }

    /// Attach the discrete-track selection + seeds (#152) every segment muxer
    /// registers after the program audio.
    fn set_tracks(&mut self, selection: &'a AudioSelection, streams: &'a [MuxStream<'a>]) {
        if !selection.tracks.is_empty() {
            self.tracks = Some((selection, streams));
        }
    }

    /// Attach OUTMETA per-output metadata + an optional tag-path display matrix
    /// to apply to every segment muxer (ADR-0088/0089). A no-op when the borrow
    /// carries an empty `MuxMetadata` and a `None` matrix.
    fn set_meta(&mut self, metadata: &'a MuxMetadata, display_matrix: Option<DisplayMatrix>) {
        self.meta = Some((metadata, display_matrix));
    }

    /// Apply the attached OUTMETA metadata + display matrix to a freshly-opened
    /// segment `muxer` (streams already registered), before `write_header`.
    fn apply_segment_meta(&self, muxer: &mut Muxer, video_index: usize) -> Result<()> {
        let Some((metadata, display_matrix)) = self.meta else {
            return Ok(());
        };
        for entry in metadata.entries() {
            match entry.scope {
                MetadataScope::Format => muxer
                    .set_format_metadata(&entry.key, &entry.value)
                    .map_err(ff)?,
                MetadataScope::Stream { index } => muxer
                    .set_stream_metadata(index, &entry.key, &entry.value)
                    .map_err(ff)?,
            }
        }
        if let Some(matrix) = display_matrix {
            muxer
                .set_stream_display_matrix(video_index, matrix)
                .map_err(ff)?;
        }
        Ok(())
    }

    /// Run the encode/segment drive loop: pull frames from `source`, convert and
    /// re-stamp each (invariants #1/#3), encode once, and split the packet stream
    /// into GOP-aligned segments. On a mid-run error the caller finalizes the
    /// open segment best-effort before propagating.
    fn drive<S: VideoFrameSource>(
        &mut self,
        encoder: &mut VideoEncoder,
        converter: &mut FrameConverter,
        source: &mut S,
    ) -> Result<()> {
        let mut tick: i64 = 0;
        while let Some(frame) = source.next_frame()? {
            let prepared = converter.prepare(frame.frame, tick)?;
            encoder.send_frame(&prepared).map_err(ff)?;
            self.drain_packets(encoder)?;
            tick = tick.saturating_add(1);
        }
        encoder.send_eof().map_err(ff)?;
        self.drain_packets(encoder)
    }

    /// Drain all currently-available encoded packets from `encoder`, starting a
    /// fresh segment on each keyframe and writing each packet into the current
    /// segment. The packet type stays inferred — `receive_packet`'s output flows
    /// straight into `write_packet`.
    fn drain_packets(&mut self, encoder: &mut VideoEncoder) -> Result<()> {
        while let Some(packet) = encoder.receive_packet().map_err(ff)? {
            let is_key = packet.is_key();
            let pts = pts_from_packet(packet.pts(), self.stats.packets, self.time_base);
            if is_key {
                self.start_segment(pts)?;
            }
            let (muxer, index) = self.current_muxer()?;
            muxer.write_packet(index, packet).map_err(ff)?;
            self.record(is_key, pts);
        }
        Ok(())
    }

    /// Drive the segment loop from a [`PacketSource`] (the encode-once-mux-many
    /// packet-fed path): pull each pre-encoded packet, split on its keyframe flag
    /// (the GOP boundary — invariant #7), and write its own owned copy into the
    /// current segment muxer. Mirrors [`Self::drain_packets`] exactly, but the
    /// packets arrive already encoded rather than from a local encoder, so a
    /// File and a Segment sink fed the SAME packets produce the SAME media.
    fn drive_from_packets<P: PacketSource>(&mut self, source: &mut P) -> Result<()> {
        while let Some(packet) = source.next_packet()? {
            // A damaged muxer cannot be reused. Drain the source until a fresh
            // decoder-ready video access point; never publish its partial TS.
            if self.recovering
                && !(packet.kind() == StreamKind::Video
                    && packet.is_keyframe()
                    && match (self.reseed.as_ref(), self.seed) {
                        (Some(params), _) | (None, StreamSeed::Params(params)) => {
                            params.is_decoder_ready_rap(&packet)
                        }
                        (None, StreamSeed::Encoder(_)) => false,
                    })
            {
                continue;
            }
            let pts = pts_from_packet(packet.pts(), self.stats.packets, self.time_base);
            let boundary = packet.kind() == StreamKind::Video && packet.is_keyframe();
            let config_changed = boundary && source.take_decoder_config_change();
            if config_changed {
                if let Some(params) = source.take_decoder_config_params() {
                    self.reseed = Some(params);
                }
            }
            if let Err(error) = self.write_packet_marking(packet, pts, config_changed) {
                // A segment-boundary failure can be a playlist publication or
                // file-system fault, not a damaged muxer. Let the supervisor
                // handle it rather than silently hide a stale live playlist.
                if self.live.is_none() || self.publication_failed {
                    self.abandon_incomplete_segments();
                    return Err(error);
                }
                self.abandon_incomplete_segments();
                // Unsupported codecs and encoder-seeded live sinks have no
                // decoder-ready RAP classifier. Fail now rather than silently
                // draining an unbounded live packet source forever.
                let supported = match (self.reseed.as_ref(), self.seed) {
                    (Some(params), _) | (None, StreamSeed::Params(params)) => {
                        params.supports_decoder_ready_rap()
                    }
                    (None, StreamSeed::Encoder(_)) => false,
                };
                if !supported {
                    return Err(Error::Output(format!(
                        "HLS muxer recovery unsupported for this video codec: {error}"
                    )));
                }
                if self.recovering && boundary {
                    self.recovery_failures = self.recovery_failures.saturating_add(1);
                    if self.recovery_failures >= 3 {
                        return Err(error);
                    }
                }
                self.recovering = true;
            } else if self.recovering {
                self.recovering = false;
                self.recovery_failures = 0;
            }
        }
        if self.recovering {
            return Err(Error::Output(
                "HLS muxer did not recover before the packet source ended".to_owned(),
            ));
        }
        Ok(())
    }

    /// Write one source packet with no decoder-config change (unit tests).
    #[cfg(test)]
    fn write_source_packet(&mut self, packet: EncodedPacket, pts: MediaTime) -> Result<()> {
        self.write_packet_marking(packet, pts, false)
    }

    /// As [`Self::write_source_packet`]; `config_changed` (a video keyframe
    /// opening a new decoder configuration) marks the segment that starts at
    /// it `EXT-X-DISCONTINUITY` in every media playlist.
    fn write_packet_marking(
        &mut self,
        packet: EncodedPacket,
        pts: MediaTime,
        config_changed: bool,
    ) -> Result<()> {
        match packet.kind() {
            StreamKind::Video => {
                // Only a VIDEO keyframe rotates a segment (the GOP boundary —
                // invariant #7); the first packet is always a keyframe.
                let is_key = packet.is_keyframe();
                if is_key {
                    // Survives a failed start: the recovery path's next RAP
                    // retries the rebuild and the mark (review V2).
                    self.change_pending |= config_changed;
                    self.start_segment_with(pts, self.change_pending)?;
                    if std::mem::take(&mut self.change_pending) {
                        self.mark_started_segment_discontinuous();
                    }
                }
                self.write_audio_rendition(&packet)?;
                #[cfg(test)]
                if std::mem::take(&mut self.fail_packet_write_once) {
                    return Err(Error::Output("injected packet write failure".to_owned()));
                }
                let owned = packet.into_owned_packet();
                let (muxer, index) = self.current_muxer()?;
                muxer.write_packet(index, owned).map_err(ff)?;
                self.record(is_key, pts);
            }
            StreamKind::AudioTrack(track) => {
                self.write_audio_rendition(&packet)?;
                // A discrete track joins the current segment like program
                // audio; a track this sink does not carry is dropped.
                let index = self
                    .current
                    .as_ref()
                    .and_then(|s| track_stream_index(&s.track_indices, track));
                let Some(index) = index else {
                    return Ok(());
                };
                let owned = packet.into_owned_packet();
                if let Some(segment) = self.current.as_mut() {
                    segment.muxer.write_packet(index, owned).map_err(ff)?;
                }
                self.record(false, pts);
            }
            StreamKind::Audio => {
                self.write_audio_rendition(&packet)?;
                // Audio never rotates a segment — it joins the current one.
                // Audio that arrives before the first video keyframe (no
                // segment open yet) or for a video-only run (no audio stream
                // registered) is skipped rather than mis-routed.
                let audio_index = self.current.as_ref().and_then(|s| s.audio_index);
                let Some(audio_index) = audio_index else {
                    return Ok(());
                };
                let owned = packet.into_owned_packet();
                if let Some(segment) = self.current.as_mut() {
                    segment.muxer.write_packet(audio_index, owned).map_err(ff)?;
                }
                self.record(false, pts);
            }
            // `StreamKind` is `#[non_exhaustive]`: skip a future kind this
            // segmenter registers no stream for rather than mis-route it.
            _ => {}
        }
        Ok(())
    }

    /// The segment just started (its predecessor is already pushed) begins a
    /// new decoder configuration: every media playlist (main, video and audio
    /// renditions) lists it after `EXT-X-DISCONTINUITY`, at the same point.
    fn mark_started_segment_discontinuous(&mut self) {
        if let Some(renditions) = self.renditions.as_mut() {
            for state in renditions
                .states
                .iter_mut()
                .chain(std::iter::once(&mut renditions.video))
            {
                state.live.set_initial_discontinuity();
            }
            for sub in &mut renditions.subtitles {
                sub.live.set_initial_discontinuity();
            }
        }
        if let Some(live) = self.live.as_mut() {
            live.set_initial_discontinuity();
        }
    }

    /// Drop every damaged in-flight file and muxer before reopening at a RAP.
    /// Never put a partial segment into any media playlist.
    fn abandon_incomplete_segments(&mut self) {
        if let Some(segment) = self.current.take() {
            let is_unpublished = segment.write_path != segment.path;
            let write_path = segment.write_path.clone();
            drop(segment);
            if is_unpublished {
                let _ = std::fs::remove_file(write_path);
            }
        }
        if let Some(renditions) = self.renditions.as_mut() {
            // A failed reset keeps the advertised master until the recovery
            // rebuild succeeds (review R3-1): it re-renders it then.
            renditions.pending_master = None;
            for state in renditions
                .states
                .iter_mut()
                .chain(std::iter::once(&mut renditions.video))
            {
                if let Some(segment) = state.current.take() {
                    let is_unpublished = segment.write_path != segment.path;
                    let write_path = segment.write_path.clone();
                    drop(segment);
                    if is_unpublished {
                        let _ = std::fs::remove_file(write_path);
                    }
                }
                state.live.set_initial_discontinuity();
            }
            for sub in &mut renditions.subtitles {
                sub.live.set_initial_discontinuity();
            }
        }
        if let Some(live) = self.live.as_mut() {
            live.set_initial_discontinuity();
        }
    }

    /// Error path: give every background playlist publisher (main and
    /// renditions) up to `bound` to land the manifest it already holds.
    fn flush_published_best_effort(&mut self, bound: std::time::Duration) {
        // One shared deadline across every publisher: they write in parallel
        // (each on its own thread), so the total wait is `bound`, not
        // `bound` x renditions.
        let deadline = std::time::Instant::now() + bound;
        let left = || deadline.saturating_duration_since(std::time::Instant::now());
        if let Some(live) = self.live.as_mut() {
            live.flush_best_effort(left());
        }
        if let Some(renditions) = self.renditions.as_mut() {
            for state in renditions
                .states
                .iter_mut()
                .chain(std::iter::once(&mut renditions.video))
            {
                state.live.flush_best_effort(left());
            }
            for sub in &mut renditions.subtitles {
                sub.live.flush_best_effort(left());
            }
            // The master is queued only once every media playlist exists, so
            // landing it after them keeps the ordering; without this a failed
            // run could leave flushed renditions with no discoverable master.
            if let Some(master) = renditions.master_writer.take() {
                master.flush_best_effort(left());
            }
        }
    }

    /// Best-effort finalize of the currently-open segment on the error path: the
    /// long-lived segment muxer's trailer is written (so its container is
    /// structurally closed) and the segment dropped — WITHOUT publishing the
    /// in-flight segment (the last file it was redirected to is left exactly as
    /// written, matching [`finalize_audio_segments_best_effort`]). Any finish
    /// failure is intentionally swallowed — the original drive error is the one
    /// that propagates. Idempotent (no-ops once `current` is taken).
    fn finalize_open_segment_best_effort(&mut self) {
        let Some(mut segment) = self.current.take() else {
            return;
        };
        // Deliberate drop of the finish result: the run is already failing, so
        // the drive error wins; we only want the bytes flushed / trailer written.
        match segment.muxer.finish() {
            Ok(()) | Err(_) => {}
        }
        note_segment_finalized();
        self.finalize_audio_segments_best_effort();
    }

    /// Close the current segment (if any — published, muxer kept open and
    /// redirected rather than rebuilt) and start a new one at `start_pts`.
    /// Called on every keyframe so segments are GOP-aligned and each begins on
    /// a keyframe. The one long-lived video [`Muxer`] (and each audio
    /// rendition's own long-lived muxer) is built on the FIRST call only; every
    /// later call redirects it in place — see [`SegmentState`]'s doc for why
    /// this is the continuity-counter fix, not an optimization.
    fn start_segment(&mut self, start_pts: MediaTime) -> Result<()> {
        self.start_segment_with(start_pts, false)
    }

    /// [`Self::start_segment`]; `config_changed` (a keyframe opening a new
    /// decoder configuration, #342 B3) rebuilds the segment muxer with the
    /// next TS `tables_version` instead of redirecting the long-lived one,
    /// so the PAT/PMT `version_number` changes at the segment that starts
    /// here. Legal: that segment is `EXT-X-DISCONTINUITY` anyway, so its
    /// continuity counters may restart.
    fn start_segment_with(&mut self, start_pts: MediaTime, config_changed: bool) -> Result<()> {
        if let Err(error) = self.close_current(start_pts) {
            self.publication_failed = true;
            return Err(error);
        }
        // Filename index: the live flavour uses a monotonic counter (so a name is
        // never reused even though older segments are pruned from the window),
        // while the batch flavour keeps its historical `done.len()` (byte-identical
        // to before — `done` only grows in batch). The two cannot be conflated:
        // under live, `done.len()` would recycle an index onto a still-listed
        // segment.
        let index = if self.live.is_some() {
            let i = self.seg_index;
            self.seg_index = self.seg_index.saturating_add(1);
            i
        } else {
            // `done.len()` (a `usize`) widened to `u64` for a uniform format; the
            // batch path historically formatted the `usize` directly, which prints
            // identically.
            u64::try_from(self.done.len()).unwrap_or(u64::MAX)
        };
        let path = self.dir.join(format!("{}{index}.ts", self.prefix));
        // Atomic publish (HLS-1, live only): the muxer writes to a same-directory
        // `<seg>.tmp` and is renamed onto `path` once the segment closes. The batch
        // flavour muxes in place (`write_path == path`), unchanged.
        let write_path = if self.live.is_some() {
            segment_temp_path(&path)
        } else {
            path.clone()
        };
        // The config change is handled BEFORE any redirect (review R2-V1):
        // the main muxer and the master's video-only rendition muxer are
        // both released here and rebuilt below from the NEW encoder's
        // parameters at the next `tables_version`.
        if config_changed {
            // Bumped once per change: a retry of this change at the next
            // RAP (after a failed build, `change_pending`) reuses the bumped
            // version; it is cleared once both muxers are built below.
            if !self.rebuild_pending {
                self.rebuild_pending = true;
                self.tables_version = (self.tables_version + 1) % 32;
            }
            if let Some(mut old) = self.current.take() {
                // Flush and close the finished segment's file (already
                // published by `close_current`); the muxer is then rebuilt.
                old.muxer.release_output().map_err(ff)?;
            }
            if let Some(renditions) = self.renditions.as_mut() {
                if let Some(mut old) = renditions.video.current.take() {
                    old.muxer.release_output().map_err(ff)?;
                }
            }
        }
        self.open_audio_segments(index)?;
        if let Some(segment) = self.current.as_mut() {
            // Redirect the SAME long-lived muxer to the new file. Every
            // PID's continuity counter, PCR bookkeeping, etc. on this
            // `AVFormatContext` carries over unbroken (the fix: see
            // `Muxer::redirect_output`'s doc). `mpegts_flags=resend_headers`
            // (set inside `redirect_output`) makes the new file
            // independently decodable (fresh PAT/PMT/SDT at its start).
            segment.muxer.redirect_output(&write_path).map_err(ff)?;
            segment.path = path;
            segment.write_path = write_path;
            segment.start_pts = start_pts;
            segment.index = index;
        } else {
            // First segment of the run: build the ONE long-lived muxer +
            // register its streams. Rebuilt for a later segment only at a
            // decoder-configuration change (with the next tables_version,
            // bumped above).
            #[cfg(test)]
            if self.tables_version > 0 && std::mem::take(&mut self.fail_rebuild_once) {
                return Err(Error::Output(
                    "injected segment muxer rebuild failure".to_owned(),
                ));
            }
            let mut muxer = if self.tables_version == 0 {
                Muxer::create_as(&write_path, "mpegts").map_err(ff)?
            } else {
                let version = self.tables_version.to_string();
                Muxer::create_with_options(
                    &write_path,
                    "mpegts",
                    &[("tables_version", version.as_str())],
                )
                .map_err(ff)?
            };
            // Register video then audio BEFORE the header — every segment
            // carries the same pinned two-stream layout (ADR-R005 §3.3,
            // AUD-4).
            let stream_index = match self.reseed.as_ref() {
                Some(params) => muxer
                    .add_stream_from_parameters(params, self.time_base)
                    .map_err(ff)?,
                None => self.seed.add_stream(&mut muxer, self.time_base)?,
            };
            let audio_index = match self.audio {
                Some(a) => Some(
                    muxer
                        .add_stream_from_parameters(a.params, a.time_base)
                        .map_err(ff)?,
                ),
                None => None,
            };
            let track_indices = match self.tracks {
                Some((selection, streams)) => add_track_streams(&mut muxer, selection, streams)?,
                None => Vec::new(),
            };
            // OUTMETA: tag the container/streams (SDT/PMT, …) once before the
            // header (ADR-0088 §3); the display matrix is `None` for MPEG-TS
            // (no rotation tag — validated at config time) so it is a no-op
            // here.
            self.apply_segment_meta(&mut muxer, stream_index)?;
            muxer.write_header().map_err(ff)?;
            self.current = Some(OpenSegment {
                muxer,
                stream_index,
                audio_index,
                track_indices,
                path,
                write_path,
                start_pts,
                index,
            });
        }
        // Both muxers are built: the change (if any) is complete.
        self.rebuild_pending = false;
        Ok(())
    }

    /// Borrow the current segment's muxer + registered stream index, so the
    /// caller can write a packet of inferred type into it.
    ///
    /// # Errors
    /// [`Error::Output`] if the encoder produced a packet before any keyframe
    /// opened a segment.
    fn current_muxer(&mut self) -> Result<(&mut Muxer, usize)> {
        let segment = self.current.as_mut().ok_or_else(|| {
            Error::Output("encoder produced a non-keyframe before any keyframe".to_owned())
        })?;
        Ok((&mut segment.muxer, segment.stream_index))
    }

    /// Record one written packet (and whether it was a keyframe) and advance the
    /// running segment edge to `pts`.
    fn record(&mut self, is_key: bool, pts: MediaTime) {
        self.last_pts = pts;
        self.stats.packets = self.stats.packets.saturating_add(1);
        if is_key {
            self.stats.keyframes = self.stats.keyframes.saturating_add(1);
        }
    }

    /// Publish the current segment (if any), recording its duration as the gap
    /// from its start PTS to `end_pts`, bounded below by one frame. The
    /// long-lived video muxer (and every audio rendition's own long-lived
    /// muxer) is **kept open** — its trailer is NOT written here, so it can be
    /// [`Muxer::redirect_output`]ed to the next segment's file by the
    /// following [`Self::start_segment`] call without ever restarting a
    /// continuity counter. [`Self::finish`] is where the final open segment's
    /// muxer(s) actually get a trailer.
    ///
    /// For the **live** flavour this is also where atomic publish happens: the
    /// `<seg>.tmp` is renamed onto its final path (so a fronting server never
    /// sees a half-written segment) once every buffered byte has landed
    /// (`redirect_output`'s own flush, run by the caller before this), then the
    /// closed segment is pushed into the rolling [`LivePlaylist`] (which
    /// re-publishes the windowed `.m3u8` atomically and prunes the evicted
    /// `.ts`). The batch flavour leaves the muxed-in-place file untouched and
    /// only records it for the finalize-time playlist render — byte-identical
    /// to before.
    /// Slice F (#345): write one `.vtt` per subtitle slot for the video
    /// segment `[start, end)` just closed (same `index`, `duration`, start
    /// PTS — so the subtitle playlists stay aligned with the video ones) and
    /// push it into the slot's media playlist. The `X-TIMESTAMP-MAP` maps
    /// LOCAL zero (= `start`) to the 90 kHz PTS the segment's first video PES
    /// actually carries in the written TS (the muxer adds its own offset),
    /// read back from the file. Cues are drained from the feed first
    /// (non-blocking). A slot with no cue still gets a header-only `.vtt`.
    fn close_subtitle_segments(
        &mut self,
        start: MediaTime,
        end: MediaTime,
        duration: f64,
        index: u64,
        main_ts: &Path,
    ) -> Result<()> {
        let Some(renditions) = self.renditions.as_mut() else {
            return Ok(());
        };
        if renditions.subtitles.is_empty() {
            return Ok(());
        }
        if let Some(feed) = self.subtitle_feed {
            for cue in feed.drain() {
                if let Some(sub) = renditions.subtitles.get_mut(usize::from(cue.slot)) {
                    sub.held.push(cue, &renditions.subtitle_stats);
                }
            }
        }
        // Prefer the master variant's own (video rendition) segment: it is
        // what a player pairs the subtitles with.
        let video_ts = self
            .dir
            .join(format!("{}{index}.ts", renditions.video.prefix));
        let mpegts = read_first_video_pts(&video_ts)
            .or_else(|| read_first_video_pts(main_ts))
            .unwrap_or_else(|| crate::hls::subtitle::pts_90k_of(start));
        for sub in &mut renditions.subtitles {
            let base = *sub.base.get_or_insert(start);
            let pieces = sub.held.publish(start, end);
            let text = crate::hls::subtitle::render_vtt(
                &pieces.iter().collect::<Vec<_>>(),
                start,
                end,
                base,
                mpegts,
            );
            let path = self.dir.join(format!("{}{index}.vtt", sub.prefix));
            let tmp = segment_temp_path(&path);
            std::fs::write(&tmp, text.as_bytes())
                .and_then(|()| std::fs::rename(&tmp, &path))
                .map_err(|e| Error::Output(format!("publishing subtitle segment: {e}")))?;
            let uri = path
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .ok_or_else(|| Error::Output("subtitle segment lacks filename".to_owned()))?
                .to_owned();
            sub.live
                .push_closed_segment(uri, path, duration, start.as_nanos())?;
            sub.held.retire_before(end);
            renditions.subtitle_stats.note_segment_written();
        }
        Ok(())
    }

    /// Publish (or, after a live reset, republish) the rendition master,
    /// LAST: after video and every audio media playlist exist (review R3-1).
    ///
    /// # Errors
    /// The master publisher could not be spawned.
    fn publish_master(&mut self) -> Result<()> {
        let Some(renditions) = self.renditions.as_mut() else {
            return Ok(());
        };
        // Publish master last, after video and every audio playlist exist.
        // A run without valid video bitrate metadata still serves the legacy
        // index and media playlists, but never publishes a false BANDWIDTH.
        // A live reset's master (review R3-1): this close has just pushed
        // the reset segment (with its discontinuity) into every media
        // playlist's publisher, so the master advertising the new
        // parameters is queued now — after them, atomically, through the
        // same writer (latest wins).
        if let Some(text) = renditions.pending_master.take() {
            match renditions.master_writer.as_ref() {
                Some(writer) if renditions.master_published => {
                    writer.publish(text.clone());
                    renditions.master_text = Some(text);
                }
                // Not advertised yet: the first publication below carries
                // the new parameters.
                _ => renditions.master_text = Some(text),
            }
        }
        if let Some(master_text) = renditions.master_text.as_ref() {
            if !renditions.master_published
                && renditions
                    .states
                    .iter()
                    .all(|state| state.live.playlist_path().exists())
                && renditions
                    .subtitles
                    .iter()
                    .all(|sub| sub.live.playlist_path().exists())
                && renditions.video.live.playlist_path().exists()
            {
                // Ordering is preserved: every media playlist exists before
                // the master is queued. Pruning of the previous run's files
                // still waits until the on-disk master equals this text
                // (`current_renditions_ready`).
                let writer = BackgroundFile::spawn_with(
                    renditions.master.clone(),
                    self.master_writer.clone(),
                )?;
                writer.publish(master_text.clone());
                renditions.master_writer = Some(writer);
                renditions.master_published = true;
            }
        } else if renditions.video.live.playlist_path().exists() {
            // No trustworthy BANDWIDTH: retire the previous master only once
            // this run has playable media. Never advertise obsolete tracks.
            match std::fs::remove_file(&renditions.master) {
                Ok(()) | Err(_) => {}
            }
        }
        Ok(())
    }

    fn close_current(&mut self, end_pts: MediaTime) -> Result<()> {
        let Some(segment) = self.current.as_mut() else {
            return Ok(());
        };
        note_segment_finalized();
        let span_ns = end_pts
            .as_nanos()
            .saturating_sub(segment.start_pts.as_nanos());
        let frame_ns = rescale(1, self.time_base, Rational::new(1, NANOS_PER_SEC));
        let duration = seconds_from_ns(span_ns.max(frame_ns));
        let start_pts_ns = segment.start_pts.as_nanos();
        self.close_audio_segments(duration, start_pts_ns)?;
        let Some(segment) = self.current.as_mut() else {
            return Ok(());
        };
        // Live atomic publish: flush every buffered byte of THIS segment onto
        // `write_path` before renaming it into place — the following
        // `start_segment` call's `redirect_output` also flushes, but that is
        // too late for an atomic rename of the file we are about to retire.
        if self.live.is_some() && segment.write_path != segment.path {
            segment.muxer.flush().map_err(ff)?;
            // Windows cannot rename an open file: close it first (the muxer
            // stays alive and is redirected to the next segment).
            #[cfg(windows)]
            segment.muxer.release_output().map_err(ff)?;
        }
        let segment = &*segment;
        #[cfg(test)]
        if self.fail_playlist_publish_once.take().is_some() {
            return Err(Error::Output(
                "injected playlist publication failure".to_owned(),
            ));
        }
        if let Some(live) = self.live.as_mut() {
            if segment.write_path != segment.path {
                std::fs::rename(&segment.write_path, &segment.path).map_err(|e| {
                    Error::Output(format!(
                        "renaming segment {} -> {}: {e}",
                        segment.write_path.display(),
                        segment.path.display()
                    ))
                })?;
            }
            let uri = segment
                .path
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .ok_or_else(|| Error::Output("segment path has no file name".to_owned()))?;
            // The segment's first-sample PTS on the tick-derived internal ns
            // timeline — the exact input the ADR-M010 epoch maps to wall time.
            live.push_closed_segment(
                uri.to_owned(),
                segment.path.clone(),
                duration,
                segment.start_pts.as_nanos(),
            )?;
        }
        let (seg_start, seg_index, seg_path) =
            (segment.start_pts, segment.index, segment.path.clone());
        let seg_end = seg_start.saturating_add(MediaTime::from_nanos(span_ns.max(frame_ns)));
        self.close_subtitle_segments(seg_start, seg_end, duration, seg_index, &seg_path)?;
        self.publish_master()?;
        if let Some(renditions) = self.renditions.as_ref() {
            // Never reap a file named by the still-advertised master when its
            // replacement could not be published (including unknown bitrate).
            if current_renditions_ready(renditions) {
                prune_prior_rendition_manifests(
                    self.dir,
                    renditions,
                    self.seg_index
                        .saturating_sub(segment_grace(renditions.retention_window)),
                );
            }
        }
        if let Some(renditions) = self.renditions.as_ref() {
            // A prior master remains authoritative until the new master exists.
            // A missing rendition or a failed publication cannot retire its TS.
            if current_renditions_ready(renditions) {
                prune_prior_live_segments(
                    self.dir,
                    self.prefix,
                    self.seg_index,
                    renditions.retention_window,
                );
            }
        }
        let Some(segment) = self.current.as_ref() else {
            return Ok(());
        };
        self.done.push((segment.path.clone(), duration));
        // Live media history lives in the bounded playlist, not this batch
        // report. Keep at most one manifest window here for a finite result.
        if let Some(live) = self.live.as_ref() {
            bound_live_report(&mut self.done, live.window_size());
        }
        Ok(())
    }

    /// Finalize the final open segment and append every segment to `playlist`.
    ///
    /// Publishes the last segment ([`Self::close_current`], muxers still open),
    /// THEN writes the trailer on the long-lived video muxer and every audio
    /// rendition's long-lived muxer ([`Self::finish_audio_segments`]) — the
    /// true end-of-run close, now that nothing will redirect them again.
    ///
    /// For the **live** flavour this also finalizes the on-disk rolling playlist
    /// (`#EXT-X-ENDLIST` + a last atomic publish): the `LivePlaylist` owns the
    /// authoritative on-disk `.m3u8`, so the caller does not write the returned
    /// `playlist` to disk for a live sink (it is built only for the run report).
    fn finish(&mut self, playlist: &mut MediaPlaylist, frame_ns: i64) -> Result<()> {
        let end = self
            .last_pts
            .saturating_add(MediaTime::from_nanos(frame_ns));
        self.close_current(end)?;
        if let Some(mut segment) = self.current.take() {
            segment.muxer.finish().map_err(ff)?;
        }
        self.finish_audio_segments()?;
        for (path, duration) in &self.done {
            let uri = path
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .ok_or_else(|| Error::Output("segment path has no file name".to_owned()))?;
            playlist.push_segment(Segment::new(uri.to_owned(), *duration));
        }
        // Live: stamp the on-disk rolling playlist with EXT-X-ENDLIST and publish
        // it one last time. The batch flavour's playlist is finalized by the
        // caller (`finish_segments`).
        if let Some(live) = self.live.as_mut() {
            live.finalize()?;
        }
        if let Some(renditions) = self.renditions.as_mut() {
            for state in renditions
                .states
                .iter_mut()
                .chain(std::iter::once(&mut renditions.video))
            {
                state.live.finalize()?;
            }
            for sub in &mut renditions.subtitles {
                sub.live.finalize()?;
            }
            // Every media playlist is durable now (finalize joined each
            // publisher). Publish the master if a close has not queued it yet
            // (short runs close before the background media writes land).
            if let Some(text) = renditions.master_text.clone() {
                let writer = match renditions.master_writer.take() {
                    Some(writer) => Some(writer),
                    None if renditions
                        .states
                        .iter()
                        .all(|state| state.live.playlist_path().exists())
                        && renditions
                            .subtitles
                            .iter()
                            .all(|sub| sub.live.playlist_path().exists())
                        && renditions.video.live.playlist_path().exists() =>
                    {
                        Some(BackgroundFile::spawn_with(
                            renditions.master.clone(),
                            self.master_writer.clone(),
                        )?)
                    }
                    None => None,
                };
                if let Some(writer) = writer {
                    writer.finish(text)?;
                    renditions.master_published = true;
                }
            }
        }
        // The master is published off the packet path (#220), so a close may
        // have run before it landed. Now that it is durable, reap the previous
        // generations exactly as a later close would have.
        if let Some(renditions) = self.renditions.as_ref() {
            if current_renditions_ready(renditions) {
                prune_prior_rendition_manifests(
                    self.dir,
                    renditions,
                    self.seg_index
                        .saturating_sub(segment_grace(renditions.retention_window)),
                );
                prune_prior_live_segments(
                    self.dir,
                    self.prefix,
                    self.seg_index,
                    renditions.retention_window,
                );
            }
        }
        Ok(())
    }

    /// Consume the state, yielding the ordered list of segment paths written.
    fn into_segment_paths(self) -> Vec<PathBuf> {
        self.done.into_iter().map(|(path, _)| path).collect()
    }
}

/// Keep the finite run report bounded while the live playlist owns media history.
fn bound_live_report(done: &mut Vec<(PathBuf, f64)>, window: usize) {
    let excess = done.len().saturating_sub(window);
    done.drain(..excess);
}

/// The master playlist text for a video variant at `video` params plus the
/// audio renditions `states` (`None`: no trustworthy BANDWIDTH — no video
/// bit rate — so no master is advertised). One rendering for the run's start
/// and for a live reset's new parameters (review R3-1).
fn render_master(
    audio_entries: &[AudioRendition],
    subtitle_entries: &[SubtitleMedia],
    video_uri: &str,
    video: &StreamCodecParameters,
    time_base: Rational,
    states: &[AudioRenditionState<'_>],
) -> Option<String> {
    let max_audio_rate = states
        .iter()
        .filter_map(|state| state.stream.params.bit_rate())
        .max()
        .unwrap_or(0);
    // BANDWIDTH is mandatory for a listed variant. With no configured
    // video rate, keep the master absent rather than advertise a lie.
    let bandwidth = video
        .bit_rate()
        .filter(|rate| *rate > 0)
        .map(|rate| rate.saturating_add(max_audio_rate))?;
    let mut variant = VariantStream::new(video_uri.to_owned(), bandwidth);
    if let Some((width, height)) = video.video_dimensions() {
        variant = variant.with_resolution(width, height);
    }
    if let Some(video_codec) = video.hls_codec() {
        if states.is_empty() {
            variant = variant.with_codecs(video_codec);
        } else if let Some(audio_codec) = states
            .first()
            .and_then(|state| state.stream.params.hls_codec())
        {
            if states
                .iter()
                .all(|state| state.stream.params.hls_codec() == Some(audio_codec))
            {
                variant = variant.with_codecs(format!("{video_codec},{audio_codec}"));
            }
        }
    }
    if time_base.num > 0 && time_base.den > 0 {
        let numerator = u32::try_from(time_base.den).ok();
        let denominator = u32::try_from(time_base.num).ok();
        if let (Some(numerator), Some(denominator)) = (numerator, denominator) {
            let fps = f64::from(numerator) / f64::from(denominator);
            if (0.0..=120.0).contains(&fps) && fps > 0.0 {
                variant = variant.with_frame_rate(fps);
            }
        }
    }
    let mut master = MasterPlaylist::new();
    for audio in audio_entries {
        master.push_audio(audio.clone());
    }
    for subtitle in subtitle_entries {
        master.push_subtitle(subtitle.clone());
    }
    master.push_variant(variant);
    Some(master.render())
}

fn current_renditions_ready(renditions: &AudioRenditions<'_>) -> bool {
    match renditions.master_text.as_ref() {
        Some(text) => {
            std::fs::read_to_string(&renditions.master).is_ok_and(|published| published == *text)
        }
        None => !renditions.master.exists() && renditions.video.live.playlist_path().exists(),
    }
}

/// Retire old rendition manifests only after every segment they list has
/// passed the same bounded grace as the TS reaper. An old master cached by a
/// client can continue to load its immutable rendition URI across a restart.
fn prune_prior_rendition_manifests(dir: &Path, renditions: &AudioRenditions<'_>, oldest: u64) {
    let active: Vec<&Path> = renditions
        .states
        .iter()
        .chain(std::iter::once(&renditions.video))
        .map(|state| state.live.playlist_path())
        .chain(
            renditions
                .subtitles
                .iter()
                .map(|sub| sub.live.playlist_path()),
        )
        .collect();
    prune_prior_rendition_manifests_inactive(dir, &active, oldest);
}

fn prune_prior_rendition_manifests_inactive(dir: &Path, active: &[&Path], oldest: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !Path::new(name)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("m3u8"))
            || !(name.starts_with("video-")
                || name.starts_with("audio-program-")
                || name.starts_with("audio-track")
                || is_generated_subtitle_manifest(name)
                || matches!(name, "video.m3u8" | "audio-program.m3u8"))
            || active.contains(&entry.path().as_path())
        {
            continue;
        }
        // Retire a playlist before its first media URI crosses the TS grace
        // edge. The old master has already been atomically replaced.
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        // Parse through the same fail-closed URI policy as both TS reapers.
        // Uncertain manifests cannot authorize removal of their own references.
        let Some(listed) = media_segment_uris(&text) else {
            continue;
        };
        if listed.iter().any(|uri| {
            segment_number(uri).is_some_and(|index| index < oldest) || !dir.join(uri).exists()
        }) {
            match std::fs::remove_file(entry.path()) {
                Ok(()) | Err(_) => {}
            }
        }
    }
}

/// Whether `name` is exactly a subtitle media playlist this sink generates:
/// `subs<slot>-g<generation>.m3u8` with decimal slot and generation (ABU-2).
/// Anything else (`subtitles.m3u8`, `subscriptions.m3u8`, …) is not ours and
/// is never removed by rendition cleanup.
fn is_generated_subtitle_manifest(name: &str) -> bool {
    let decimal = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    name.strip_prefix("subs")
        .and_then(|rest| rest.strip_suffix(".m3u8"))
        .and_then(|rest| rest.split_once("-g"))
        .is_some_and(|(slot, generation)| {
            decimal(slot)
                && decimal(generation)
                && slot.parse::<u8>().is_ok()
                && generation.parse::<u64>().is_ok()
        })
}

/// The segment index of a subtitle segment this sink generates:
/// `subs<slot>-<index>.vtt` with all-ASCII-digit slot and index (ABU2-2).
/// Anything else (`subs+1-1.vtt`, `subs1-+1.vtt`, …) is not ours: `None`.
fn generated_subtitle_segment_index(name: &str) -> Option<u64> {
    let decimal = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let (slot, index) = name
        .strip_prefix("subs")?
        .strip_suffix(".vtt")?
        .split_once('-')?;
    if !decimal(slot) || !decimal(index) {
        return None;
    }
    slot.parse::<u8>().ok()?;
    index.parse().ok()
}

/// Reap prior-run TS files after the new manifests have advanced beyond them.
/// Keep three playlist windows plus three segment closes for an old client.
fn segment_grace(window: usize) -> u64 {
    u64::try_from(window)
        .unwrap_or(u64::MAX)
        .saturating_mul(3)
        .saturating_add(3)
}

fn segment_number(uri: &str) -> Option<u64> {
    uri.strip_suffix(".ts")
        .or_else(|| uri.strip_suffix(".vtt"))?
        .rsplit('-')
        .next()?
        .parse()
        .ok()
}

/// The 90 kHz PTS of the first video PES of a written TS segment (slice F
/// #345), reading at most its first MiB.
fn read_first_video_pts(path: &Path) -> Option<u64> {
    use std::io::Read as _;
    let file = std::fs::File::open(path).ok()?;
    let mut head = Vec::new();
    file.take(1 << 20).read_to_end(&mut head).ok()?;
    crate::hls::subtitle::first_video_pts_90k(&head)
}

fn prune_prior_live_segments(dir: &Path, prefix: &str, next: u64, window: usize) {
    let oldest = next.saturating_sub(segment_grace(window));
    // Snapshot after expired manifests have been retired. An unreadable or
    // malformed playlist makes deletion unsafe, so keep the files instead.
    let Some(references) = media_segment_references(dir) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        // Slice F (#345): a prior run's subtitle `.vtt` ages out with its TS.
        if name.strip_suffix(".vtt").is_some() {
            if !references.contains(std::ffi::OsStr::new(name))
                && generated_subtitle_segment_index(name).is_some_and(|index| index < oldest)
            {
                match std::fs::remove_file(entry.path()) {
                    Ok(()) | Err(_) => {}
                }
            }
            continue;
        }
        let Some(stem) = name.strip_suffix(".ts") else {
            continue;
        };
        let number = stem
            .strip_prefix(prefix)
            .or_else(|| stem.strip_prefix("video-"))
            .or_else(|| stem.strip_prefix("audio-program-"))
            .or_else(|| {
                stem.strip_prefix("audio-track").and_then(|s| {
                    s.split_once('-')
                        .and_then(|(track, index)| track.parse::<u16>().ok().map(|_| index))
                })
            });
        if !references.contains(std::ffi::OsStr::new(name))
            && number
                .and_then(|index| index.parse::<u64>().ok())
                .is_some_and(|index| index < oldest)
        {
            match std::fs::remove_file(entry.path()) {
                Ok(()) | Err(_) => {}
            }
        }
    }
}

/// Highest previous segment number plus one, shared by all audio/video tracks.
fn next_segment_index(dir: &Path, prefix: &str) -> Result<u64> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| Error::Output(format!("reading HLS segment directory: {e}")))?;
    let mut next = 0u64;
    for entry in entries {
        let entry = entry.map_err(|e| Error::Output(format!("reading HLS segment entry: {e}")))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(stem) = name.strip_suffix(".ts") else {
            continue;
        };
        let number = stem
            .strip_prefix(prefix)
            .or_else(|| stem.strip_prefix("video-"))
            .or_else(|| stem.strip_prefix("audio-program-"))
            .or_else(|| {
                stem.strip_prefix("audio-track")
                    .and_then(|s| s.split_once('-').map(|(_, number)| number))
            });
        let Some(number) = number else { continue };
        if let Ok(index) = number.parse::<u64>() {
            next = next.max(index.saturating_add(1));
        }
    }
    Ok(next)
}

/// Derive the same-directory temp path a live segment is muxed to before being
/// renamed into place (HLS-1 atomic publish): `seg3.ts` -> `seg3.ts.tmp`. The
/// temp sits beside the final file so the subsequent `rename(2)` is atomic and
/// `EXDEV`-free.
fn segment_temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map_or_else(
        || std::ffi::OsString::from("segment"),
        std::ffi::OsStr::to_os_string,
    );
    name.push(".tmp");
    match path.parent() {
        Some(parent) => parent.join(name),
        None => PathBuf::from(name),
    }
}

/// Compute a packet's presentation time on the internal ns timeline from its
/// encoder-time-base PTS. An absent PTS falls back to the running packet count
/// (a monotonic stand-in), so durations stay sane even for codecs that omit it.
fn pts_from_packet(raw: Option<i64>, packet_count: u64, time_base: Rational) -> MediaTime {
    let ticks = raw.unwrap_or_else(|| i64::try_from(packet_count).unwrap_or(i64::MAX));
    MediaTime::from_nanos(rescale(ticks, time_base, Rational::new(1, NANOS_PER_SEC)))
}

/// Record a packet (and whether it was a keyframe) into `stats`.
fn record(stats: &mut EncodeStats, is_key: bool) {
    stats.packets = stats.packets.saturating_add(1);
    if is_key {
        stats.keyframes = stats.keyframes.saturating_add(1);
    }
}

/// Convert an integer nanosecond span into seconds (HLS `EXTINF` is decimal
/// seconds, so this float is for the text layer only — invariant #3 keeps the
/// authoritative time as integer ns).
fn seconds_from_ns(ns: i64) -> f64 {
    // `as` is banned. Split into whole seconds + remainder so each part fits an
    // i32 exactly, then recombine in f64 — exact for any non-negative ns.
    let ns = ns.max(0);
    let whole = ns / NANOS_PER_SEC;
    let frac = ns % NANOS_PER_SEC;
    // `whole` may exceed i32; saturate it through i64->f64 via integer string is
    // overkill for segment durations (seconds), so clamp to a generous i32.
    let whole_f = f64::from(i32::try_from(whole.min(i64::from(i32::MAX))).unwrap_or(i32::MAX));
    let frac_f = f64::from(i32::try_from(frac).unwrap_or(0)) / 1_000_000_000.0;
    whole_f + frac_f
}

#[cfg(all(test, feature = "ffmpeg"))]
mod tests {
    //! Unit tests using the real `ffmpeg` encode/decode path. The seed-encoder
    //! seam ([`SEED_ENCODER_BUILDS`]) is private, so this lives in-crate where it
    //! can be read directly (an integration test could not observe it).
    // Test helpers here use the test-only ergonomics the repo allows in tests
    // (the clippy.toml `allow-*-in-tests` options do not reach helper fns inside
    // a `#[cfg(test)]` module, so the relaxation is stated explicitly — matching
    // the `#![allow(...)]` header every `tests/*.rs` file carries).
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]
    use std::path::Path;
    use std::process::Command;
    use std::sync::atomic::Ordering;

    use ffmpeg_next as ffmpeg;
    use multiview_core::time::Rational;
    use multiview_ffmpeg::{DecodedVideoFrame, StreamVideoDecoder};

    use super::{
        bound_live_report, prune_prior_live_segments, prune_prior_rendition_manifests_inactive,
        AudioEncodeConfig, EncodeConfig, Result, SegmentSink, VideoFrameSource,
        SEED_ENCODER_BUILDS, SEGMENT_FINALIZES,
    };
    use crate::error::Error;

    const WIDTH: u32 = 160;
    const HEIGHT: u32 = 120;

    #[test]
    fn packet_write_failure_discards_partial_and_resumes_at_rap() {
        use super::{PacketSource, SegmentState, StreamSeed};
        use crate::hls::LivePlaylist;
        use ffmpeg::codec::packet::{Flags, Packet};
        use multiview_ffmpeg::{EncodedPacket, StreamCodecParameters, VideoEncoder};

        struct Packets(std::vec::IntoIter<EncodedPacket>);
        impl PacketSource for Packets {
            fn next_packet(&mut self) -> Result<Option<EncodedPacket>> {
                Ok(self.0.next())
            }
        }
        let _counter_guard = counter_guard();
        let dir = tempfile::tempdir().unwrap();
        let encoder = VideoEncoder::new(&EncodeConfig::mpeg2(WIDTH, HEIGHT).target()).unwrap();
        let params = StreamCodecParameters::from_encoder(&encoder);
        let time_base = encoder.time_base();
        let mut state = SegmentState::new(
            StreamSeed::Params(&params),
            time_base,
            None,
            dir.path(),
            "seg",
        );
        state.live = Some(LivePlaylist::new(dir.path().join("index.m3u8"), 3));
        let mut packets = Vec::new();
        for index in 0..5 {
            let mut packet = Packet::copy(&[0, 0, 1, 0, 0, 8, 0, 0]);
            packet.set_flags(if index == 0 || index == 3 {
                Flags::KEY
            } else {
                Flags::empty()
            });
            packet.set_pts(Some(index));
            packet.set_dts(Some(index));
            packets.push(EncodedPacket::from_packet(packet));
        }
        // The initial access unit is written into the live .tmp file. The
        // next write fails and must not advertise that partial file.
        let first = packets.remove(0);
        state
            .write_source_packet(first, multiview_core::time::MediaTime::ZERO)
            .unwrap();
        state.fail_packet_write_once = true;
        state
            .drive_from_packets(&mut Packets(packets.into_iter()))
            .unwrap();
        let mut report = crate::hls::MediaPlaylist::new(crate::hls::SegmentType::MpegTs);
        state.finish(&mut report, 33_333_333).unwrap();
        assert!(!dir.path().join("seg0.ts.tmp").exists());
        let playlist = std::fs::read_to_string(dir.path().join("index.m3u8")).unwrap();
        assert!(!playlist.contains("seg0.ts"));
        assert!(playlist.contains("#EXT-X-DISCONTINUITY\n"));
        assert!(playlist.contains("seg1.ts"));
    }

    /// A video keyframe flagged as a decoder-config change (a routing move
    /// onto different parameter sets) puts exactly one `EXT-X-DISCONTINUITY`
    /// immediately before the segment that STARTS at it; an unflagged run has
    /// none.
    #[test]
    fn a_decoder_config_change_marks_the_segment_starting_at_its_keyframe() {
        use super::{PacketSource, SegmentState, StreamSeed};
        use crate::hls::LivePlaylist;
        use ffmpeg::codec::packet::{Flags, Packet};
        use multiview_ffmpeg::{EncodedPacket, StreamCodecParameters, VideoEncoder};

        struct Flagged {
            packets: std::vec::IntoIter<EncodedPacket>,
            flag_pts: Option<i64>,
            last_pts: Option<i64>,
        }
        impl PacketSource for Flagged {
            fn next_packet(&mut self) -> Result<Option<EncodedPacket>> {
                let next = self.packets.next();
                self.last_pts = next.as_ref().and_then(EncodedPacket::pts);
                Ok(next)
            }
            fn take_decoder_config_change(&mut self) -> bool {
                self.last_pts.is_some() && self.last_pts == self.flag_pts
            }
        }
        let run = |flag_pts: Option<i64>| -> String {
            let dir = tempfile::tempdir().unwrap();
            let encoder = VideoEncoder::new(&EncodeConfig::mpeg2(WIDTH, HEIGHT).target()).unwrap();
            let params = StreamCodecParameters::from_encoder(&encoder);
            let mut state = SegmentState::new(
                StreamSeed::Params(&params),
                encoder.time_base(),
                None,
                dir.path(),
                "seg",
            );
            state.live = Some(LivePlaylist::new(dir.path().join("index.m3u8"), 10));
            // Keyframes at 0, 3, 6, 9: segments seg0..seg3 start there.
            let packets: Vec<EncodedPacket> = (0..12)
                .map(|index| {
                    let mut packet = Packet::copy(&[0, 0, 1, 0, 0, 8, 0, 0]);
                    packet.set_flags(if index % 3 == 0 {
                        Flags::KEY
                    } else {
                        Flags::empty()
                    });
                    packet.set_pts(Some(index));
                    packet.set_dts(Some(index));
                    EncodedPacket::from_packet(packet)
                })
                .collect();
            let mut source = Flagged {
                packets: packets.into_iter(),
                flag_pts,
                last_pts: None,
            };
            state.drive_from_packets(&mut source).unwrap();
            let mut report = crate::hls::MediaPlaylist::new(crate::hls::SegmentType::MpegTs);
            state.finish(&mut report, 33_333_333).unwrap();
            std::fs::read_to_string(dir.path().join("index.m3u8")).unwrap()
        };
        let _counter_guard = counter_guard();
        let plain = run(None);
        assert!(
            !plain.contains("#EXT-X-DISCONTINUITY\n"),
            "no flag, no discontinuity:\n{plain}"
        );
        let flagged = run(Some(6));
        assert_eq!(
            flagged.matches("#EXT-X-DISCONTINUITY\n").count(),
            1,
            "{flagged}"
        );
        let lines: Vec<&str> = flagged.lines().collect();
        let at = lines
            .iter()
            .position(|l| *l == "#EXT-X-DISCONTINUITY")
            .unwrap();
        let next_uri = lines[at + 1..]
            .iter()
            .find(|l| !l.starts_with('#'))
            .unwrap();
        assert_eq!(
            *next_uri, "seg2.ts",
            "the discontinuity precedes the segment starting at the flagged keyframe:\n{flagged}"
        );
    }

    /// Real MPEG-2 packets: `n` ticks from `start` encoded at `w`×`h`, every
    /// third a keyframe, plus that encoder's stream parameters.
    fn sized_packets(
        w: u32,
        h: u32,
        start: u64,
        n: u64,
    ) -> (
        Vec<multiview_ffmpeg::EncodedPacket>,
        multiview_ffmpeg::StreamCodecParameters,
    ) {
        rated_packets(w, h, 2_000_000, start, n)
    }

    /// [`sized_packets`] at bit rate `bit_rate` (bit/s).
    fn rated_packets(
        w: u32,
        h: u32,
        bit_rate: usize,
        start: u64,
        n: u64,
    ) -> (
        Vec<multiview_ffmpeg::EncodedPacket>,
        multiview_ffmpeg::StreamCodecParameters,
    ) {
        use multiview_core::frame::FrameMeta;
        use multiview_core::pixel::PixelFormat;
        use multiview_core::time::MediaTime;
        let mut config = EncodeConfig::mpeg2(w, h);
        config.bit_rate = bit_rate;
        let mut enc = super::VideoRenditionEncoder::new(&config).unwrap();
        enc.start_at(start);
        let params = enc.codec_params().clone();
        let mut out = Vec::new();
        for tick in start..start + n {
            if (tick - start) % 3 == 0 {
                enc.force_next_keyframe();
            }
            let mut video = ffmpeg::util::frame::Video::new(ffmpeg::format::Pixel::YUV420P, w, h);
            for plane in 0..3 {
                video
                    .data_mut(plane)
                    .fill(if plane == 0 { 80 } else { 128 });
            }
            let frame = DecodedVideoFrame {
                frame: video,
                meta: FrameMeta {
                    pts: MediaTime::ZERO,
                    width: w,
                    height: h,
                    format: PixelFormat::Nv12,
                    color: multiview_core::color::ColorInfo::default(),
                },
                raw_pts: None,
                a53_cc: None,
            };
            out.extend(enc.encode_frame_at(frame, tick).unwrap());
        }
        out.extend(enc.finish().unwrap());
        (out, params)
    }

    /// Width × height of the video stream a written `.ts` file declares: the
    /// MPEG-2 sequence header (`00 00 01 B3`, 12-bit width, 12-bit height) of
    /// its first video access unit, read from the TS payload bytes (segments
    /// this short are below libavformat's probe threshold).
    fn ts_video_size(path: &std::path::Path) -> (u32, u32) {
        let bytes = std::fs::read(path).unwrap();
        // Concatenate the payloads of the TS packets (skip header/adaptation).
        let mut payload = Vec::new();
        for p in bytes.chunks_exact(188) {
            assert_eq!(p[0], 0x47, "TS sync");
            let pid = (u16::from(p[1] & 0x1f) << 8) | u16::from(p[2]);
            if pid == 0 || pid == 0x1fff || p[3] & 0x10 == 0 {
                continue;
            }
            let mut at = 4;
            if p[3] & 0x20 != 0 {
                at += 1 + usize::from(p[4]);
            }
            payload.extend_from_slice(&p[at.min(188)..]);
        }
        let i = payload
            .windows(4)
            .position(|w| w == [0, 0, 1, 0xb3])
            .unwrap_or_else(|| panic!("no MPEG-2 sequence header in {}", path.display()));
        let h = &payload[i + 4..i + 7];
        let width = (u32::from(h[0]) << 4) | (u32::from(h[1]) >> 4);
        let height = (u32::from(h[1] & 0x0f) << 8) | u32::from(h[2]);
        (width, height)
    }

    /// A source switching encoders at a flagged keyframe and handing the new
    /// encoder's parameters with the change (as a live format reset does).
    struct Switching {
        packets: std::vec::IntoIter<multiview_ffmpeg::EncodedPacket>,
        switch_pts: i64,
        params: Option<multiview_ffmpeg::StreamCodecParameters>,
        last_pts: Option<i64>,
    }
    impl super::PacketSource for Switching {
        fn next_packet(&mut self) -> Result<Option<multiview_ffmpeg::EncodedPacket>> {
            let next = self.packets.next();
            self.last_pts = next.as_ref().and_then(multiview_ffmpeg::EncodedPacket::pts);
            Ok(next)
        }
        fn take_decoder_config_change(&mut self) -> bool {
            self.last_pts == Some(self.switch_pts)
        }
        fn take_decoder_config_params(
            &mut self,
        ) -> Option<multiview_ffmpeg::StreamCodecParameters> {
            if self.last_pts == Some(self.switch_pts) {
                self.params.take()
            } else {
                None
            }
        }
    }

    /// #342 B3 review V2: the segment muxer rebuilt at a live format reset is
    /// seeded from the NEW encoder's parameters: segments before the switch
    /// declare the old size, the switch segment and later the new one, with
    /// one discontinuity and the bumped table version.
    #[test]
    fn a_rebuilt_segment_muxer_declares_the_new_encoders_size() {
        use super::{SegmentState, StreamSeed};
        use crate::hls::LivePlaylist;
        let _counter_guard = counter_guard();
        let (mut packets, old_params) = sized_packets(160, 120, 0, 6);
        let (new_packets, new_params) = sized_packets(80, 120, 6, 6);
        packets.extend(new_packets);
        let dir = tempfile::tempdir().unwrap();
        let mut state = SegmentState::new(
            StreamSeed::Params(&old_params),
            Rational::new(1, 30),
            None,
            dir.path(),
            "seg",
        );
        state.live = Some(LivePlaylist::new(dir.path().join("index.m3u8"), 10));
        let mut source = Switching {
            packets: packets.into_iter(),
            switch_pts: 6,
            params: Some(new_params),
            last_pts: None,
        };
        state.drive_from_packets(&mut source).unwrap();
        let mut report = crate::hls::MediaPlaylist::new(crate::hls::SegmentType::MpegTs);
        state.finish(&mut report, 33_333_333).unwrap();
        let sizes: Vec<(u32, u32)> = (0..4)
            .map(|i| ts_video_size(&dir.path().join(format!("seg{i}.ts"))))
            .collect();
        assert_eq!(sizes, vec![(160, 120), (160, 120), (80, 120), (80, 120)]);
        let playlist = std::fs::read_to_string(dir.path().join("index.m3u8")).unwrap();
        assert_eq!(
            playlist.matches("#EXT-X-DISCONTINUITY\n").count(),
            1,
            "{playlist}"
        );
        let (_, before) = ts_table_versions(&dir.path().join("seg1.ts"));
        let (_, after) = ts_table_versions(&dir.path().join("seg2.ts"));
        assert_eq!(after, (before + 1) % 32);
    }

    /// Review V2: a failed rebuild is not fatal: the segment is abandoned and
    /// the sink recovers at the next RAP, where the rebuild and the
    /// discontinuity happen (seeded from the new parameters).
    #[test]
    fn a_failed_rebuild_recovers_at_the_next_rap() {
        use super::{SegmentState, StreamSeed};
        use crate::hls::LivePlaylist;
        let _counter_guard = counter_guard();
        let (mut packets, old_params) = sized_packets(160, 120, 0, 6);
        let (new_packets, new_params) = sized_packets(80, 120, 6, 9);
        packets.extend(new_packets);
        let dir = tempfile::tempdir().unwrap();
        let mut state = SegmentState::new(
            StreamSeed::Params(&old_params),
            Rational::new(1, 30),
            None,
            dir.path(),
            "seg",
        );
        state.live = Some(LivePlaylist::new(dir.path().join("index.m3u8"), 10));
        state.fail_rebuild_once = true;
        let mut source = Switching {
            packets: packets.into_iter(),
            switch_pts: 6,
            params: Some(new_params),
            last_pts: None,
        };
        state
            .drive_from_packets(&mut source)
            .expect("recovered, not fatal");
        let mut report = crate::hls::MediaPlaylist::new(crate::hls::SegmentType::MpegTs);
        state.finish(&mut report, 33_333_333).unwrap();
        let playlist = std::fs::read_to_string(dir.path().join("index.m3u8")).unwrap();
        // The segment at the switch was abandoned; the next RAP (pts 9)
        // opened the rebuilt muxer at the new size.
        let uris: Vec<&str> = playlist.lines().filter(|l| !l.starts_with('#')).collect();
        let last = dir.path().join(uris.last().unwrap());
        assert_eq!(ts_video_size(&last), (80, 120), "{playlist}");
        let first = dir.path().join(uris[0]);
        assert_eq!(ts_video_size(&first), (160, 120));
        assert!(playlist.contains("#EXT-X-DISCONTINUITY\n"), "{playlist}");
        // The retried rebuild uses the ONE bumped version (review R2-V1).
        let (_, before) = ts_table_versions(&first);
        let (_, rebuilt) = ts_table_versions(&last);
        assert_eq!(rebuilt, (before + 1) % 32, "{playlist}");
    }

    /// A live `SegmentState` with the master/rendition layout (the master's
    /// video-only rendition alongside the main muxer), over `dir`.
    fn rendition_state<'a>(
        dir: &'a std::path::Path,
        params: &'a multiview_ffmpeg::StreamCodecParameters,
    ) -> super::SegmentState<'a> {
        use super::{MuxStream, SegmentState, StreamSeed};
        use crate::hls::LivePlaylist;
        let mut state = SegmentState::new(
            StreamSeed::Params(params),
            Rational::new(1, 30),
            None,
            dir,
            "seg",
        );
        let playlist = dir.join("index.m3u8");
        state.live = Some(LivePlaylist::new(playlist.clone(), 10));
        state
            .set_audio_renditions(
                &playlist,
                10,
                crate::epoch::SharedEpoch::new(),
                MuxStream::new(params, Rational::new(1, 30)),
                None,
                &[],
                &[],
            )
            .unwrap();
        state
    }

    /// The `.ts` URIs a media playlist lists, in order.
    fn playlist_uris(path: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
            .map(str::to_owned)
            .collect()
    }

    /// #342 B3 review R2-V1: on a live reset of an HLS output with the
    /// master/rendition layout, the master's video-only rendition is rebuilt
    /// exactly like the main muxer: its segment at the flagged keyframe
    /// declares the NEW encoder's size with the bumped PAT/PMT
    /// `version_number`, and both the main and the rendition media playlists
    /// carry one discontinuity there.
    #[test]
    fn a_reset_rebuilds_the_masters_video_rendition_from_the_new_encoder() {
        let _counter_guard = counter_guard();
        let (mut packets, old_params) = sized_packets(160, 120, 0, 6);
        let (new_packets, new_params) = sized_packets(80, 120, 6, 6);
        packets.extend(new_packets);
        let dir = tempfile::tempdir().unwrap();
        let mut state = rendition_state(dir.path(), &old_params);
        let mut source = Switching {
            packets: packets.into_iter(),
            switch_pts: 6,
            params: Some(new_params),
            last_pts: None,
        };
        state.drive_from_packets(&mut source).unwrap();
        let mut report = crate::hls::MediaPlaylist::new(crate::hls::SegmentType::MpegTs);
        state.finish(&mut report, 33_333_333).unwrap();
        let video_playlist = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| {
                p.extension().is_some_and(|x| x == "m3u8")
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("video-g"))
            })
            .expect("the video rendition playlist");
        let uris = playlist_uris(&video_playlist);
        assert_eq!(uris.len(), 4, "{uris:?}");
        let sizes: Vec<(u32, u32)> = uris
            .iter()
            .map(|u| ts_video_size(&dir.path().join(u)))
            .collect();
        assert_eq!(sizes, vec![(160, 120), (160, 120), (80, 120), (80, 120)]);
        let (_, before) = ts_table_versions(&dir.path().join(&uris[1]));
        let (_, after) = ts_table_versions(&dir.path().join(&uris[2]));
        assert_eq!(
            after,
            (before + 1) % 32,
            "the rendition's PMT version bumps"
        );
        for playlist in [dir.path().join("index.m3u8"), video_playlist] {
            let text = std::fs::read_to_string(&playlist).unwrap();
            assert_eq!(
                text.matches("#EXT-X-DISCONTINUITY\n").count(),
                1,
                "{}: {text}",
                playlist.display()
            );
        }
    }

    /// The master's single variant line (`#EXT-X-STREAM-INF:...`).
    fn master_variant(dir: &std::path::Path) -> String {
        std::fs::read_to_string(dir.join("master.m3u8"))
            .unwrap()
            .lines()
            .find(|l| l.starts_with("#EXT-X-STREAM-INF:"))
            .unwrap()
            .to_owned()
    }

    /// #342 B3 review R3-1: after a live reset (160x120 at 2 Mbit/s →
    /// 80x120 at 1 Mbit/s) the master advertises the NEW variant —
    /// RESOLUTION and BANDWIDTH from the new encoder's parameters — and it
    /// is published only once the reset segment is in the media playlists.
    /// Before the reset it advertises the old variant.
    #[test]
    fn a_reset_republishes_the_master_with_the_new_variant() {
        let _counter_guard = counter_guard();
        let (mut packets, old_params) = rated_packets(160, 120, 2_000_000, 0, 6);
        let (new_packets, new_params) = rated_packets(80, 120, 1_000_000, 6, 6);
        packets.extend(new_packets);
        let dir = tempfile::tempdir().unwrap();
        let mut state = rendition_state(dir.path(), &old_params);
        // Before the reset: the first segments only.
        let before: Vec<_> = packets
            .iter()
            .take(6)
            .map(|p| p.clone_shared_ref().unwrap())
            .collect();
        let mut first = Switching {
            packets: before.into_iter(),
            switch_pts: -1,
            params: None,
            last_pts: None,
        };
        state.drive_from_packets(&mut first).unwrap();
        let mut source = Switching {
            packets: packets.into_iter().skip(6).collect::<Vec<_>>().into_iter(),
            switch_pts: 6,
            params: Some(new_params),
            last_pts: None,
        };
        // Before the reset the master advertises the old variant (whether or
        // not its background write has landed yet).
        let master_before = state
            .renditions
            .as_ref()
            .and_then(|r| r.master_text.clone())
            .expect("a master is rendered");
        assert!(
            master_before.contains("RESOLUTION=160x120"),
            "{master_before}"
        );
        assert!(
            master_before.contains("BANDWIDTH=2000000"),
            "{master_before}"
        );
        state.drive_from_packets(&mut source).unwrap();
        // Mid-run (before `finish`): the reset segment has closed, so the
        // master advertising the new variant is republished on its own; the
        // on-disk master reaches it (atomic: only ever old or new text).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let text = std::fs::read_to_string(dir.path().join("master.m3u8")).unwrap_or_default();
            assert!(
                text.is_empty() || text.contains("RESOLUTION=") && text.contains("#EXTM3U"),
                "never a torn master: {text}"
            );
            if text.contains("RESOLUTION=80x120") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the new master is republished during the run: {text}"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let mut report = crate::hls::MediaPlaylist::new(crate::hls::SegmentType::MpegTs);
        state.finish(&mut report, 33_333_333).unwrap();
        let variant = master_variant(dir.path());
        assert!(variant.contains("RESOLUTION=80x120"), "{variant}");
        assert!(variant.contains("BANDWIDTH=1000000"), "{variant}");
        assert!(!variant.contains("160x120"), "{variant}");
    }

    /// Review R2-V1: a failed rebuild of the video rendition is contained
    /// like the main muxer's: the segment is abandoned and both muxers
    /// recover at the next RAP, rebuilt from the new parameters (never a
    /// fatal error, never the old size after the switch).
    #[test]
    fn a_failed_video_rendition_rebuild_recovers_at_the_next_rap() {
        let _counter_guard = counter_guard();
        let (mut packets, old_params) = sized_packets(160, 120, 0, 6);
        let (new_packets, new_params) = sized_packets(80, 120, 6, 9);
        packets.extend(new_packets);
        let dir = tempfile::tempdir().unwrap();
        let mut state = rendition_state(dir.path(), &old_params);
        state.fail_rendition_rebuild_once = true;
        let mut source = Switching {
            packets: packets.into_iter(),
            switch_pts: 6,
            params: Some(new_params),
            last_pts: None,
        };
        state
            .drive_from_packets(&mut source)
            .expect("recovered, not fatal");
        assert!(
            !state.fail_rendition_rebuild_once,
            "the video rendition was rebuilt (and the injected failure hit)"
        );
        // Review R3-1: the master follows the reset once the recovery
        // rebuild succeeded.
        assert!(
            state
                .renditions
                .as_ref()
                .and_then(|r| r.master_text.as_deref())
                .is_some_and(|t| t.contains("RESOLUTION=80x120")),
            "the recovered rebuild re-renders the master"
        );
        let mut report = crate::hls::MediaPlaylist::new(crate::hls::SegmentType::MpegTs);
        state.finish(&mut report, 33_333_333).unwrap();
        for playlist in std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "m3u8"))
            .filter(|p| p.file_name().is_some_and(|n| n != "master.m3u8"))
        {
            let uris = playlist_uris(&playlist);
            let text = std::fs::read_to_string(&playlist).unwrap();
            let last = dir.path().join(uris.last().unwrap());
            assert_eq!(
                ts_video_size(&last),
                (80, 120),
                "{}: {text}",
                playlist.display()
            );
            assert_eq!(ts_video_size(&dir.path().join(&uris[0])), (160, 120));
            assert!(text.contains("#EXT-X-DISCONTINUITY\n"), "{text}");
            // The retried rebuild uses the ONE bumped version, not two.
            let (_, first) = ts_table_versions(&dir.path().join(&uris[0]));
            let (_, rebuilt) = ts_table_versions(&last);
            assert_eq!(rebuilt, (first + 1) % 32, "{}", playlist.display());
        }
    }

    /// The `version_number` of the PAT and of the PMT it points at, from the
    /// first sections of each in a written `.ts` file.
    fn ts_table_versions(path: &std::path::Path) -> (u8, u8) {
        let bytes = std::fs::read(path).unwrap();
        let section = |pid: u16| -> Option<Vec<u8>> {
            bytes.chunks_exact(188).find_map(|p| {
                let this = (u16::from(p[1] & 0x1f) << 8) | u16::from(p[2]);
                let start = p[1] & 0x40 != 0;
                if p[0] != 0x47 || this != pid || !start {
                    return None;
                }
                let mut at = 4;
                if p[3] & 0x20 != 0 {
                    at += 1 + usize::from(p[4]);
                }
                let pointer = usize::from(p[at]);
                Some(p[at + 1 + pointer..].to_vec())
            })
        };
        let pat = section(0).unwrap();
        assert_eq!(pat[0], 0x00, "PAT table id");
        let pat_version = (pat[5] >> 1) & 0x1f;
        // The first program entry after the 8-byte header.
        let pmt_pid = (u16::from(pat[10] & 0x1f) << 8) | u16::from(pat[11]);
        let pmt = section(pmt_pid).unwrap();
        assert_eq!(pmt[0], 0x02, "PMT table id");
        (pat_version, (pmt[5] >> 1) & 0x1f)
    }

    /// #342 B3: a decoder-configuration change also bumps the TS PAT/PMT
    /// `version_number` of the segment it starts (the muxer is rebuilt with
    /// `tables_version`+1), and only there: earlier segments keep the old
    /// version, later ones keep the new one; one `EXT-X-DISCONTINUITY`.
    #[test]
    fn a_decoder_config_change_bumps_the_ts_table_version_at_its_segment() {
        use super::{PacketSource, SegmentState, StreamSeed};
        use crate::hls::LivePlaylist;
        use ffmpeg::codec::packet::{Flags, Packet};
        use multiview_ffmpeg::{EncodedPacket, StreamCodecParameters, VideoEncoder};

        struct Flagged {
            packets: std::vec::IntoIter<EncodedPacket>,
            last_pts: Option<i64>,
        }
        impl PacketSource for Flagged {
            fn next_packet(&mut self) -> Result<Option<EncodedPacket>> {
                let next = self.packets.next();
                self.last_pts = next.as_ref().and_then(EncodedPacket::pts);
                Ok(next)
            }
            fn take_decoder_config_change(&mut self) -> bool {
                self.last_pts == Some(6)
            }
        }
        let _counter_guard = counter_guard();
        let dir = tempfile::tempdir().unwrap();
        let encoder = VideoEncoder::new(&EncodeConfig::mpeg2(WIDTH, HEIGHT).target()).unwrap();
        let params = StreamCodecParameters::from_encoder(&encoder);
        let mut state = SegmentState::new(
            StreamSeed::Params(&params),
            encoder.time_base(),
            None,
            dir.path(),
            "seg",
        );
        state.live = Some(LivePlaylist::new(dir.path().join("index.m3u8"), 10));
        let packets: Vec<EncodedPacket> = (0..12)
            .map(|index| {
                let mut packet = Packet::copy(&[0, 0, 1, 0, 0, 8, 0, 0]);
                packet.set_flags(if index % 3 == 0 {
                    Flags::KEY
                } else {
                    Flags::empty()
                });
                packet.set_pts(Some(index));
                packet.set_dts(Some(index));
                EncodedPacket::from_packet(packet)
            })
            .collect();
        let mut source = Flagged {
            packets: packets.into_iter(),
            last_pts: None,
        };
        state.drive_from_packets(&mut source).unwrap();
        let mut report = crate::hls::MediaPlaylist::new(crate::hls::SegmentType::MpegTs);
        state.finish(&mut report, 33_333_333).unwrap();
        let playlist = std::fs::read_to_string(dir.path().join("index.m3u8")).unwrap();
        assert_eq!(
            playlist.matches("#EXT-X-DISCONTINUITY\n").count(),
            1,
            "{playlist}"
        );
        let versions: Vec<(u8, u8)> = (0..4)
            .map(|i| ts_table_versions(&dir.path().join(format!("seg{i}.ts"))))
            .collect();
        assert_eq!(versions[0], versions[1], "unchanged before the flag");
        assert_eq!(versions[2], versions[3], "stable after the flag");
        assert_ne!(versions[1].1, versions[2].1, "PMT version_number changed");
        assert_ne!(versions[1].0, versions[2].0, "PAT version_number changed");
        assert_eq!(versions[2].1, (versions[1].1 + 1) % 32, "bumped by one");
    }

    #[test]
    fn unsupported_recovery_fails_before_live_source_can_drain() {
        use super::{PacketSource, SegmentState, StreamSeed};
        use crate::hls::LivePlaylist;
        use ffmpeg::codec::packet::{Flags, Packet};
        use multiview_ffmpeg::{EncodedPacket, VideoEncoder};
        struct Endless {
            pulls: usize,
        }
        impl PacketSource for Endless {
            fn next_packet(&mut self) -> Result<Option<EncodedPacket>> {
                self.pulls += 1;
                assert!(
                    self.pulls <= 2,
                    "unsupported recovery must not drain the live source"
                );
                let mut packet = Packet::copy(&[0, 0, 1, 0, 0, 8, 0, 0]);
                packet.set_flags(Flags::KEY);
                let pts = i64::try_from(self.pulls).expect("bounded test pull count");
                packet.set_pts(Some(pts));
                packet.set_dts(Some(pts));
                Ok(Some(EncodedPacket::from_packet(packet)))
            }
        }
        let _counter_guard = counter_guard();
        let dir = tempfile::tempdir().unwrap();
        let encoder = VideoEncoder::new(&EncodeConfig::mpeg2(WIDTH, HEIGHT).target()).unwrap();
        let mut state = SegmentState::new(
            StreamSeed::Encoder(&encoder),
            encoder.time_base(),
            None,
            dir.path(),
            "seg",
        );
        state.live = Some(LivePlaylist::new(dir.path().join("index.m3u8"), 3));
        state.fail_packet_write_once = true;
        let mut source = Endless { pulls: 0 };
        let error = state
            .drive_from_packets(&mut source)
            .expect_err("unsupported recovery must fail");
        assert!(error.to_string().contains("recovery unsupported"));
        assert_eq!(source.pulls, 1);
        assert!(!dir.path().join("seg0.ts.tmp").exists());
        assert!(!dir.path().join("index.m3u8").exists());
    }

    #[test]
    fn source_end_during_recovery_returns_error_instead_of_stale_success() {
        use super::{PacketSource, SegmentState, StreamSeed};
        use crate::hls::LivePlaylist;
        use ffmpeg::codec::packet::{Flags, Packet};
        use multiview_ffmpeg::{EncodedPacket, StreamCodecParameters, VideoEncoder};
        struct Packets(std::vec::IntoIter<EncodedPacket>);
        impl PacketSource for Packets {
            fn next_packet(&mut self) -> Result<Option<EncodedPacket>> {
                Ok(self.0.next())
            }
        }
        let _counter_guard = counter_guard();
        let dir = tempfile::tempdir().unwrap();
        let encoder = VideoEncoder::new(&EncodeConfig::mpeg2(WIDTH, HEIGHT).target()).unwrap();
        let params = StreamCodecParameters::from_encoder(&encoder);
        let mut state = SegmentState::new(
            StreamSeed::Params(&params),
            encoder.time_base(),
            None,
            dir.path(),
            "seg",
        );
        state.live = Some(LivePlaylist::new(dir.path().join("index.m3u8"), 3));
        let mut packet = Packet::copy(&[0, 0, 1, 0, 0, 8, 0, 0]);
        packet.set_flags(Flags::KEY);
        packet.set_pts(Some(0));
        packet.set_dts(Some(0));
        state
            .write_source_packet(
                EncodedPacket::from_packet(packet),
                multiview_core::time::MediaTime::ZERO,
            )
            .unwrap();
        state.fail_packet_write_once = true;
        let mut bad = Packet::copy(&[0, 0, 1, 0, 0, 8, 0, 0]);
        bad.set_pts(Some(1));
        bad.set_dts(Some(1));
        let mut source = Packets(vec![EncodedPacket::from_packet(bad)].into_iter());
        let error = state
            .drive_from_packets(&mut source)
            .expect_err("unrecovered source end");
        assert!(error.to_string().contains("did not recover"));
        assert!(!dir.path().join("seg0.ts.tmp").exists());
        assert!(!dir.path().join("index.m3u8").exists());
    }

    #[test]
    fn playlist_publication_fault_is_terminal_and_keeps_prior_manifest() {
        use super::{SegmentState, StreamSeed};
        use crate::hls::LivePlaylist;
        use ffmpeg::codec::packet::{Flags, Packet};
        use multiview_ffmpeg::{EncodedPacket, StreamCodecParameters, VideoEncoder};
        let _counter_guard = counter_guard();
        let dir = tempfile::tempdir().unwrap();
        let encoder = VideoEncoder::new(&EncodeConfig::mpeg2(WIDTH, HEIGHT).target()).unwrap();
        let params = StreamCodecParameters::from_encoder(&encoder);
        let mut state = SegmentState::new(
            StreamSeed::Params(&params),
            encoder.time_base(),
            None,
            dir.path(),
            "seg",
        );
        state.live = Some(LivePlaylist::new(dir.path().join("index.m3u8"), 3));
        let make_packet = |pts| {
            let mut packet = Packet::copy(&[0, 0, 1, 0, 0, 8, 0, 0]);
            packet.set_flags(Flags::KEY);
            packet.set_pts(Some(pts));
            packet.set_dts(Some(pts));
            EncodedPacket::from_packet(packet)
        };
        state
            .write_source_packet(make_packet(0), multiview_core::time::MediaTime::ZERO)
            .unwrap();
        state
            .write_source_packet(
                make_packet(1),
                multiview_core::time::MediaTime::from_nanos(33_333_333),
            )
            .unwrap();
        let previous = std::fs::read_to_string(dir.path().join("index.m3u8")).unwrap();
        assert!(previous.contains("seg0.ts"));
        state.fail_playlist_publish_once = Some(());
        let failure = state.start_segment(multiview_core::time::MediaTime::from_nanos(66_666_666));
        assert!(failure.is_err());
        assert!(state.publication_failed);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("index.m3u8")).unwrap(),
            previous
        );
        assert!(!previous.contains("seg1.ts"));
        state.abandon_incomplete_segments();
        assert!(dir.path().join("seg0.ts").exists());
        assert!(!dir.path().join("seg1.ts.tmp").exists());
    }

    #[test]
    fn live_report_stays_bounded_across_many_segment_cuts() {
        let mut done = Vec::new();
        for index in 0..10_000 {
            done.push((format!("seg{index}.ts").into(), 1.0));
            bound_live_report(&mut done, 6);
            assert!(done.len() <= 6);
        }
        assert_eq!(done.len(), 6);
        assert_eq!(done[0].0, Path::new("seg9994.ts"));
    }

    /// EMPIRICAL per-codec emission proof (t70 causal 2 / t62 finding: the
    /// no-reorder claim must be codec-EMPIRICAL, never a doc assertion). For
    /// every encodable LGPL/GPL codec in this build, encode a strictly
    /// consecutive tick sequence through the REAL rendition encoder and PIN:
    /// (1) EMISSION ORDER equals submission order — the i-th emitted packet's
    /// PTS is exactly the i-th submitted tick, across interleaved
    /// `encode_frame_at` calls AND the EOF `finish()` tail (a single
    /// reordering packet fails the test); (2) the restamped DTS equals PTS on
    /// every packet (the output-clock rule; decode order == presentation order
    /// for these proven encoders); (3) every submitted tick is emitted exactly
    /// once — no frame lost inside the encoder, none duplicated. This is the
    /// proof the `DTS := PTS` restamp and the emission-keyed dispatcher
    /// watermark rely on; mpeg2video's one-frame DELAY is thereby proven
    /// compatible (delayed but strictly ordered), not assumed.
    #[test]
    fn proven_codecs_emit_in_submission_order_with_restamped_dts() {
        use multiview_core::frame::FrameMeta;
        use multiview_core::pixel::PixelFormat;
        use multiview_core::time::MediaTime;
        const WIDTH: u32 = 64;
        const HEIGHT: u32 = 48;
        const TICKS: u64 = 12;

        // mpeg2video is the LGPL default (always present). libx264/libx265 are
        // probed and skipped when the build lacks gpl-codecs.
        let mut codecs = vec!["mpeg2video"];
        for optional in ["libx264", "libx265"] {
            if ffmpeg::encoder::find_by_name(optional).is_some() {
                codecs.push(optional);
            }
        }
        for codec in codecs {
            let mut config = EncodeConfig::mpeg2(WIDTH, HEIGHT);
            config.codec_name = codec.to_owned();
            config.gop = 30;
            config.cadence = Rational::FPS_29_97;
            let mut rendition = super::VideoRenditionEncoder::new(&config)
                .unwrap_or_else(|e| panic!("{codec} must open: {e}"));

            let mut emitted: Vec<(i64, i64)> = Vec::new();
            for tick in 0..TICKS {
                let frame = DecodedVideoFrame {
                    frame: ffmpeg::util::frame::Video::new(
                        ffmpeg::format::Pixel::YUV420P,
                        WIDTH,
                        HEIGHT,
                    ),
                    meta: FrameMeta {
                        pts: MediaTime::ZERO,
                        width: WIDTH,
                        height: HEIGHT,
                        format: PixelFormat::Nv12,
                        color: multiview_core::color::ColorInfo::default(),
                    },
                    raw_pts: None,
                    a53_cc: None,
                };
                for packet in rendition
                    .encode_frame_at(frame, tick)
                    .unwrap_or_else(|e| panic!("{codec} tick {tick}: {e}"))
                {
                    let (pts, dts) = (packet.pts(), packet.dts());
                    emitted.push((
                        pts.expect("encoder stamps PTS"),
                        dts.expect("encoder stamps DTS"),
                    ));
                }
            }
            for packet in rendition
                .finish()
                .unwrap_or_else(|e| panic!("{codec} finish: {e}"))
            {
                let (pts, dts) = (packet.pts(), packet.dts());
                emitted.push((pts.expect("tail stamps PTS"), dts.expect("tail stamps DTS")));
            }

            assert_eq!(
                emitted.len(),
                usize::try_from(TICKS).unwrap(),
                "{codec}: every submitted tick must be emitted exactly once (delayed emission                  is fine; loss or duplication is not), got {emitted:?}"
            );
            for (index, (pts, dts)) in emitted.iter().enumerate() {
                let expected = i64::try_from(index).unwrap();
                assert_eq!(
                    *pts, expected,
                    "{codec}: emission order must equal submission order — packet {index} has                      PTS {pts}, a reordering encoder is NOT proven no-reorder"
                );
                assert_eq!(
                    *dts, *pts,
                    "{codec}: the restamped DTS must equal PTS (decode order == presentation                      order for proven no-reorder encoders), packet {index}"
                );
            }
        }
    }

    /// A solid YUV420P test frame of luma `luma`.
    fn live_open_frame(luma: u8) -> DecodedVideoFrame {
        use multiview_core::frame::FrameMeta;
        use multiview_core::pixel::PixelFormat;
        use multiview_core::time::MediaTime;
        let mut video =
            ffmpeg::util::frame::Video::new(ffmpeg::format::Pixel::YUV420P, WIDTH, HEIGHT);
        for plane in 0..3 {
            video
                .data_mut(plane)
                .fill(if plane == 0 { luma } else { 128 });
        }
        DecodedVideoFrame {
            frame: video,
            meta: FrameMeta {
                pts: MediaTime::ZERO,
                width: WIDTH,
                height: HEIGHT,
                format: PixelFormat::Nv12,
                color: multiview_core::color::ColorInfo::default(),
            },
            raw_pts: None,
            a53_cc: None,
        }
    }

    /// The NAL type of each Annex B start code in `au`.
    fn annexb_nal_types(au: &[u8], hevc: bool) -> Vec<u8> {
        au.windows(4)
            .filter(|w| w[..3] == [0, 0, 1])
            .map(|w| {
                if hevc {
                    (w[3] >> 1) & 0x3F
                } else {
                    w[3] & 0x1F
                }
            })
            .collect()
    }

    /// The NAL units of an Annex B access unit that are parameter sets.
    fn annexb_parameter_sets(au: &[u8], hevc: bool) -> Vec<Vec<u8>> {
        let mut starts = Vec::new();
        let mut i = 0;
        while i + 3 <= au.len() {
            if au[i..i + 3] == [0, 0, 1] {
                starts.push(i + 3);
                i += 3;
            } else {
                i += 1;
            }
        }
        let mut out = Vec::new();
        for (n, &start) in starts.iter().enumerate() {
            let mut end = starts.get(n + 1).map_or(au.len(), |&next| next - 3);
            while end > start && au[end - 1] == 0 {
                end -= 1;
            }
            let nal = &au[start..end];
            let ty = if hevc {
                (nal[0] >> 1) & 0x3F
            } else {
                nal[0] & 0x1F
            };
            let is_ps = if hevc {
                (32..=34).contains(&ty)
            } else {
                ty == 7 || ty == 8
            };
            if is_ps {
                out.push(nal.to_vec());
            }
        }
        out
    }

    /// Run an encoder from tick 0 to `t0`, check that a forced keyframe at
    /// `t0` is a one-shot self-contained IDR, and return tick 0's parameter
    /// sets (the running encoder's decoder configuration).
    fn running_encoder_sets(
        config: &EncodeConfig,
        codec: &str,
        hevc: bool,
        t0: u64,
    ) -> Vec<Vec<u8>> {
        let mut running = super::VideoRenditionEncoder::new(config).unwrap();
        let first = running.encode_frame_at(live_open_frame(60), 0).unwrap();
        let reference = annexb_parameter_sets(first[0].payload().unwrap(), hevc);
        assert!(
            !reference.is_empty(),
            "{codec}: tick 0 carries parameter sets"
        );
        for tick in 1..t0 {
            running.encode_frame_at(live_open_frame(60), tick).unwrap();
        }
        // A forced keyframe on the running encoder is a self-contained IDR.
        running.force_next_keyframe();
        let forced = running.encode_frame_at(live_open_frame(60), t0).unwrap();
        assert!(
            running.codec_params().is_self_contained_rap(&forced[0]),
            "{codec}: forced keyframe {} packets, key {}, NAL types {:?}",
            forced.len(),
            forced[0].is_keyframe(),
            annexb_nal_types(forced[0].payload().unwrap(), hevc)
        );
        let ordinary = running
            .encode_frame_at(live_open_frame(60), t0 + 1)
            .unwrap();
        assert!(!ordinary[0].is_keyframe(), "{codec}: the force is one-shot");
        reference
    }

    /// Open an encoder live at `t0`: tick 0 is refused, the first packet is
    /// a self-contained IDR at absolute PTS `t0` whose parameter sets equal
    /// `reference`, and PTS continues from there.
    fn assert_live_open_at(
        config: &EncodeConfig,
        codec: &str,
        hevc: bool,
        t0: u64,
        reference: &[Vec<u8>],
    ) {
        let mut live = super::VideoRenditionEncoder::new(config)
            .unwrap()
            .starting_at(t0);
        assert_eq!(live.next_tick(), t0);
        assert!(
            live.encode_frame_at(live_open_frame(180), 0).is_err(),
            "{codec}: tick 0 refused"
        );
        let opened = live.encode_frame_at(live_open_frame(180), t0).unwrap();
        assert_eq!(
            opened[0].pts(),
            Some(i64::try_from(t0).unwrap()),
            "{codec}: absolute PTS"
        );
        assert_eq!(opened[0].dts(), opened[0].pts());
        assert!(
            live.codec_params().is_self_contained_rap(&opened[0]),
            "{codec}: first IDR"
        );
        assert_eq!(
            annexb_parameter_sets(opened[0].payload().unwrap(), hevc),
            reference,
            "{codec}: parameter sets of a live-opened encoder equal the running encoder's"
        );
        let next = live.encode_frame_at(live_open_frame(180), t0 + 1).unwrap();
        assert_eq!(next[0].pts(), Some(i64::try_from(t0 + 1).unwrap()));
    }

    /// ADR-0108 §4.2/§4.3: an encoder opened live with `starting_at(T0)` emits
    /// absolute PTS from T0, its first packet is a self-contained IDR (in-band
    /// parameter sets), a forced keyframe on a running encoder is one too, and
    /// for libx264/libx265 the live-opened encoder's parameter sets equal the
    /// running encoder's (the measured identity the move relies on; NVENC is
    /// measured on hardware).
    #[test]
    fn a_live_opened_rendition_starts_at_t0_with_a_self_contained_idr() {
        const T0: u64 = 200;
        for (codec, hevc) in [("libx264", false), ("libx265", true)] {
            if ffmpeg::encoder::find_by_name(codec).is_none() {
                continue;
            }
            let mut config = EncodeConfig::mpeg2(WIDTH, HEIGHT);
            config.codec_name = codec.to_owned();
            config.gop = 60;
            config.cadence = Rational::FPS_29_97;
            let reference = running_encoder_sets(&config, codec, hevc, T0);
            assert_live_open_at(&config, codec, hevc, T0, &reference);
        }
    }

    #[test]
    fn concrete_rendition_opens_zero_latency_and_emits_every_tick() {
        use multiview_core::frame::FrameMeta;
        use multiview_core::pixel::PixelFormat;
        use multiview_core::time::MediaTime;
        for codec in ["libx264", "libx265"] {
            if ffmpeg::encoder::find_by_name(codec).is_none() {
                continue;
            }
            let mut config = EncodeConfig::mpeg2(WIDTH, HEIGHT);
            config.codec_name = codec.to_owned();
            config.gop = 30;
            config.cadence = Rational::FPS_29_97;
            let mut rendition = super::VideoRenditionEncoder::new(&config).unwrap();
            assert!(
                rendition.no_reorder_proven(),
                "{codec} opened reorder/delay"
            );
            for tick in 0..65_u64 {
                let frame = DecodedVideoFrame {
                    frame: ffmpeg::util::frame::Video::new(
                        ffmpeg::format::Pixel::YUV420P,
                        WIDTH,
                        HEIGHT,
                    ),
                    meta: FrameMeta {
                        pts: MediaTime::ZERO,
                        width: WIDTH,
                        height: HEIGHT,
                        format: PixelFormat::Nv12,
                        color: multiview_core::color::ColorInfo::default(),
                    },
                    raw_pts: None,
                    a53_cc: None,
                };
                let packets = rendition.encode_frame_at(frame, tick).unwrap();
                assert!(
                    !packets.is_empty(),
                    "{codec} buffered input tick {tick} despite zero-latency proof"
                );
                assert!(
                    packets
                        .iter()
                        .all(|packet| packet.dts() == Some(i64::try_from(tick).unwrap_or(i64::MAX))),
                    "{codec} emitted delayed DTS for tick {tick}"
                );
            }
            assert!(
                rendition.finish().unwrap().is_empty(),
                "{codec} hid delayed frames at EOF"
            );
        }
    }

    #[test]
    fn prior_rendition_retirement_and_ts_reaper_preserve_uri_references() {
        for (uri, pinned, fail_closed) in [
            ("video-0.ts?token=x", "video-0.ts", false),
            ("video-0.ts#fragment", "video-0.ts", false),
            ("video%2D0.ts?token=x", "video-0.ts", false),
            ("/video-0.ts", "video-0.ts", true),
            ("https://example.test/video-0.ts", "video-0.ts", true),
            ("../video-0.ts", "video-0.ts", true),
            ("video%252F0.ts", "video-0.ts", true),
            ("video-%ZZ.ts", "video-0.ts", true),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let stale = dir.path().join("audio-program-g0.m3u8");
            std::fs::write(&stale, format!("#EXTM3U\n{uri}\n")).unwrap();
            let old = dir.path().join(pinned);
            std::fs::write(&old, b"retained").unwrap();
            let unlisted = dir.path().join("video-1.ts");
            std::fs::write(&unlisted, b"unlisted").unwrap();
            prune_prior_rendition_manifests_inactive(dir.path(), &[], 0);
            assert!(stale.exists(), "retired manifest with URI {uri}");
            prune_prior_live_segments(dir.path(), "seg", 100, 2);
            assert!(old.exists(), "reaper removed referenced URI {uri}");
            assert_eq!(unlisted.exists(), fail_closed, "URI {uri} GC behavior");
        }
    }

    #[test]
    fn retirement_uses_normalized_uri_age_without_removing_other_references() {
        for uri in ["video-0.ts?token=x", "video-0.ts#fragment", "video%2D0.ts"] {
            let dir = tempfile::tempdir().unwrap();
            let stale = dir.path().join("audio-program-g0.m3u8");
            let sibling = dir.path().join("video-g0.m3u8");
            std::fs::write(&stale, format!("#EXTM3U\n{uri}\n")).unwrap();
            std::fs::write(&sibling, format!("#EXTM3U\n{uri}\n")).unwrap();
            let segment = dir.path().join("video-0.ts");
            std::fs::write(&segment, b"pinned").unwrap();
            prune_prior_rendition_manifests_inactive(dir.path(), &[sibling.as_path()], 5);
            assert!(!stale.exists(), "old URI {uri} did not retire");
            prune_prior_live_segments(dir.path(), "seg", 100, 2);
            assert!(
                segment.exists(),
                "retirement removed sibling reference {uri}"
            );
        }
    }

    #[test]
    fn audio_encode_config_maps_to_a_planar_float_target() {
        // AUD-4: the optional program-audio config translates to the
        // multiview-ffmpeg AudioEncodeTarget the ProgramEncoder opens. AAC is fed
        // planar float (fltp) at the program-bus rate; the channel count selects
        // the layout. The default mpeg2() example stays video-only (audio: None)
        // so every existing video-only sink/test is unchanged.
        use ffmpeg::format::sample::Type as SampleType;
        use ffmpeg::format::Sample;
        use ffmpeg::ChannelLayout;

        assert!(EncodeConfig::mpeg2(WIDTH, HEIGHT).audio.is_none());

        let stereo = AudioEncodeConfig::aac(48_000, 2, 128_000).audio_target();
        assert_eq!(stereo.codec_name, "aac");
        assert_eq!(stereo.sample_rate, 48_000);
        assert_eq!(stereo.bit_rate, 128_000);
        assert_eq!(stereo.format, Sample::F32(SampleType::Planar));
        assert_eq!(stereo.channel_layout, ChannelLayout::STEREO);

        let mono = AudioEncodeConfig::aac(48_000, 1, 96_000).audio_target();
        assert_eq!(mono.channel_layout, ChannelLayout::MONO);
    }

    /// Serializes every test that touches the process-global seed/finalize
    /// counters (the two counter-reading tests and the `SegmentState`-driving
    /// tests, which close segments), so none interleaves with another's
    /// reset-and-read window.
    static COUNTER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Take [`COUNTER_LOCK`], tolerating poison from an earlier failed test.
    /// Every test that can reach `note_segment_finalized` or
    /// `note_seed_encoder_built` (anything that opens/closes a segment or
    /// builds a seed encoder) must hold this for its whole duration, not only
    /// the tests that read the counters: an unlocked test that closes a segment
    /// between a reader's reset and read inflates its count.
    fn counter_guard() -> std::sync::MutexGuard<'static, ()> {
        COUNTER_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn generate_clip(path: &Path, seconds: u32, fps: u32) {
        let status = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc=size={WIDTH}x{HEIGHT}:rate={fps}:duration={seconds}"),
                "-pix_fmt",
                "yuv420p",
                "-c:v",
                "mpeg2video",
                "-g",
                &fps.to_string(),
                "-keyint_min",
                &fps.to_string(),
                "-sc_threshold",
                "0",
                "-f",
                "mpegts",
            ])
            .arg(path)
            .status()
            .expect("spawn ffmpeg CLI");
        assert!(status.success(), "ffmpeg CLI failed to generate the clip");
    }

    struct DecodeSource {
        input: ffmpeg::format::context::Input,
        decoder: StreamVideoDecoder,
        stream_index: usize,
        drained: bool,
    }

    impl DecodeSource {
        fn open(path: &Path) -> Self {
            let input = ffmpeg::format::input(&path).expect("open input container");
            let stream = input
                .streams()
                .best(ffmpeg::media::Type::Video)
                .expect("input has a video stream");
            let stream_index = stream.index();
            let params = stream.parameters();
            let time_base = multiview_ffmpeg::from_ff_rational(stream.time_base());
            let decoder = StreamVideoDecoder::new(params, time_base).expect("build stream decoder");
            Self {
                input,
                decoder,
                stream_index,
                drained: false,
            }
        }
    }

    impl VideoFrameSource for DecodeSource {
        fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>> {
            loop {
                if let Some(frame) = self
                    .decoder
                    .receive_frame()
                    .map_err(|e| Error::Output(e.to_string()))?
                {
                    return Ok(Some(frame));
                }
                if self.drained {
                    return Ok(None);
                }
                let mut packet = ffmpeg::codec::packet::Packet::empty();
                match packet.read(&mut self.input) {
                    Ok(()) => {
                        if packet.stream() == self.stream_index {
                            self.decoder
                                .send_packet(&packet)
                                .map_err(|e| Error::Output(e.to_string()))?;
                        }
                    }
                    Err(ffmpeg::Error::Eof) => {
                        self.decoder
                            .send_eof()
                            .map_err(|e| Error::Output(e.to_string()))?;
                        self.drained = true;
                    }
                    Err(other) => return Err(Error::Output(other.to_string())),
                }
            }
        }
    }

    /// A source that yields `before_err` frames, then errors — modelling a
    /// mid-run input failure (`?` returns) with a segment muxer still open.
    struct FailAfter {
        inner: DecodeSource,
        remaining: usize,
    }

    impl VideoFrameSource for FailAfter {
        fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>> {
            if self.remaining == 0 {
                return Err(Error::Output("injected mid-run source failure".to_owned()));
            }
            self.remaining -= 1;
            self.inner.next_frame()
        }
    }

    fn config(fps: u32, gop: u32) -> EncodeConfig {
        let mut cfg = EncodeConfig::mpeg2(WIDTH, HEIGHT);
        cfg.cadence = Rational::new(i64::from(fps), 1);
        cfg.gop = gop;
        cfg
    }

    /// Count the `seg*.ts` segment files written into `dir` (one per opened
    /// segment, since [`SegmentSink`] writes each segment muxer's header on open).
    fn opened_segment_count(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .expect("read dir")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.extension().and_then(std::ffi::OsStr::to_str) == Some("ts")
                    && p.file_name()
                        .and_then(std::ffi::OsStr::to_str)
                        .is_some_and(|n| n.starts_with("seg"))
            })
            .count()
    }

    #[test]
    fn segment_sink_finalizes_open_segment_when_source_errors_mid_run() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("src.ts");
        // 3 seconds @ 30 fps, 1-second GOP => up to 3 GOP-aligned segments.
        generate_clip(&src, 3, 30);

        let _guard = counter_guard();
        SEGMENT_FINALIZES.store(0, Ordering::Relaxed);
        let sink = SegmentSink::new(config(30, 30), dir.path(), "seg");
        // Yield ~2.5 GOPs so at least one full segment plus a partial one are
        // open, then fail mid-run.
        let mut source = FailAfter {
            inner: DecodeSource::open(&src),
            remaining: 75,
        };
        let err = sink
            .run(&mut source)
            .expect_err("run must surface the source error");
        assert!(
            matches!(err, Error::Output(_)),
            "the injected source error must propagate, got {err:?}"
        );

        // Every segment that was opened (header written, file on disk) must have
        // been finalized before the error propagated — including the one that was
        // still open when the source failed. The bug finished only the segments
        // closed at a later keyframe, leaving the open one un-finalized.
        let opened = opened_segment_count(dir.path());
        assert!(
            opened >= 2,
            "test needs >= 2 opened segments to be meaningful, got {opened}"
        );
        let finalized = SEGMENT_FINALIZES.load(Ordering::Relaxed);
        assert_eq!(
            finalized,
            u64::try_from(opened).expect("opened count fits u64"),
            "every opened segment must be finalized on the error path: \
             opened {opened}, finalized {finalized}"
        );
    }

    #[test]
    fn segment_sink_builds_exactly_one_seed_encoder_for_many_segments() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("src.ts");
        // 3 seconds @ 30 fps, 1-second GOP => 3 GOP-aligned segments.
        generate_clip(&src, 3, 30);

        let _guard = counter_guard();
        SEED_ENCODER_BUILDS.store(0, Ordering::Relaxed);
        let sink = SegmentSink::new(config(30, 30), dir.path(), "seg");
        let mut source = DecodeSource::open(&src);
        let result = sink.run(&mut source).expect("segment sink run");

        // Sanity: the run actually produced multiple segments (otherwise a
        // per-segment seed and a one-time seed would be indistinguishable).
        assert!(
            result.segments.len() >= 2,
            "test needs >= 2 segments to be meaningful, got {}",
            result.segments.len()
        );

        // The seed encoder (codec-parameter source for each new segment muxer)
        // must be built exactly ONCE for the whole run — never once per segment
        // (invariant #7: the codec is fixed across the encode).
        let builds = SEED_ENCODER_BUILDS.load(Ordering::Relaxed);
        assert_eq!(
            builds,
            1,
            "expected exactly one seed encoder for the whole run, got {builds} \
             across {} segments",
            result.segments.len()
        );
    }

    /// ADR-0108 §5.2 Rule 1: an audio bus opened live at an arbitrary tick
    /// starts on the global AAC grid: its first packet PTS is the first
    /// multiple of 1024 at or after `total_at(T_admit)`, then steps 1024.
    #[test]
    fn a_live_opened_audio_bus_starts_on_the_global_aac_grid() {
        use multiview_audio::cadence::SampleClock;
        use multiview_audio::crossfade::{grid_ceil, AAC_FRAME_SAMPLES};
        let clock = SampleClock::new(48_000, Rational::new(30_000, 1_001));
        let config = AudioEncodeConfig::aac(48_000, 2, 128_000);
        assert!(super::AudioRenditionEncoder::starting_at_sample(&config, 1000).is_err());
        assert!(super::AudioRenditionEncoder::starting_at_sample(&config, -1024).is_err());
        for admit in [1_u64, 7, 333, 512, 4_099] {
            let g = grid_ceil(clock.total_at(admit), AAC_FRAME_SAMPLES);
            let mut enc = super::AudioRenditionEncoder::starting_at_sample(
                &config,
                i64::try_from(g).unwrap(),
            )
            .unwrap();
            assert_eq!(enc.frame_samples(), Some(1024));
            assert_eq!(enc.next_sample(), i64::try_from(g).unwrap());
            let mut pts = Vec::new();
            let mut tick = admit;
            while pts.len() < 12 {
                let frames =
                    usize::try_from(clock.total_at(tick + 1) - clock.total_at(tick)).unwrap();
                for p in enc
                    .encode_interleaved(&vec![0.1; frames * 2], frames)
                    .unwrap()
                {
                    pts.push(p.pts().unwrap());
                }
                tick += 1;
            }
            let first = pts
                .iter()
                .copied()
                .find(|&p| p >= i64::try_from(g).unwrap())
                .unwrap();
            assert_eq!(
                first,
                i64::try_from(g).unwrap(),
                "admit {admit}: first frame at G"
            );
            for pair in pts.windows(2) {
                assert_eq!(pair[1] - pair[0], 1024, "admit {admit}: step 1024 {pts:?}");
            }
            assert!(pts.iter().all(|p| p.rem_euclid(1024) == 0), "on the grid");
        }
    }

    /// Slice F (#345): a live `SegmentState` with the master/rendition
    /// layout plus `subtitles` `WebVTT` renditions fed by `feed`.
    fn subtitle_state<'a>(
        dir: &'a std::path::Path,
        params: &'a multiview_ffmpeg::StreamCodecParameters,
        feed: &'a crate::hls::SubtitleFeedReceiver,
        subtitles: &[crate::hls::SubtitleRendition],
    ) -> super::SegmentState<'a> {
        use super::{MuxStream, SegmentState, StreamSeed};
        use crate::hls::LivePlaylist;
        let mut state = SegmentState::new(
            StreamSeed::Params(params),
            Rational::new(1, 30),
            None,
            dir,
            "seg",
        );
        let playlist = dir.join("index.m3u8");
        state.live = Some(LivePlaylist::new(playlist.clone(), 10));
        state.subtitle_feed = Some(feed);
        state
            .set_audio_renditions(
                &playlist,
                10,
                crate::epoch::SharedEpoch::new(),
                MuxStream::new(params, Rational::new(1, 30)),
                None,
                &[],
                subtitles,
            )
            .unwrap();
        state
    }

    /// `(media sequence, [(EXTINF, uri)])` of a media playlist.
    fn playlist_entries(path: &std::path::Path) -> (String, Vec<(String, String)>) {
        let text = std::fs::read_to_string(path).unwrap();
        let sequence = text
            .lines()
            .find_map(|l| l.strip_prefix("#EXT-X-MEDIA-SEQUENCE:"))
            .unwrap()
            .to_owned();
        let mut entries = Vec::new();
        let mut extinf = None;
        for line in text.lines() {
            if let Some(d) = line.strip_prefix("#EXTINF:") {
                extinf = Some(d.to_owned());
            } else if !line.starts_with('#') && !line.is_empty() {
                entries.push((extinf.take().unwrap(), line.to_owned()));
            }
        }
        (sequence, entries)
    }

    fn named_playlist(dir: &std::path::Path, prefix: &str) -> std::path::PathBuf {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| {
                p.extension().is_some_and(|x| x == "m3u8")
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with(prefix))
            })
            .unwrap_or_else(|| panic!("no {prefix}* playlist"))
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    // reason: one end-to-end run checked across master, playlists and files.
    fn subtitle_renditions_align_with_video_and_map_the_real_ts_pts() {
        use crate::hls::subtitle::{first_video_pts_90k, pts_90k_of};
        use crate::hls::{subtitle_feed, SubtitleCue, SubtitleRendition};
        use multiview_core::time::MediaTime;
        let _counter_guard = counter_guard();
        // 12 ticks at 1/30, a keyframe every 3: segments of 100 ms.
        let (packets, params) = sized_packets(160, 120, 0, 12);
        let dir = tempfile::tempdir().unwrap();
        let (feed, rx) = subtitle_feed();
        let ms = |v: i64| MediaTime::from_nanos(v * 1_000_000);
        // Spans segments 0 and 1; escaped text; slot 1 gets nothing.
        assert!(feed.offer(SubtitleCue {
            slot: 0,
            start: ms(50),
            end: ms(150),
            lines: vec!["Fish & <chips>".to_owned()],
        }));
        assert!(feed.offer(SubtitleCue {
            slot: 0,
            start: ms(250),
            end: ms(280),
            lines: vec!["third".to_owned()],
        }));
        let renditions = [
            SubtitleRendition {
                name: "English".to_owned(),
                language: Some("en".to_owned()),
            },
            SubtitleRendition {
                name: "Deutsch".to_owned(),
                language: Some("de".to_owned()),
            },
        ];
        let mut state = subtitle_state(dir.path(), &params, &rx, &renditions);
        let mut source = Switching {
            packets: packets.into_iter(),
            switch_pts: -1,
            params: None,
            last_pts: None,
        };
        state.drive_from_packets(&mut source).unwrap();
        let mut report = crate::hls::MediaPlaylist::new(crate::hls::SegmentType::MpegTs);
        state.finish(&mut report, 33_333_333).unwrap();

        let master = std::fs::read_to_string(dir.path().join("master.m3u8")).unwrap();
        assert!(master.contains(
            "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=NO,AUTOSELECT=YES,URI=\"subs0-g0.m3u8\""
        ), "{master}");
        assert!(master.contains("URI=\"subs1-g0.m3u8\""), "{master}");
        assert!(master.contains(",SUBTITLES=\"subs\""), "{master}");

        let (video_seq, video) = playlist_entries(&named_playlist(dir.path(), "video-g"));
        assert_eq!(video.len(), 4, "{video:?}");
        for slot in 0..2 {
            let (seq, subs) = playlist_entries(&dir.path().join(format!("subs{slot}-g0.m3u8")));
            assert_eq!(seq, video_seq, "same media sequence");
            assert_eq!(subs.len(), video.len());
            for ((vd, vuri), (sd, suri)) in video.iter().zip(&subs) {
                assert_eq!(vd, sd, "same EXTINF");
                let index = vuri
                    .strip_prefix("video-")
                    .unwrap()
                    .strip_suffix(".ts")
                    .unwrap();
                assert_eq!(*suri, format!("subs{slot}-{index}.vtt"), "same index");
            }
        }
        let mut offsets = Vec::new();
        for (i, (_, vuri)) in video.iter().enumerate() {
            let index = vuri
                .strip_prefix("video-")
                .unwrap()
                .strip_suffix(".ts")
                .unwrap();
            let ts = std::fs::read(dir.path().join(vuri)).unwrap();
            let carried = first_video_pts_90k(&ts).expect("a video PES PTS");
            let vtt =
                std::fs::read_to_string(dir.path().join(format!("subs0-{index}.vtt"))).unwrap();
            // One shared WebVTT timeline per rendition (LOCAL zero = its first
            // segment start), so each map names the segment's own LOCAL.
            let local = format!("00:00:00.{:03}", 100 * i);
            assert!(
                vtt.starts_with(&format!(
                    "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:{carried},LOCAL:{local}\n"
                )),
                "the map names the PTS the TS carries: {vtt}"
            );
            let start = ms(100 * i64::try_from(i).unwrap());
            offsets.push(i128::from(carried) - i128::from(pts_90k_of(start)));
            let empty =
                std::fs::read_to_string(dir.path().join(format!("subs1-{index}.vtt"))).unwrap();
            assert_eq!(
                empty,
                format!("WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:{carried},LOCAL:{local}\n"),
                "an empty slot still gets a valid header-only segment"
            );
        }
        assert!(
            offsets.windows(2).all(|w| w[0] == w[1]),
            "one constant muxer offset: {offsets:?}"
        );
        let vtt = |i: usize| {
            let index = video[i]
                .1
                .strip_prefix("video-")
                .unwrap()
                .strip_suffix(".ts")
                .unwrap()
                .to_owned();
            std::fs::read_to_string(dir.path().join(format!("subs0-{index}.vtt"))).unwrap()
        };
        // RFC 8216 §3.5: the spanning cue carries its total interval, with
        // the same timestamps in both segments.
        for i in [0, 1] {
            assert!(
                vtt(i).contains("00:00:00.050 --> 00:00:00.150\nFish &amp; &lt;chips&gt;\n"),
                "{}",
                vtt(i)
            );
        }
        assert!(
            vtt(2).contains("00:00:00.250 --> 00:00:00.280\nthird\n"),
            "{}",
            vtt(2)
        );
        assert!(!vtt(3).contains("-->"), "{}", vtt(3));
        assert_eq!(rx.stats().segments_written(), 8);
    }

    #[test]
    fn subtitle_files_and_playlists_retire_with_the_ts_grace() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["subs0-1.vtt", "subs0-50.vtt", "subs3-2.vtt", "subsX-1.vtt"] {
            std::fs::write(dir.path().join(name), "WEBVTT\n").unwrap();
        }
        // next 60, window 3: grace 12, so index < 48 retires.
        prune_prior_live_segments(dir.path(), "seg", 60, 3);
        assert!(!dir.path().join("subs0-1.vtt").exists());
        assert!(!dir.path().join("subs3-2.vtt").exists());
        assert!(dir.path().join("subs0-50.vtt").exists());
        assert!(dir.path().join("subsX-1.vtt").exists(), "not ours");
        // A prior run's subtitle playlist listing an aged-out vtt retires; one
        // still listing fresh segments stays.
        std::fs::write(dir.path().join("subs0-50.vtt"), "WEBVTT\n").unwrap();
        std::fs::write(
            dir.path().join("subs0-g0.m3u8"),
            "#EXTM3U\n#EXTINF:1.0,\nsubs0-1.vtt\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("subs0-g40.m3u8"),
            "#EXTM3U\n#EXTINF:1.0,\nsubs0-50.vtt\n",
        )
        .unwrap();
        prune_prior_rendition_manifests_inactive(dir.path(), &[], 48);
        assert!(!dir.path().join("subs0-g0.m3u8").exists());
        assert!(dir.path().join("subs0-g40.m3u8").exists());
        // A vtt still listed by a manifest is never reaped.
        std::fs::write(dir.path().join("subs0-3.vtt"), "WEBVTT\n").unwrap();
        std::fs::write(
            dir.path().join("index.m3u8"),
            "#EXTM3U\n#EXTINF:1.0,\nsubs0-3.vtt\n",
        )
        .unwrap();
        prune_prior_live_segments(dir.path(), "seg", 60, 3);
        assert!(dir.path().join("subs0-3.vtt").exists());
    }

    #[test]
    fn only_generated_subtitle_segments_are_reaped() {
        let dir = tempfile::tempdir().unwrap();
        let foreign = [
            "subs+1-1.vtt",
            "subs1-+1.vtt",
            "subs-1-1.vtt",
            "subs1--1.vtt",
            "subs1-1-1.vtt",
            "subs1-.vtt",
            "subs-1.vtt",
            "subs999-1.vtt",
            "subs 1-1.vtt",
            "subs1-1 .vtt",
            "subtitles-1.vtt",
        ];
        for name in foreign.iter().chain(&["subs1-1.vtt", "subs12-0.vtt"]) {
            std::fs::write(dir.path().join(name), "WEBVTT\n").unwrap();
        }
        // next 60, window 3: grace 12, so index < 48 retires.
        prune_prior_live_segments(dir.path(), "seg", 60, 3);
        for name in foreign {
            assert!(dir.path().join(name).exists(), "{name} is not ours");
        }
        assert!(!dir.path().join("subs1-1.vtt").exists());
        assert!(!dir.path().join("subs12-0.vtt").exists());
        assert_eq!(
            super::generated_subtitle_segment_index("subs3-47.vtt"),
            Some(47)
        );
        assert_eq!(
            super::generated_subtitle_segment_index("subs+3-47.vtt"),
            None
        );
    }

    #[test]
    fn only_generated_subtitle_manifests_are_cleaned_up() {
        let dir = tempfile::tempdir().unwrap();
        // Every manifest lists a segment that does not exist, which would
        // retire a manifest this sink owns.
        let listing = "#EXTM3U\n#EXTINF:1,\nother-1.ts\n";
        let foreign = [
            "subtitles.m3u8",
            "subscriptions.m3u8",
            "subs.m3u8",
            "subs0.m3u8",
            "subs-g1.m3u8",
            "subs0-g.m3u8",
            "subs0-gx.m3u8",
            "subs0-g1-x.m3u8",
            "subs999-g1.m3u8",
            "subs+1-g1.m3u8",
        ];
        for name in foreign.iter().chain(&["subs0-g1.m3u8", "subs7-g12.m3u8"]) {
            std::fs::write(dir.path().join(name), listing).unwrap();
        }
        prune_prior_rendition_manifests_inactive(dir.path(), &[], 48);
        for name in foreign {
            assert!(dir.path().join(name).exists(), "{name} is not ours");
        }
        assert!(!dir.path().join("subs0-g1.m3u8").exists());
        assert!(!dir.path().join("subs7-g12.m3u8").exists());
        assert!(super::is_generated_subtitle_manifest("subs3-g0.m3u8"));
        assert!(!super::is_generated_subtitle_manifest("subtitles.m3u8"));
    }

    #[test]
    fn duplicate_subtitle_names_reach_the_master_distinct() {
        use crate::hls::{subtitle_feed, SubtitleRendition};
        let _counter_guard = counter_guard();
        let (packets, params) = sized_packets(160, 120, 0, 6);
        let dir = tempfile::tempdir().unwrap();
        let (_feed, rx) = subtitle_feed();
        let en = || SubtitleRendition {
            name: "en".to_owned(),
            language: Some("en".to_owned()),
        };
        let mut state = subtitle_state(dir.path(), &params, &rx, &[en(), en()]);
        let mut source = Switching {
            packets: packets.into_iter(),
            switch_pts: -1,
            params: None,
            last_pts: None,
        };
        state.drive_from_packets(&mut source).unwrap();
        let mut report = crate::hls::MediaPlaylist::new(crate::hls::SegmentType::MpegTs);
        state.finish(&mut report, 33_333_333).unwrap();
        let master = std::fs::read_to_string(dir.path().join("master.m3u8")).unwrap();
        assert!(master.contains("NAME=\"en\",LANGUAGE"), "{master}");
        assert!(master.contains("NAME=\"en (2)\",LANGUAGE"), "{master}");
    }
}
