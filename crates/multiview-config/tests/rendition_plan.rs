//! Slice-1 tests for the pure production rendition plan (broadcast output
//! contract).
//!
//! These pin the **planning** contract only: distinct video renditions are
//! de-duplicated by exact key, audio renditions are independent of video, one
//! binding per output preserves config order and ids, a whole-config count is
//! bounded, and an unknown or unencodable codec fails with an actionable error
//! instead of a silent MPEG-2 substitution.
//!
//! No encoder, muxer, socket or GPU is touched here: the plan is pure and the
//! build capability is a test double.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::rendition::{
    plan_from_config, EncodeCapabilities, RenditionAudioCodec, RenditionPlanError,
    RenditionVideoCodec,
};
use multiview_config::MultiviewConfig;
use multiview_core::time::Rational;

/// A fixed build-capability double for the pure plan.
struct TestCapabilities {
    /// The video codecs this "build" can encode.
    video: &'static [RenditionVideoCodec],
    /// The audio codecs this "build" can encode.
    audio: &'static [RenditionAudioCodec],
}

impl EncodeCapabilities for TestCapabilities {
    fn video_encoder_available(&self, codec: RenditionVideoCodec) -> bool {
        self.video.contains(&codec)
    }

    fn audio_encoder_available(&self, codec: RenditionAudioCodec) -> bool {
        self.audio.contains(&codec)
    }
}

/// Every codec the plan can name, so only the case under test fails.
const ALL_VIDEO: &[RenditionVideoCodec] = &[
    RenditionVideoCodec::H264,
    RenditionVideoCodec::H265,
    RenditionVideoCodec::Mpeg2Video,
    RenditionVideoCodec::Ffv1,
    RenditionVideoCodec::Mjpeg,
];
/// Every audio codec the plan can name.
const ALL_AUDIO: &[RenditionAudioCodec] = &[RenditionAudioCodec::Aac, RenditionAudioCodec::Opus];

/// A capability double that encodes everything.
fn all_capabilities() -> TestCapabilities {
    TestCapabilities {
        video: ALL_VIDEO,
        audio: ALL_AUDIO,
    }
}

/// Parse a full document with the given exact cadence, outputs and audio block.
fn doc(fps: &str, outputs: &str, audio: &str) -> MultiviewConfig {
    let text = format!(
        r##"schema_version = 1

[canvas]
width = 1920
height = 1080
fps = "{fps}"
pixel_format = "nv12"
background = "#101014"

[canvas.color]
profile = "sdr-bt709-limited"

[layout]
kind = "preset"
preset = "2x2"

{outputs}
{audio}
"##
    );
    MultiviewConfig::load_from_toml(&text).expect("test config document parses")
}

/// Parse a full document at the contract cadence `30000/1001`.
fn config(outputs: &str, audio: &str) -> MultiviewConfig {
    doc("30000/1001", outputs, audio)
}

const HLS_H264: &str = r#"
[[outputs]]
id = "hls_a"
kind = "hls"
path = "/srv/hls"
codec = "h264"
"#;

const SRT_HEVC: &str = r#"
[[outputs]]
id = "srt_b"
kind = "srt"
url = "srt://[::1]:9000"
codec = "hevc"
"#;

const SRT_H264: &str = r#"
[[outputs]]
id = "srt_share"
kind = "srt"
url = "srt://[::1]:9001"
codec = "h264"
"#;

const WHEP_DEFAULT: &str = r#"
[[outputs]]
id = "whep"
kind = "webrtc"
label = "Program WHEP"
"#;

const HLS_BAD_CODEC: &str = r#"
[[outputs]]
id = "hls_bad"
kind = "hls"
path = "/srv/bad"
codec = "mpeg4"
"#;

const SRT_H264_TRACKS_A: &str = r#"
[[outputs]]
id = "srt_a"
kind = "srt"
url = "srt://[::1]:9100"
codec = "h264"
[outputs.audio]
mode = "tracks"
tracks = ["prog", "trk_a"]
"#;

