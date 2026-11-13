//! ADR-0108 slice 1: `[[routing.output]] { output, video, audio }` — an output's
//! selectable feed. Absent config is exactly today's behaviour (Program, follow).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use multiview_config::{
    AudioFeed, FollowAudio, MultiviewConfig, OutputBus, OutputCrosspoint, VideoFeed,
};

const BASE: &str = r##"
schema_version = 1
[canvas]
width = 640
height = 360
fps = "30000/1001"
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
kind = "test"
[[sources]]
id = "bars"
kind = "test"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "a"
[[outputs]]
id = "restream"
kind = "hls"
path = "/tmp/output-feeds/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
[[outputs]]
id = "srt-out"
kind = "hls"
path = "/tmp/output-feeds/b/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;

const SWITCHER: &str = r#"
[switcher]
[[switcher.mix_effects]]
id = "me1"
program = "main"
program_source = "a"
preview_source = "bars"
"#;

fn load(extra: &str) -> Result<MultiviewConfig, String> {
    let cfg =
        MultiviewConfig::load_from_toml(&format!("{BASE}{extra}")).map_err(|e| e.to_string())?;
    cfg.validate().map_err(|e| e.to_string())?;
    Ok(cfg)
}

#[test]
fn absent_routing_is_program_and_follow_for_every_output() {
    let cfg = load("").unwrap();
    let feeds = cfg.output_feeds();
    assert_eq!(feeds.len(), 2);
    for (feed, id) in feeds.iter().zip(["restream", "srt-out"]) {
        assert_eq!(feed.output, id);
        assert_eq!(feed.effective_video(), VideoFeed::PROGRAM);
        assert_eq!(feed.audio, AudioFeed::Follow);
        assert_eq!(feed.effective_video().picture_bus(), "program");
    }
    // The serialized document gains no routing key (A/B: defaults unchanged).
    let json = cfg.to_json().unwrap();
    assert!(!json.contains("\"video\""), "{json}");
    assert!(!json.contains("\"routing\""), "{json}");
    assert_eq!(
        cfg.routing_table().output[0],
        OutputCrosspoint::program("restream", "main")
    );
}

#[test]
fn a_source_feed_parses_round_trips_and_is_distinct_from_program() {
    let cfg = load(
        r#"
[[routing.output]]
output = "srt-out"
video = { source = "bars" }
"#,
    )
    .unwrap();
    let feeds = cfg.output_feeds();
    assert_eq!(feeds[0].effective_video(), VideoFeed::PROGRAM);
    assert_eq!(feeds[1].effective_video(), VideoFeed::Source("bars".into()));
    assert_eq!(feeds[1].effective_video().picture_bus(), "src:bars");
    let parsed = MultiviewConfig::load_from_json(&cfg.to_json().unwrap()).unwrap();
    assert_eq!(parsed, cfg);
    // Even when Program currently shows `a`, `{source = "a"}` is not `{bus = "program"}`.
    assert_ne!(VideoFeed::Source("a".into()), VideoFeed::PROGRAM);
}

#[test]
fn unknown_source_feed_is_refused() {
    let err = load(
        r#"
[[routing.output]]
output = "srt-out"
video = { source = "nope" }
"#,
    )
    .unwrap_err();
    assert!(err.contains("unknown source \"nope\""), "{err}");
}

#[test]
fn preview_needs_a_declared_me_both_spellings() {
    for row in [r#"video = { bus = "preview" }"#, r#"bus = "preview""#] {
        let doc = format!("\n[[routing.output]]\noutput = \"srt-out\"\n{row}\n");
        let err = load(&doc).unwrap_err();
        assert!(err.contains("without a declared M/E"), "{row}: {err}");
        let cfg = load(&format!("{SWITCHER}{doc}")).unwrap();
        assert_eq!(
            cfg.output_feeds()[1].effective_video(),
            VideoFeed::Bus(OutputBus::Preview)
        );
    }
}

