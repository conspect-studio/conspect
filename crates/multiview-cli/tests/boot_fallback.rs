//! #317 part 2: "a start that cannot build the saved state must not be an
//! outage". A saved accepted state the engine cannot start + a last-good copy →
//! start from last-good, keep the rejected state on disk, raise the alarm; no
//! last-good copy → the clear error, as before. Last-good is written only after
//! the first tick.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use multiview_cli::boot::{fallback_after_build_failure, resolve_start_config};
use multiview_cli::last_good::{last_good_path_for, spawn_recorder, MAX_REJECTED, REJECTED_DIR};
use multiview_config::{MultiviewConfig, StartMode};
use multiview_control::{boot_model::BootModel, command_bus, AppState, EngineStateSnapshot};
use multiview_engine::{EnginePublisher, StopSignal};
use multiview_events::{Event, WarningCode};

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
path = "/tmp/boot-fallback.m3u8"
codec = "mpeg2video"
segment_ms = 1000
[switcher]
[[switcher.mix_effects]]
id = "me1"
program = "main"
program_source = "in_a"
preview_source = "in_b"
"##;

fn desired(path: &Path) -> PathBuf {
    path.parent().unwrap().join(".multiview/desired.json")
}

/// A real accepted envelope written by the control plane's own commit.
async fn install(dir: &Path) -> (PathBuf, MultiviewConfig) {
    let path = dir.join("conspect.toml");
    std::fs::write(&path, BOOT).unwrap();
    let config = MultiviewConfig::load_from_toml(BOOT).unwrap();
    let (sender, _rx) = command_bus(8);
    let state = AppState::new(
        Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(8)),
        sender,
        Arc::new(multiview_control::InMemoryRepository::new()),
        Arc::new(multiview_control::ApiKeyStore::new(
            b"fallback-test".to_vec(),
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
    multiview_control::boot_model::commit_desired(&state, config.clone(), layouts)
        .await
        .unwrap();
    (path, config)
}

/// Record the current accepted state as last-good, the way a started run does.
async fn mark_started(path: &Path) {
    let accepted = multiview_control::boot_model::load_desired(path)
        .unwrap()
        .unwrap();
    let ticks = Arc::new(AtomicU64::new(0));
    let observed = Arc::clone(&ticks);
    let task = spawn_recorder(
        path.to_path_buf(),
        accepted,
        move || observed.load(Ordering::Acquire),
        StopSignal::new(),
    );
    ticks.fetch_add(1, Ordering::AcqRel);
    task.await.unwrap();
}

/// Rewrite the accepted state into the 12-November shape (6 cells, one M/E).
fn make_six_cell(path: &Path) {
    let file = desired(path);
    let mut document: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
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
    std::fs::write(&file, document.to_string()).unwrap();
}

fn rejected_files(path: &Path) -> Vec<PathBuf> {
    let dir = path.parent().unwrap().join(".multiview").join(REJECTED_DIR);
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .map(|d| d.filter_map(Result::ok).map(|e| e.path()).collect())
        .unwrap_or_default();
    files.sort();
    files
}

#[tokio::test]
async fn last_good_is_written_only_after_the_first_tick() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _) = install(dir.path()).await;
    let accepted = multiview_control::boot_model::load_desired(&path)
        .unwrap()
        .unwrap();
    // A run stopped before its first tick never becomes last-good.
    let stop = StopSignal::new();
    let task = spawn_recorder(path.clone(), accepted.clone(), || 0, stop.clone());
    stop.stop();
    task.await.unwrap();
    assert!(!last_good_path_for(&path).exists(), "no tick, no last-good");
    // After the first tick it is written, byte-equal to what started.
    mark_started(&path).await;
    let written = std::fs::read_to_string(last_good_path_for(&path)).unwrap();
    assert_eq!(written, serde_json::to_string(&accepted).unwrap());
}