const RTSP_H264_TRACKS_B: &str = r#"
[[outputs]]
id = "rtsp_b"
kind = "rtsp_server"
mount = "/mv"
codec = "h264"
[outputs.audio]
mode = "tracks"
tracks = ["trk_b"]
"#;

const NDI_OUT: &str = r#"
[[outputs]]
id = "ndi_out"
kind = "ndi"
name = "Program"
"#;

const AES67_OUT: &str = r#"
[[outputs]]
id = "aes67_out"
kind = "aes67"
label = "ST2110-30"
multicast = "[ff3e::1]:5004"
"#;

const DISPLAY_OUT: &str = r#"
[[outputs]]
id = "head0"
kind = "display"
connector = "auto"
"#;

const AUDIO_TWO_TRACKS: &str = r#"
[audio]
sample_rate_hz = 48000

[[audio.routes]]
input_id = "cam_a"
channels = { kind = "stereo" }
target_track = "trk_a"
include_in_program_bus = true

[[audio.routes]]
input_id = "cam_b"
channels = { kind = "stereo" }
target_track = "trk_b"
include_in_program_bus = false
"#;

#[test]
fn mixed_codec_pair_yields_one_rendition_per_codec() {
    let cfg = config(&format!("{HLS_H264}{SRT_HEVC}"), "");
    let plan = plan_from_config(&cfg, &all_capabilities()).expect("both codecs are available");

    assert_eq!(plan.video.len(), 2, "one video key per distinct codec");
    assert!(plan
        .video
        .iter()
        .any(|key| key.codec == RenditionVideoCodec::H264));
    assert!(plan
        .video
        .iter()
        .any(|key| key.codec == RenditionVideoCodec::H265));

    assert_eq!(plan.bindings.len(), 2);
    assert_eq!(plan.bindings[0].output_id, "hls_a");
    assert_eq!(plan.bindings[0].output_index, 0);
    assert_eq!(plan.bindings[1].output_id, "srt_b");
    assert_eq!(plan.bindings[1].output_index, 1);
    assert_eq!(
        plan.bindings[0].video.as_ref().map(|key| key.codec),
        Some(RenditionVideoCodec::H264)
    );
    assert_eq!(
        plan.bindings[1].video.as_ref().map(|key| key.codec),
        Some(RenditionVideoCodec::H265)
    );
    assert_ne!(plan.bindings[0].video, plan.bindings[1].video);
}

#[test]
fn identical_third_output_adds_a_binding_not_an_encoder() {
    let cfg = config(&format!("{HLS_H264}{SRT_HEVC}{SRT_H264}"), "");
    let plan = plan_from_config(&cfg, &all_capabilities()).expect("plan");

    assert_eq!(
        plan.video_encoder_count(),
        2,
        "a third identical H.264 output must not add an encoder"
    );
    assert_eq!(plan.bindings.len(), 3);
    assert_eq!(
        plan.bindings[0].video, plan.bindings[2].video,
        "the third output binds the same H.264 rendition as the first"
    );
    assert_ne!(
        plan.bindings[0].video, plan.bindings[1].video,
        "the HEVC output is a distinct rendition"
    );
}

#[test]
fn same_video_different_audio_shares_one_video_key_with_distinct_audio_keys() {
    let cfg = config(
        &format!("{SRT_H264_TRACKS_A}{RTSP_H264_TRACKS_B}"),
        AUDIO_TWO_TRACKS,
    );
    let plan = plan_from_config(&cfg, &all_capabilities()).expect("plan");

    assert_eq!(plan.video_encoder_count(), 1, "same video key is shared");
    assert_eq!(plan.bindings[0].video, plan.bindings[1].video);

    assert_eq!(plan.audio.len(), 3, "program bus + trk_a + trk_b");
    assert_eq!(plan.audio_encoder_count(), 3);
    assert_eq!(
        plan.bindings[0].audio.len(),
        2,
        "output A carries the program bus and trk_a"
    );
    assert_eq!(plan.bindings[1].audio.len(), 1, "output B carries trk_b");
    assert_ne!(
        plan.bindings[0].audio, plan.bindings[1].audio,
        "different audio selections are different bindings"
    );
    assert!(
        plan.bindings
            .iter()
            .flat_map(|binding| binding.audio.iter())
            .all(|key| key.codec == RenditionAudioCodec::Aac),
        "the packet transports carry AAC"
    );
}

