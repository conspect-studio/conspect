//! The pure production **rendition plan**: a whole-config projection of the
//! configured outputs onto shared video and audio renditions, plus their mux
//! bindings (broadcast output contract, slice 1).
//!
//! A [`VideoRenditionKey`] names one distinct encoded video rendition. Two
//! outputs with an equal key mean bit-identical coded video, so they may share
//! one encoder. An [`AudioRenditionKey`] names one distinct encoded audio
//! rendition and never contains a video key, so one AAC track is shared by an
//! H.264 and an HEVC video rendition instead of being encoded twice. A
//! [`MuxBinding`] binds one configured output to exactly one video rendition
//! (or none, for a raw tap) and its ordered audio renditions.
//!
//! This module is **pure**: it reads a parsed [`MultiviewConfig`], calls the
//! injected [`EncodeCapabilities`] seam, and returns data. It performs no I/O,
//! opens no encoder and reserves no runtime session; the caller owns that.
//! Unsupported requests are rejected with a field-addressed
//! [`RenditionPlanError`], never a silent MPEG-2 substitution.
//!
//! # What a key can express today
//!
//! Only fields the current schema actually carries take part in a video key:
//! the output's requested `codec`, its `gpu_pin`, its `orientation`, and the
//! program canvas (geometry, exact rational cadence, pixel format, colour).
//! Profile/level, bit-rate, rate control, GOP policy and encoder options have no
//! per-output config surface yet, so they cannot distinguish keys here; adding
//! them is a later slice. Audio keys carry the declared routing identity, the
//! working sample rate, the route's channel layout and a deterministic routing
//! epoch.
//!
//! # Ceilings
//!
//! The plan enforces the contract's initial per-program ceilings
//! ([`MAX_VIDEO_RENDITIONS_PER_PROGRAM`], [`MAX_AUDIO_RENDITIONS_PER_PROGRAM`])
//! over the **whole** config, not per output.

use std::fmt;

use multiview_core::time::Rational;
use thiserror::Error;

use crate::audio::{AudioChannels, AudioRouting, OutputAudio, OutputAudioMode, PROGRAM_TRACK};
use crate::placement::DevicePin;
use crate::program::ProgramId;
use crate::schema::{CanvasColor, Output, OutputOrientation};
use crate::MultiviewConfig;

/// Maximum distinct video renditions one program may plan (contract safety
/// ceiling; an implementation limit, not a measured performance claim).
pub const MAX_VIDEO_RENDITIONS_PER_PROGRAM: usize = 8;

/// Maximum distinct audio renditions one program may plan (contract safety
/// ceiling; an implementation limit, not a measured performance claim).
pub const MAX_AUDIO_RENDITIONS_PER_PROGRAM: usize = 32;

/// A logical video codec a configured output can request today.
///
/// This is the *requested* family only. Which concrete libav encoder (software
/// or NVENC) satisfies it is decided by the injected [`EncodeCapabilities`] and
/// the runtime encoder selection; the plan never substitutes a different family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RenditionVideoCodec {
    /// H.264 / AVC.
    H264,
    /// H.265 / HEVC.
    H265,
    /// MPEG-2 video (the LGPL-clean fallback codec — only when requested).
    Mpeg2Video,
    /// FFV1 (lossless).
    Ffv1,
    /// Motion-JPEG.
    Mjpeg,
}

impl RenditionVideoCodec {
    /// Normalize a configured `codec` token to a known logical codec.
    ///
    /// The token is trimmed and ASCII-lowercased first, so `"H264"` and
    /// `" h264 "` resolve. Returns [`None`] for an unknown token: the caller
    /// must reject it (the current pipeline silently defaults unknown tokens to
    /// MPEG-2, and that substitution is exactly what the contract removes).
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        match token.trim().to_ascii_lowercase().as_str() {
            "h264" | "avc" => Some(Self::H264),
            "h265" | "hevc" => Some(Self::H265),
            "mpeg2" | "mpeg2video" => Some(Self::Mpeg2Video),
            "ffv1" => Some(Self::Ffv1),
            "mjpeg" => Some(Self::Mjpeg),
            _ => None,
        }
    }

    /// The canonical lowercase token for this codec.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::H265 => "hevc",
            Self::Mpeg2Video => "mpeg2video",
            Self::Ffv1 => "ffv1",
            Self::Mjpeg => "mjpeg",
        }
    }
}