#[test]
fn bus_alias_must_agree_with_video() {
    let err = load(&format!(
        "{SWITCHER}\n[[routing.output]]\noutput = \"srt-out\"\nbus = \"preview\"\nvideo = {{ source = \"bars\" }}\n"
    ))
    .unwrap_err();
    assert!(err.contains("disagree"), "{err}");
    load(&format!(
        "{SWITCHER}\n[[routing.output]]\noutput = \"srt-out\"\nbus = \"preview\"\nvideo = {{ bus = \"preview\" }}\n"
    ))
    .unwrap();
}

#[test]
fn a_layout_feed_validates_an_empty_id_does_not_and_explicit_audio_is_accepted() {
    // ADR-0108 slice 3 (#306): a layout feed is routable; its existence is
    // checked against the layouts library at take time, not here.
    load("\n[[routing.output]]\noutput = \"srt-out\"\nvideo = { layout = \"NewsOne\" }\n").unwrap();
    let err = load("\n[[routing.output]]\noutput = \"srt-out\"\nvideo = { layout = \" \" }\n")
        .unwrap_err();
    assert!(err.contains("layout id"), "{err}");
    // ADR-0108 slice 2: explicit audio feeds validate (rules: tests below).
    load("\n[[routing.output]]\noutput = \"srt-out\"\naudio = \"none\"\n").unwrap();
    load("\n[[routing.output]]\noutput = \"srt-out\"\naudio = \"follow\"\n").unwrap();
}

#[test]
fn unknown_keys_are_rejected_no_operator_grouping_key() {
    for key in ["route_group = \"g\"", "encoder = \"x\""] {
        let err = load(&format!(
            "\n[[routing.output]]\noutput = \"srt-out\"\nvideo = {{ source = \"bars\" }}\n{key}\n"
        ))
        .unwrap_err();
        assert!(err.contains("unknown field"), "{key}: {err}");
    }
}

#[test]
fn follow_audio_resolution_for_every_feed_kind() {
    assert_eq!(
        FollowAudio::of(&VideoFeed::PROGRAM),
        FollowAudio::ProgramMix
    );
    assert_eq!(
        FollowAudio::of(&VideoFeed::Bus(OutputBus::Clean)),
        FollowAudio::ProgramMix
    );
    assert_eq!(
        FollowAudio::of(&VideoFeed::Bus(OutputBus::Preview)),
        FollowAudio::PreviewSource
    );
    assert_eq!(
        FollowAudio::of(&VideoFeed::Source("bars".into())),
        FollowAudio::Source("bars".into())
    );
    assert_eq!(
        FollowAudio::of(&VideoFeed::Layout("L".into())),
        FollowAudio::LayoutDefault
    );
}

#[test]
fn audio_feed_wire_shapes() {
    let json = serde_json::to_value(AudioFeed::Source("x".into())).unwrap();
    assert_eq!(json, serde_json::json!({"source": "x"}));
    assert_eq!(
        serde_json::to_value(AudioFeed::Follow).unwrap(),
        serde_json::json!("follow")
    );
    let video = serde_json::to_value(VideoFeed::PROGRAM).unwrap();
    assert_eq!(video, serde_json::json!({"bus": "program"}));
}

/// A/B: every shipped example keeps exactly today's output routing (Program,
/// follow) and its serialized form gains no routing keys.
#[test]
fn every_shipped_example_keeps_program_on_every_output() {
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    let mut seen = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let Ok(cfg) = MultiviewConfig::load_from_toml(&std::fs::read_to_string(&path).unwrap())
        else {
            continue;
        };
        if cfg.routing.is_some() {
            continue;
        }
        seen += 1;
        for feed in cfg.output_feeds() {
            assert_eq!(feed.effective_video(), VideoFeed::PROGRAM, "{path:?}");
            assert_eq!(feed.audio, AudioFeed::Follow, "{path:?}");
        }
        let json = cfg.to_json().unwrap();
        let reparsed = MultiviewConfig::load_from_json(&json).unwrap();
        assert_eq!(reparsed.to_json().unwrap(), json, "{path:?}");
        assert!(reparsed.routing.is_none(), "{path:?}");
    }
    assert!(seen >= 5, "expected shipped examples, saw {seen}");
}