#[test]
fn reversing_the_output_order_preserves_keys_and_reorders_bindings_only() {
    let forward = config(&format!("{HLS_H264}{SRT_HEVC}{SRT_H264}"), "");
    let reverse = config(&format!("{SRT_H264}{SRT_HEVC}{HLS_H264}"), "");
    let forward_plan =
        plan_from_config(&forward, &all_capabilities()).expect("forward plan builds");
    let reverse_plan =
        plan_from_config(&reverse, &all_capabilities()).expect("reverse plan builds");

    assert_eq!(
        forward_plan.video_encoder_count(),
        reverse_plan.video_encoder_count(),
        "the encoder count is order-independent"
    );
    for key in &forward_plan.video {
        assert!(
            reverse_plan.video.contains(key),
            "every forward key exists in the reverse plan"
        );
    }
    for key in &reverse_plan.video {
        assert!(
            forward_plan.video.contains(key),
            "every reverse key exists in the forward plan"
        );
    }

    assert_eq!(reverse_plan.bindings[0].output_id, "srt_share");
    assert_eq!(reverse_plan.bindings[1].output_id, "srt_b");
    assert_eq!(reverse_plan.bindings[2].output_id, "hls_a");
    for output_id in ["hls_a", "srt_b", "srt_share"] {
        let forward_binding = forward_plan.binding(output_id).expect("forward binding");
        let reverse_binding = reverse_plan.binding(output_id).expect("reverse binding");
        assert_eq!(
            forward_binding.video, reverse_binding.video,
            "output {output_id} keeps its rendition across reordering"
        );
    }
}

#[test]
fn rational_cadence_equality_is_exact_by_value() {
    let canonical = config(HLS_H264, "");
    let unreduced = doc("60000/2002", HLS_H264, "");
    let canonical_plan =
        plan_from_config(&canonical, &all_capabilities()).expect("canonical plan builds");
    let unreduced_plan =
        plan_from_config(&unreduced, &all_capabilities()).expect("unreduced plan builds");

    assert_eq!(
        canonical_plan.video[0].cadence,
        Rational::new(30_000, 1001),
        "the key cadence is the reduced exact rational"
    );
    assert_eq!(
        canonical_plan.video[0], unreduced_plan.video[0],
        "60000/2002 and 30000/1001 are the same key"
    );
}

#[test]
fn unknown_codec_is_rejected_with_the_offending_field() {
    let cfg = config(HLS_BAD_CODEC, "");
    let err = plan_from_config(&cfg, &all_capabilities()).expect_err("mpeg4 is not a known token");

    assert!(matches!(
        err,
        RenditionPlanError::UnknownVideoCodec {
            output_index: 0,
            ..
        }
    ));
    let text = err.to_string();
    assert!(text.contains("outputs[0]"), "names the output slot: {text}");
    assert!(text.contains("hls_bad"), "names the output: {text}");
    assert!(text.contains("mpeg4"), "names the token: {text}");
    assert!(
        text.contains("no fallback codec is substituted"),
        "states the no-fallback policy: {text}"
    );
}

