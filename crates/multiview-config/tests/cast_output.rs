//! `Output::Cast` config-schema tests (ADR-0111): the gated HLS + CMAF output
//! a Google Cast receiver plays. Pure serde + validation: no engine.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::{
    MultiviewConfig, Output, OutputTransport, PlaybackContext, RenditionAudioCodec, TrackCapacity,
    TrackDelivery,
};

fn base(width: u32, height: u32, fps: &str) -> String {
    format!(
        r##"
schema_version = 1
[canvas]
width = {width}
height = {height}
fps = "{fps}"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]
[[sources]]
id = "a"
kind = "bars"
[[sources]]
id = "b"
kind = "bars"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "a"
[[outputs]]
id = "program-hls"
kind = "hls"
path = "/srv/hls/program/index.m3u8"
codec = "h264"
"##
    )
}

const CAST: &str = "\n[[outputs]]\nkind = \"cast\"\nid = \"cast-1\"\n";

fn load(doc: &str) -> Result<MultiviewConfig, String> {
    let cfg = MultiviewConfig::load_from_toml(doc).map_err(|e| e.to_string())?;
    cfg.validate().map_err(|e| e.to_string())?;
    Ok(cfg)
}

fn load_hd(extra: &str) -> Result<MultiviewConfig, String> {
    load(&format!("{}{extra}", base(1920, 1080, "30000/1001")))
}

#[test]
fn minimal_cast_output_defaults_to_h264_and_no_dir() {
    let out: Output = toml::from_str("kind = \"cast\"\nid = \"cast-1\"\n").unwrap();
    let Output::Cast {
        id,
        codec,
        dir,
        gpu_pin,
        audio,
        metadata,
    } = &out
    else {
        panic!("expected Cast, got {out:?}");
    };
    assert_eq!(id, "cast-1");
    assert_eq!(codec, "h264");
    assert!(dir.is_none() && gpu_pin.is_none() && audio.is_none() && metadata.is_none());
    out.validate().unwrap();
}

#[test]
fn the_id_is_required() {
    let err = toml::from_str::<Output>("kind = \"cast\"\n").unwrap_err();
    assert!(err.to_string().contains("id"), "{err}");
}

#[test]
fn serde_round_trips_toml_and_json() {
    let toml_str = r#"
kind = "cast"
id = "Cast_2-b"
dir = "/var/lib/multiview/cast/two"
gpu_pin = { vendor = "nvidia", stable_id = "GPU-1" }
audio = { mode = "program" }
"#;
    let out: Output = toml::from_str(toml_str).unwrap();
    let again: Output = toml::from_str(&toml::to_string(&out).unwrap()).unwrap();
    assert_eq!(out, again);
    let json = serde_json::to_value(&out).unwrap();
    assert_eq!(json["kind"], "cast");
    assert_eq!(json["id"], "Cast_2-b");
    // The default codec is not written back.
    assert!(json.get("codec").is_none(), "{json}");
    assert_eq!(serde_json::from_value::<Output>(json).unwrap(), out);
    // A whole document round-trips too.
    let cfg = load_hd(CAST).unwrap();
    let text = toml::to_string(&cfg).unwrap();
    assert_eq!(MultiviewConfig::load_from_toml(&text).unwrap(), cfg);
}

#[test]
fn the_id_must_be_a_short_safe_token() {
    for bad in ["cast 1", "cast/1", "..", "cäst", &"x".repeat(65)] {
        let doc = format!("\n[[outputs]]\nkind = \"cast\"\nid = \"{bad}\"\n");
        let err = load_hd(&doc).unwrap_err();
        assert!(err.contains("1 to 64 characters"), "{bad:?}: {err}");
    }
    // An empty id is refused by the rule every output kind shares.
    let err = load_hd("\n[[outputs]]\nkind = \"cast\"\nid = \"\"\n").unwrap_err();
    assert!(err.contains("declares an empty id"), "{err}");
    let longest = "x".repeat(64);
    load_hd(&format!(
        "\n[[outputs]]\nkind = \"cast\"\nid = \"{longest}\"\n"
    ))
    .unwrap();
    load_hd("\n[[outputs]]\nkind = \"cast\"\nid = \"Cast_2-b\"\n").unwrap();
}

#[test]
fn the_id_must_be_unique_among_all_outputs() {
    let err = load_hd(&format!("{CAST}{CAST}")).unwrap_err();
    assert!(err.contains("duplicate output id \"cast-1\""), "{err}");
    // It may not collide with another kind's explicit id either.
    let err = load_hd("\n[[outputs]]\nkind = \"cast\"\nid = \"program-hls\"\n").unwrap_err();
    assert!(err.contains("duplicate output id"), "{err}");
}

#[test]
fn the_codec_must_be_an_h264_token() {
    for good in ["h264", "avc", "H264"] {
        load_hd(&format!("{CAST}codec = \"{good}\"\n")).unwrap();
    }
    for bad in ["hevc", "mpeg2video", "vp9", ""] {
        let err = load_hd(&format!("{CAST}codec = \"{bad}\"\n")).unwrap_err();
        assert!(
            err.contains("a cast output needs \"h264\"") || err.contains("empty codec"),
            "{bad:?}: {err}"
        );
    }
}