impl fmt::Display for RenditionVideoCodec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A logical audio codec one program can plan today.
///
/// There is no per-output audio-codec config field yet: the codec is derived
/// from the transport. The WebRTC kinds carry one Opus m-line (ADR-0049); the
/// packet transports carry AAC. An explicit config surface is a later slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RenditionAudioCodec {
    /// AAC (the packet transports' program/discrete-track codec).
    Aac,
    /// Opus (the single WebRTC audio m-line).
    Opus,
}

impl RenditionAudioCodec {
    /// The canonical lowercase token for this codec.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Aac => "aac",
            Self::Opus => "opus",
        }
    }
}

impl fmt::Display for RenditionAudioCodec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The transport/container family of one configured output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum OutputTransport {
    /// RTSP server.
    RtspServer,
    /// Low-latency HLS packager.
    LlHls,
    /// HLS packager.
    Hls,
    /// NDI sender (a raw canvas tap: no encode, no packet fan-out).
    Ndi,
    /// RTMP push.
    Rtmp,
    /// SRT push.
    Srt,
    /// RIST push.
    Rist,
    /// WebRTC program output served over WHEP (consumes, never encodes).
    Webrtc,
    /// WHIP push (consumes, never encodes).
    WhipPush,
    /// AES67 / ST 2110-30 raw-PCM multicast (a separate audio tap).
    Aes67,
    /// Local DRM/KMS display head (a raw scanout tap).
    Display,
    /// Cast output: HLS + CMAF fMP4 written while a cast stream is active
    /// (consumes, never encodes).
    Cast,
}

impl OutputTransport {
    /// Classify a configured output.
    #[must_use]
    pub const fn of(output: &Output) -> Self {
        match output {
            Output::RtspServer { .. } => Self::RtspServer,
            Output::LlHls { .. } => Self::LlHls,
            Output::Hls { .. } => Self::Hls,
            Output::Ndi { .. } => Self::Ndi,
            Output::Rtmp { .. } => Self::Rtmp,
            Output::Srt { .. } => Self::Srt,
            Output::Rist { .. } => Self::Rist,
            Output::Webrtc { .. } => Self::Webrtc,
            Output::WhipPush { .. } => Self::WhipPush,
            Output::Aes67 { .. } => Self::Aes67,
            Output::Display { .. } => Self::Display,
            Output::Cast { .. } => Self::Cast,
        }
    }

    /// The codec of the **encoded** audio rendition this transport carries, or
    /// [`None`] for a tap that carries no shared encoded audio.
    ///
    /// NDI, AES67 and a display head carry a raw channel-map/PCM flow, not a
    /// shared encoded rendition; AES67 and a display head are separate taps by
    /// contract, so they contribute no audio key.
    #[must_use]
    pub const fn encoded_audio_codec(self) -> Option<RenditionAudioCodec> {
        match self {
            Self::Webrtc | Self::WhipPush => Some(RenditionAudioCodec::Opus),
            Self::RtspServer
            | Self::LlHls
            | Self::Hls
            | Self::Rtmp
            | Self::Srt
            | Self::Rist
            | Self::Cast => Some(RenditionAudioCodec::Aac),
            Self::Ndi | Self::Aes67 | Self::Display => None,
        }
    }
}

