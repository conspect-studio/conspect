//! RT-4 — decoupled-routing config model tests (ADR-0034 / RT-4 backlog row).
//!
//! The load-bearing guarantee is **back-compat by desugar**: a legacy v1/v2
//! document (cells with `input_id`, an `[audio]` block, outputs) and its
//! DESUGARED v3 [`RoutingTable`] must solve to **identical** routing. The
//! property test generates random valid legacy documents, desugars, and asserts
//! the derived crosspoints match the legacy bindings one-for-one.
//!
//! Plus: an explicit `routing` table round-trips through TOML and JSON with the
//! adjacently/internally-tagged wire shape (never untagged); `validate()`
//! rejects a crosspoint to a non-existent destination and an inconsistent
//! both-populated document; a `Language`/`Index` selector is accepted at
//! config-time (resolution deferred to admission).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;
use std::fmt::Write as _;

use multiview_config::routing::{StreamRef, StreamSelector};
use multiview_config::MultiviewConfig;
use multiview_core::stream::StreamKind;
use proptest::prelude::*;

/// A minimal valid legacy (v1) base document: a 2x2 grid, four sources, an
/// `[audio]` program-bus + discrete-track block, and two outputs.
const LEGACY_BASE: &str = r##"
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
kind = "grid"
columns = ["1fr", "1fr"]
rows = ["1fr", "1fr"]
gap = 8
areas = ["a b", "c d"]

[[sources]]
id = "in_a"
kind = "test"
[[sources]]
id = "in_b"
kind = "test"
[[sources]]
id = "in_c"
kind = "test"
[[sources]]
id = "in_d"
kind = "test"

[[cells]]
id = "cell_a"
area = "a"
fit = "contain"
[cells.source]
input_id = "in_a"

[[cells]]
id = "cell_b"
area = "b"
fit = "contain"
[cells.source]
input_id = "in_b"

[[cells]]
id = "cell_c"
area = "c"
fit = "contain"
[cells.source]
input_id = "in_c"

[[cells]]
id = "cell_d"
area = "d"
fit = "contain"
[cells.source]
input_id = "in_d"

[audio]
sample_rate_hz = 48000
[[audio.routes]]
input_id = "in_a"
channels = { kind = "stereo" }
target_track = "cam_a"
include_in_program_bus = true
[[audio.routes]]
input_id = "in_b"
channels = { kind = "stereo" }
include_in_program_bus = true

[[outputs]]
kind = "rtsp_server"
mount = "/multiview"
codec = "h264"

[[outputs]]
kind = "ndi"
name = "MV PROGRAM"
"##;

fn load_legacy() -> MultiviewConfig {
    MultiviewConfig::load_from_toml(LEGACY_BASE).expect("legacy base parses")
}

#[test]
fn legacy_base_validates_and_has_no_explicit_routing() {
    let cfg = load_legacy();
    cfg.validate().expect("legacy base validates");
    assert!(
        cfg.routing.is_none(),
        "absent [routing] ⇒ None (legacy path)"
    );
}

#[test]
fn desugared_video_crosspoints_match_legacy_cell_bindings() {
    let cfg = load_legacy();
    let table = cfg.routing_table();

    // Build a cell→input map from the desugared video crosspoints.
    let mut got: BTreeMap<String, String> = BTreeMap::new();
    for xp in &table.video {
        // Every desugared video crosspoint is Video/Best.
        assert_eq!(xp.source.kind, StreamKind::Video, "desugar uses Video kind");
        assert_eq!(
            xp.source.selector,
            StreamSelector::Best,
            "desugar uses Best selector"
        );
        got.insert(xp.cell.clone(), xp.source.input_id.clone());
    }

    // The legacy bindings, read straight off the cells.
    let mut want: BTreeMap<String, String> = BTreeMap::new();
    for cell in &cfg.cells {
        if let Some(input_id) = &cell.source.input_id {
            want.insert(cell.id.clone(), input_id.clone());
        }
    }

    assert_eq!(
        got, want,
        "desugared video crosspoints must equal the legacy cell→input bindings"
    );
    assert_eq!(want.len(), 4, "all four cells bind a source");
}