#[test]
fn an_authored_dir_may_not_be_empty() {
    let err = load_hd(&format!("{CAST}dir = \"\"\n")).unwrap_err();
    assert!(err.contains("dir, when set, must be non-empty"), "{err}");
    load_hd(&format!("{CAST}dir = \"/var/lib/cast/one\"\n")).unwrap();
}

#[test]
fn the_canvas_limit_is_1920x1080_at_30_fps_and_the_error_names_it() {
    const LIMIT: &str =
        "a cast output needs a canvas of at most 1920x1080 at 30 fps until per-output formats exist";
    for (w, h, fps) in [
        (1920, 1080, "30/1"),
        (1920, 1080, "30000/1001"),
        (1280, 720, "25/1"),
        (1080, 1080, "24000/1001"),
    ] {
        load(&format!("{}{CAST}", base(w, h, fps))).unwrap();
    }
    for (w, h, fps) in [
        (3840, 2160, "30/1"),
        (1920, 1088, "30/1"),
        (1922, 1080, "30/1"),
        (1920, 1080, "60000/1001"),
        (1920, 1080, "50/1"),
        (1280, 720, "30001/1000"),
    ] {
        let err = load(&format!("{}{CAST}", base(w, h, fps))).unwrap_err();
        assert!(err.contains(LIMIT), "{w}x{h}@{fps}: {err}");
        assert!(err.contains("cast-1"), "{err}");
        // The same canvas without a cast output stays valid.
        load(&base(w, h, fps)).unwrap();
    }
}

const TRACKS: &str = r#"
[audio]
sample_rate_hz = 48000
[[audio.routes]]
input_id = "a"
channels = { kind = "stereo" }
target_track = "eng"
"#;

#[test]
fn discrete_audio_tracks_are_not_selectable_on_a_cast_output() {
    let out: Output = toml::from_str("kind = \"cast\"\nid = \"c\"\n").unwrap();
    let cap = out.audio_capability();
    assert_eq!(cap.delivery, TrackDelivery::None);
    assert_eq!(cap.discrete_capacity, TrackCapacity::AtMost(0));
    // The output's own selection.
    let doc = format!("{CAST}audio = {{ mode = \"tracks\", tracks = [\"eng\"] }}\n{TRACKS}");
    let err = load_hd(&doc).unwrap_err();
    assert!(err.contains("cast cast-1"), "{err}");
    // A routed `tracks` feed, even a single track.
    let doc = format!(
        "{CAST}{TRACKS}\n[[routing.output]]\noutput = \"cast-1\"\naudio = {{ tracks = [\"eng\"] }}\n"
    );
    let err = load_hd(&doc).unwrap_err();
    assert!(
        err.contains("a cast output carries the program audio only"),
        "{err}"
    );
    // Program audio is fine, selected or routed.
    load_hd(&format!("{CAST}audio = {{ mode = \"program\" }}\n")).unwrap();
    load_hd(&format!(
        "{CAST}\n[[routing.output]]\noutput = \"cast-1\"\naudio = \"program\"\n"
    ))
    .unwrap();
}

#[test]
fn a_cast_output_is_routable_like_an_hls_output() {
    let cfg = load_hd(&format!(
        "{CAST}\n[[routing.output]]\noutput = \"cast-1\"\nvideo = {{ source = \"b\" }}\n"
    ))
    .unwrap();
    let row = cfg
        .routing
        .as_ref()
        .and_then(|r| r.output.iter().find(|xp| xp.output == "cast-1"))
        .expect("the cast routing row");
    assert_eq!(
        row.video,
        Some(multiview_config::VideoFeed::Source("b".to_owned()))
    );
    let err = load_hd(&format!(
        "{CAST}\n[[routing.output]]\noutput = \"cast-1\"\nvideo = {{ source = \"nope\" }}\n"
    ))
    .unwrap_err();
    assert!(err.contains("nope"), "{err}");
}

#[test]
fn accessors_transport_and_playback_of_a_cast_output() {
    let out: Output =
        toml::from_str("kind = \"cast\"\nid = \"cast-1\"\n[metadata]\ntitle = \"Program\"\n")
            .unwrap();
    assert_eq!(out.explicit_id(), Some("cast-1"));
    assert_eq!(out.id(), "cast-1");
    assert_eq!(out.label(), "cast cast-1");
    assert!(out.orientation().is_none() && out.gpu_pin().is_none());
    assert_eq!(OutputTransport::of(&out), OutputTransport::Cast);
    assert_eq!(
        OutputTransport::of(&out).encoded_audio_codec(),
        Some(RenditionAudioCodec::Aac)
    );
    // No metadata carrier: an authored title is reported as dropped.
    let plan = out.metadata_plan();
    assert!(
        matches!(
            plan.title,
            Some(multiview_config::MetadataField::Dropped { .. })
        ),
        "{plan:?}"
    );
    // No public playback URL, with or without a public host or an HLS route.
    let ctx = PlaybackContext {
        public_host: Some("studio.example"),
        hls_route: Some("/hls/cast-1"),
        hls_playlist: Some("master.m3u8"),
        ..PlaybackContext::default()
    };
    for urls in [
        out.playback_urls(&ctx),
        out.playback_urls(&PlaybackContext::default()),
    ] {
        assert_eq!(urls.len(), 1);
        assert!(
            urls[0].url.is_none() && urls[0].vlc_command.is_none(),
            "{urls:?}"
        );
        assert!(urls[0].notes[0].contains("no playback URL"), "{urls:?}");
    }
}
