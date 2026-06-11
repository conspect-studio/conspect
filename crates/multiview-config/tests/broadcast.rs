//! Schema + validation tests for the broadcast monitoring config-as-code
//! surface: fault probes, tally profiles, salvos, and multi-head walls.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::PathBuf;

use multiview_config::{
    LoudnessTarget, MultiviewConfig, Probe, ProbeKind, Salvo, TallyProfile, WallConfig,
};
use multiview_core::tally::TallyColor;

/// Absolute path to the workspace `examples/` directory.
fn examples_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
}

/// Load the broadcast example document.
fn load_broadcast() -> MultiviewConfig {
    let path = examples_dir().join("broadcast-monitor.toml");
    let text = std::fs::read_to_string(&path).expect("read broadcast-monitor.toml");
    MultiviewConfig::load_from_toml(&text).expect("load broadcast-monitor.toml")
}

#[test]
fn broadcast_example_loads_and_validates() {
    let cfg = load_broadcast();
    cfg.validate().expect("broadcast example should validate");
    assert_eq!(cfg.schema_version, 1);

    // The monitoring surface is populated.
    assert_eq!(cfg.probes.len(), 4, "four probes");
    assert_eq!(cfg.tally_profiles.len(), 1, "one tally profile");
    assert_eq!(cfg.salvos.len(), 1, "one salvo");
    assert_eq!(cfg.walls.len(), 1, "one wall");
}

#[test]
fn broadcast_example_round_trips_through_json_and_toml() {
    let cfg = load_broadcast();

    let json = cfg.to_json().expect("to_json");
    let from_json = MultiviewConfig::load_from_json(&json).expect("from_json");
    assert_eq!(cfg, from_json, "JSON round-trip");

    let toml_text = cfg.to_toml().expect("to_toml");
    let from_toml = MultiviewConfig::load_from_toml(&toml_text).expect("from_toml");
    assert_eq!(cfg, from_toml, "TOML round-trip");
}

#[test]
fn probe_kinds_map_to_alarm_kinds_and_carry_thresholds() {
    let cfg = load_broadcast();

    let black = cfg
        .probes
        .iter()
        .find(|p| p.id == "black_a")
        .expect("black_a probe");
    assert_eq!(
        black.kind.alarm_kind(),
        multiview_core::alarm::AlarmKind::Black
    );
    match black.kind {
        ProbeKind::Black {
            luma_threshold,
            zone,
        } => {
            assert_eq!(luma_threshold, 16);
            assert!((zone.w - 0.8).abs() < f32::EPSILON);
        }
        _ => panic!("black_a should be a Black probe"),
    }
    assert_eq!(black.dwell.up_ms, 2000);
    assert_eq!(black.dwell.down_ms, 500);
    assert_eq!(
        black.severity,
        multiview_core::alarm::PerceivedSeverity::Major
    );

    let loud = cfg
        .probes
        .iter()
        .find(|p| p.id == "loud_d")
        .expect("loud_d probe");
    assert_eq!(
        loud.kind.alarm_kind(),
        multiview_core::alarm::AlarmKind::LoudnessViolation
    );
    match loud.kind {
        ProbeKind::Loudness {
            target: LoudnessTarget::R128 { target_lufs, .. },
        } => assert!((target_lufs - (-23.0)).abs() < f32::EPSILON),
        _ => panic!("loud_d should be an R128 loudness probe"),
    }
}

#[test]
fn probe_dwell_and_zone_default_when_omitted() {
    // A document with a black probe that omits zone+dwell uses the full-frame
    // zone and the symmetric one-second dwell.
    let doc = make_doc_with_extra(
        r#"
[[probes]]
id = "p1"
cell = "tile_a"
kind = "freeze"
difference_threshold = 3
"#,
    );
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
    cfg.validate().expect("validate");
    let probe = &cfg.probes[0];
    assert_eq!(probe.dwell.up_ms, 1000);
    assert_eq!(probe.dwell.down_ms, 1000);
    match probe.kind {
        ProbeKind::Freeze { zone, .. } => {
            assert!((zone.x).abs() < f32::EPSILON);
            assert!((zone.w - 1.0).abs() < f32::EPSILON);
        }
        _ => panic!("expected freeze"),
    }
    // Severity defaults to Cleared (the no-alarm default) when omitted.
    assert_eq!(
        probe.severity,
        multiview_core::alarm::PerceivedSeverity::Cleared
    );
}

#[test]
fn probe_referencing_unknown_cell_is_rejected() {
    let doc = make_doc_with_extra(
        r#"
[[probes]]
id = "p_bad"
cell = "NO_SUCH_CELL"
kind = "black"
luma_threshold = 16
"#,
    );
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
    let err = cfg.validate().expect_err("unknown cell must fail");
    assert!(
        err.to_string().contains("NO_SUCH_CELL"),
        "error names the bad cell: {err}"
    );
}