/// The encoder-affecting colour description of the program canvas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenditionColorKey {
    /// The working-colour-space profile name.
    pub profile: String,
    /// Explicit primaries (only when `profile = "custom"`).
    pub primaries: Option<String>,
    /// Explicit transfer characteristic.
    pub transfer: Option<String>,
    /// Explicit matrix coefficients.
    pub matrix: Option<String>,
    /// Explicit range (limited/full).
    pub range: Option<String>,
}

impl RenditionColorKey {
    /// Project the canvas colour block onto its key form.
    fn from_canvas(color: &CanvasColor) -> Self {
        Self {
            profile: color.profile.clone(),
            primaries: color.primaries.clone(),
            transfer: color.transfer.clone(),
            matrix: color.matrix.clone(),
            range: color.range.clone(),
        }
    }
}

/// The identity of one distinct encoded **video** rendition.
///
/// Equal keys mean bit-identical coded video and may share one encoder. The key
/// deliberately excludes the transport/container and the mux time-base: those
/// belong to the [`MuxBinding`], so an HLS H.264 and an SRT H.264 with the same
/// picture share one encoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoRenditionKey {
    /// The program that owns this rendition.
    pub program_id: ProgramId,
    /// The requested logical video codec.
    pub codec: RenditionVideoCodec,
    /// Encoded width in pixels (the canvas width today).
    pub width: u32,
    /// Encoded height in pixels (the canvas height today).
    pub height: u32,
    /// Exact rational output cadence, in reduced form.
    pub cadence: Rational,
    /// Working pixel format (`nv12`, `p010`, …).
    pub pixel_format: String,
    /// The canvas colour description.
    pub color: RenditionColorKey,
    /// The output's `gpu_pin`, if authored (placement/device identity).
    pub gpu_pin: Option<DevicePin>,
    /// The output's presentation orientation, if authored.
    ///
    /// Included conservatively: a pixel-rotating orientation genuinely changes
    /// the coded picture, and telling a tag-only orientation apart from a
    /// pixel-rotating one needs the tag/pixels resolution that is a later slice.
    /// Until then two orientations never share one encoder (correct, merely
    /// conservative).
    pub orientation: Option<OutputOrientation>,
}

/// The identity of one audio stream inside a binding.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AudioTrackId {
    /// The mixed program bus.
    ProgramBus,
    /// One declared discrete track: the source it takes audio from plus the
    /// track name it is routed to. Carrying both prevents accidental reuse
    /// after a track rename onto a different source.
    Discrete {
        /// The managed source id the route takes audio from.
        input_id: String,
        /// The named discrete output track.
        track: String,
    },
}

/// The identity of one distinct encoded **audio** rendition.
///
/// Audio keys are independent of video keys: one AAC rendition is shared by an
/// H.264 and an HEVC binding. Two audio keys are distinct when their track
/// identity, codec, sample rate, channel layout or routing epoch differs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioRenditionKey {
    /// The program that owns this rendition.
    pub program_id: ProgramId,
    /// The stream identity inside the program.
    pub track: AudioTrackId,
    /// The codec this transport carries (derived; see [`RenditionAudioCodec`]).
    pub codec: RenditionAudioCodec,
    /// The working sample rate in Hz (exact integer).
    pub sample_rate_hz: u32,
    /// The declared channel layout for a discrete track; [`None`] for the
    /// program bus, whose mix layout is resolved by the runtime.
    pub channels: Option<AudioChannels>,
    /// A deterministic fingerprint of the `[audio]` routing block. Any routing
    /// change bumps it, so a rebuilt mixer cannot be mistaken for the previous
    /// sample clock.
    pub routing_epoch: u64,
}

/// One configured output's binding: which video rendition it carries and its
/// ordered audio renditions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MuxBinding {
    /// The output's stable id (explicit `id`, else the derived label).
    pub output_id: String,
    /// The output's position in `config.outputs` (stable ordering).
    pub output_index: usize,
    /// The output's transport/container family.
    pub transport: OutputTransport,
    /// The video rendition this output carries; [`None`] for a raw-frame or
    /// audio-only tap (NDI, a display head, AES67).
    pub video: Option<VideoRenditionKey>,
    /// The audio renditions in stable stream-index order (program bus first,
    /// then declared discrete tracks).
    pub audio: Vec<AudioRenditionKey>,
}

