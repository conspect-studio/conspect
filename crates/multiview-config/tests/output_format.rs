//! Slice B (#342): per-output `format` and `slots` — parse, round-trip, the
//! absent case, and every refusal code with its cause.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::{
    Anchor, ConfigError, MultiviewConfig, OutputAudioMode, OutputFitMode, Scan, SubtitleFormat,
    MAX_SUBTITLE_SLOTS, SLOT_FORMAT_UNSUPPORTED, SLOT_MEDIUM_MISMATCH,
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

/// Slice C (#343) intentionally replaces slice B's blanket "slice C"
/// refusal: a converting kind at a valid rate is accepted (it runs on its own
/// output clock) while a Program-rate output remains.
#[test]
fn fps_different_from_canvas_is_accepted_on_a_converting_kind() {
    let doc = format!(
        "{PLAIN}{}",
        hls_format("width = 1280\nheight = 720\nfps = \"25/1\"")
    );
    parse(&doc)
        .validate()
        .expect("a 25 fps output beside Program");
    // An equal rate written differently is the same rate (no clock at all).
    parse(&hls_format(
        "width = 1280\nheight = 720\nfps = \"60000/2002\"",
    ))
    .validate()
    .expect("equal rational rate");
    // Every converting kind may carry its own rate.
    for (kind, extra) in [
        ("srt", "url = \"srt://[::1]:9000\""),
        ("rist", "url = \"rist://[::1]:9001\""),
        ("rtmp", "url = \"rtmp://[::1]/live/k\""),
        ("ll_hls", "path = \"/srv/ll\""),
    ] {
        let doc = format!(
            "{PLAIN}\n[[outputs]]\nkind = \"{kind}\"\nid = \"r-{kind}\"\n{extra}\ncodec = \"h264\"\n\
             [outputs.format]\nwidth = 1920\nheight = 1080\nfps = \"50/1\"\n"
        );
        match MultiviewConfig::load_from_toml(&format!("{HEAD}\n{doc}")) {
            Ok(cfg) => cfg.validate().unwrap_or_else(|e| panic!("{kind}: {e}")),
            // A kind whose minimal fields differ here is not what this test
            // pins; parse failures are reported, never silently passed.
            Err(e) => panic!("{kind} did not parse: {e}"),
        }
    }
}

#[test]
fn an_invalid_output_rate_is_slot_format_unsupported() {
    for fps in ["24000/1000", "121/1", "0/1", "25/2", "240000/1001", "3/1"] {
        assert_code(
            &format!(
                "{PLAIN}{}",
                hls_format(&format!("width = 1280\nheight = 720\nfps = \"{fps}\""))
            ),
            SLOT_FORMAT_UNSUPPORTED,
            "not an output rate",
        );
    }
}

#[test]
fn a_rate_on_a_kind_without_an_output_clock_is_slot_format_unsupported() {
    assert_code(
        &format!(
            "{PLAIN}{}",
            r#"
[[outputs]]
kind = "webrtc"
label = "web"
[outputs.format]
width = 1920
height = 1080
fps = "25/1"
"#
        ),
        SLOT_FORMAT_UNSUPPORTED,
        "no per-output clock",
    );
}

#[test]
fn more_distinct_rates_than_max_output_clocks_is_capacity() {
    let rate = |id: &str, fps: &str| {
        format!(
            "\n[[outputs]]\nkind = \"hls\"\nid = \"{id}\"\npath = \"/srv/{id}\"\ncodec = \"h264\"\n\
             [outputs.format]\nwidth = 1280\nheight = 720\nfps = \"{fps}\"\n"
        )
    };
    // Two distinct rates (and a repeat) fit the default of 2.
    let two = format!(
        "{PLAIN}{}{}{}",
        rate("a", "25/1"),
        rate("b", "50/1"),
        rate("c", "25/1")
    );
    parse(&two).validate().expect("two clocks fit the default");
    let three = format!("{two}{}", rate("d", "24000/1001"));
    let msg = refusal(&three);
    assert!(msg.starts_with("capacity: "), "{msg}");
    assert!(msg.contains("max_output_clocks allows 2"), "{msg}");
    // Raising the bound admits it; above the hard max is refused.
    parse(&format!("{three}\n[routing]\nmax_output_clocks = 3\n"))
        .validate()
        .expect("raised bound");
    let msg = refusal(&format!("{three}\n[routing]\nmax_output_clocks = 9\n"));
    assert!(msg.contains("max_output_clocks must be at most 8"), "{msg}");
}

