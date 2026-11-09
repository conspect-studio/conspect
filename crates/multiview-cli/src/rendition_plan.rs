//! Runtime-facing seam that projects the configured outputs onto the pure
//! rendition plan (broadcast output contract, slice 1).
//!
//! The pure plan lives in [`multiview_config::rendition`]; this module is the
//! only place the plan meets the concrete build. It implements
//! [`EncodeCapabilities`] on top of `multiview-ffmpeg`'s existing
//! compile-feature probes ([`multiview_ffmpeg::can_encode`] /
//! [`multiview_ffmpeg::can_encode_audio`]) and exposes
//! [`plan_output_renditions`] for the runtime builder.
//!
//! **No encoder is opened and no runtime session is reserved here.** The probe
//! is pure compile-feature data (the ordered candidate-encoder list), which is
//! exactly the capability check the contract requires before acceptance. Wiring
//! the plan into `Pipeline::build` (and probing the actually linked `FFmpeg`
//! encoders) is the next serial unit; this module deliberately does not touch
//! the running pipeline.

use multiview_config::rendition::{
    plan_from_config, EncodeCapabilities, RenditionAudioCodec, RenditionPlan, RenditionPlanError,
    RenditionVideoCodec,
};
use multiview_config::MultiviewConfig;
use multiview_ffmpeg::{AudioCodec, VideoCodec};

/// The real build-capability adapter for the pure plan.
///
/// Answers from the compiled feature set via `multiview-ffmpeg`'s existing
/// probes: this is a build-capability check only and opens nothing. A linked
/// `FFmpeg` may still lack an external encoder (for example `libvpx`); the
/// linked-availability refinement belongs to the wiring unit.
#[derive(Debug, Clone, Copy, Default)]
pub struct FfmpegEncodeCapabilities;

impl EncodeCapabilities for FfmpegEncodeCapabilities {
    fn video_encoder_available(&self, codec: RenditionVideoCodec) -> bool {
        video_codec(codec).is_some_and(multiview_ffmpeg::can_encode)
    }

    fn audio_encoder_available(&self, codec: RenditionAudioCodec) -> bool {
        audio_codec(codec).is_some_and(multiview_ffmpeg::can_encode_audio)
    }
}

/// Build the whole-config rendition plan for `config` with the real build
/// capabilities.
///
/// # Errors
///
/// Returns [`RenditionPlanError`] when a configured output names an unknown or
/// unencodable codec (never a substituted fallback), selects an undeclared
/// audio track, or the whole config exceeds a per-program rendition ceiling.
pub fn plan_output_renditions(
    config: &MultiviewConfig,
) -> Result<RenditionPlan, RenditionPlanError> {
    plan_from_config(config, &FfmpegEncodeCapabilities)
}

/// Map a plan codec to its `multiview-ffmpeg` logical codec, if one exists.
fn video_codec(codec: RenditionVideoCodec) -> Option<VideoCodec> {
    Some(match codec {
        RenditionVideoCodec::H264 => VideoCodec::H264,
        RenditionVideoCodec::H265 => VideoCodec::H265,
        RenditionVideoCodec::Mpeg2Video => VideoCodec::Mpeg2Video,
        RenditionVideoCodec::Ffv1 => VideoCodec::Ffv1,
        RenditionVideoCodec::Mjpeg => VideoCodec::Mjpeg,
        // `RenditionVideoCodec` is `#[non_exhaustive]`: a future variant the
        // adapter does not know is reported unavailable rather than guessed.
        _ => return None,
    })
}

/// Map a plan audio codec to its `multiview-ffmpeg` logical codec, if one
/// exists.
fn audio_codec(codec: RenditionAudioCodec) -> Option<AudioCodec> {
    Some(match codec {
        RenditionAudioCodec::Aac => AudioCodec::Aac,
        RenditionAudioCodec::Opus => AudioCodec::Opus,
        _ => return None,
    })
}