/// The whole-config plan: the distinct renditions plus every output binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenditionPlan {
    /// The program this plan was built for.
    pub program_id: ProgramId,
    /// Distinct video renditions, in first-appearance order.
    pub video: Vec<VideoRenditionKey>,
    /// Distinct audio renditions, in first-appearance order.
    pub audio: Vec<AudioRenditionKey>,
    /// One binding per configured output, in config order.
    pub bindings: Vec<MuxBinding>,
}

impl RenditionPlan {
    /// The number of video encoders this plan admits (one per distinct video
    /// rendition). A third output with an identical key does not raise it.
    ///
    /// Not `const`: `Vec::len` in a `const fn` needs Rust 1.87 and this
    /// workspace's MSRV is 1.85.
    #[must_use]
    pub fn video_encoder_count(&self) -> usize {
        self.video.len()
    }

    /// The number of audio encoders this plan admits (one per distinct audio
    /// rendition).
    ///
    /// Not `const`: see [`Self::video_encoder_count`].
    #[must_use]
    pub fn audio_encoder_count(&self) -> usize {
        self.audio.len()
    }

    /// The binding for a configured output id, if present.
    #[must_use]
    pub fn binding(&self, output_id: &str) -> Option<&MuxBinding> {
        self.bindings
            .iter()
            .find(|binding| binding.output_id.as_str() == output_id)
    }
}

/// The build-capability seam the plan consults before admitting a rendition.
///
/// The plan is pure, so availability is injected. A production adapter answers
/// from the existing `multiview-ffmpeg` capability probes
/// (`can_encode`/`can_encode_audio`), which are compile-feature data and open no
/// encoder; a test double answers from a fixed list. A `false` answer rejects
/// the requested codec — the plan never falls back to another codec.
pub trait EncodeCapabilities {
    /// Whether this build can encode `codec`.
    fn video_encoder_available(&self, codec: RenditionVideoCodec) -> bool;

    /// Whether this build can encode `codec`.
    fn audio_encoder_available(&self, codec: RenditionAudioCodec) -> bool;
}

/// A rendition-plan rejection. Every variant names the offending field so the
/// control plane can map it to a typed validation response.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum RenditionPlanError {
    /// A configured output names a codec token the schema does not define.
    #[error(
        "outputs[{output_index}] ({output_id}): unknown video codec {token:?}; known tokens are \
         h264/avc, h265/hevc, mpeg2/mpeg2video, ffv1 and mjpeg (no fallback codec is substituted)"
    )]
    UnknownVideoCodec {
        /// The output's position in `config.outputs`.
        output_index: usize,
        /// The output's stable id.
        output_id: String,
        /// The token as authored.
        token: String,
    },
    /// A configured output names a codec this build cannot encode.
    #[error(
        "outputs[{output_index}] ({output_id}): video codec {codec} is not encodable by this build; \
         no fallback codec is substituted"
    )]
    UnavailableVideoCodec {
        /// The output's position in `config.outputs`.
        output_index: usize,
        /// The output's stable id.
        output_id: String,
        /// The requested logical codec.
        codec: RenditionVideoCodec,
    },
    /// A configured output's transport carries an audio codec this build cannot
    /// encode.
    #[error(
        "outputs[{output_index}] ({output_id}): audio codec {codec} is not encodable by this build; \
         no fallback codec is substituted"
    )]
    UnavailableAudioCodec {
        /// The output's position in `config.outputs`.
        output_index: usize,
        /// The output's stable id.
        output_id: String,
        /// The codec the transport carries.
        codec: RenditionAudioCodec,
    },
    /// An output selects a discrete track the `[audio]` routing never declared.
    #[error(
        "outputs[{output_index}] ({output_id}): audio track {track:?} is not declared by the \
         [audio] routing block"
    )]
    UndeclaredAudioTrack {
        /// The output's position in `config.outputs`.
        output_index: usize,
        /// The output's stable id.
        output_id: String,
        /// The selected track name.
        track: String,
    },
    /// The whole config plans more distinct video renditions than the ceiling.
    #[error(
        "program {program_id}: {found} distinct video renditions exceed the per-program ceiling \
         of {limit}"
    )]
    TooManyVideoRenditions {
        /// The program the plan was built for.
        program_id: ProgramId,
        /// How many distinct video keys the config requires.
        found: usize,
        /// The enforced ceiling.
        limit: usize,
    },
    /// The whole config plans more distinct audio renditions than the ceiling.
    #[error(
        "program {program_id}: {found} distinct audio renditions exceed the per-program ceiling \
         of {limit}"
    )]
    TooManyAudioRenditions {
        /// The program the plan was built for.
        program_id: ProgramId,
        /// How many distinct audio keys the config requires.
        found: usize,
        /// The enforced ceiling.
        limit: usize,
    },
    /// The canvas cadence is not a usable rational.
    #[error("canvas fps {num}/{den} is not a valid rational cadence")]
    InvalidCadence {
        /// The cadence numerator as parsed.
        num: i64,
        /// The cadence denominator as parsed.
        den: i64,
    },
}

