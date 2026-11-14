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
        format,
        slots,
    } = &out
    else {
        panic!("expected Cast, got {out:?}");
    };
    assert!(format.is_none() && slots.is_none());
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
fn a_cast_dir_is_absolute_and_shared_with_no_other_writing_output() {
    let cast = |id: &str, dir: &str| {
        format!("\n[[outputs]]\nkind = \"cast\"\nid = \"{id}\"\ndir = \"{dir}\"\n")
    };
    // Absolute only.
    for relative in ["cast/one", "./cast", "../cast"] {
        let err = load_hd(&cast("cast-1", relative)).unwrap_err();
        assert!(
            err.contains("cast output \"cast-1\"") && err.contains("must be an absolute path"),
            "{relative}: {err}"
        );
    }
    // Two cast outputs: never the same directory, also after normalisation.
    for second in [
        "/var/lib/cast/one",
        "/var/lib/cast/./one/",
        "/var/lib/cast/two/../one",
    ] {
        let err = load_hd(&format!(
            "{}{}",
            cast("cast-1", "/var/lib/cast/one"),
            cast("cast-2", second)
        ))
        .unwrap_err();
        assert!(
            err.contains("cast outputs \"cast-1\" and \"cast-2\" use the same dir"),
            "{second}: {err}"
        );
    }
    // One cast directory inside another is refused too.
    let err = load_hd(&format!(
        "{}{}",
        cast("cast-1", "/var/lib/cast"),
        cast("cast-2", "/var/lib/cast/two")
    ))
    .unwrap_err();
    assert!(
        err.contains("cast output \"cast-1\"") && err.contains("output \"cast-2\" writes to"),
        "{err}"
    );
    load_hd(&format!(
        "{}{}",
        cast("cast-1", "/var/lib/cast/one"),
        cast("cast-2", "/var/lib/cast/two")
    ))
    .unwrap();
    // BASE's HLS output writes to /srv/hls/program (the playlist's parent):
    // equal, inside and around it are all refused, naming both outputs.
    for dir in [
        "/srv/hls/program",
        "/srv/hls/program/cast",
        "/srv/hls",
        "/srv",
        "/",
        "/srv/hls/x/../program/",
    ] {
        let err = load_hd(&cast("cast-1", dir)).unwrap_err();
        assert!(
            err.contains("cast output \"cast-1\"")
                && err.contains("/srv/hls/program that output \"program-hls\" writes to"),
            "{dir}: {err}"
        );
    }
    // A sibling directory is fine; so is a name that merely shares a prefix.
    load_hd(&cast("cast-1", "/srv/hls/cast-1")).unwrap();
    load_hd(&cast("cast-1", "/srv/hls/program-cast")).unwrap();
    // An HLS output whose path is a directory (no .m3u8) is that directory.
    let hls_dir = "\n[[outputs]]\nid = \"dir-hls\"\nkind = \"ll_hls\"\npath = \"/srv/ll\"\ncodec = \"h264\"\n";
    let err = load_hd(&format!("{hls_dir}{}", cast("cast-1", "/srv/ll/sub"))).unwrap_err();
    assert!(
        err.contains("/srv/ll that output \"dir-hls\" writes to"),
        "{err}"
    );
}

const FORMAT_HD: &str = "[outputs.format]\nwidth = 1920\nheight = 1080\n";

#[test]
fn the_picture_limit_is_1920x1080_and_the_error_names_the_fix() {
    for (w, h, fps) in [
        (1920, 1080, "30/1"),
        (1920, 1080, "30000/1001"),
        (1280, 720, "25/1"),
        (1080, 1080, "24000/1001"),
    ] {
        load(&format!("{}{CAST}", base(w, h, fps))).unwrap();
    }
    for (w, h) in [(3840, 2160), (1920, 1088), (1922, 1080)] {
        let err = load(&format!("{}{CAST}", base(w, h, "30/1"))).unwrap_err();
        assert!(
            err.contains(&format!(
                "cast output \"cast-1\": the picture is {w}x{h}; a Cast receiver decodes at most \
                 1920x1080 at 30 fps. Add [outputs.format] width = 1920 height = 1080 to this output."
            )),
            "{w}x{h}: {err}"
        );
        // The same canvas without a cast output stays valid, and the fix the
        // error names makes the document valid.
        load(&base(w, h, "30/1")).unwrap();
        load(&format!("{}{CAST}{FORMAT_HD}", base(w, h, "30/1"))).unwrap();
    }
    // A declared format is the picture that is checked, on any canvas.
    let big = "[outputs.format]\nwidth = 2560\nheight = 1440\n";
    let err = load(&format!("{}{CAST}{big}", base(3840, 2160, "30/1"))).unwrap_err();
    assert!(err.contains("the picture is 2560x1440"), "{err}");
    let err = load(&format!("{}{CAST}{big}", base(1280, 720, "30/1"))).unwrap_err();
    assert!(err.contains("the picture is 2560x1440"), "{err}");
    load(&format!(
        "{}{CAST}[outputs.format]\nwidth = 1280\nheight = 720\n",
        base(3840, 2160, "30/1")
    ))
    .unwrap();
}