#[test]
fn every_output_at_another_rate_is_slot_format_unsupported() {
    assert_code(
        &hls_format("width = 1280\nheight = 720\nfps = \"25/1\""),
        SLOT_FORMAT_UNSUPPORTED,
        "keep at least one output at the Program's rate",
    );
}

#[test]
fn a_non_program_feed_onto_a_rate_output_is_slot_format_unsupported() {
    let doc = format!(
        "{PLAIN}\n[[outputs]]\nkind = \"hls\"\nid = \"r25\"\npath = \"/srv/r\"\ncodec = \"h264\"\n\
         [outputs.format]\nwidth = 1280\nheight = 720\nfps = \"25/1\"\n"
    );
    parse(&format!(
        "{doc}\n[[routing.output]]\noutput = \"r25\"\nvideo = {{ bus = \"program\" }}\n"
    ))
    .validate()
    .expect("Program onto a rate output");
    assert_code(
        &format!("{doc}\n[[routing.output]]\noutput = \"r25\"\nvideo = {{ source = \"cam_a\" }}\n"),
        SLOT_FORMAT_UNSUPPORTED,
        "its feed must be Program",
    );
}

/// #343 review M2: a rate output carries the Program audio; a persisted
/// audio feed other than `follow`/`program` (silence, a source's audio, a
/// track selection) would be silently ignored, so it is refused.
#[test]
fn a_non_program_audio_feed_onto_a_rate_output_is_slot_format_unsupported() {
    let doc = format!(
        "{PLAIN}\n[[outputs]]\nkind = \"hls\"\nid = \"r25\"\npath = \"/srv/r\"\ncodec = \"h264\"\n\
         [outputs.format]\nwidth = 1280\nheight = 720\nfps = \"25/1\"\n"
    );
    for ok in ["\"follow\"", "\"program\""] {
        parse(&format!(
            "{doc}\n[[routing.output]]\noutput = \"r25\"\naudio = {ok}\n"
        ))
        .validate()
        .unwrap_or_else(|e| panic!("audio = {ok} onto a rate output: {e}"));
    }
    for feed in [
        "\"none\"",
        "{ source = \"cam_b\" }",
        "{ tracks = [\"program\", \"spa\"] }",
    ] {
        assert_code(
            &format!("{doc}\n[[routing.output]]\noutput = \"r25\"\naudio = {feed}\n"),
            SLOT_FORMAT_UNSUPPORTED,
            "its audio feed must be Program",
        );
    }
    // The same feeds stay valid on a Program-rate output.
    parse(&format!(
        "{doc}\n[[routing.output]]\noutput = \"plain\"\naudio = \"none\"\n"
    ))
    .validate()
    .expect("silence on a Program-rate output");
}

/// #343 review A1: every rate output opens its own encoder, so identical
/// rate outputs count one encoder each toward the per-program ceiling.
#[test]
fn identical_rate_outputs_count_one_encoder_each() {
    let rate = |id: usize| {
        format!(
            "\n[[outputs]]\nkind = \"hls\"\nid = \"r{id}\"\npath = \"/srv/r{id}\"\ncodec = \"h264\"\n\
             [outputs.format]\nwidth = 1280\nheight = 720\nfps = \"25/1\"\n"
        )
    };
    let with = |n: usize| format!("{PLAIN}{}", (0..n).map(rate).collect::<String>());
    let max = multiview_config::rendition::MAX_VIDEO_RENDITIONS_PER_PROGRAM;
    // Program's encoder + (max - 1) rate encoders fit.
    let fits = parse(&with(max - 1));
    assert_eq!(
        multiview_config::rendition::distinct_base_video_keys(&fits),
        max
    );
    fits.validate().expect("at the ceiling");
    assert_code(
        &with(max),
        SLOT_FORMAT_UNSUPPORTED,
        "distinct video encoders",
    );
    assert_code(
        &with(100),
        SLOT_FORMAT_UNSUPPORTED,
        "distinct video encoders",
    );
}