/// Build the whole-config rendition plan for `config`.
///
/// Every configured output is visited **in config order** and contributes one
/// [`MuxBinding`]; distinct renditions are de-duplicated by key, so a second
/// output with an equal key adds a binding and no encoder. Capability is
/// consulted for every requested video codec and every transport-implied audio
/// codec; an unavailable codec fails the whole plan.
///
/// # Errors
///
/// Returns [`RenditionPlanError::UnknownVideoCodec`] for an unknown token,
/// [`RenditionPlanError::UnavailableVideoCodec`] /
/// [`RenditionPlanError::UnavailableAudioCodec`] when the injected capability
/// refuses a codec, [`RenditionPlanError::UndeclaredAudioTrack`] for an
/// undeclared selection, [`RenditionPlanError::InvalidCadence`] for a degenerate
/// canvas cadence, and the ceiling variants when the whole config exceeds a
/// per-program rendition limit.
pub fn plan_from_config(
    config: &MultiviewConfig,
    capabilities: &dyn EncodeCapabilities,
) -> Result<RenditionPlan, RenditionPlanError> {
    let program_id = ProgramId::main();
    let parsed_cadence = config.canvas.fps.rational();
    if !parsed_cadence.is_valid() {
        return Err(RenditionPlanError::InvalidCadence {
            num: parsed_cadence.num,
            den: parsed_cadence.den,
        });
    }
    let cadence = parsed_cadence.reduce();
    let color = RenditionColorKey::from_canvas(&config.canvas.color);
    let audio_context = config.audio.as_ref().map(AudioPlanContext::new);

    let mut video: Vec<VideoRenditionKey> = Vec::new();
    let mut audio: Vec<AudioRenditionKey> = Vec::new();
    let mut bindings: Vec<MuxBinding> = Vec::with_capacity(config.outputs.len());

    for (output_index, output) in config.outputs.iter().enumerate() {
        let output_id = output.id();
        let transport = OutputTransport::of(output);

        let video_key = video_key_for_output(
            output,
            output_index,
            &program_id,
            config,
            cadence,
            &color,
            capabilities,
        )?;
        if let Some(key) = &video_key {
            push_unique(&mut video, key.clone());
        }

        let audio_keys = audio_keys_for_output(
            output,
            transport,
            &program_id,
            audio_context.as_ref(),
            capabilities,
            output_index,
            &output_id,
        )?;
        for key in &audio_keys {
            push_unique(&mut audio, key.clone());
        }

        bindings.push(MuxBinding {
            output_id,
            output_index,
            transport,
            video: video_key,
            audio: audio_keys,
        });
    }

    if video.len() > MAX_VIDEO_RENDITIONS_PER_PROGRAM {
        return Err(RenditionPlanError::TooManyVideoRenditions {
            program_id,
            found: video.len(),
            limit: MAX_VIDEO_RENDITIONS_PER_PROGRAM,
        });
    }
    if audio.len() > MAX_AUDIO_RENDITIONS_PER_PROGRAM {
        return Err(RenditionPlanError::TooManyAudioRenditions {
            program_id,
            found: audio.len(),
            limit: MAX_AUDIO_RENDITIONS_PER_PROGRAM,
        });
    }

    Ok(RenditionPlan {
        program_id,
        video,
        audio,
        bindings,
    })
}