#[test]
fn a_canvas_above_30_fps_refuses_a_cast_output_until_per_output_rates() {
    for fps in ["50/1", "60000/1001", "30001/1000"] {
        for extra in ["", FORMAT_HD] {
            let err = load(&format!("{}{CAST}{extra}", base(1920, 1080, fps))).unwrap_err();
            assert!(
                err.contains("a cast output cannot be used on this canvas until per-output frame rates exist (#343)")
                    && err.contains("cast-1")
                    && err.contains(fps),
                "{fps}: {err}"
            );
        }
        load(&base(1920, 1080, fps)).unwrap();
    }
}

#[test]
fn slots_are_one_h264_video_and_one_program_audio() {
    let video = |codec: &str| format!("{CAST}[[outputs.slots.video]]\ncodec = \"{codec}\"\n");
    for good in ["h264", "avc", "libx264", "h264_nvenc"] {
        load_hd(&video(good)).unwrap();
    }
    for bad in ["hevc", "mpeg2video"] {
        let err = load_hd(&video(bad)).unwrap_err();
        assert!(
            err.contains(&format!(
                "cast output \"cast-1\": codec {bad:?} is not supported: a cast output needs \"h264\""
            )),
            "{bad}: {err}"
        );
    }
    load_hd(&format!(
        "{CAST}[[outputs.slots.video]]\nbitrate_kbps = 6000\n"
    ))
    .unwrap();
    let err = load_hd(&format!(
        "{CAST}[[outputs.slots.video]]\n[[outputs.slots.video]]\n"
    ))
    .unwrap_err();
    assert!(
        err.contains("declares 2 video slot(s); this medium carries 1"),
        "{err}"
    );
    load_hd(&format!("{CAST}[[outputs.slots.audio]]\n")).unwrap();
    let err = load_hd(&format!(
        "{CAST}[[outputs.slots.audio]]\n[[outputs.slots.audio]]\ntrack = \"eng\"\n{TRACKS}"
    ))
    .unwrap_err();
    assert!(
        err.contains("declares 2 audio slot(s); this medium carries 1"),
        "{err}"
    );
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

#[test]
fn a_cast_output_accepts_every_feed_kind_an_hls_output_accepts() {
    const SWITCHER: &str = "\n[switcher]\n[[switcher.mix_effects]]\nid = \"me1\"\n\
                            program = \"main\"\nprogram_source = \"a\"\npreview_source = \"b\"\n";
    // The same row on the HLS output and on the cast output: equal verdicts.
    for (feed, switcher) in [
        ("{ bus = \"program\" }", false),
        ("{ source = \"b\" }", false),
        ("{ layout = \"NewsOne\" }", false),
        ("{ layout = \" \" }", false),
        ("{ source = \"nope\" }", false),
        ("{ bus = \"preview\" }", true),
        ("{ bus = \"clean\" }", true),
        ("{ bus = \"preview\" }", false),
    ] {
        let doc = |output: &str| {
            format!(
                "{CAST}{}\n[[routing.output]]\noutput = \"{output}\"\nvideo = {feed}\n",
                if switcher { SWITCHER } else { "" }
            )
        };
        let hls = load_hd(&doc("program-hls"));
        let cast = load_hd(&doc("cast-1"));
        assert_eq!(
            hls.is_ok(),
            cast.is_ok(),
            "{feed}: hls {hls:?}, cast {cast:?}"
        );
    }
    // A stored layout (ADR-0108 slice 3) is routable to a cast output.
    load_hd(&format!(
        "{CAST}\n[[routing.output]]\noutput = \"cast-1\"\nvideo = {{ layout = \"NewsOne\" }}\n"
    ))
    .unwrap();
}

#[test]
fn the_video_key_is_shared_only_without_an_own_format_codec_or_bit_rate() {
    use multiview_config::rendition::distinct_base_video_keys as keys;
    // BASE has one H.264 HLS output: one key.
    assert_eq!(keys(&load_hd("").unwrap()), 1);
    // No format: the cast output shares the HLS output's key (+0 encoders).
    assert_eq!(keys(&load_hd(CAST).unwrap()), 1);
    // A format equal to the canvas is the canvas key: still shared.
    assert_eq!(keys(&load_hd(&format!("{CAST}{FORMAT_HD}")).unwrap()), 1);
    // Its own format: its own key (one more encoder).
    let own = format!("{CAST}[outputs.format]\nwidth = 1280\nheight = 720\n");
    assert_eq!(keys(&load_hd(&own).unwrap()), 2);
    // Its own bit rate: its own key too.
    let rate = format!("{CAST}[[outputs.slots.video]]\nbitrate_kbps = 6000\n");
    assert_eq!(keys(&load_hd(&rate).unwrap()), 2);
    // A UHD canvas: the HLS output at the canvas, the cast output at HD.
    let uhd = format!("{}{CAST}{FORMAT_HD}", base(3840, 2160, "30/1"));
    assert_eq!(keys(&load(&uhd).unwrap()), 2);
    // The slot capability of the medium.
    let out: Output = toml::from_str("kind = \"cast\"\nid = \"c\"\n").unwrap();
    assert!(out.format().is_none() && out.slots().is_none());
    let cap = out.slot_capability();
    assert_eq!((cap.video_max, cap.audio_max), (1, Some(1)));
    assert!(cap.format_convertible && !cap.raw_video);
}