#[test]
fn probe_with_out_of_range_zone_is_rejected() {
    let doc = make_doc_with_extra(
        r#"
[[probes]]
id = "p_zone"
cell = "tile_a"
kind = "black"
luma_threshold = 16
zone = { x = 0.5, y = 0.0, w = 0.9, h = 0.5 }
"#,
    );
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
    // x + w = 1.4 > 1.0
    assert!(cfg.validate().is_err(), "out-of-range zone must fail");
}

#[test]
fn duplicate_probe_id_is_rejected() {
    let doc = make_doc_with_extra(
        r#"
[[probes]]
id = "dup"
cell = "tile_a"
kind = "black"
luma_threshold = 16
[[probes]]
id = "dup"
cell = "tile_b"
kind = "black"
luma_threshold = 16
"#,
    );
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
    let err = cfg.validate().expect_err("duplicate probe id must fail");
    assert!(err.to_string().contains("dup"), "names the dup id: {err}");
}

#[test]
fn tally_profile_resolves_index_and_word() {
    let cfg = load_broadcast();
    let profile = &cfg.tally_profiles[0];
    assert_eq!(profile.cell_for_index(2), Some("tile_c"));
    assert_eq!(profile.cell_for_index(99), None);
    // bit 0 -> red.
    assert_eq!(profile.color_for_word(0b001), TallyColor::Red);
    // bit 1 (green) declared after bit 0; word with both lit -> latest lit wins.
    assert_eq!(profile.color_for_word(0b011), TallyColor::Green);
    // no lit bit -> off.
    assert_eq!(profile.color_for_word(0), TallyColor::Off);
}

#[test]
fn tally_profile_with_duplicate_bit_is_rejected() {
    let doc = make_doc_with_extra(
        r#"
[[tally_profiles]]
id = "tp"
bit_colors = [
  { bit = 0, color = "Red" },
  { bit = 0, color = "Green" },
]
"#,
    );
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
    let err = cfg.validate().expect_err("duplicate bit must fail");
    assert!(err.to_string().contains("bit 0"), "names the bit: {err}");
}

#[test]
fn tally_profile_index_to_unknown_cell_is_rejected() {
    let doc = make_doc_with_extra(
        r#"
[[tally_profiles]]
id = "tp"
index_cells = [{ index = 0, cell = "ghost" }]
"#,
    );
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
    let err = cfg.validate().expect_err("unknown cell must fail");
    assert!(err.to_string().contains("ghost"), "names the cell: {err}");
}

#[test]
fn salvo_recalls_resolve_and_round_trip() {
    let cfg = load_broadcast();
    let salvo = &cfg.salvos[0];
    assert_eq!(salvo.id, "vtr_review");
    assert_eq!(salvo.layout.as_deref(), Some("1+5"));
    assert_eq!(salvo.sources.len(), 1);
    assert_eq!(salvo.sources[0].cell, "tile_a");
    assert_eq!(salvo.sources[0].input_id, "vtr_1");
    assert_eq!(salvo.tally[0].color, TallyColor::Amber);
    assert_eq!(salvo.umd[0].text, "VTR 1 — REVIEW");
}

#[test]
fn empty_salvo_is_rejected() {
    let doc = make_doc_with_extra(
        r#"
[[salvos]]
id = "nothing"
"#,
    );
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
    let err = cfg.validate().expect_err("empty salvo must fail");
    assert!(
        err.to_string().contains("nothing"),
        "names the salvo: {err}"
    );
}

#[test]
fn salvo_binding_unknown_source_is_rejected() {
    let doc = make_doc_with_extra(
        r#"
[[salvos]]
id = "s1"
sources = [{ cell = "tile_a", input_id = "ghost_src" }]
"#,
    );
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
    let err = cfg.validate().expect_err("unknown source must fail");
    assert!(
        err.to_string().contains("ghost_src"),
        "names the source: {err}"
    );
}

#[test]
fn salvo_rebinding_a_cell_twice_is_rejected() {
    let doc = make_doc_with_extra(
        r#"
[[salvos]]
id = "s_dup"
sources = [
  { cell = "tile_a", input_id = "cam_a" },
  { cell = "tile_a", input_id = "cam_b" },
]
"#,
    );
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
    let err = cfg.validate().expect_err("double-bound cell must fail");
    assert!(err.to_string().contains("tile_a"), "names the cell: {err}");
}