#[tokio::test]
async fn non_bootable_saved_state_with_last_good_starts_from_last_good_and_keeps_the_rejected_state(
) {
    let dir = tempfile::tempdir().unwrap();
    let (path, boot) = install(dir.path()).await;
    mark_started(&path).await;
    make_six_cell(&path);
    let rejected_bytes = std::fs::read(desired(&path)).unwrap();

    let start = resolve_start_config(boot, BOOT.to_owned(), &path).expect("falls back, starts");
    assert_eq!(start.running.cells.len(), 1, "started from last-good");
    multiview_config::boot_admission(&start.running).expect("the started state boots");
    let fallback = start.boot_fallback.clone().expect("fallback recorded");
    assert!(
        fallback
            .reason
            .contains("multi-cell layouts are not replaced"),
        "the exact start-up reason: {}",
        fallback.reason
    );
    // The rejected state is kept, byte for byte, under a clear name.
    assert_eq!(std::fs::read(&fallback.kept_at).unwrap(), rejected_bytes);
    assert!(fallback
        .kept_at
        .starts_with(path.parent().unwrap().join(".multiview/rejected")));
    // The accepted state is now the last-good one, so the next restart agrees.
    let restored = multiview_control::boot_model::load_desired(&path)
        .unwrap()
        .unwrap();
    assert_eq!(restored.config.cells.len(), 1);
    let again = resolve_start_config(
        MultiviewConfig::load_from_toml(BOOT).unwrap(),
        BOOT.to_owned(),
        &path,
    )
    .unwrap();
    assert!(again.boot_fallback.is_none(), "the next start is clean");

    // The alarm: a latched, critical health warning naming reason and file.
    let publisher = EnginePublisher::<EngineStateSnapshot, Event>::new(8);
    let mut events = publisher.subscribe();
    multiview_control::emit_accepted_state_rejected(&publisher, fallback.message(), 1);
    let event = events.try_recv().expect("warning published");
    let Event::HealthWarningRaised(warning) = event.event.as_ref() else {
        panic!("expected a health warning, got {:?}", event.event);
    };
    assert_eq!(warning.code, WarningCode::AcceptedStateRejected);
    assert!(warning.active);
    assert!(warning
        .message
        .contains("multi-cell layouts are not replaced"));
    assert!(warning
        .message
        .contains(&fallback.kept_at.display().to_string()));
}

#[tokio::test]
async fn non_bootable_saved_state_without_last_good_fails_with_the_clear_error() {
    let dir = tempfile::tempdir().unwrap();
    let (path, boot) = install(dir.path()).await;
    make_six_cell(&path);
    let before = std::fs::read(desired(&path)).unwrap();
    let error = resolve_start_config(boot, BOOT.to_owned(), &path).expect_err("no fallback");
    assert!(
        error.contains("accepted desired state cannot be resumed"),
        "{error}"
    );
    assert!(
        error.contains("multi-cell layouts are not replaced"),
        "{error}"
    );
    assert_eq!(
        std::fs::read(desired(&path)).unwrap(),
        before,
        "nothing moved"
    );
    assert!(rejected_files(&path).is_empty());
}

#[tokio::test]
async fn a_build_failure_falls_back_only_when_last_good_differs_and_builds() {
    let dir = tempfile::tempdir().unwrap();
    let (path, boot) = install(dir.path()).await;
    mark_started(&path).await;
    // Same state as last-good: nothing different to fall back to.
    let start = resolve_start_config(boot.clone(), BOOT.to_owned(), &path).unwrap();
    let same = fallback_after_build_failure(&start, &path, "encoder missing", |_| Ok(()));
    assert_eq!(same.unwrap_err(), "encoder missing");
    // A different (admissible) accepted state whose build fails.
    let file = desired(&path);
    let text = std::fs::read_to_string(&file)
        .unwrap()
        .replace("\"preview_source\":\"in_b\"", "\"preview_source\":\"in_a\"");
    std::fs::write(&file, &text).unwrap();
    let start = resolve_start_config(boot.clone(), BOOT.to_owned(), &path).unwrap();
    assert!(start.boot_fallback.is_none());
    // The last-good build ALSO fails (an environment fault): the accepted state is
    // NOT moved aside.
    let both = fallback_after_build_failure(&start, &path, "boom", |_| Err::<(), _>("boom".into()));
    assert!(both.unwrap_err().contains("does not build either"));
    assert!(
        rejected_files(&path).is_empty(),
        "never quarantined on a global fault"
    );
    // The last-good build succeeds: fall back.
    let (next, ()) = fallback_after_build_failure(&start, &path, "boom", |_| Ok(())).unwrap();
    assert!(next.boot_fallback.unwrap().reason.contains("boom"));
    assert_eq!(rejected_files(&path).len(), 1);
}

#[tokio::test]
async fn kept_rejected_states_are_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let (path, boot) = install(dir.path()).await;
    mark_started(&path).await;
    for _ in 0..(MAX_REJECTED + 3) {
        make_six_cell(&path);
        resolve_start_config(boot.clone(), BOOT.to_owned(), &path).unwrap();
    }
    assert_eq!(rejected_files(&path).len(), MAX_REJECTED);
}
