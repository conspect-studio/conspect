//! #308 E2 — `Salvo.patch`: the patch API's exact crosspoint JSON (hand
//! serde, never `untagged`), round trips, and validation (≤ 64 members, no
//! destination twice, a patch alone is "something", document references).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::needless_pass_by_value
)]

use multiview_config::{
    MultiviewConfig, OutputBus, PatchOnAir, PatchSourceSpec, Salvo, MAX_PATCH_CROSSPOINTS,
};
use serde_json::{json, Value};

fn salvo(patch: Value) -> Result<Salvo, serde_json::Error> {
    serde_json::from_value(json!({ "id": "s", "patch": patch }))
}

fn video(output: &str, source: Value) -> Value {
    json!({ "plane": "video", "destination": {"output": output, "slot": 0}, "source": source })
}

#[test]
fn every_patch_source_shape_round_trips_exactly() {
    let shapes = [
        json!({"source": "cam"}),
        json!({"source": "cam", "track": 2}),
        json!({"layout": "NewsOne"}),
        json!({"bus": "preview"}),
        json!("follow"),
        json!("none"),
    ];
    for shape in shapes {
        let parsed: PatchSourceSpec = serde_json::from_value(shape.clone()).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), shape);
    }
    assert_eq!(
        serde_json::from_value::<PatchSourceSpec>(json!({"bus": "clean"})).unwrap(),
        PatchSourceSpec::Bus(OutputBus::Clean)
    );
    for bad in [
        json!({"source": 1}),
        json!({"layout": "a", "bus": "program"}),
        json!({"bus": "aux"}),
        json!({"source": "a", "extra": 1}),
        json!("program"),
        json!(3),
    ] {
        assert!(
            serde_json::from_value::<PatchSourceSpec>(bad.clone()).is_err(),
            "{bad}"
        );
    }
}

#[test]
fn a_patch_salvo_round_trips_and_a_legacy_salvo_serializes_unchanged() {
    let patch = json!([
        { "plane": "video", "destination": {"mixer": "me1", "input": 4}, "source": {"source": "cam"} },
        video("output-0", json!({"layout": "NewsOne"})),
        { "plane": "audio", "destination": {"output": "output-0", "slot": 0}, "source": "follow" },
    ]);
    let s: Salvo =
        serde_json::from_value(json!({ "id": "s", "patch": patch, "on_air": "allow" })).unwrap();
    s.validate().unwrap();
    assert_eq!(s.on_air, PatchOnAir::Allow);
    let back = serde_json::to_value(&s).unwrap();
    assert_eq!(back["patch"], patch);
    assert_eq!(back["on_air"], "allow");
    // A legacy salvo carries neither field on the wire.
    let legacy: Salvo = serde_json::from_value(json!({ "id": "l", "layout": "quad" })).unwrap();
    let wire = serde_json::to_value(&legacy).unwrap();
    assert!(
        wire.get("patch").is_none() && wire.get("on_air").is_none(),
        "{wire}"
    );
}

#[test]
fn patch_validation_bounds_and_duplicates() {
    // A patch alone is "something".
    salvo(json!([video("output-0", json!({"source": "a"}))]))
        .unwrap()
        .validate()
        .unwrap();
    let over: Vec<Value> = (0..=MAX_PATCH_CROSSPOINTS)
        .map(|i| video(&format!("output-{i}"), json!({"source": "a"})))
        .collect();
    let err = salvo(Value::Array(over)).unwrap().validate().unwrap_err();
    assert!(err.to_string().contains("at most 64"), "{err}");
    // The same output slot twice (slot absent = 0).
    let dup = json!([
        video("output-0", json!({"source": "a"})),
        { "plane": "video", "destination": {"output": "output-0"}, "source": {"source": "b"} },
    ]);
    let err = salvo(dup).unwrap().validate().unwrap_err();
    assert!(err.to_string().contains("twice"), "{err}");
    // Video and audio of one slot are two destinations.
    salvo(json!([
        video("output-0", json!({"source": "a"})),
        { "plane": "audio", "destination": {"output": "output-0", "slot": 0}, "source": "follow" },
    ]))
    .unwrap()
    .validate()
    .unwrap();
    for bad in [
        json!([{ "plane": "video", "destination": {"mixer": "me1"}, "source": "none" }]),
        json!([{ "plane": "video", "destination": {"mixer": "me1", "input": 1, "output": "o"}, "source": "none" }]),
        json!([{ "plane": "video", "destination": {}, "source": "none" }]),
        json!([{ "plane": "smell", "destination": {"output": "o"}, "source": "none" }]),
        json!([video("output-0", json!({"source": ""}))]),
    ] {
        assert!(salvo(bad.clone()).unwrap().validate().is_err(), "{bad}");
    }
}

const DOC: &str = r##"
schema_version = 1
[canvas]
width = 64
height = 64
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "absolute"
[[sources]]
id = "a"
kind = "bars"
[[sources]]
id = "b"
kind = "bars"
[[cells]]
id = "pgm"
rect = { x = 0.0, y = 0.0, w = 1.0, h = 1.0 }
[cells.source]
input_id = "a"
[[outputs]]
kind = "hls"
path = "/tmp/salvo-patch/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
[switcher]
[[switcher.mix_effects]]
id = "me1"
program = "main"
program_source = "a"
preview_source = "b"
program_input = 1
preview_input = 2
[[switcher.mix_effects.inputs]]
number = 1
video = { source = "a" }
[[switcher.mix_effects.inputs]]
number = 2
video = { source = "b" }
"##;

#[test]
fn document_validation_resolves_sources_and_mixer_inputs() {
    let with = |patch: &str| {
        MultiviewConfig::load_from_toml(&format!("{DOC}\n[[salvos]]\nid = \"s\"\n{patch}\n"))
            .and_then(|c| c.validate())
    };
    with(
        r#"[[salvos.patch]]
plane = "video"
destination = { mixer = "me1", input = 2 }
source = { source = "a" }"#,
    )
    .unwrap();
    let err = with(
        r#"[[salvos.patch]]
plane = "video"
destination = { mixer = "me1", input = 9 }
source = { source = "a" }"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("undeclared mixer input"), "{err}");
    let err = with(
        r#"[[salvos.patch]]
plane = "video"
destination = { output = "output-0", slot = 0 }
source = { source = "ghost" }"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("unknown source"), "{err}");
    // `"follow"` is a bare string in TOML too.
    with(
        r#"[[salvos.patch]]
plane = "audio"
destination = { output = "output-0", slot = 0 }
source = "follow""#,
    )
    .unwrap();
}
