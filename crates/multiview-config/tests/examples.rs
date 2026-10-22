//! Acceptance suite: every shipped `examples/*.toml` must load, round-trip
//! through JSON, and validate (including grid solving) successfully.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::PathBuf;

use multiview_config::MultiviewConfig;

/// Absolute path to the workspace `examples/` directory.
fn examples_dir() -> PathBuf {
    // CARGO_MANIFEST_DIR = crates/multiview-config
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
}

/// The example files that constitute the acceptance suite.
const EXAMPLES: &[&str] = &[
    "2x2.toml",
    "3x3.toml",
    "1plus5.toml",
    "pip.toml",
    "public-streams-2x2.toml",
    "broadcast-monitor.toml",
    "world-clock.toml",
    "webrtc.toml",
    "countdown-timer.toml",
];

#[test]
fn every_example_loads_and_validates() {
    let dir = examples_dir();
    for name in EXAMPLES {
        let path = dir.join(name);
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {name}: {e}"));
        let cfg =
            MultiviewConfig::load_from_toml(&text).unwrap_or_else(|e| panic!("load {name}: {e}"));
        cfg.validate()
            .unwrap_or_else(|e| panic!("validate {name}: {e}"));
        // Schema version is pinned to 1 in every shipped example.
        assert_eq!(cfg.schema_version, 1, "{name}: schema_version");
    }
}

#[test]
fn every_example_grid_solves_to_a_valid_core_layout() {
    let dir = examples_dir();
    for name in EXAMPLES {
        let path = dir.join(name);
        let text = std::fs::read_to_string(&path).unwrap();
        let cfg = MultiviewConfig::load_from_toml(&text).unwrap();
        let layout = cfg
            .solve_layout()
            .unwrap_or_else(|e| panic!("solve {name}: {e}"));
        // The solved core layout must itself pass core's structural validation.
        layout
            .validate()
            .unwrap_or_else(|e| panic!("core validate {name}: {e}"));
        // Every declared cell must produce exactly one solved core cell.
        assert_eq!(
            layout.cells.len(),
            cfg.cells.len(),
            "{name}: solved cell count"
        );
        // The output cadence must survive as an exact rational.
        assert!(layout.canvas.cadence().is_valid(), "{name}: cadence valid");
    }
}

#[test]
fn examples_round_trip_through_json_and_toml() {
    let dir = examples_dir();
    for name in EXAMPLES {
        let path = dir.join(name);
        let text = std::fs::read_to_string(&path).unwrap();
        let cfg = MultiviewConfig::load_from_toml(&text).unwrap();

        // TOML -> JSON -> back must preserve the document.
        let json = cfg
            .to_json()
            .unwrap_or_else(|e| panic!("{name} to_json: {e}"));
        let from_json = MultiviewConfig::load_from_json(&json)
            .unwrap_or_else(|e| panic!("{name} from_json: {e}"));
        assert_eq!(cfg, from_json, "{name}: JSON round-trip");

        // TOML -> TOML -> back must preserve the document.
        let toml_text = cfg
            .to_toml()
            .unwrap_or_else(|e| panic!("{name} to_toml: {e}"));
        let from_toml = MultiviewConfig::load_from_toml(&toml_text)
            .unwrap_or_else(|e| panic!("{name} from_toml: {e}"));
        assert_eq!(cfg, from_toml, "{name}: TOML round-trip");
    }
}

#[test]
fn known_fps_values_parse_to_exact_rationals() {
    use multiview_core::time::Rational;
    let dir = examples_dir();

    let cfg =
        MultiviewConfig::load_from_toml(&std::fs::read_to_string(dir.join("2x2.toml")).unwrap())
            .unwrap();
    assert_eq!(cfg.canvas.fps.rational(), Rational::FPS_29_97);

    let cfg =
        MultiviewConfig::load_from_toml(&std::fs::read_to_string(dir.join("3x3.toml")).unwrap())
            .unwrap();
    assert_eq!(cfg.canvas.fps.rational(), Rational::FPS_25);

    let cfg =
        MultiviewConfig::load_from_toml(&std::fs::read_to_string(dir.join("1plus5.toml")).unwrap())
            .unwrap();
    assert_eq!(cfg.canvas.fps.rational(), Rational::FPS_59_94);
}
