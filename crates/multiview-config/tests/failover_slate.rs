//! The configurable failover-slate policy (`on_loss`) round-trips on BOTH the
//! layout cell and the program, defaults to `Bars` when omitted (the broadcast
//! "we have a problem" standard), and keeps existing documents (which carry no
//! `on_loss`) valid (back-compat).
//!
//! This is the single shared `FailoverSlate` policy selectable per LAYOUT source
//! (tile) AND per PROGRAM (passthrough/transcode), choosing what is shown on
//! source loss — the operator's "back to bars and tone until we have it again",
//! configurable the same way in both render paths.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::{Cell, FailoverSlate, ProgramSpec};

/// A minimal cell TOML with an explicit `on_loss`.
fn cell_toml(on_loss: &str) -> String {
    format!(
        r#"
id = "cell-a"
area = "main"
on_loss = {{ slate = "{on_loss}" }}
[source]
input_id = "cam-a"
"#
    )
}

#[test]
fn cell_on_loss_round_trips_each_choice() {
    for (token, expected) in [
        ("bars", FailoverSlate::Bars),
        ("no_signal", FailoverSlate::NoSignal),
        ("black", FailoverSlate::Black),
    ] {
        let cell: Cell = toml::from_str(&cell_toml(token)).expect("cell parses");
        assert_eq!(cell.on_loss, expected, "on_loss for {token:?}");

        // Round-trips back through TOML unchanged.
        let text = toml::to_string(&cell).expect("cell serializes");
        let back: Cell = toml::from_str(&text).expect("cell re-parses");
        assert_eq!(back.on_loss, expected, "on_loss survives a TOML round-trip");

        // And through the JSON wire form (internally tagged, never untagged).
        let json = serde_json::to_string(&cell).expect("cell JSON");
        assert!(
            json.contains("\"slate\":\"") && json.contains(token),
            "on_loss is internally tagged by `slate`: {json}"
        );
        let back_json: Cell = serde_json::from_str(&json).expect("cell from JSON");
        assert_eq!(back_json.on_loss, expected);
    }
}

#[test]
fn cell_on_loss_defaults_to_bars_when_omitted() {
    // A cell with NO on_loss (every pre-existing document) defaults to Bars —
    // the broadcast standard, and the engine/output default — so the option is
    // never a surprise NoSignal.
    let cell: Cell = toml::from_str(
        r#"
id = "cell-a"
area = "main"
[source]
input_id = "cam-a"
"#,
    )
    .expect("legacy cell with no on_loss parses");
    assert_eq!(
        cell.on_loss,
        FailoverSlate::Bars,
        "an omitted on_loss defaults to Bars (back-compat default)"
    );
    assert_eq!(FailoverSlate::default(), FailoverSlate::Bars);
}

/// A `"main"` multiview program spec with an explicit `on_loss`.
fn program_with_on_loss(on_loss: &str) -> String {
    format!(
        r##"{{
            "id": "main",
            "kind": "multiview",
            "on_loss": {{ "slate": "{on_loss}" }},
            "canvas": {{
                "width": 1920,
                "height": 1080,
                "fps": "25/1",
                "pixel_format": "nv12",
                "background": "#101014",
                "color": {{ "profile": "sdr-bt709-limited" }}
            }},
            "layout": {{ "kind": "preset", "preset": "1x1" }}
        }}"##
    )
}

#[test]
fn program_on_loss_round_trips_each_choice() {
    for (token, expected) in [
        ("bars", FailoverSlate::Bars),
        ("no_signal", FailoverSlate::NoSignal),
        ("black", FailoverSlate::Black),
    ] {
        let spec: ProgramSpec =
            serde_json::from_str(&program_with_on_loss(token)).expect("program parses");
        assert_eq!(spec.on_loss, expected, "program on_loss for {token:?}");

        let json = serde_json::to_string(&spec).expect("program JSON");
        let back: ProgramSpec = serde_json::from_str(&json).expect("program re-parses");
        assert_eq!(
            back.on_loss, expected,
            "program on_loss survives round-trip"
        );
    }
}

#[test]
fn program_on_loss_defaults_to_bars_when_omitted() {
    // The non-layout passthrough/transcode case is configurable "the same way":
    // a program with no on_loss defaults to Bars, identical to the cell default.
    let spec: ProgramSpec = serde_json::from_str(
        r##"{
            "id": "main",
            "kind": "multiview",
            "canvas": {
                "width": 1920,
                "height": 1080,
                "fps": "25/1",
                "pixel_format": "nv12",
                "background": "#101014",
                "color": { "profile": "sdr-bt709-limited" }
            },
            "layout": { "kind": "preset", "preset": "1x1" }
        }"##,
    )
    .expect("legacy program with no on_loss parses");
    assert_eq!(
        spec.on_loss,
        FailoverSlate::Bars,
        "an omitted program on_loss defaults to Bars (back-compat default)"
    );
}

#[test]
fn cell_and_program_use_the_same_failover_policy_type() {
    // The SAME shared enum drives both surfaces — a layout tile AND a passthrough
    // program — so "configurable the same way" is a type-level guarantee, not two
    // parallel options that can drift.
    let cell: Cell = toml::from_str(&cell_toml("black")).unwrap();
    let spec: ProgramSpec = serde_json::from_str(&program_with_on_loss("black")).unwrap();
    let cell_policy: FailoverSlate = cell.on_loss;
    let program_policy: FailoverSlate = spec.on_loss;
    assert_eq!(cell_policy, program_policy);
}
