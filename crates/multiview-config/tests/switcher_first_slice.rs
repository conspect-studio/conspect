//! The first switcher configuration slice fails closed on unknown buses/references.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use multiview_config::{ConfigDiff, MultiviewConfig, OutputBus};

const BASE: &str = r##"
schema_version = 1
[canvas]
width = 640
height = 360
fps = "25/1"
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
id = "b"
kind = "test"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "a"
[[outputs]]
id = "out1"
kind = "hls"
path = "/tmp/switcher-first-slice/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;

fn config() -> MultiviewConfig {
    MultiviewConfig::load_from_toml(BASE).unwrap()
}

fn me(cfg: &mut MultiviewConfig) {
    cfg.switcher = Some(multiview_config::SwitcherConfig {
        mix_effects: vec![multiview_config::MixEffectsConfig {
            id: "me1".into(),
            program: "main".into(),
            program_source: "a".into(),
            preview_source: "b".into(),
            flip_flop: true,
            inputs: vec![],
            program_input: None,
            preview_input: None,
            transition: None,
        }],
        extra: serde_json::Map::new(),
    });
}

#[test]
fn legacy_config_is_unchanged_and_audio_off() {
    let cfg = config();
    cfg.validate().unwrap();
    assert_eq!(cfg.effective_programs()[0].id.as_str(), "main");
    assert!(!cfg.declares_program_audio());
    assert_eq!(cfg.routing_table().output[0].bus, OutputBus::Program);
    let json = cfg.to_json().unwrap();
    assert!(!json.contains("\"switcher\""));
    assert!(!json.contains("\"programs\""));
    assert!(!json.contains("\"bus\""));
}

#[test]
fn declared_me_and_preview_output_validate_and_round_trip() {
    let mut cfg = config();
    me(&mut cfg);
    cfg.routing = Some(cfg.routing_table());
    cfg.routing.as_mut().unwrap().output[0].bus = OutputBus::Preview;
    cfg.validate().unwrap();
    assert!(cfg.declares_program_audio());
    let parsed = MultiviewConfig::load_from_json(&cfg.to_json().unwrap()).unwrap();
    assert_eq!(cfg, parsed);
    assert_eq!(parsed.routing.unwrap().output[0].bus, OutputBus::Preview);
}

#[test]
fn preview_output_never_silently_falls_back_to_program() {
    let mut cfg = config();
    cfg.routing = Some(cfg.routing_table());
    cfg.routing.as_mut().unwrap().output[0].bus = OutputBus::Preview;
    assert!(cfg
        .validate()
        .unwrap_err()
        .to_string()
        .contains("without a declared M/E"));
    me(&mut cfg);
    cfg.validate().unwrap();
    cfg.switcher.as_mut().unwrap().mix_effects[0].preview_source = "missing".into();
    assert!(cfg
        .validate()
        .unwrap_err()
        .to_string()
        .contains("undeclared source"));
}

#[test]
fn unknown_bus_and_undeclared_program_fail_closed() {
    let mut cfg = config();
    cfg.routing = Some(cfg.routing_table());
    cfg.routing.as_mut().unwrap().output[0].program = "other".into();
    assert!(cfg
        .validate()
        .unwrap_err()
        .to_string()
        .contains("undeclared program"));
    let bad = cfg
        .to_json()
        .unwrap()
        .replace("\"other\"", "\"main\"")
        .replace(
            "\"program\": \"main\"",
            "\"program\": \"main\", \"bus\": \"unknown\"",
        );
    assert!(MultiviewConfig::load_from_json(&bad).is_err());
}

#[test]
fn switcher_and_program_edits_are_restart_class_not_silently_lost() {
    let old = config();
    let mut next = old.clone();
    me(&mut next);
    assert!(ConfigDiff::between(&old, &next)
        .changed_sections
        .contains("switcher"));
    next.switcher = None;
    next.programs = next.effective_programs();
    assert!(ConfigDiff::between(&old, &next)
        .changed_sections
        .contains("programs"));
}
