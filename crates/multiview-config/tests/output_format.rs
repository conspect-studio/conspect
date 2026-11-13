//! Slice B (#342): per-output `format` and `slots` — parse, round-trip, the
//! absent case, and every refusal code with its cause.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::{
    Anchor, ConfigError, MultiviewConfig, OutputAudioMode, OutputFitMode, Scan,
    SLOT_FORMAT_UNSUPPORTED, SLOT_MEDIUM_MISMATCH,
};

const HEAD: &str = r##"
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

[[sources]]
id = "cam_a"
kind = "test"
[[sources]]
id = "cam_b"
kind = "test"

[[cells]]
id = "cell_a"
rect = { x = 0.0, y = 0.0, w = 0.5, h = 1.0 }
[cells.source]
input_id = "cam_a"

[audio]
sample_rate_hz = 48000

[[audio.routes]]
input_id = "cam_a"
channels = { kind = "stereo" }
target_track = "spa"
language = "spa"
include_in_program_bus = true
gain_db = 0.0
mute = false

[[audio.routes]]
input_id = "cam_b"
channels = { kind = "stereo" }
target_track = "deu"
language = "deu"
include_in_program_bus = true
gain_db = 0.0
mute = false
"##;

fn doc(outputs: &str) -> String {
    format!("{HEAD}\n{outputs}")
}

fn parse(outputs: &str) -> MultiviewConfig {
    MultiviewConfig::load_from_toml(&doc(outputs)).expect("doc must parse")
}

fn refusal(outputs: &str) -> String {
    match parse(outputs).validate() {
        Err(ConfigError::Validation(msg)) => msg,
        other => panic!("expected a validation refusal, got {other:?}"),
    }
}

fn assert_code(outputs: &str, code: &str, cause: &str) {
    let msg = refusal(outputs);
    assert!(
        msg.starts_with(&format!("{code}: ")),
        "expected prefix {code}, got {msg:?}"
    );
    assert!(msg.contains(cause), "expected cause {cause:?} in {msg:?}");
}

const VERTICAL: &str = r#"
[[outputs]]
kind = "hls"
id = "vert-1"
path = "/srv/hls/v"
codec = "h264"
[outputs.format]
width = 1080
height = 1920
fps = "30000/1001"
scan = "progressive"
color = "sdr-bt709-limited"
pixel_format = "nv12"
aspect = "9:16"
fit = { mode = "cover", anchor = "center" }
[[outputs.slots.video]]
codec = "h264"
bitrate_kbps = 6000
[[outputs.slots.audio]]
language = "eng"
[[outputs.slots.audio]]
track = "spa"
language = "spa"
"#;

const PLAIN: &str = r#"
[[outputs]]
kind = "hls"
id = "plain"
path = "/srv/hls/p"
codec = "h264"
"#;

#[test]
fn vertical_cover_example_parses_and_validates() {
    let cfg = parse(VERTICAL);
    cfg.validate().expect("the 9:16 cover example is valid");
    let out = &cfg.outputs[0];
    let format = out.format().expect("format");
    assert_eq!((format.width, format.height), (1080, 1920));
    assert_eq!(format.scan, Scan::Progressive);
    assert_eq!(format.fit.mode, OutputFitMode::Cover);
    assert_eq!(format.fit.anchor, Anchor::Center);
    let slots = out.slots().expect("slots");
    assert_eq!(slots.video[0].bitrate_kbps, Some(6000));
    assert_eq!(slots.audio.len(), 2);
    let effective = out.effective_audio().expect("slots imply a selection");
    assert_eq!(effective.mode, OutputAudioMode::Tracks);
    assert_eq!(effective.tracks, vec!["prog".to_owned(), "spa".to_owned()]);
}

#[test]
fn format_and_slots_round_trip_toml_and_json() {
    let cfg = parse(VERTICAL);
    let toml = cfg.to_toml().expect("to toml");
    assert_eq!(
        MultiviewConfig::load_from_toml(&toml).expect("reparse"),
        cfg
    );
    let json = cfg.to_json().expect("to json");
    assert_eq!(
        MultiviewConfig::load_from_json(&json).expect("reparse"),
        cfg
    );
}

#[test]
fn fit_defaults_to_contain_center() {
    let cfg = parse(
        r#"
[[outputs]]
kind = "srt"
url = "srt://[::1]:9000"
codec = "h264"
[outputs.format]
width = 1280
height = 720
"#,
    );
    cfg.validate().expect("valid");
    let format = cfg.outputs[0].format().expect("format");
    assert_eq!(format.fit.mode, OutputFitMode::Contain);
    assert_eq!(format.fit.anchor, Anchor::Center);
}

