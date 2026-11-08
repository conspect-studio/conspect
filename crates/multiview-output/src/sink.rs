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
    AudioEncodeTarget, AudioEncoder, DecodedVideoFrame, EncodedPacket, Muxer, ScaleSpec, Scaler,
    StreamCodecParameters, StreamKind, VideoEncodeTarget, VideoEncoder,
};

use crate::epoch::SharedEpoch;
use crate::error::{Error, Result};
use crate::hls::{
    atomic_write, media_segment_references, media_segment_uris, AudioRendition, LivePlaylist,
    MasterPlaylist, MediaPlaylist, Segment, SegmentType, VariantStream,
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
        let encoder = VideoEncoder::new(&config.target()).map_err(ff)?;
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
        let mut encoder = VideoEncoder::new(&self.config.target()).map_err(ff)?;
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
        return Err(err);
    }
    let frame_ns = rescale(1, time_base, Rational::new(1, NANOS_PER_SEC));
    let mut playlist = MediaPlaylist::new(SegmentType::MpegTs);
    state.finish(&mut playlist, frame_ns)?;
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
        let seed = VideoEncoder::new(&self.config.target()).map_err(ff)?;
        note_seed_encoder_built();
        let mut encoder = VideoEncoder::new(&self.config.target()).map_err(ff)?;
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
        let mut encoder = VideoEncoder::new(&self.config.target()).map_err(ff)?;
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
        }
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
                state.seg_index = next;
                state.live = Some(live);
                state.set_audio_renditions(
                    playlist_path,
                    *window,
                    epoch.clone(),
                    video,
                    audio,
                    track_streams,
                );
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
    retention_window: usize,
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
            recovery_failures: 0,
            publication_failed: false,
            #[cfg(test)]
            fail_playlist_publish_once: None,
            stats: EncodeStats::default(),
            live: None,
            seg_index: 0,
            meta: None,
            tracks: None,
            renditions: None,
        }
    }

    /// Build independent audio-only renditions from the same encoded packets.
    #[allow(clippy::too_many_lines)]
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
    ) {
        let default_selection = AudioSelection::default();
        let selection = self
            .tracks
            .map_or(&default_selection, |(selection, _)| selection);
        let mut master = MasterPlaylist::new();
        let mut states = Vec::new();
        let parent = playlist_path.parent().unwrap_or_else(|| Path::new("."));
        let generation = self.seg_index;
        if selection.program {
            if let Some(stream) = audio {
                let uri = format!("audio-program-g{generation}.m3u8");
                let mut live = LivePlaylist::new(parent.join(&uri), window);
                live.set_epoch_source(epoch.clone());
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
        let video_uri = format!("video-g{generation}.m3u8");
        let mut video_live = LivePlaylist::new(parent.join(&video_uri), window);
        video_live.set_epoch_source(epoch);
        video_live.set_media_sequence(self.seg_index);
        if self.seg_index != 0 {
            video_live.set_initial_discontinuity();
        }
        let max_audio_rate = states
            .iter()
            .filter_map(|state| state.stream.params.bit_rate())
            .max()
            .unwrap_or(0);
        // BANDWIDTH is mandatory for a listed variant. With no configured
        // video rate, keep the master absent rather than advertise a lie.
        let bandwidth = video
            .params
            .bit_rate()
            .filter(|rate| *rate > 0)
            .map(|rate| rate.saturating_add(max_audio_rate));
        let mut variant = VariantStream::new(video_uri, bandwidth.unwrap_or(0));
        if let Some((width, height)) = video.params.video_dimensions() {
            variant = variant.with_resolution(width, height);
        }
        if let Some(video_codec) = video.params.hls_codec() {
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
        if video.time_base.num > 0 && video.time_base.den > 0 {
            let numerator = u32::try_from(video.time_base.den).ok();
            let denominator = u32::try_from(video.time_base.num).ok();
            if let (Some(numerator), Some(denominator)) = (numerator, denominator) {
                let fps = f64::from(numerator) / f64::from(denominator);
                if (0.0..=120.0).contains(&fps) && fps > 0.0 {
                    variant = variant.with_frame_rate(fps);
                }
            }
        }
        master.push_variant(variant);
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
            master_text: bandwidth.map(|_| master.render()),
            retention_window: window,
        });
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
                let mut muxer = Muxer::create_as(&write_path, "mpegts").map_err(ff)?;
                let stream_index = muxer
                    .add_stream_from_parameters(state.stream.params, state.stream.time_base)
                    .map_err(ff)?;
                muxer.write_header().map_err(ff)?;
                state.current = Some(AudioSegment {
                    muxer,
                    stream_index,
                    path,
                    write_path,
                    packets: 0,
                });
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
            segment
                .muxer
                .write_packet(segment.stream_index, packet.to_owned_packet())
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
                    && match self.seed {
                        StreamSeed::Params(params) => params.is_decoder_ready_rap(&packet),
                        StreamSeed::Encoder(_) => false,
                    })
            {
                continue;
            }
            let pts = pts_from_packet(packet.pts(), self.stats.packets, self.time_base);
            let boundary = packet.kind() == StreamKind::Video && packet.is_keyframe();
            if let Err(error) = self.write_source_packet(packet, pts) {
                // A segment-boundary failure can be a playlist publication or
                // file-system fault, not a damaged muxer. Let the supervisor
                // handle it rather than silently hide a stale live playlist.
                if self.live.is_none() || self.publication_failed {
                    self.abandon_incomplete_segments();
                    return Err(error);
                }
                self.abandon_incomplete_segments();
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
        Ok(())
    }

    fn write_source_packet(&mut self, packet: EncodedPacket, pts: MediaTime) -> Result<()> {
        match packet.kind() {
            StreamKind::Video => {
                // Only a VIDEO keyframe rotates a segment (the GOP boundary —
                // invariant #7); the first packet is always a keyframe.
                let is_key = packet.is_keyframe();
                if is_key {
                    self.start_segment(pts)?;
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
        }
        if let Some(live) = self.live.as_mut() {
            live.set_initial_discontinuity();
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
        } else {
            // First segment of the run: build the ONE long-lived muxer +
            // register its streams. Never rebuilt for a later segment.
            let mut muxer = Muxer::create_as(&write_path, "mpegts").map_err(ff)?;
            // Register video then audio BEFORE the header — every segment
            // carries the same pinned two-stream layout (ADR-R005 §3.3,
            // AUD-4).
            let stream_index = self.seed.add_stream(&mut muxer, self.time_base)?;
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
            });
        }
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
        if let Some(renditions) = self.renditions.as_ref() {
            // Publish master last, after video and every audio playlist exist.
            // A run without valid video bitrate metadata still serves the legacy
            // index and media playlists, but never publishes a false BANDWIDTH.
            if let Some(master_text) = renditions.master_text.as_ref() {
                if renditions
                    .states
                    .iter()
                    .all(|state| state.live.playlist_path().exists())
                    && renditions.video.live.playlist_path().exists()
                {
                    atomic_write(&renditions.master, master_text.as_bytes())?;
                }
            } else if renditions.video.live.playlist_path().exists() {
                // No trustworthy BANDWIDTH: retire the previous master only once
                // this run has playable media. Never advertise obsolete tracks.
                match std::fs::remove_file(&renditions.master) {
                    Ok(()) | Err(_) => {}
                }
            }
        }
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

/// Reap prior-run TS files after the new manifests have advanced beyond them.
/// Keep three playlist windows plus three segment closes for an old client.
fn segment_grace(window: usize) -> u64 {
    u64::try_from(window)
        .unwrap_or(u64::MAX)
        .saturating_mul(3)
        .saturating_add(3)
}

fn segment_number(uri: &str) -> Option<u64> {
    uri.strip_suffix(".ts")?.rsplit('-').next()?.parse().ok()
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

    #[test]
    fn playlist_publication_fault_is_terminal_and_keeps_prior_manifest() {
        use super::{SegmentState, StreamSeed};
        use crate::hls::LivePlaylist;
        use ffmpeg::codec::packet::{Flags, Packet};
        use multiview_ffmpeg::{EncodedPacket, StreamCodecParameters, VideoEncoder};
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

    /// Serializes the counter-based tests: the seed/finalize counters are
    /// process-global statics, and both tests exercise both counters (opening
    /// segments increments finalizes; each segment build increments seed
    /// builds), so they must not interleave or they pollute each other's reads.
    static COUNTER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

        let _guard = COUNTER_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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

        let _guard = COUNTER_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
}
