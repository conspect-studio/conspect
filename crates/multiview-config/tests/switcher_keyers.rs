//! Document-level validation of `[[switcher.downstream_keyers]]` (ADR-0113,
//! slice K2a): keyers are admitted on the document's own canvas, and Clean is
//! refused while a DSK is defined until pre-DSK Clean lands (K2b).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::{MultiviewConfig, OutputBus};

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
[[sources]]
id = "gfx"
kind = "test"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "a"
[[outputs]]
id = "out1"
kind = "hls"
path = "/tmp/switcher-keyers/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
[switcher]
[[switcher.mix_effects]]
id = "me1"
program = "main"
program_source = "a"
preview_source = "b"
"##;

const DSK: &str = r#"
[[switcher.downstream_keyers]]
id = "dsk1"
program = "main"
fill = "gfx"
"#;

fn load(extra: &str) -> MultiviewConfig {
    MultiviewConfig::load_from_toml(&format!("{BASE}{extra}")).unwrap()
}

#[test]
fn a_dsk_document_validates_and_round_trips() {
    let cfg = load(DSK);
    cfg.validate().unwrap();
    let switcher = cfg.switcher.as_ref().unwrap();
    assert_eq!(switcher.downstream_keyers.len(), 1);
    assert!(switcher.extra.is_empty(), "keyers are typed, not extra");
    let again = MultiviewConfig::load_from_toml(&cfg.to_toml().unwrap()).unwrap();
    assert_eq!(again.switcher, cfg.switcher);
}

#[test]
fn the_fly_is_checked_against_the_documents_canvas() {
    // Inside 1920x1080 but outside this 640x360 canvas.
    let cfg = load(&format!(
        "{DSK}fly = {{ x = 1280, y = 60, w = 576, h = 324 }}\n"
    ));
    let error = cfg.validate().unwrap_err().to_string();
    assert!(error.contains("outside the 640x360 canvas"), "{error}");
}

#[test]
fn two_full_canvas_dsks_exceed_the_default_budget() {
    let second = DSK.replace("dsk1", "dsk2") + "z = 2\n";
    let cfg = load(&format!("{DSK}{second}"));
    let error = cfg.validate().unwrap_err().to_string();
    assert!(error.contains("keyer_budget"), "{error}");
}

#[test]
fn clean_is_refused_while_a_dsk_is_defined() {
    let mut cfg = load(DSK);
    cfg.routing = Some(cfg.routing_table());
    cfg.routing.as_mut().unwrap().output[0].bus = OutputBus::Clean;
    let error = cfg.validate().unwrap_err().to_string();
    assert!(error.contains("clean-unavailable-with-dsk"), "{error}");
    // Without a DSK, Clean stays the Program alias of today.
    let mut legacy = load("");
    legacy.routing = Some(legacy.routing_table());
    legacy.routing.as_mut().unwrap().output[0].bus = OutputBus::Clean;
    legacy.validate().unwrap();
}

/// Review F3: a DSK is keyed on its program's M/E drive. A document that
/// defines a DSK but no M/E on that program would validate, then boot would
/// install no keyer (no `on_at_start`, leases held, Clean refused for
/// nothing), so validation refuses it.
#[test]
fn a_dsk_without_a_mix_effects_on_its_program_is_refused() {
    let without_me = BASE.replace(
        "[[switcher.mix_effects]]\nid = \"me1\"\nprogram = \"main\"\nprogram_source = \"a\"\npreview_source = \"b\"\n",
        "",
    );
    assert!(!without_me.contains("mix_effects"), "fixture edit applied");
    let cfg = MultiviewConfig::load_from_toml(&format!("{without_me}{DSK}")).unwrap();
    let error = cfg.validate().unwrap_err().to_string();
    assert!(error.contains("dsk-requires-mix-effects"), "{error}");
    // The same document with the M/E validates.
    load(DSK).validate().unwrap();
}
