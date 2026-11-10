//! Runtime-seam test for [`multiview_cli::rendition_plan`] (broadcast output
//! contract, slice 1).
//!
//! The adapter must answer from the build's existing `can_encode` probe and
//! must not open an encoder. The `h264` case is written to agree with
//! `multiview-ffmpeg`'s own probe, so it holds for every feature combination
//! (the default LGPL build, `ffmpeg`, `gpl-codecs`, `cuda`).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_cli::rendition_plan::plan_output_renditions;
use multiview_config::rendition::RenditionPlanError;
use multiview_config::MultiviewConfig;
use multiview_ffmpeg::{can_encode, VideoCodec};

const MPEG2_DOC: &str = r##"
schema_version = 1

[canvas]
width = 1920
height = 1080
fps = "30000/1001"
pixel_format = "nv12"
background = "#101014"

[canvas.color]
profile = "sdr-bt709-limited"

[layout]
kind = "preset"
preset = "2x2"

[[outputs]]
id = "hls_mpeg2"
kind = "hls"
path = "/srv/mpeg2"
codec = "mpeg2video"
"##;

const H264_DOC: &str = r##"
schema_version = 1

[canvas]
width = 1920
height = 1080
fps = "30000/1001"
pixel_format = "nv12"
background = "#101014"

[canvas.color]
profile = "sdr-bt709-limited"

[layout]
kind = "preset"
preset = "2x2"

[[outputs]]
id = "hls_h264"
kind = "hls"
path = "/srv/h264"
codec = "h264"
"##;

#[test]
fn mpeg2_output_plans_exactly_one_rendition() {
    let config = MultiviewConfig::load_from_toml(MPEG2_DOC).expect("the test config parses");
    let plan =
        plan_output_renditions(&config).expect("mpeg2video is LGPL-clean and always encodable");

    assert_eq!(plan.video.len(), 1);
    assert_eq!(plan.video[0].codec.as_str(), "mpeg2video");
    assert_eq!(plan.bindings.len(), 1);
    assert_eq!(plan.bindings[0].output_id, "hls_mpeg2");
}

#[test]
fn h264_plan_agrees_with_the_build_capability_probe() {
    let config = MultiviewConfig::load_from_toml(H264_DOC).expect("the test config parses");
    let h264_available = can_encode(VideoCodec::H264);

    match plan_output_renditions(&config) {
        Ok(plan) => {
            assert!(
                h264_available,
                "a plan was produced although can_encode(h264) is false"
            );
            assert_eq!(plan.video.len(), 1);
            assert_eq!(plan.video[0].codec.as_str(), "h264");
        }
        Err(error) => {
            assert!(
                !h264_available,
                "the plan failed although can_encode(h264) is true: {error}"
            );
            assert!(matches!(
                &error,
                RenditionPlanError::UnavailableVideoCodec { .. }
            ));
            assert!(
                error.to_string().contains("no fallback"),
                "the refusal must state that no fallback is substituted: {error}"
            );
        }
    }
}