#[test]
fn absent_format_and_slots_are_none_and_serialize_unchanged() {
    let cfg = parse(PLAIN);
    cfg.validate().expect("valid");
    let out = &cfg.outputs[0];
    assert!(out.format().is_none());
    assert!(out.slots().is_none());
    assert!(out.effective_audio().is_none());
    let toml = cfg.to_toml().expect("to toml");
    // `pixel_format` is the canvas's; no per-output `format` table or key.
    assert!(
        !toml.contains("outputs.format")
            && !toml.lines().any(|l| l.trim_start().starts_with("format")),
        "no format key emitted: {toml}"
    );
    assert!(!toml.contains("slots"), "no slots key emitted: {toml}");
    let json = cfg.to_json().expect("to json");
    assert!(!json.contains("\"format\""));
    assert!(!json.contains("\"slots\""));
    assert_eq!(
        MultiviewConfig::load_from_toml(&toml).expect("reparse"),
        cfg
    );
}

#[test]
fn unknown_format_field_is_rejected_at_parse() {
    let text = doc(r#"
[[outputs]]
kind = "hls"
path = "/srv/hls/p"
codec = "h264"
[outputs.format]
width = 1280
height = 720
frobnicate = 1
"#);
    assert!(MultiviewConfig::load_from_toml(&text).is_err());
}

#[test]
fn hls_with_two_audio_slots_is_accepted() {
    parse(
        r#"
[[outputs]]
kind = "hls"
path = "/srv/hls/a"
codec = "h264"
[[outputs.slots.audio]]
language = "eng"
channels = 2
sample_rate = 48000
codec = "aac"
[[outputs.slots.audio]]
track = "deu"
"#,
    )
    .validate()
    .expect("HLS carries N audio slots");
}

#[test]
fn webrtc_with_two_audio_slots_is_slot_medium_mismatch() {
    assert_code(
        r#"
[[outputs]]
kind = "webrtc"
label = "web"
[[outputs.slots.audio]]
[[outputs.slots.audio]]
track = "spa"
"#,
        SLOT_MEDIUM_MISMATCH,
        "2 audio slot(s)",
    );
}

#[test]
fn hevc_slot_on_webrtc_is_slot_format_unsupported() {
    assert_code(
        r#"
[[outputs]]
kind = "webrtc"
label = "web"
[[outputs.slots.video]]
codec = "hevc"
"#,
        SLOT_FORMAT_UNSUPPORTED,
        "\"hevc\" is not carried",
    );
}

#[test]
fn two_video_slots_are_slot_medium_mismatch() {
    assert_code(
        r#"
[[outputs]]
kind = "hls"
path = "/srv/hls/a"
codec = "h264"
[[outputs.slots.video]]
[[outputs.slots.video]]
"#,
        SLOT_MEDIUM_MISMATCH,
        "2 video slot(s)",
    );
}

#[test]
fn codec_on_raw_output_is_slot_medium_mismatch() {
    assert_code(
        r#"
[[outputs]]
kind = "ndi"
name = "pgm"
[[outputs.slots.video]]
codec = "h264"
"#,
        SLOT_MEDIUM_MISMATCH,
        "raw (unencoded)",
    );
}

#[test]
fn aes67_video_slot_is_slot_medium_mismatch() {
    assert_code(
        r#"
[[outputs]]
kind = "aes67"
label = "pcm"
multicast = "[ff3e::1]:5004"
[[outputs.slots.video]]
"#,
        SLOT_MEDIUM_MISMATCH,
        "carries 0",
    );
}

#[test]
fn legacy_rtmp_codec_and_audio_limits() {
    assert_code(
        r#"
[[outputs]]
kind = "rtmp"
url = "rtmp://[::1]/live/k"
codec = "h264"
[[outputs.slots.video]]
codec = "hevc"
"#,
        SLOT_FORMAT_UNSUPPORTED,
        "not carried",
    );
    assert_code(
        r#"
[[outputs]]
kind = "rtmp"
url = "rtmp://[::1]/live/k"
codec = "h264"
[[outputs.slots.audio]]
[[outputs.slots.audio]]
track = "spa"
"#,
        SLOT_MEDIUM_MISMATCH,
        "audio slot(s)",
    );
}

fn hls_format(extra: &str) -> String {
    format!(
        r#"
[[outputs]]
kind = "hls"
path = "/srv/hls/f"
codec = "h264"
[outputs.format]
{extra}
"#
    )
}

#[test]
fn fps_different_from_canvas_is_slot_format_unsupported() {
    assert_code(
        &hls_format("width = 1280\nheight = 720\nfps = \"25/1\""),
        SLOT_FORMAT_UNSUPPORTED,
        "slice C",
    );
    // An equal rate written differently is the same rate.
    parse(&hls_format(
        "width = 1280\nheight = 720\nfps = \"60000/2002\"",
    ))
    .validate()
    .expect("equal rational rate");
}

#[test]
fn colour_different_from_canvas_is_slot_format_unsupported() {
    assert_code(
        &hls_format("width = 1280\nheight = 720\ncolor = \"hdr-pq\""),
        SLOT_FORMAT_UNSUPPORTED,
        "#310",
    );
}

#[test]
fn interlaced_scan_is_slot_format_unsupported() {
    for scan in ["tff", "bff"] {
        assert_code(
            &hls_format(&format!("width = 1920\nheight = 1080\nscan = \"{scan}\"")),
            SLOT_FORMAT_UNSUPPORTED,
            "interlaced",
        );
    }
}

#[test]
fn pixel_format_different_is_slot_format_unsupported() {
    assert_code(
        &hls_format("width = 1280\nheight = 720\npixel_format = \"p010\""),
        SLOT_FORMAT_UNSUPPORTED,
        "pixel_format",
    );
}

#[test]
fn anamorphic_aspect_is_slot_format_unsupported() {
    assert_code(
        &hls_format("width = 1440\nheight = 1080\naspect = \"16:9\""),
        SLOT_FORMAT_UNSUPPORTED,
        "square pixels",
    );
    parse(&hls_format("width = 1440\nheight = 1080\naspect = \"4:3\""))
        .validate()
        .expect("square-pixel aspect");
}

#[test]
fn bad_sizes_are_slot_format_unsupported() {
    for (w, h) in [(1281, 720), (0, 720), (8, 720), (8194, 720), (7680, 4320)] {
        assert_code(
            &hls_format(&format!("width = {w}\nheight = {h}")),
            SLOT_FORMAT_UNSUPPORTED,
            "even",
        );
    }
}

#[test]
fn non_canvas_format_on_unconverted_kind_is_slot_format_unsupported() {
    assert_code(
        r#"
[[outputs]]
kind = "webrtc"
label = "web"
[outputs.format]
width = 1280
height = 720
"#,
        SLOT_FORMAT_UNSUPPORTED,
        "not converted in slice B",
    );
    // The canvas format itself is fine on any kind.
    parse(
        r#"
[[outputs]]
kind = "webrtc"
label = "web"
[outputs.format]
width = 1920
height = 1080
"#,
    )
    .validate()
    .expect("identity format");
}

#[test]
fn audio_slot_rules() {
    // Both the selection table and slots.audio: refused.
    let msg = refusal(
        r#"
[[outputs]]
kind = "hls"
path = "/srv/hls/a"
codec = "h264"
[outputs.audio]
mode = "program"
[[outputs.slots.audio]]
"#,
    );
    assert!(
        msg.contains("both an `audio` selection and `slots.audio`"),
        "{msg}"
    );
    // Slot 0 names a track: refused.
    let msg = refusal(
        r#"
[[outputs]]
kind = "hls"
path = "/srv/hls/a"
codec = "h264"
[[outputs.slots.audio]]
track = "spa"
"#,
    );
    assert!(msg.contains("slot 0 is the program mix"), "{msg}");
    // Slot 1 names an undeclared track: refused.
    let msg = refusal(
        r#"
[[outputs]]
kind = "hls"
path = "/srv/hls/a"
codec = "h264"
[[outputs.slots.audio]]
[[outputs.slots.audio]]
track = "fra"
"#,
    );
    assert!(msg.contains("undeclared track \"fra\""), "{msg}");
    // Slot 1 names no track: refused.
    let msg = refusal(
        r#"
[[outputs]]
kind = "hls"
path = "/srv/hls/a"
codec = "h264"
[[outputs.slots.audio]]
[[outputs.slots.audio]]
"#,
    );
    assert!(msg.contains("must name a declared discrete track"), "{msg}");
    // Off-grid channels / sample rate / codec: refused.
    for (field, cause) in [
        ("channels = 6", "channels must be 2"),
        ("sample_rate = 44100", "sample_rate must be 48000"),
        ("codec = \"opus\"", "codec must be"),
    ] {
        assert_code(
            &format!(
                r#"
[[outputs]]
kind = "hls"
path = "/srv/hls/a"
codec = "h264"
[[outputs.slots.audio]]
{field}
"#
            ),
            SLOT_FORMAT_UNSUPPORTED,
            cause,
        );
    }
}

#[test]
fn a_huge_aspect_never_overflows() {
    // 18446744073709551615 * 9 is above 2^64: a u64 product would wrap.
    assert_code(
        &hls_format("width = 1920\nheight = 1080\naspect = \"18446744073709551615:9\""),
        SLOT_FORMAT_UNSUPPORTED,
        "square pixels",
    );
    // A huge but exactly proportional pair is accepted (16:9 scaled).
    parse(&hls_format(
        "width = 1920\nheight = 1080\naspect = \"18446744073709551600:10376293541461622775\"",
    ))
    .validate()
    .expect("proportional");
}

#[test]
fn video_slot_bitrate_is_bounded() {
    for kbps in [0_u64, 1_000_001, 4_294_967_295] {
        assert_code(
            &format!(
                r#"
[[outputs]]
kind = "hls"
path = "/srv/hls/a"
codec = "h264"
[[outputs.slots.video]]
bitrate_kbps = {kbps}
"#
            ),
            SLOT_FORMAT_UNSUPPORTED,
            "bitrate_kbps must be 1..=1000000",
        );
    }
    parse(
        r#"
[[outputs]]
kind = "hls"
path = "/srv/hls/a"
codec = "h264"
[[outputs.slots.video]]
bitrate_kbps = 1000000
"#,
    )
    .validate()
    .expect("the cap itself is accepted");
}