/// `[audio]` with two named discrete tracks, and the outputs of [`BASE`]
/// plus one of each transport family for the capability rule.
const TRACKS: &str = r#"
[audio]
sample_rate_hz = 48000
[[audio.routes]]
input_id = "a"
channels = { kind = "stereo" }
target_track = "eng"
[[audio.routes]]
input_id = "bars"
channels = { kind = "stereo" }
target_track = "deu"
"#;

fn feed(output: &str, audio: &str) -> String {
    format!("\n[[routing.output]]\noutput = \"{output}\"\naudio = {audio}\n")
}

#[test]
fn explicit_program_none_and_source_audio_feeds_validate() {
    for audio in [r#""program""#, r#""none""#, r#"{ source = "bars" }"#] {
        let cfg = load(&feed("srt-out", audio)).unwrap();
        assert!(!cfg.output_feeds()[1].audio.is_follow(), "{audio}");
        let parsed = MultiviewConfig::load_from_json(&cfg.to_json().unwrap()).unwrap();
        assert_eq!(parsed, cfg, "{audio} round-trips");
    }
    let err = load(&feed("srt-out", r#"{ source = "nope" }"#)).unwrap_err();
    assert!(err.contains("unknown source \"nope\""), "{err}");
}

#[test]
fn a_tracks_feed_names_declared_distinct_tracks() {
    load(&format!(
        "{TRACKS}{}",
        feed("srt-out", r#"{ tracks = ["eng"] }"#)
    ))
    .unwrap();
    let err = load(&format!("{TRACKS}{}", feed("srt-out", r"{ tracks = [] }"))).unwrap_err();
    assert!(err.contains("at least one track"), "{err}");
    let err = load(&format!(
        "{TRACKS}{}",
        feed("srt-out", r#"{ tracks = ["fra"] }"#)
    ))
    .unwrap_err();
    assert!(err.contains("undeclared track \"fra\""), "{err}");
    // `prog` is the program bus, not a discrete track.
    let err = load(&format!(
        "{TRACKS}{}",
        feed("srt-out", r#"{ tracks = ["prog"] }"#)
    ))
    .unwrap_err();
    assert!(err.contains("undeclared track"), "{err}");
    let err = load(&format!(
        "{TRACKS}{}",
        feed("srt-out", r#"{ tracks = ["eng", "eng"] }"#)
    ))
    .unwrap_err();
    assert!(err.contains("twice"), "{err}");
    // Without an [audio] block nothing is declared.
    let err = load(&feed("srt-out", r#"{ tracks = ["eng"] }"#)).unwrap_err();
    assert!(err.contains("undeclared track"), "{err}");
}

#[test]
fn more_than_one_track_only_where_tracks_are_carried_at_once() {
    // HLS (select-one): one track only.
    let err = load(&format!(
        "{TRACKS}{}",
        feed("srt-out", r#"{ tracks = ["eng", "deu"] }"#)
    ))
    .unwrap_err();
    assert!(err.contains("cannot carry 2 audio tracks at once"), "{err}");
    // SRT (MPEG-TS PIDs, simultaneous): any number.
    let srt = "\n[[outputs]]\nid = \"srt\"\nkind = \"srt\"\nurl = \"srt://[::1]:9000\"\ncodec = \"mpeg2video\"\n";
    let doc = format!(
        "{srt}{TRACKS}{}",
        feed("srt", r#"{ tracks = ["eng", "deu"] }"#)
    );
    let cfg = MultiviewConfig::load_from_toml(&format!("{BASE}{doc}")).unwrap();
    cfg.validate().unwrap();
}

#[test]
fn start_up_accepts_an_explicit_audio_feed_it_re_applies_as_a_boot_take() {
    // ADR-0108 slice 2: a persisted explicit audio feed is admitted; the run
    // re-applies it as an audio-only boot take (it used to be refused).
    for audio in [
        r#""none""#,
        r#""program""#,
        r#"{ source = "bars" }"#,
        r#""follow""#,
    ] {
        let cfg = load(&feed("srt-out", audio)).unwrap();
        multiview_config::outputs_carry_program(&cfg).unwrap();
    }
    // #329 F1 (behaviour intentionally changed): a directly routed source
    // video feed is accepted at start-up (the run issues it as a boot take);
    // a layout feed is accepted too (below).
    let cfg = load("\n[[routing.output]]\noutput = \"srt-out\"\nvideo = { source = \"bars\" }\n")
        .unwrap();
    multiview_config::outputs_carry_program(&cfg).unwrap();
    let layout = MultiviewConfig::load_from_toml(&format!(
        "{BASE}\n[[routing.output]]\noutput = \"srt-out\"\nvideo = {{ layout = \"L\" }}\n"
    ))
    .unwrap();
    // ADR-0108 slice 3 (#306, behaviour intentionally changed): a persisted
    // layout feed no longer refuses a start (it starts on Program).
    multiview_config::outputs_carry_program(&layout).unwrap();
}

fn routing(tracks: &[(&str, &str)]) -> multiview_config::AudioRouting {
    use std::fmt::Write as _;
    let mut doc = String::from("[audio]\nsample_rate_hz = 48000\n");
    for (input, track) in tracks {
        let _ = writeln!(
            doc,
            "[[audio.routes]]\ninput_id = \"{input}\"\nchannels = {{ kind = \"stereo\" }}\ntarget_track = \"{track}\""
        );
    }
    MultiviewConfig::load_from_toml(&format!("{BASE}{doc}"))
        .unwrap()
        .audio
        .unwrap()
}

#[test]
fn a_renamed_track_is_rewritten_in_every_tracks_feed() {
    // Two tracks on `restream` (HLS, select-one) would not validate; the
    // propagation unit works on the parsed document.
    let mut cfg = MultiviewConfig::load_from_toml(&format!(
        "{BASE}{TRACKS}{}{}",
        feed("restream", r#"{ tracks = ["eng", "deu"] }"#),
        feed("srt-out", r#""program""#)
    ))
    .unwrap();
    let old = routing(&[("a", "eng"), ("bars", "deu")]);
    let new = routing(&[("a", "english"), ("bars", "deu")]);
    let updates =
        multiview_config::propagate_routing_feed_change(Some(&old), &new, cfg.routing.as_mut());
    assert_eq!(updates.len(), 1, "{updates:?}");
    assert_eq!(updates[0].output, "restream");
    assert_eq!(
        updates[0].renamed,
        vec![("eng".to_owned(), "english".to_owned())]
    );
    assert!(updates[0].removed.is_empty() && !updates[0].became_program);
    let feeds = cfg.output_feeds();
    assert_eq!(
        feeds[0].audio,
        AudioFeed::Tracks(vec!["english".into(), "deu".into()])
    );
    assert_eq!(feeds[1].audio, AudioFeed::Program, "untouched");
}

#[test]
fn a_removed_track_is_dropped_and_an_emptied_feed_becomes_program() {
    let mut cfg = MultiviewConfig::load_from_toml(&format!(
        "{BASE}{TRACKS}{}{}",
        feed("restream", r#"{ tracks = ["eng", "deu"] }"#),
        feed("srt-out", r#"{ tracks = ["deu"] }"#)
    ))
    .unwrap();
    let old = routing(&[("a", "eng"), ("bars", "deu")]);
    let new = routing(&[("a", "eng")]);
    let updates =
        multiview_config::propagate_routing_feed_change(Some(&old), &new, cfg.routing.as_mut());
    assert_eq!(updates.len(), 2, "{updates:?}");
    let feeds = cfg.output_feeds();
    assert_eq!(feeds[0].audio, AudioFeed::Tracks(vec!["eng".into()]));
    assert_eq!(feeds[1].audio, AudioFeed::Program, "an emptied feed");
    assert!(updates[1].became_program);
    assert_eq!(updates[1].removed, vec!["deu".to_owned()]);
    cfg.validate().unwrap();
    // Nothing changed: no update.
    let none =
        multiview_config::propagate_routing_feed_change(Some(&new), &new, cfg.routing.as_mut());
    assert_eq!(none, Vec::new());
    let first = multiview_config::propagate_routing_feed_change(None, &new, cfg.routing.as_mut());
    assert_eq!(first, Vec::new(), "no prior document: nothing to propagate");
}