#[test]
fn unencodable_codec_is_rejected_without_a_fallback() {
    let capabilities = TestCapabilities {
        video: &[RenditionVideoCodec::H265],
        audio: ALL_AUDIO,
    };
    let cfg = config(HLS_H264, "");
    let err = plan_from_config(&cfg, &capabilities).expect_err("this build cannot encode H.264");

    assert!(matches!(
        err,
        RenditionPlanError::UnavailableVideoCodec { .. }
    ));
    let text = err.to_string();
    assert!(text.contains("h264"), "names the codec: {text}");
    assert!(
        text.contains("no fallback codec is substituted"),
        "explains the missing fallback: {text}"
    );
}

#[test]
fn webrtc_default_codec_admits_the_h264_program_rendition() {
    let cfg = config(WHEP_DEFAULT, "");
    let plan =
        plan_from_config(&cfg, &all_capabilities()).expect("the implicit h264 default plans");

    assert_eq!(
        plan.video.len(),
        1,
        "a WebRTC-only config admits exactly one program rendition"
    );
    assert_eq!(plan.video[0].codec, RenditionVideoCodec::H264);
    assert_eq!(
        plan.bindings[0].video.as_ref().map(|key| key.codec),
        Some(RenditionVideoCodec::H264),
        "the WHEP binding consumes the H.264 rendition, never MPEG-2"
    );
}

#[test]
fn webrtc_consumer_never_adds_a_second_h264_encoder() {
    let cfg = config(&format!("{HLS_H264}{WHEP_DEFAULT}"), "");
    let plan = plan_from_config(&cfg, &all_capabilities()).expect("plan");

    assert_eq!(plan.video_encoder_count(), 1);
    assert_eq!(plan.bindings[0].video, plan.bindings[1].video);
}

#[test]
fn raw_taps_carry_no_encoded_rendition() {
    let cfg = config(
        &format!("{NDI_OUT}{AES67_OUT}{DISPLAY_OUT}"),
        AUDIO_TWO_TRACKS,
    );
    let plan = plan_from_config(&cfg, &all_capabilities()).expect("plan");

    assert!(plan.video.is_empty(), "raw taps encode no video");
    assert!(plan.audio.is_empty(), "raw taps share no encoded audio");
    assert_eq!(plan.bindings.len(), 3);
    assert!(plan
        .bindings
        .iter()
        .all(|binding| binding.video.is_none() && binding.audio.is_empty()));
}

#[test]
fn more_than_eight_distinct_video_renditions_is_rejected() {
    let mut outputs = String::new();
    for index in 0..9u32 {
        outputs.push_str(&format!(
            "[[outputs]]\nid = \"srt_{index}\"\nkind = \"srt\"\n\
             url = \"srt://[::1]:9{index:03}\"\ncodec = \"h264\"\n\
             gpu_pin = {{ vendor = \"nvidia\", stable_id = \"GPU-{index}\" }}\n\n"
        ));
    }
    let cfg = config(&outputs, "");
    let err = plan_from_config(&cfg, &all_capabilities()).expect_err("the ceiling is enforced");

    assert!(matches!(
        err,
        RenditionPlanError::TooManyVideoRenditions {
            found: 9,
            limit: 8,
            ..
        }
    ));
}

#[test]
fn more_than_thirty_two_distinct_audio_renditions_is_rejected() {
    let outputs = r#"
[[outputs]]
id = "srt_auto"
kind = "srt"
url = "srt://[::1]:9200"
codec = "h264"
[outputs.audio]
mode = "auto"
"#;
    let mut audio = String::from("[audio]\nsample_rate_hz = 48000\n\n");
    for index in 0..33u32 {
        audio.push_str(&format!(
            "[[audio.routes]]\ninput_id = \"cam_{index}\"\n\
             channels = {{ kind = \"stereo\" }}\ntarget_track = \"trk_{index}\"\n\n"
        ));
    }
    let cfg = config(outputs, &audio);
    let err = plan_from_config(&cfg, &all_capabilities()).expect_err("the ceiling is enforced");

    assert!(matches!(
        err,
        RenditionPlanError::TooManyAudioRenditions { limit: 32, .. }
    ));
}