/// Push `candidate` unless an equal value is already present, preserving
/// first-appearance order.
fn push_unique<T: PartialEq>(items: &mut Vec<T>, candidate: T) {
    if !items.contains(&candidate) {
        items.push(candidate);
    }
}

/// Build one output's video rendition key, or [`None`] for a raw tap.
///
/// The WebRTC kinds are included: they consume the program rendition they name
/// (default `h264`) and therefore admit exactly one video key, which other
/// outputs may share. They never encode a second time.
fn video_key_for_output(
    output: &Output,
    output_index: usize,
    program_id: &ProgramId,
    config: &MultiviewConfig,
    cadence: Rational,
    color: &RenditionColorKey,
    capabilities: &dyn EncodeCapabilities,
) -> Result<Option<VideoRenditionKey>, RenditionPlanError> {
    let token = match output {
        Output::RtspServer { codec, .. }
        | Output::LlHls { codec, .. }
        | Output::Hls { codec, .. }
        | Output::Rtmp { codec, .. }
        | Output::Srt { codec, .. }
        | Output::Rist { codec, .. }
        | Output::Webrtc { codec, .. }
        | Output::WhipPush { codec, .. }
        | Output::Cast { codec, .. } => codec.as_str(),
        Output::Ndi { .. } | Output::Aes67 { .. } | Output::Display { .. } => return Ok(None),
    };
    let codec =
        RenditionVideoCodec::parse(token).ok_or_else(|| RenditionPlanError::UnknownVideoCodec {
            output_index,
            output_id: output.id(),
            token: token.to_owned(),
        })?;
    if !capabilities.video_encoder_available(codec) {
        return Err(RenditionPlanError::UnavailableVideoCodec {
            output_index,
            output_id: output.id(),
            codec,
        });
    }
    Ok(Some(VideoRenditionKey {
        program_id: program_id.clone(),
        codec,
        width: config.canvas.width,
        height: config.canvas.height,
        cadence,
        pixel_format: config.canvas.pixel_format.clone(),
        color: color.clone(),
        gpu_pin: output.gpu_pin().cloned(),
        orientation: output.orientation().copied(),
    }))
}

/// One declared discrete track, in route order.
struct DiscreteTrack {
    /// The route's `target_track`.
    name: String,
    /// The route's `input_id`.
    input_id: String,
    /// The route's declared channel layout.
    channels: AudioChannels,
}

/// The audio-side context derived from the document's `[audio]` block.
struct AudioPlanContext {
    /// The working sample rate in Hz.
    sample_rate_hz: u32,
    /// The deterministic routing fingerprint.
    routing_epoch: u64,
    /// The declared discrete tracks, in route order, de-duplicated by name.
    discrete: Vec<DiscreteTrack>,
}