#[test]
fn wall_lowers_to_a_valid_core_video_wall() {
    let cfg = load_broadcast();
    let wall = &cfg.walls[0];
    assert_eq!(wall.cols, 2);
    assert_eq!(wall.rows, 1);
    assert_eq!(wall.heads.len(), 2);

    let core = wall.to_core();
    core.validate().expect("core wall validates");
    assert_eq!(core.heads[0].id, "head_left");
    assert_eq!(core.heads[0].canvas.fps_num, 25);
    assert_eq!(core.heads[0].canvas.fps_den, 1);
    assert_eq!(core.bezel.horizontal_px, 12);
}

#[test]
fn wall_with_wrong_head_count_is_rejected() {
    let doc = make_doc_with_extra(
        r#"
[[walls]]
name = "bad_wall"
cols = 2
rows = 2
heads = [
  { id = "h1", width = 1920, height = 1080, fps = "25/1", layout = "x" },
]
"#,
    );
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
    assert!(cfg.validate().is_err(), "head count != cols*rows must fail");
}

#[test]
fn wall_with_duplicate_head_id_is_rejected() {
    let doc = make_doc_with_extra(
        r#"
[[walls]]
name = "dup_head_wall"
cols = 2
rows = 1
heads = [
  { id = "h", width = 1920, height = 1080, fps = "25/1", layout = "x" },
  { id = "h", width = 1920, height = 1080, fps = "25/1", layout = "y" },
]
"#,
    );
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
    let err = cfg.validate().expect_err("duplicate head id must fail");
    assert!(err.to_string().contains("\"h\""), "names the head: {err}");
}

#[test]
fn wall_head_rejects_float_fps_at_parse_time() {
    let doc = make_doc_with_extra(
        r#"
[[walls]]
name = "w"
cols = 1
rows = 1
heads = [
  { id = "h", width = 1920, height = 1080, fps = 25.0, layout = "x" },
]
"#,
    );
    assert!(
        MultiviewConfig::load_from_toml(&doc).is_err(),
        "float fps in a head must fail to parse"
    );
}

#[test]
fn probe_severity_defaults_to_cleared_when_omitted() {
    // A silence probe authored without `severity` deserializes to the Cleared
    // (no-alarm) default and maps to the Silence alarm kind.
    let doc = make_doc_with_extra(
        r#"
[[probes]]
id = "p"
cell = "tile_a"
kind = "silence"
level_dbfs = -50.0
"#,
    );
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
    cfg.validate().expect("validate");
    let probe: &Probe = &cfg.probes[0];
    assert_eq!(
        probe.severity,
        multiview_core::alarm::PerceivedSeverity::Cleared
    );
    assert_eq!(
        probe.kind.alarm_kind(),
        multiview_core::alarm::AlarmKind::Silence
    );
}

#[test]
fn a_layout_only_salvo_and_a_portrait_head_round_trip() {
    // A salvo that only recalls a layout is a valid (non-empty) salvo, and a
    // head can declare a portrait orientation with an NTSC rational fps.
    let doc = make_doc_with_extra(
        r#"
[[salvos]]
id = "s"
layout = "2x2"

[[tally_profiles]]
id = "tp_empty"

[[walls]]
name = "w"
cols = 1
rows = 1
heads = [
  { id = "h", width = 1280, height = 720, fps = "30000/1001", orientation = "Portrait", layout = "main" },
]
"#,
    );
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
    cfg.validate().expect("validate");

    let salvo: &Salvo = &cfg.salvos[0];
    assert_eq!(salvo.layout.as_deref(), Some("2x2"));

    let profile: &TallyProfile = &cfg.tally_profiles[0];
    assert!(profile.bit_colors.is_empty());

    let wall: &WallConfig = &cfg.walls[0];
    let core = wall.to_core();
    assert_eq!(
        core.heads[0].orientation,
        multiview_core::layout::Orientation::Portrait
    );
    assert_eq!(core.heads[0].canvas.fps_den, 1001);

    // The whole document survives a JSON round-trip with these features set.
    let json = cfg.to_json().expect("to_json");
    let back = MultiviewConfig::load_from_json(&json).expect("from_json");
    assert_eq!(cfg, back);
}

/// Build a minimal valid grid document and append `extra` TOML to it. The base
/// declares cells `tile_a`/`tile_b` bound to sources `cam_a`/`cam_b`.
fn make_doc_with_extra(extra: &str) -> String {
    let base = r##"
schema_version = 1
[canvas]
width = 1920
height = 1080
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr", "1fr"]
rows = ["1fr"]
gap = 0
areas = ["a b"]
[[sources]]
id = "cam_a"
kind = "test"
[[sources]]
id = "cam_b"
kind = "test"
[[cells]]
id = "tile_a"
area = "a"
[cells.source]
input_id = "cam_a"
[[cells]]
id = "tile_b"
area = "b"
[cells.source]
input_id = "cam_b"
[[outputs]]
kind = "rtsp_server"
mount = "/multiview"
codec = "h264"
"##;
    format!("{base}{extra}")
}
