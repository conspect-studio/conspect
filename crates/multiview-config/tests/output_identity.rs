//! RT-12 — stable output identity + `OutputRef` (ADR-0034 / RT-12 backlog row).
//!
//! Each configured output gains a stable `id`. The model is **backward
//! compatible**: an output that declares no `id` gets a *derived* stable id (its
//! [`Output::label`]), so a v1/v2 document keeps routing identically — the
//! desugared output crosspoints reference that derived id. A new
//! [`OutputRef`]`{ output, program }` addresses one output's program; `program`
//! defaults to `"main"` (the single-program world until ADR-0030's `ProgramSet`
//! lands).
//!
//! These tests pin: an explicit `id` round-trips through TOML/JSON; an absent
//! `id` derives the stable label; `OutputRef` round-trips internally-tagged
//! (never untagged) and defaults `program` to `"main"`; validation rejects a
//! duplicate output id and an `OutputRef` to an unknown output.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::routing::OutputRef;
use multiview_config::MultiviewConfig;

/// A v3 document declaring two outputs, the first with an explicit operator
/// `id`, the second relying on the derived (label-based) stable id.
const BASE: &str = r##"
schema_version = 3

[canvas]
width = 1920
height = 1080
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
id = "in_a"
kind = "test"

[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"

[[outputs]]
id = "program-out"
kind = "rtsp_server"
mount = "/multiview"
codec = "h264"

[[outputs]]
kind = "ndi"
name = "MV PROGRAM"
"##;

fn load() -> MultiviewConfig {
    MultiviewConfig::load_from_toml(BASE).expect("base parses")
}

#[test]
fn base_with_output_id_validates() {
    let cfg = load();
    cfg.validate()
        .expect("doc with an explicit output id validates");
}

#[test]
fn explicit_output_id_is_used() {
    let cfg = load();
    let first = &cfg.outputs[0];
    assert_eq!(
        first.id(),
        "program-out",
        "an explicit id is the output's stable id"
    );
}

#[test]
fn absent_output_id_derives_the_stable_label() {
    let cfg = load();
    let ndi = &cfg.outputs[1];
    // No explicit id ⇒ the derived stable id equals the output's label.
    assert_eq!(
        ndi.id(),
        ndi.label(),
        "an absent id derives the stable label"
    );
    assert_eq!(ndi.id(), "ndi MV PROGRAM");
}

#[test]
fn output_id_round_trips_through_toml_and_json() {
    let cfg = load();

    let json = cfg.to_json().expect("to_json");
    let from_json = MultiviewConfig::load_from_json(&json).expect("from_json");
    assert_eq!(cfg, from_json, "JSON round-trip is lossless");

    let toml_text = cfg.to_toml().expect("to_toml");
    let from_toml = MultiviewConfig::load_from_toml(&toml_text).expect("from_toml");
    assert_eq!(cfg, from_toml, "TOML round-trip is lossless");

    // The explicit id survives the round-trip.
    assert_eq!(from_json.outputs[0].id(), "program-out");
}

#[test]
fn duplicate_output_id_is_rejected() {
    // Two outputs sharing the same explicit id must be rejected (id uniqueness).
    let bad = BASE.replace(
        r#"kind = "ndi"
name = "MV PROGRAM""#,
        r#"id = "program-out"
kind = "ndi"
name = "MV PROGRAM""#,
    );
    let cfg = MultiviewConfig::load_from_toml(&bad).expect("parse");
    let err = cfg
        .validate()
        .expect_err("two outputs with the same id must be rejected");
    assert!(
        err.to_string().contains("program-out"),
        "error names the duplicate id, got: {err}"
    );
}

#[test]
fn derived_id_colliding_with_explicit_id_is_rejected() {
    // A derived id (the label) that collides with another output's explicit id is
    // still a duplicate — uniqueness is over the *resolved* ids, not just the
    // explicit ones.
    let bad = BASE.replace(r#"id = "program-out""#, r#"id = "ndi MV PROGRAM""#);
    let cfg = MultiviewConfig::load_from_toml(&bad).expect("parse");
    let err = cfg
        .validate()
        .expect_err("an explicit id colliding with another output's derived id must be rejected");
    assert!(
        err.to_string().contains("ndi MV PROGRAM"),
        "error names the colliding id, got: {err}"
    );
}

#[test]
fn empty_output_id_is_rejected() {
    let bad = BASE.replace(r#"id = "program-out""#, r#"id = """#);
    let cfg = MultiviewConfig::load_from_toml(&bad).expect("parse");
    let err = cfg
        .validate()
        .expect_err("an empty explicit output id must be rejected");
    assert!(
        err.to_string().to_lowercase().contains("empty"),
        "error explains the empty id, got: {err}"
    );
}

#[test]
fn output_ref_defaults_program_to_main() {
    // An OutputRef without a `program` defaults to "main".
    let json = serde_json::json!({ "output": "program-out" });
    let oref: OutputRef = serde_json::from_value(json).expect("de OutputRef without program");
    assert_eq!(oref.output, "program-out");
    assert_eq!(oref.program, "main", "program defaults to \"main\"");
}

#[test]
fn output_ref_round_trips_internally_tagged_never_untagged() {
    let oref = OutputRef::new("program-out", "main");
    let v = serde_json::to_value(&oref).expect("ser OutputRef");
    assert_eq!(
        v,
        serde_json::json!({ "output": "program-out", "program": "main" }),
        "OutputRef carries `output` + `program` at one level (never untagged)"
    );
    let back: OutputRef = serde_json::from_value(v).expect("de OutputRef");
    assert_eq!(oref, back, "OutputRef round-trips through JSON");
}

#[test]
fn output_ref_constructor_defaults_to_main_program() {
    let oref = OutputRef::to_main("program-out");
    assert_eq!(oref.program, "main");
    assert_eq!(oref.output, "program-out");
}

/// A v3 document whose explicit routing carries an output crosspoint addressing
/// the output by its **stable id** (not its label), exercising the `OutputRef` →
/// known-output validation.
const EXPLICIT_ROUTING: &str = r##"
schema_version = 3

[canvas]
width = 1920
height = 1080
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
id = "in_a"
kind = "test"

[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"

[[outputs]]
id = "program-out"
kind = "rtsp_server"
mount = "/multiview"
codec = "h264"

[routing]
[[routing.video]]
cell = "cell_a"
[routing.video.source]
input_id = "in_a"
kind = { kind = "video" }
selector = { by = "best" }

[[routing.output]]
output = "program-out"
program = "main"
"##;

#[test]
fn explicit_output_crosspoint_addresses_output_by_stable_id() {
    let cfg = MultiviewConfig::load_from_toml(EXPLICIT_ROUTING).expect("parse");
    cfg.validate()
        .expect("an output crosspoint addressing the stable output id validates");
}

#[test]
fn explicit_output_crosspoint_to_unknown_output_id_is_rejected() {
    let bad = EXPLICIT_ROUTING.replace(r#"output = "program-out""#, r#"output = "ghost-out""#);
    let cfg = MultiviewConfig::load_from_toml(&bad).expect("parse");
    let err = cfg
        .validate()
        .expect_err("an output crosspoint to an unknown output id must be rejected");
    assert!(
        err.to_string().contains("ghost-out"),
        "error names the unknown output, got: {err}"
    );
}

#[test]
fn desugared_output_crosspoints_reference_the_stable_output_id() {
    // With no explicit routing, the desugar derives output crosspoints whose
    // `output` is the stable id (the explicit id where given, else the label).
    let cfg = load(); // BASE: out 0 has id "program-out", out 1 derives its label.
    let table = cfg.routing_table();
    let outputs: Vec<&str> = table.output.iter().map(|xp| xp.output.as_str()).collect();
    assert!(
        outputs.contains(&"program-out"),
        "explicit id is referenced, got: {outputs:?}"
    );
    assert!(
        outputs.contains(&"ndi MV PROGRAM"),
        "derived label id is referenced, got: {outputs:?}"
    );
    for xp in &table.output {
        assert_eq!(xp.program, "main", "desugar targets the main program");
    }
}