impl AudioPlanContext {
    /// Derive the context from a routing block.
    fn new(routing: &AudioRouting) -> Self {
        let mut discrete: Vec<DiscreteTrack> = Vec::new();
        for route in &routing.routes {
            let Some(track) = route.target_track.as_deref() else {
                continue;
            };
            if discrete.iter().any(|known| known.name.as_str() == track) {
                continue;
            }
            discrete.push(DiscreteTrack {
                name: track.to_owned(),
                input_id: route.input_id.clone(),
                channels: route.channels,
            });
        }
        Self {
            sample_rate_hz: routing.sample_rate_hz,
            routing_epoch: routing_fingerprint(routing),
            discrete,
        }
    }

    /// The declared channel layout for a discrete track name, if declared.
    fn channels_for(&self, track: &str) -> Option<AudioChannels> {
        self.discrete
            .iter()
            .find(|known| known.name.as_str() == track)
            .map(|known| known.channels)
    }

    /// Resolve one output's audio selection into its ordered stream identities.
    ///
    /// `mode = "program"` carries the mixed bus only, `mode = "auto"` carries
    /// the bus plus every declared discrete track, and `mode = "tracks"` carries
    /// the named subset in declared route order (program bus first when named).
    fn resolve_selection(
        &self,
        selection: &OutputAudio,
        output_index: usize,
        output_id: &str,
    ) -> Result<Vec<AudioTrackId>, RenditionPlanError> {
        match selection.mode {
            OutputAudioMode::Program => Ok(vec![AudioTrackId::ProgramBus]),
            OutputAudioMode::Auto => {
                let mut tracks = Vec::with_capacity(self.discrete.len() + 1);
                tracks.push(AudioTrackId::ProgramBus);
                tracks.extend(self.discrete.iter().map(|known| AudioTrackId::Discrete {
                    input_id: known.input_id.clone(),
                    track: known.name.clone(),
                }));
                Ok(tracks)
            }
            OutputAudioMode::Tracks => {
                for name in &selection.tracks {
                    if name.as_str() == PROGRAM_TRACK {
                        continue;
                    }
                    if self.channels_for(name).is_none() {
                        return Err(RenditionPlanError::UndeclaredAudioTrack {
                            output_index,
                            output_id: output_id.to_owned(),
                            track: name.clone(),
                        });
                    }
                }
                let mut tracks = Vec::new();
                if selection
                    .tracks
                    .iter()
                    .any(|name| name.as_str() == PROGRAM_TRACK)
                {
                    tracks.push(AudioTrackId::ProgramBus);
                }
                for known in &self.discrete {
                    if selection
                        .tracks
                        .iter()
                        .any(|name| name.as_str() == known.name.as_str())
                    {
                        tracks.push(AudioTrackId::Discrete {
                            input_id: known.input_id.clone(),
                            track: known.name.clone(),
                        });
                    }
                }
                Ok(tracks)
            }
        }
    }
}

/// Build `output`'s ordered audio rendition keys.
///
/// Returns an empty vector when the document declares no `[audio]` routing
/// (the engine carries no managed audio then) or when the transport carries no
/// shared encoded audio (NDI, AES67, a display head — separate taps).
fn audio_keys_for_output(
    output: &Output,
    transport: OutputTransport,
    program_id: &ProgramId,
    context: Option<&AudioPlanContext>,
    capabilities: &dyn EncodeCapabilities,
    output_index: usize,
    output_id: &str,
) -> Result<Vec<AudioRenditionKey>, RenditionPlanError> {
    let Some(codec) = transport.encoded_audio_codec() else {
        return Ok(Vec::new());
    };
    let Some(context) = context else {
        return Ok(Vec::new());
    };
    if !capabilities.audio_encoder_available(codec) {
        return Err(RenditionPlanError::UnavailableAudioCodec {
            output_index,
            output_id: output_id.to_owned(),
            codec,
        });
    }
    let tracks = match output.audio() {
        Some(selection) => context.resolve_selection(selection, output_index, output_id)?,
        None => vec![AudioTrackId::ProgramBus],
    };
    let mut keys = Vec::with_capacity(tracks.len());
    for track in tracks {
        let channels = match &track {
            AudioTrackId::ProgramBus => None,
            AudioTrackId::Discrete { track, .. } => context.channels_for(track),
        };
        keys.push(AudioRenditionKey {
            program_id: program_id.clone(),
            track,
            codec,
            sample_rate_hz: context.sample_rate_hz,
            channels,
            routing_epoch: context.routing_epoch,
        });
    }
    Ok(keys)
}