/// #343 review A2: a rate output whose top-level codec is an encoder alias
/// (`h264_nvenc`, `libx264`: no rendition key) still counts as an encoder.
#[test]
fn rate_outputs_with_an_encoder_alias_codec_count_too() {
    for codec in ["h264_nvenc", "libx264"] {
        let rate = |id: usize| {
            format!(
                "\n[[outputs]]\nkind = \"hls\"\nid = \"r{id}\"\npath = \"/srv/r{id}\"\ncodec = \"{codec}\"\n\
                 [outputs.format]\nwidth = 1280\nheight = 720\nfps = \"25/1\"\n"
            )
        };
        let doc = format!("{PLAIN}{}", (0..100).map(rate).collect::<String>());
        assert_eq!(
            multiview_config::rendition::distinct_base_video_keys(&parse(&doc)),
            101,
            "{codec}"
        );
        assert_code(&doc, SLOT_FORMAT_UNSUPPORTED, "distinct video encoders");
    }
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

#[test]
fn slot_codec_tokens_follow_the_engine_table() {
    let slot = |kind: &str, codec: &str| {
        format!(
            r#"
[[outputs]]
kind = "{kind}"
url = "srt://[::1]:9000"
codec = "h264"
[[outputs.slots.video]]
codec = "{codec}"
"#
        )
    };
    for codec in [
        "h264_nvenc",
        "hevc_nvenc",
        "libx264",
        "libx265",
        "mpeg2video",
    ] {
        parse(&slot("srt", codec))
            .validate()
            .unwrap_or_else(|e| panic!("{codec}: {e:?}"));
    }
    for codec in [
        "libopenh264",
        "mpeg2",
        "mpeg2_qsv",
        "h264_vaapi",
        "ffv1",
        "mjpeg",
        "vp9",
    ] {
        assert_code(
            &slot("srt", codec),
            SLOT_FORMAT_UNSUPPORTED,
            "is not carried",
        );
    }
}

fn sized_output(i: u32) -> String {
    format!(
        r#"
[[outputs]]
kind = "hls"
id = "o{i}"
path = "/srv/hls/{i}"
codec = "h264"
[outputs.format]
width = {w}
height = 720
"#,
        w = 640 + 16 * i
    )
}

#[test]
fn more_than_eight_distinct_base_formats_are_refused() {
    let eight: String = (0..8).map(sized_output).collect();
    parse(&eight)
        .validate()
        .expect("eight distinct base keys fit");
    // A ninth distinct format is a ninth encoder.
    let nine: String = (0..9).map(sized_output).collect();
    assert_code(&nine, SLOT_FORMAT_UNSUPPORTED, "9 distinct video encoders");
    // Repeating a format shares its key: still eight.
    let shared = eight.clone()
        + &sized_output(3)
            .replace("id = \"o3\"", "id = \"again\"")
            .replace("/srv/hls/3", "/srv/hls/again");
    parse(&shared)
        .validate()
        .expect("a repeated format shares a key");
    // A different bit rate on the same format is another key.
    let bitrate = eight
        + &sized_output(3)
            .replace("id = \"o3\"", "id = \"fast\"")
            .replace("/srv/hls/3", "/srv/hls/fast")
        + "[[outputs.slots.video]]\nbitrate_kbps = 9000\n";
    assert_code(
        &bitrate,
        SLOT_FORMAT_UNSUPPORTED,
        "9 distinct video encoders",
    );
}

// ---- Slice F (#345): subtitle slots -------------------------------------

fn hls_subs(slots: &str) -> String {
    format!(
        r#"
[[outputs]]
kind = "hls"
id = "subs"
path = "/srv/hls/s"
codec = "h264"
{slots}
"#
    )
}

const TWO_SUBS: &str = r#"
[[outputs.slots.subtitle]]
language = "en"
name = "English (CC)"
source = "cam_a"
[[outputs.slots.subtitle]]
format = "webvtt"
language = "es-419"
"#;

#[test]
fn hls_webvtt_subtitle_slots_parse_validate_and_round_trip() {
    let cfg = parse(&hls_subs(TWO_SUBS));
    cfg.validate().expect("HLS carries WebVTT subtitle slots");
    let slots = cfg.outputs[0].slots().expect("slots");
    assert_eq!(slots.subtitle.len(), 2);
    assert_eq!(slots.subtitle[0].format, SubtitleFormat::Webvtt);
    assert_eq!(slots.subtitle[0].language.as_deref(), Some("en"));
    assert_eq!(slots.subtitle[0].name.as_deref(), Some("English (CC)"));
    assert_eq!(slots.subtitle[0].source.as_deref(), Some("cam_a"));
    assert_eq!(slots.subtitle[1].source, None);
    assert!(slots.video.is_empty() && slots.audio.is_empty());
    // Subtitle slots do not imply an audio selection.
    assert!(cfg.outputs[0].effective_audio().is_none());
    let toml = cfg.to_toml().expect("to toml");
    assert_eq!(
        MultiviewConfig::load_from_toml(&toml).expect("reparse"),
        cfg
    );
    let json = cfg.to_json().expect("to json");
    assert!(json.contains("\"subtitle\""));
    assert_eq!(
        MultiviewConfig::load_from_json(&json).expect("reparse"),
        cfg
    );
}

#[test]
fn absent_subtitle_slots_serialize_byte_identically() {
    // A slots table with only audio: no `subtitle` key appears.
    let cfg = parse(VERTICAL);
    let toml = cfg.to_toml().expect("to toml");
    let json = cfg.to_json().expect("to json");
    assert!(!toml.contains("subtitle"), "{toml}");
    assert!(!json.contains("subtitle"), "{json}");
    // Re-serialising the reparsed document yields the same bytes.
    let again = MultiviewConfig::load_from_toml(&toml).expect("reparse");
    assert_eq!(again.to_toml().expect("to toml"), toml);
}

#[test]
fn unknown_subtitle_field_and_format_are_rejected_at_parse() {
    let bad_field = doc(&hls_subs("[[outputs.slots.subtitle]]\nlang = \"en\"\n"));
    assert!(MultiviewConfig::load_from_toml(&bad_field).is_err());
    let bad_format = doc(&hls_subs("[[outputs.slots.subtitle]]\nformat = \"srt\"\n"));
    assert!(MultiviewConfig::load_from_toml(&bad_format).is_err());
}

#[test]
fn eight_subtitle_slots_accepted_nine_refused() {
    let eight = "[[outputs.slots.subtitle]]\n".repeat(MAX_SUBTITLE_SLOTS);
    parse(&hls_subs(&eight)).validate().expect("8 slots");
    let nine = "[[outputs.slots.subtitle]]\n".repeat(MAX_SUBTITLE_SLOTS + 1);
    assert_code(&hls_subs(&nine), SLOT_MEDIUM_MISMATCH, "9 subtitle slot(s)");
}

#[test]
fn ll_hls_webvtt_is_slot_format_unsupported() {
    assert_code(
        r#"
[[outputs]]
kind = "ll_hls"
path = "/srv/hls/ll"
codec = "h264"
[[outputs.slots.subtitle]]
"#,
        SLOT_FORMAT_UNSUPPORTED,
        "not built yet",
    );
}

fn ts_output(kind_lines: &str, format: &str) -> String {
    format!("[[outputs]]\n{kind_lines}\ncodec = \"h264\"\n[[outputs.slots.subtitle]]\nformat = \"{format}\"\n")
}

#[test]
fn ts_and_rtmp_subtitles_are_refused_with_the_right_code() {
    let kinds = [
        "kind = \"srt\"\nurl = \"srt://[::1]:9000\"",
        "kind = \"rist\"\nurl = \"rist://[::1]:9000\"",
        "kind = \"rtmp\"\nurl = \"rtmp://[::1]/live/k\"",
    ];
    for kind in kinds {
        for format in ["cea608", "dvb"] {
            assert_code(
                &ts_output(kind, format),
                SLOT_FORMAT_UNSUPPORTED,
                "not built yet",
            );
        }
        assert_code(
            &ts_output(kind, "webvtt"),
            SLOT_MEDIUM_MISMATCH,
            "\"webvtt\" is not carried",
        );
    }
}

#[test]
fn subtitles_on_other_kinds_are_slot_medium_mismatch() {
    let kinds = [
        "kind = \"rtsp_server\"\nmount = \"/m\"\ncodec = \"h264\"",
        "kind = \"webrtc\"\nlabel = \"web\"",
        "kind = \"ndi\"\nname = \"pgm\"",
        "kind = \"display\"",
        "kind = \"aes67\"\nlabel = \"pcm\"\nmulticast = \"[ff3e::1]:5004\"",
    ];
    for kind in kinds {
        for format in ["webvtt", "cea608", "dvb"] {
            assert_code(
                &format!(
                    "[[outputs]]\n{kind}\n[[outputs.slots.subtitle]]\nformat = \"{format}\"\n"
                ),
                SLOT_MEDIUM_MISMATCH,
                "is not carried by this medium",
            );
        }
    }
}

#[test]
fn subtitle_language_is_validated() {
    for ok in ["en", "es-419", "zh-Hant-TW", &"a".repeat(35)] {
        parse(&hls_subs(&format!(
            "[[outputs.slots.subtitle]]\nlanguage = \"{ok}\"\n"
        )))
        .validate()
        .unwrap_or_else(|e| panic!("{ok:?} accepted: {e}"));
    }
    for bad in ["", "en_US", "en US", "é", &"a".repeat(36)] {
        assert_code(
            &hls_subs(&format!(
                "[[outputs.slots.subtitle]]\nlanguage = \"{bad}\"\n"
            )),
            SLOT_FORMAT_UNSUPPORTED,
            "language",
        );
    }
}

#[test]
fn subtitle_name_is_validated() {
    parse(&hls_subs(&format!(
        "[[outputs.slots.subtitle]]\nname = \"{}\"\n",
        "é".repeat(64)
    )))
    .validate()
    .expect("64 characters accepted");
    for bad in [
        "é".repeat(65),
        "quote \\\"here\\\"".to_owned(),
        "tab\\there".to_owned(),
        "nl\\nhere".to_owned(),
    ] {
        assert_code(
            &hls_subs(&format!("[[outputs.slots.subtitle]]\nname = \"{bad}\"\n")),
            SLOT_FORMAT_UNSUPPORTED,
            "name must be",
        );
    }
}

#[test]
fn subtitle_rendition_names_must_be_distinct() {
    // Same language, no names: both would be NAME="en".
    assert_code(
        &hls_subs(
            "[[outputs.slots.subtitle]]\nlanguage = \"en\"\n\
             [[outputs.slots.subtitle]]\nlanguage = \"en\"\n",
        ),
        SLOT_FORMAT_UNSUPPORTED,
        "subtitle slot 1 name \"en\" repeats slot 0's",
    );
    // Duplicate explicit names.
    assert_code(
        &hls_subs(
            "[[outputs.slots.subtitle]]\nname = \"CC\"\nlanguage = \"en\"\n\
             [[outputs.slots.subtitle]]\nname = \"CC\"\nlanguage = \"de\"\n",
        ),
        SLOT_FORMAT_UNSUPPORTED,
        "subtitle slot 1 name \"CC\" repeats slot 0's",
    );
    // An explicit name colliding with another slot's language fallback.
    assert_code(
        &hls_subs(
            "[[outputs.slots.subtitle]]\nlanguage = \"de\"\n\
             [[outputs.slots.subtitle]]\nname = \"de\"\nlanguage = \"en\"\n",
        ),
        SLOT_FORMAT_UNSUPPORTED,
        "subtitle slot 1 name \"de\" repeats slot 0's",
    );
    // ... and with the default label.
    assert_code(
        &hls_subs(
            "[[outputs.slots.subtitle]]\nname = \"Subtitles 2\"\n\
             [[outputs.slots.subtitle]]\n",
        ),
        SLOT_FORMAT_UNSUPPORTED,
        "subtitle slot 1 name \"Subtitles 2\" repeats slot 0's",
    );
    // Same language with distinct names is fine.
    parse(&hls_subs(
        "[[outputs.slots.subtitle]]\nlanguage = \"en\"\n\
         [[outputs.slots.subtitle]]\nlanguage = \"en\"\nname = \"en (SDH)\"\n",
    ))
    .validate()
    .expect("distinct effective names");
}

#[test]
fn subtitle_source_must_be_declared() {
    let msg = refusal(&hls_subs(
        "[[outputs.slots.subtitle]]\nsource = \"ghost\"\n",
    ));
    assert!(
        msg.contains("subtitle slot 0 names unknown source \"ghost\""),
        "{msg}"
    );
    parse(&hls_subs(
        "[[outputs.slots.subtitle]]\nsource = \"cam_b\"\n",
    ))
    .validate()
    .expect("declared source");
}

/// #343 × #345: subtitle slots are sampled at Program's cadence; a rate
/// output does not carry them yet, so the pair is refused.
#[test]
fn subtitle_slots_on_a_rate_output_are_slot_format_unsupported() {
    let rate_subs = format!(
        "{PLAIN}{}",
        hls_subs(&format!(
            "[outputs.format]\nwidth = 1280\nheight = 720\nfps = \"25/1\"\n{TWO_SUBS}"
        ))
    );
    assert_code(
        &rate_subs,
        SLOT_FORMAT_UNSUPPORTED,
        "subtitle slots on a rate output",
    );
}
