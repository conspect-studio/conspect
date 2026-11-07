//! AUD-7: audio routing config schema — round-trip serde (TOML + JSON) and
//! reference/consistency validation.
//!
//! These tests pin the declarative half (the runtime that consumes the routes
//! is AUD-3/AUD-4, out of scope here): a valid multi-track routing document
//! parses and round-trips losslessly across TOML and JSON, and every semantic
//! invariant `validate()` enforces rejects a document that violates it with a
//! typed [`multiview_config::ConfigError`] — never a panic.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::{
    propagate_routing_change, AudioChannels, AudioRouting, ConfigError, MultiviewConfig,
    OutputAudio, OutputAudioMode,
};

/// A complete, valid document carrying a program bus + two discrete tracks and a
/// per-output audio selection, used as the base for mutation.
const BASE: &str = r##"
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

[[cells]]
id = "cell_b"
rect = { x = 0.5, y = 0.0, w = 0.5, h = 1.0 }
[cells.source]
input_id = "cam_b"

[[outputs]]
kind = "rtsp_server"
mount = "/multiview"
codec = "h264"
[outputs.audio]
mode = "tracks"
tracks = ["prog", "trk_a", "trk_b"]

[audio]
sample_rate_hz = 48000

[[audio.routes]]
input_id = "cam_a"
channels = { kind = "stereo" }
target_track = "trk_a"
language = "eng"
title = "Camera A"
include_in_program_bus = true
gain_db = -3.0
mute = false

[[audio.routes]]
input_id = "cam_b"
channels = { kind = "stereo" }
target_track = "trk_b"
language = "deu"
title = "Camera B"
include_in_program_bus = true
gain_db = 0.0
mute = false
"##;

fn parse(doc: &str) -> MultiviewConfig {
    MultiviewConfig::load_from_toml(doc).expect("BASE-derived doc must parse")
}

#[test]
fn base_document_is_valid() {
    let cfg = parse(BASE);
    cfg.validate()
        .expect("the base routing document must validate");

    let audio = cfg.audio.as_ref().expect("audio block present");
    assert_eq!(audio.sample_rate_hz, 48_000);
    assert_eq!(audio.routes.len(), 2);
    assert_eq!(audio.routes[0].input_id, "cam_a");
    assert_eq!(audio.routes[0].target_track.as_deref(), Some("trk_a"));
    assert!(audio.routes[0].include_in_program_bus);
    assert_eq!(audio.routes[0].channels, AudioChannels::Stereo);
}

#[test]
fn round_trips_through_toml_and_json_losslessly() {
    let cfg = parse(BASE);

    let toml_text = cfg.to_toml().expect("serialize to TOML");
    let from_toml = MultiviewConfig::load_from_toml(&toml_text).expect("re-parse TOML");
    assert_eq!(cfg, from_toml, "TOML round-trip must be lossless");

    let json_text = cfg.to_json().expect("serialize to JSON");
    let from_json = MultiviewConfig::load_from_json(&json_text).expect("re-parse JSON");
    assert_eq!(cfg, from_json, "JSON round-trip must be lossless");

    // Cross-format identity: TOML→value and JSON→value must agree (the union
    // tags are robust across the self-describing and non-self-describing forms).
    assert_eq!(
        from_toml, from_json,
        "TOML and JSON must decode identically"
    );
}