#[test]
fn desugared_audio_crosspoints_match_legacy_audio_routes() {
    let cfg = load_legacy();
    let table = cfg.routing_table();

    // Each declared audio route → an audio crosspoint keyed by its destination
    // (the named discrete track, or the program bus when no track is named),
    // sourcing the route's input_id.
    let mut got: BTreeMap<String, String> = BTreeMap::new();
    for xp in &table.audio {
        got.insert(xp.target.clone(), xp.source.input_id.clone());
    }

    // in_a routes to the discrete track "cam_a"; in_b contributes to the bus.
    assert_eq!(
        got.get("cam_a").map(String::as_str),
        Some("in_a"),
        "discrete track cam_a sources in_a"
    );
    assert!(
        got.values().any(|v| v == "in_b"),
        "in_b appears as an audio crosspoint source"
    );
}

#[test]
fn desugared_output_crosspoints_default_to_main_program() {
    let cfg = load_legacy();
    let table = cfg.routing_table();
    assert_eq!(
        table.output.len(),
        2,
        "two outputs ⇒ two output crosspoints"
    );
    for xp in &table.output {
        assert_eq!(
            xp.program, "main",
            "desugared output crosspoint targets the \"main\" program"
        );
    }
}

/// Build a legacy grid document from generated knobs (n cells, n sources).
fn build_legacy(n: u8, perm: &[usize]) -> String {
    let cols = vec!["\"1fr\""; usize::from(n)].join(", ");
    let areas = (0..n)
        .map(|i| format!("c{i}"))
        .collect::<Vec<_>>()
        .join(" ");

    let mut s = String::new();
    writeln!(s, "schema_version = 2").unwrap();
    writeln!(s, "[canvas]").unwrap();
    writeln!(s, "width = 1920").unwrap();
    writeln!(s, "height = 1080").unwrap();
    writeln!(s, "fps = \"30000/1001\"").unwrap();
    writeln!(s, "pixel_format = \"nv12\"").unwrap();
    writeln!(s, "background = \"#101014\"").unwrap();
    writeln!(s, "[canvas.color]").unwrap();
    writeln!(s, "profile = \"sdr-bt709-limited\"").unwrap();
    writeln!(s, "[layout]").unwrap();
    writeln!(s, "kind = \"grid\"").unwrap();
    writeln!(s, "columns = [{cols}]").unwrap();
    writeln!(s, "rows = [\"1fr\"]").unwrap();
    writeln!(s, "areas = [\"{areas}\"]").unwrap();
    for i in 0..n {
        writeln!(s, "[[sources]]").unwrap();
        writeln!(s, "id = \"in_{i}\"").unwrap();
        writeln!(s, "kind = \"test\"").unwrap();
    }
    // Cell i binds source perm[i] — a permutation, so the binding is non-trivial.
    for (i, src) in perm.iter().enumerate() {
        writeln!(s, "[[cells]]").unwrap();
        writeln!(s, "id = \"cell_{i}\"").unwrap();
        writeln!(s, "area = \"c{i}\"").unwrap();
        writeln!(s, "[cells.source]").unwrap();
        writeln!(s, "input_id = \"in_{src}\"").unwrap();
    }
    writeln!(s, "[[outputs]]").unwrap();
    writeln!(s, "kind = \"rtsp_server\"").unwrap();
    writeln!(s, "mount = \"/multiview\"").unwrap();
    writeln!(s, "codec = \"h264\"").unwrap();
    s
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// THE load-bearing property: a legacy doc and its desugared v3 routing
    /// table solve to identical cell→input video routing, for a random binding.
    #[test]
    fn legacy_and_desugared_v3_solve_to_identical_routing(
        perm in (1usize..6).prop_flat_map(|n| {
            let base: Vec<usize> = (0..n).collect();
            Just(base).prop_shuffle()
        }),
    ) {
        let n = u8::try_from(perm.len()).unwrap();
        let doc = build_legacy(n, &perm);
        let cfg = MultiviewConfig::load_from_toml(&doc).expect("generated legacy doc parses");
        prop_assert!(cfg.validate().is_ok(), "generated legacy doc validates");

        let table = cfg.routing_table();

        // Desugared video crosspoints vs the legacy cell bindings.
        let mut got: BTreeMap<String, String> = BTreeMap::new();
        for xp in &table.video {
            got.insert(xp.cell.clone(), xp.source.input_id.clone());
        }
        let mut want: BTreeMap<String, String> = BTreeMap::new();
        for cell in &cfg.cells {
            if let Some(id) = &cell.source.input_id {
                want.insert(cell.id.clone(), id.clone());
            }
        }
        prop_assert_eq!(got, want);
    }
}