/// A deterministic 64-bit fingerprint of the `[audio]` routing block (FNV-1a,
/// pure integer arithmetic — no clock, no randomness, no hasher version).
fn routing_fingerprint(routing: &AudioRouting) -> u64 {
    let mut hash = Fnv::new();
    hash.write_u32(routing.sample_rate_hz);
    for route in &routing.routes {
        hash.write_str(&route.input_id);
        hash.write_str(route.target_track.as_deref().unwrap_or(""));
        hash.write_u32(route.channels.channel_count());
        hash.write_str(route.language.as_deref().unwrap_or(""));
        hash.write_str(route.title.as_deref().unwrap_or(""));
        hash.write_byte(u8::from(route.include_in_program_bus));
        hash.write_byte(u8::from(route.mute));
        hash.write_u32(route.gain_db.to_bits());
    }
    hash.finish()
}

/// A minimal FNV-1a 64-bit hasher (deterministic across processes and builds).
struct Fnv(u64);

impl Fnv {
    /// The FNV-1a 64-bit offset basis.
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    /// The FNV-1a 64-bit prime.
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    /// A fresh hasher.
    const fn new() -> Self {
        Self(Self::OFFSET_BASIS)
    }

    /// Mix one byte into the hash.
    ///
    /// Not `const`: `u64::from(u8)` is not yet stable in a `const fn`, and the
    /// workspace denies `as_conversions` (R3 config module — no lint bypass).
    fn write_byte(&mut self, byte: u8) {
        self.0 ^= u64::from(byte);
        self.0 = self.0.wrapping_mul(Self::PRIME);
    }

    /// Mix a byte slice into the hash.
    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.write_byte(*byte);
        }
    }

    /// Mix a `u32` (little-endian) into the hash.
    fn write_u32(&mut self, value: u32) {
        self.write(&value.to_le_bytes());
    }

    /// Mix a string and a separator byte (so `("ab", "c")` and `("a", "bc")`
    /// differ).
    fn write_str(&mut self, value: &str) {
        self.write(value.as_bytes());
        self.write_byte(0xff);
    }

    /// The final hash.
    const fn finish(self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::RenditionVideoCodec;

    #[test]
    fn codec_tokens_normalize_and_reject_unknown() {
        assert_eq!(
            RenditionVideoCodec::parse(" H264 "),
            Some(RenditionVideoCodec::H264)
        );
        assert_eq!(
            RenditionVideoCodec::parse("avc"),
            Some(RenditionVideoCodec::H264)
        );
        assert_eq!(
            RenditionVideoCodec::parse("HEVC"),
            Some(RenditionVideoCodec::H265)
        );
        assert_eq!(
            RenditionVideoCodec::parse("h265"),
            Some(RenditionVideoCodec::H265)
        );
        assert_eq!(
            RenditionVideoCodec::parse("mpeg2video"),
            Some(RenditionVideoCodec::Mpeg2Video)
        );
        assert_eq!(RenditionVideoCodec::parse("mpeg4"), None);
        assert_eq!(RenditionVideoCodec::parse(""), None);
    }

    #[test]
    fn canonical_tokens_round_trip() {
        for codec in [
            RenditionVideoCodec::H264,
            RenditionVideoCodec::H265,
            RenditionVideoCodec::Mpeg2Video,
            RenditionVideoCodec::Ffv1,
            RenditionVideoCodec::Mjpeg,
        ] {
            assert_eq!(RenditionVideoCodec::parse(codec.as_str()), Some(codec));
        }
    }
}
