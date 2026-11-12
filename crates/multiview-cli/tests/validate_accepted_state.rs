//! #317 part 3: `multiview validate` checks what the engine will really start
//! from — the accepted state (`.multiview/desired.json`) — not only the boot
//! file. The lead's preflight on 12 November validated `conspect.toml` ("1
//! cell", true and irrelevant) while `desired.json` held a 6-cell canvas the
//! engine refused.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::Path;

use multiview_config::{MultiviewConfig, StartMode};
use multiview_control::{boot_model::BootModel, command_bus, AppState, EngineStateSnapshot};
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use std::sync::Arc;

const BOOT: &str = r##"schema_version = 1
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
id = "in_a"
kind = "bars"
[[sources]]
id = "in_b"
kind = "bars"
[[cells]]
id = "pgm"
rect = { x = 0.0, y = 0.0, w = 1.0, h = 1.0 }
[cells.source]
input_id = "in_a"
[[outputs]]
kind = "hls"
path = "/tmp/validate-accepted-state.m3u8"
codec = "mpeg2video"
segment_ms = 1000
[switcher]
[[switcher.mix_effects]]
id = "me1"
program = "main"
program_source = "in_a"
preview_source = "in_b"
"##;

/// Write a real accepted envelope through the control plane's own commit.
async fn install(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("conspect.toml");
    std::fs::write(&path, BOOT).unwrap();
    let config = MultiviewConfig::load_from_toml(BOOT).unwrap();
    let (sender, _rx) = command_bus(8);
    let state = AppState::new(
        Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(8)),
        sender,
        Arc::new(multiview_control::InMemoryRepository::new()),
        Arc::new(multiview_control::ApiKeyStore::new(
            b"validate-test".to_vec(),
        )),
    )
    .with_seeded_resources(multiview_control::seed_resources(&config).unwrap())
    .with_base_document(serde_json::to_value(&config).unwrap())
    .with_boot_model(Arc::new(BootModel::new(
        path.clone(),
        config.clone(),
        config.clone(),
        StartMode::Boot,
        false,
        None,
    )));
    let layouts = multiview_control::boot_model::layout_library(&state).unwrap();
    multiview_control::boot_model::commit_desired(&state, config, layouts)
        .await
        .unwrap();
    path
}

/// Rewrite the accepted state into the exact 12-November shape: six cells in
/// `config.cells` (and the matching selected body) with one M/E declared.
fn make_six_cell(path: &Path) {
    let desired = path.parent().unwrap().join(".multiview/desired.json");
    let mut document: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&desired).unwrap()).unwrap();
    let cells: Vec<_> = (0..6_u32)
        .map(|i| {
            serde_json::json!({
                "id": format!("c{i}"),
                "rect": {"x": f64::from(i % 3) / 3.0, "y": f64::from(i / 3) / 2.0, "w": 1.0 / 3.0, "h": 0.5},
                "source": {"input_id": if i % 2 == 0 { "in_a" } else { "in_b" }}
            })
        })
        .collect();
    let cells: Vec<multiview_config::Cell> =
        serde_json::from_value(serde_json::Value::Array(cells)).unwrap();
    let cells = serde_json::to_value(cells).unwrap();
    document["config"]["cells"] = cells.clone();
    document["selected_body"]["cells"] = cells;
    let selected = document["selected_layout_id"].clone();
    let body = document["selected_body"].clone();
    for layout in document["layouts"].as_array_mut().unwrap() {
        if layout["id"] == selected {
            layout["body"] = body.clone();
        }
    }
    std::fs::write(&desired, document.to_string()).unwrap();
}

fn validate(path: &Path) -> (i32, String) {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_multiview"))
        .arg("validate")
        .arg(path)
        .output()
        .expect("the multiview binary runs");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.code().unwrap_or(-1), text)
}

#[tokio::test]
async fn validate_passes_a_bootable_accepted_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = install(dir.path()).await;
    let (code, text) = validate(&path);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("OK   start: accepted state"), "{text}");
    assert!(text.contains("desired.json"), "{text}");
}

#[tokio::test]
async fn validate_fails_a_non_bootable_accepted_state_with_the_start_up_reason() {
    let dir = tempfile::tempdir().unwrap();
    let path = install(dir.path()).await;
    make_six_cell(&path);
    let (code, text) = validate(&path);
    assert_ne!(
        code, 0,
        "a state the engine refuses must fail the preflight: {text}"
    );
    // The boot file itself is fine; that is exactly the trap of 12 November.
    assert!(text.starts_with("OK   "), "{text}");
    assert!(text.contains("FAIL start: accepted state"), "{text}");
    assert!(
        text.contains("first-slice M/E requires one full-canvas source cell initially bound to program_source; multi-cell layouts are not replaced"),
        "the same reason start-up gives: {text}"
    );
    assert!(
        text.contains("The switcher is on: a multi-cell layout cannot be the Program canvas."),
        "{text}"
    );
}

#[test]
fn validate_without_an_accepted_state_checks_the_boot_document() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("conspect.toml");
    std::fs::write(&path, BOOT).unwrap();
    let (code, text) = validate(&path);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("OK   start: boot document"), "{text}");
}