/// A v3 document that supplies an EXPLICIT `[routing]` table (no legacy
/// cell-bindings inconsistency); used for round-trip + validation tests.
const EXPLICIT_BASE: &str = r##"
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
columns = ["1fr", "1fr"]
rows = ["1fr"]
areas = ["a b"]

[[sources]]
id = "in_a"
kind = "test"
[[sources]]
id = "in_b"
kind = "test"

[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"

[[cells]]
id = "cell_b"
area = "b"
[cells.source]
input_id = "in_b"

[[outputs]]
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

[[routing.video]]
cell = "cell_b"
[routing.video.source]
input_id = "in_b"
kind = { kind = "video" }
selector = { by = "language", language = "eng" }

[[routing.output]]
output = "rtsp_server /multiview"
program = "main"
"##;

#[test]
fn explicit_routing_parses_and_round_trips_through_toml_and_json() {
    let cfg = MultiviewConfig::load_from_toml(EXPLICIT_BASE).expect("explicit doc parses");
    cfg.validate().expect("explicit doc validates");

    let routing = cfg.routing.as_ref().expect("explicit routing present");
    assert_eq!(routing.video.len(), 2);

    // JSON round-trip.
    let json = cfg.to_json().expect("to_json");
    let from_json = MultiviewConfig::load_from_json(&json).expect("from_json");
    assert_eq!(cfg, from_json, "JSON round-trip is lossless");

    // TOML round-trip.
    let toml_text = cfg.to_toml().expect("to_toml");
    let from_toml = MultiviewConfig::load_from_toml(&toml_text).expect("from_toml");
    assert_eq!(cfg, from_toml, "TOML round-trip is lossless");
}

#[test]
fn stream_selector_wire_shape_is_internally_tagged_never_untagged() {
    // `Best` carries no payload; the tag is the discriminator.
    let best = serde_json::to_value(StreamSelector::Best).expect("ser Best");
    assert_eq!(
        best,
        serde_json::json!({ "by": "best" }),
        "Best is tagged by `by`"
    );

    // `Language` carries its payload alongside the tag (internal tagging).
    let lang =
        serde_json::to_value(StreamSelector::language("eng".to_owned())).expect("ser Language");
    assert_eq!(
        lang,
        serde_json::json!({ "by": "language", "language": "eng" }),
        "Language is internally tagged (tag + payload at one level), never untagged"
    );

    let idx = serde_json::to_value(StreamSelector::index(2)).expect("ser Index");
    assert_eq!(idx, serde_json::json!({ "by": "index", "index": 2 }));

    let sid = serde_json::to_value(StreamSelector::stream_id("v/pid:256".to_owned()))
        .expect("ser StreamId");
    assert_eq!(
        sid,
        serde_json::json!({ "by": "stream_id", "id": "v/pid:256" })
    );

    // Round-trip each through JSON.
    for sel in [
        StreamSelector::Best,
        StreamSelector::index(7),
        StreamSelector::language("fr-CA".to_owned()),
        StreamSelector::stream_id("a/pid:257".to_owned()),
    ] {
        let v = serde_json::to_value(&sel).expect("ser");
        let back: StreamSelector = serde_json::from_value(v).expect("de");
        assert_eq!(sel, back, "selector round-trips through JSON");
    }
}

#[test]
fn stream_ref_default_selector_is_best() {
    // A StreamRef without an explicit selector defaults to Best (the desugar
    // default and the most permissive selector).
    let json = serde_json::json!({
        "input_id": "in_a",
        "kind": { "kind": "video" }
    });
    let sref: StreamRef = serde_json::from_value(json).expect("de StreamRef without selector");
    assert_eq!(sref.selector, StreamSelector::Best);
    assert_eq!(sref.input_id, "in_a");
    assert_eq!(sref.kind, StreamKind::Video);
}

#[test]
fn language_and_index_selectors_are_accepted_at_config_time() {
    // A Language/Index selector cannot be resolved without the input's probed
    // inventory — that resolution is deferred to admission/runtime, so an
    // unresolved language is NOT a config error.
    let doc = EXPLICIT_BASE; // cell_b uses a `language = "eng"` selector.
    let cfg = MultiviewConfig::load_from_toml(doc).expect("parse");
    cfg.validate()
        .expect("a Language selector validates at config-time (deferred resolution)");
}

#[test]
fn crosspoint_to_nonexistent_cell_is_rejected() {
    let bad = EXPLICIT_BASE.replace(r#"cell = "cell_b""#, r#"cell = "cell_NOPE""#);
    let cfg = MultiviewConfig::load_from_toml(&bad).expect("parse");
    let err = cfg
        .validate()
        .expect_err("a video crosspoint to a non-existent cell must fail");
    assert!(
        err.to_string().contains("cell_NOPE"),
        "error names the unknown cell, got: {err}"
    );
}

#[test]
fn crosspoint_to_nonexistent_output_is_rejected() {
    let bad = EXPLICIT_BASE.replace(
        r#"output = "rtsp_server /multiview""#,
        r#"output = "rtsp_server /NOPE""#,
    );
    let cfg = MultiviewConfig::load_from_toml(&bad).expect("parse");
    let err = cfg
        .validate()
        .expect_err("an output crosspoint to a non-existent output must fail");
    assert!(
        err.to_string().contains("NOPE"),
        "error names the unknown output, got: {err}"
    );
}

#[test]
fn crosspoint_to_nonexistent_input_is_rejected() {
    let bad = EXPLICIT_BASE.replace(r#"input_id = "in_b""#, r#"input_id = "in_NOPE""#);
    let cfg = MultiviewConfig::load_from_toml(&bad).expect("parse");
    let err = cfg
        .validate()
        .expect_err("a crosspoint sourcing a non-existent input must fail");
    assert!(
        err.to_string().contains("in_NOPE"),
        "error names the unknown input, got: {err}"
    );
}

#[test]
fn both_populated_inconsistent_doc_is_rejected() {
    // An explicit routing table that contradicts the legacy cell-bindings is
    // rejected (mirror ADR-0030's both-populated rejection): cell_a binds in_a
    // in [[cells]] but the explicit routing video crosspoint points it elsewhere.
    let inconsistent = EXPLICIT_BASE.replace(
        r#"[[routing.video]]
cell = "cell_a"
[routing.video.source]
input_id = "in_a"
kind = { kind = "video" }
selector = { by = "best" }"#,
        r#"[[routing.video]]
cell = "cell_a"
[routing.video.source]
input_id = "in_b"
kind = { kind = "video" }
selector = { by = "best" }"#,
    );
    let cfg = MultiviewConfig::load_from_toml(&inconsistent).expect("parse");
    let err = cfg
        .validate()
        .expect_err("an inconsistent both-populated doc must be rejected");
    assert!(
        err.to_string().contains("cell_a")
            || err.to_string().to_lowercase().contains("inconsistent"),
        "error explains the inconsistency, got: {err}"
    );
}

#[test]
fn explicit_routing_consistent_with_legacy_bindings_validates() {
    // When the explicit routing AGREES with the legacy cell-bindings (same
    // cell→input video routing), the document is valid — the explicit table is
    // a faithful super-set of the desugar. EXPLICIT_BASE binds cell_a→in_a and
    // cell_b→in_b both in [[cells]] and in [routing.video], so it is consistent.
    let cfg = MultiviewConfig::load_from_toml(EXPLICIT_BASE).expect("parse");
    cfg.validate()
        .expect("explicit routing consistent with legacy bindings validates");
}