#[test]
fn rejects_route_referencing_unknown_source() {
    let doc = BASE.replace(r#"input_id = "cam_a""#, r#"input_id = "ghost""#);
    let cfg = parse(&doc);
    let err = cfg.validate().expect_err("unknown source must be rejected");
    assert!(matches!(err, ConfigError::Validation(_)), "got {err:?}");
    assert!(
        err.to_string().contains("ghost"),
        "names the bad ref: {err}"
    );
}

#[test]
fn rejects_output_audio_selecting_unknown_track() {
    let doc = BASE.replace(r#""prog", "trk_a", "trk_b""#, r#""prog", "trk_a", "ghost""#);
    let cfg = parse(&doc);
    let err = cfg
        .validate()
        .expect_err("unknown track selection must be rejected");
    assert!(matches!(err, ConfigError::Validation(_)), "got {err:?}");
    assert!(
        err.to_string().contains("ghost"),
        "names the bad track: {err}"
    );
}

#[test]
fn rejects_duplicate_target_track() {
    // Two routes claiming the same discrete track is ambiguous wiring.
    let doc = BASE.replace(r#"target_track = "trk_b""#, r#"target_track = "trk_a""#);
    let cfg = parse(&doc);
    let err = cfg
        .validate()
        .expect_err("duplicate target_track must be rejected");
    assert!(matches!(err, ConfigError::Validation(_)), "got {err:?}");
    assert!(err.to_string().contains("trk_a"), "names the track: {err}");
}

#[test]
fn rejects_duplicate_route_for_same_input() {
    // Two routes for the same input_id is a duplicate declaration.
    let doc = BASE.replace(
        r#"input_id = "cam_b"
channels = { kind = "stereo" }
target_track = "trk_b""#,
        r#"input_id = "cam_a"
channels = { kind = "stereo" }
target_track = "trk_b""#,
    );
    let cfg = parse(&doc);
    let err = cfg
        .validate()
        .expect_err("duplicate input route must be rejected");
    assert!(matches!(err, ConfigError::Validation(_)), "got {err:?}");
    assert!(err.to_string().contains("cam_a"), "names the input: {err}");
}

#[test]
fn rejects_program_bus_with_only_muted_members() {
    // A program-bus member that is muted contributes nothing; if EVERY bus
    // member is muted the program bus is silent — reject rather than ship a
    // dead bus an operator did not intend. (A low *gain* is NOT treated as
    // non-contributing — a quiet member is still audio the operator may want.)
    let doc = BASE
        .replace(
            r"include_in_program_bus = true
gain_db = -3.0
mute = false",
            r"include_in_program_bus = true
gain_db = -3.0
mute = true",
        )
        .replace(
            r"include_in_program_bus = true
gain_db = 0.0
mute = false",
            r"include_in_program_bus = true
gain_db = 0.0
mute = true",
        );
    let cfg = parse(&doc);
    let err = cfg
        .validate()
        .expect_err("an all-muted program bus must be rejected");
    assert!(matches!(err, ConfigError::Validation(_)), "got {err:?}");
    assert!(
        err.to_string().to_lowercase().contains("program"),
        "mentions the program bus: {err}"
    );
}

#[test]
fn rejects_non_finite_gain() {
    let doc = BASE.replace("gain_db = -3.0", "gain_db = nan");
    let cfg = parse(&doc);
    let err = cfg.validate().expect_err("a NaN gain must be rejected");
    assert!(matches!(err, ConfigError::Validation(_)), "got {err:?}");
}

#[test]
fn rejects_zero_sample_rate() {
    let doc = BASE.replace("sample_rate_hz = 48000", "sample_rate_hz = 0");
    let cfg = parse(&doc);
    let err = cfg
        .validate()
        .expect_err("a zero sample rate must be rejected");
    assert!(matches!(err, ConfigError::Validation(_)), "got {err:?}");
}

#[test]
fn rejects_empty_route_input_id() {
    let doc = BASE.replace(r#"input_id = "cam_a""#, r#"input_id = """#);
    let cfg = parse(&doc);
    let err = cfg
        .validate()
        .expect_err("an empty route input_id must be rejected");
    assert!(matches!(err, ConfigError::Validation(_)), "got {err:?}");
}

#[test]
fn programmatic_construction_validates() {
    // The schema is usable directly via its public validation seams (the DTOs
    // are `#[non_exhaustive]`, matching the rest of this crate, so they are
    // authored declaratively and decoded — never built by struct literal
    // downstream). Decode a routing block and check its consistency seam.
    let routing: AudioRouting = serde_json::from_str(
        r#"{
            "sample_rate_hz": 48000,
            "routes": [
                {
                    "input_id": "cam_a",
                    "channels": { "kind": "mono" },
                    "target_track": "trk_a",
                    "include_in_program_bus": true,
                    "gain_db": 0.0,
                    "mute": false
                }
            ]
        }"#,
    )
    .expect("routing JSON decodes");
    assert_eq!(routing.routes.len(), 1);
    assert_eq!(routing.routes[0].channels, AudioChannels::Mono);
    assert!(routing.routes[0].contributes_to_program());

    // The constructed routing's own consistency check passes.
    let declared: Vec<&str> = vec!["cam_a"];
    routing
        .validate(&declared, &["prog", "trk_a"])
        .expect("a single sane route validates");

    // And an OutputAudio selecting a known track is consistent.
    let sel: OutputAudio =
        serde_json::from_str(r#"{ "mode": "tracks", "tracks": ["prog", "trk_a"] }"#)
            .expect("output-audio JSON decodes");
    assert_eq!(sel.mode, OutputAudioMode::Tracks);
    sel.validate("out0", &["prog", "trk_a"])
        .expect("a known-track selection validates");
}

#[test]
fn auto_mode_parses_validates_and_resolves_program_plus_every_track() {
    let doc = BASE.replace(
        "mode = \"tracks\"\ntracks = [\"prog\", \"trk_a\", \"trk_b\"]",
        "mode = \"auto\"",
    );
    let cfg = parse(&doc);
    cfg.validate().expect("auto mode validates");
    let sel = cfg.outputs[0].audio().expect("audio block");
    assert_eq!(sel.mode, OutputAudioMode::Auto);
    assert_eq!(
        sel.discrete_track_count(),
        0,
        "auto never fails the capability gate"
    );
    assert_eq!(
        sel.resolve(&["trk_a", "trk_b"]),
        (true, vec!["trk_a", "trk_b"])
    );
}

#[test]
fn resolve_honours_program_and_tracks_modes() {
    let tracks: OutputAudio =
        serde_json::from_value(serde_json::json!({ "mode": "tracks", "tracks": ["trk_b"] }))
            .expect("tracks selection");
    assert_eq!(tracks.resolve(&["trk_a", "trk_b"]), (false, vec!["trk_b"]));
    let program: OutputAudio =
        serde_json::from_value(serde_json::json!({ "mode": "program" })).expect("program");
    assert_eq!(program.resolve(&["trk_a"]), (true, Vec::<&str>::new()));
}

/// A renamed `target_track` (same `input_id`, different track name) must
/// rewrite an output's `mode = "tracks"` selection in the SAME atomic commit
/// as the routing replace (ADR-R005 §4.3 — the preferred deadlock fix).
#[test]
fn propagate_routing_change_rewrites_a_renamed_track_in_every_referencing_output() {
    let mut cfg = parse(BASE);
    let old_audio = cfg.audio.clone();
    // Rename cam_a's discrete track trk_a -> trk_z (same input_id, new name).
    let new_doc = BASE.replace(r#"target_track = "trk_a""#, r#"target_track = "trk_z""#);
    let new_cfg = parse(&new_doc);
    let new_audio = new_cfg.audio.clone().expect("new audio block");

    let updates = propagate_routing_change(old_audio.as_ref(), &new_audio, &mut cfg.outputs);

    assert_eq!(updates.len(), 1, "exactly one output referenced trk_a");
    assert_eq!(
        updates[0].renamed,
        vec![("trk_a".to_owned(), "trk_z".to_owned())]
    );
    assert_eq!(updates[0].removed, Vec::<String>::new());

    let selection = cfg.outputs[0].audio().expect("audio selection");
    assert_eq!(
        selection.tracks,
        vec!["prog".to_owned(), "trk_z".to_owned(), "trk_b".to_owned()],
        "the renamed track replaces the old name in place"
    );

    // The rewritten output now validates against the NEW routing document.
    cfg.audio = Some(new_audio);
    cfg.validate().expect("rewritten output validates");
}

/// Removing a route's discrete track (or the whole route) must drop that
/// track from every output selection that referenced it.
#[test]
fn propagate_routing_change_drops_a_removed_track_from_every_referencing_output() {
    let mut cfg = parse(BASE);
    let old_audio = cfg.audio.clone();
    // cam_b no longer declares a discrete track at all.
    let new_doc = BASE.replace(
        r#"target_track = "trk_b"
language = "deu""#,
        r#"language = "deu""#,
    );
    let new_cfg = parse(&new_doc);
    let new_audio = new_cfg.audio.clone().expect("new audio block");

    let updates = propagate_routing_change(old_audio.as_ref(), &new_audio, &mut cfg.outputs);

    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].removed, vec!["trk_b".to_owned()]);
    assert_eq!(updates[0].renamed, Vec::<(String, String)>::new());

    let selection = cfg.outputs[0].audio().expect("audio selection");
    assert_eq!(
        selection.tracks,
        vec!["prog".to_owned(), "trk_a".to_owned()],
        "the removed track is dropped, the rest survives"
    );

    cfg.audio = Some(new_audio);
    cfg.validate().expect("rewritten output validates");
}

/// An output whose selection would become empty after removal falls back to
/// `["prog"]` rather than landing an invalid empty `mode = "tracks"` list.
#[test]
fn propagate_routing_change_falls_back_to_prog_when_a_selection_would_empty() {
    let doc = BASE.replace(
        r#"tracks = ["prog", "trk_a", "trk_b"]"#,
        r#"tracks = ["trk_a", "trk_b"]"#,
    );
    let mut cfg = parse(&doc);
    let old_audio = cfg.audio.clone();
    // Remove BOTH discrete tracks (no target_track on either route).
    let new_doc = doc
        .replace(
            r#"target_track = "trk_a"
language = "eng""#,
            r#"language = "eng""#,
        )
        .replace(
            r#"target_track = "trk_b"
language = "deu""#,
            r#"language = "deu""#,
        );
    let new_cfg = parse(&new_doc);
    let new_audio = new_cfg.audio.clone().expect("new audio block");

    let updates = propagate_routing_change(old_audio.as_ref(), &new_audio, &mut cfg.outputs);
    assert_eq!(updates.len(), 1);
    assert_eq!(
        updates[0].removed,
        vec!["trk_a".to_owned(), "trk_b".to_owned()]
    );

    let selection = cfg.outputs[0].audio().expect("audio selection");
    assert_eq!(
        selection.tracks,
        vec!["prog".to_owned()],
        "an emptied tracks list falls back to the program bus"
    );

    cfg.audio = Some(new_audio);
    cfg.validate().expect("fallback selection validates");
}

/// No routing change at all (identical document) yields no updates and
/// touches no output.
#[test]
fn propagate_routing_change_is_a_no_op_when_nothing_changed() {
    let mut cfg = parse(BASE);
    let audio = cfg.audio.clone().expect("audio block");
    let before = cfg.outputs.clone();
    let updates = propagate_routing_change(Some(&audio), &audio, &mut cfg.outputs);
    assert_eq!(updates.len(), 0);
    assert_eq!(cfg.outputs, before);
}

/// A first-ever routing document (`old = None`) has nothing to propagate
/// against — it can declare any tracks without touching existing outputs.
#[test]
fn propagate_routing_change_with_no_prior_document_is_a_no_op() {
    let mut cfg = parse(BASE);
    let audio = cfg.audio.clone().expect("audio block");
    let before = cfg.outputs.clone();
    let updates = propagate_routing_change(None, &audio, &mut cfg.outputs);
    assert_eq!(updates.len(), 0);
    assert_eq!(cfg.outputs, before);
}
