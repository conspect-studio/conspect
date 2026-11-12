//! #317 part 2: "a start that cannot build the saved state must not be an
//! outage". A saved accepted state the engine cannot start + a last-good copy →
//! start from last-good, keep a copy of the rejected state, raise the alarm; no
//! last-good copy → the clear error, as before. Last-good is written only after
//! the first tick.
//!
//! #324: the fallback never moves or overwrites `desired.json` (every test
//! that used to assert "desired.json now holds last-good" asserts it is
//! byte-identical instead), environment-class build errors are retried and
//! never fall back, and the warning is persisted until acknowledged.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use multiview_cli::boot::{
    build_with_fallback, commit_fallback, commit_fallback_with, resolve_start_config, BuildFailure,
    RetryPolicy, StartConfig,
};
use multiview_cli::last_good::{last_good_path_for, spawn_recorder, MAX_REJECTED, REJECTED_DIR};
use multiview_config::{MultiviewConfig, StartMode};
use multiview_control::boot_model::{boot_fallback_marker_path, AtomicWriteError};
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
    install_text(dir, BOOT).await
}

/// [`install`] for an explicit boot document.
async fn install_text(dir: &Path, boot: &str) -> (PathBuf, MultiviewConfig) {
    let path = dir.join("conspect.toml");
    std::fs::write(&path, boot).unwrap();
    let config = MultiviewConfig::load_from_toml(boot).unwrap();
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
    make_six_cell_variant(path, "");
}

/// [`make_six_cell`] with distinct bytes per `tag` (cell ids carry it).
fn make_six_cell_variant(path: &Path, tag: &str) {
    let file = desired(path);
    let mut document: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    let cells: Vec<_> = (0..6_u32)
        .map(|i| {
            serde_json::json!({
                "id": format!("c{i}{tag}"),
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

/// Three retries with no pause: the production attempt count, test speed.
fn retry() -> RetryPolicy {
    RetryPolicy::new(vec![std::time::Duration::ZERO; 3])
}

/// What `multiview run` does with a build that succeeds: decide, build, commit.
fn start_ok(boot: MultiviewConfig, path: &Path) -> Result<StartConfig, String> {
    let start = resolve_start_config(boot, BOOT.to_owned(), path)?;
    let (start, ()) = build_with_fallback(start, path, &retry(), |_| Ok(()))?;
    commit_fallback(start, path)
}

fn marker_exists(path: &Path) -> bool {
    boot_fallback_marker_path(path).exists()
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

    // Deciding writes nothing; the swap happens only after the build.
    let planned = resolve_start_config(boot.clone(), BOOT.to_owned(), &path).unwrap();
    assert!(planned.pending_fallback.is_some());
    assert_eq!(
        std::fs::read(desired(&path)).unwrap(),
        rejected_bytes,
        "not yet moved"
    );
    assert_eq!(rejected_files(&path).len(), 0);
    let start = start_ok(boot, &path).expect("falls back, starts");
    assert_eq!(start.running.cells.len(), 1, "started from last-good");
    assert!(marker_exists(&path), "#324: the fallback is persisted");
    multiview_config::boot_admission(&start.running).expect("the started state boots");
    let fallback = start.boot_fallback.clone().expect("fallback recorded");
    assert!(
        fallback
            .reason
            .contains("multi-cell layouts are not replaced"),
        "the exact start-up reason: {}",
        fallback.reason
    );
    // A copy of the rejected state is kept, byte for byte, under a clear name.
    let kept_at = fallback.kept_at.clone().expect("a copy was kept");
    assert_eq!(std::fs::read(&kept_at).unwrap(), rejected_bytes);
    assert!(kept_at.starts_with(path.parent().unwrap().join(".multiview/rejected")));
    // #324 (changed from #317, which asserted desired.json now held last-good):
    // the operator's accepted state is NEVER replaced — byte-identical.
    assert_eq!(std::fs::read(desired(&path)).unwrap(), rejected_bytes);
    // The next restart tries the saved state first again, so it falls back
    // again — reusing the same kept copy (no churn in rejected/).
    let again = start_ok(MultiviewConfig::load_from_toml(BOOT).unwrap(), &path).unwrap();
    assert!(
        again.boot_fallback.is_some(),
        "the saved state still cannot start"
    );
    assert_eq!(rejected_files(&path), vec![kept_at.clone()]);
    assert_eq!(std::fs::read(desired(&path)).unwrap(), rejected_bytes);

    // The alarm: a latched, critical health warning naming reason and file.
    let publisher = EnginePublisher::<EngineStateSnapshot, Event>::new(8);
    let mut events = publisher.subscribe();
    multiview_control::emit_accepted_state_rejected(&publisher, &fallback.message(), 1);
    let event = events.try_recv().expect("warning published");
    let Event::HealthWarningRaised(warning) = event.event.as_ref() else {
        panic!("expected a health warning, got {:?}", event.event);
    };
    assert_eq!(warning.code, WarningCode::AcceptedStateRejected);
    assert!(warning.active);
    assert!(warning
        .message
        .contains("multi-cell layouts are not replaced"));
    assert!(warning.message.contains(&kept_at.display().to_string()));
}

#[tokio::test]
async fn non_bootable_saved_state_without_last_good_fails_with_the_clear_error() {
    let dir = tempfile::tempdir().unwrap();
    let (path, boot) = install(dir.path()).await;
    make_six_cell(&path);
    let before = std::fs::read(desired(&path)).unwrap();
    let error = start_ok(boot, &path).expect_err("no fallback");
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
    assert_eq!(rejected_files(&path).len(), 0);
}

#[tokio::test]
async fn a_build_failure_falls_back_only_when_last_good_differs_and_builds() {
    let dir = tempfile::tempdir().unwrap();
    let (path, boot) = install(dir.path()).await;
    mark_started(&path).await;
    // Same state as last-good: nothing different to fall back to.
    let start = resolve_start_config(boot.clone(), BOOT.to_owned(), &path).unwrap();
    let same = build_with_fallback(start, &path, &retry(), |_| {
        Err::<(), _>("encoder missing".into())
    });
    assert_eq!(same.unwrap_err(), "encoder missing");
    // A different (admissible) accepted state whose build fails.
    let file = desired(&path);
    let text = std::fs::read_to_string(&file)
        .unwrap()
        .replace("\"preview_source\":\"in_b\"", "\"preview_source\":\"in_a\"");
    std::fs::write(&file, &text).unwrap();
    let start = resolve_start_config(boot.clone(), BOOT.to_owned(), &path).unwrap();
    assert!(start.pending_fallback.is_none());
    // The last-good build ALSO fails (an environment fault): nothing moves.
    let both = build_with_fallback(start.clone(), &path, &retry(), |_| {
        Err::<(), _>("boom".into())
    });
    assert!(both.unwrap_err().contains("does not build either"));
    assert_eq!(
        rejected_files(&path).len(),
        0,
        "never quarantined on a global fault"
    );
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        text,
        "accepted state untouched"
    );
    // Only the accepted build fails: fall back, and commit after the build.
    let accepted_cells = start.running.switcher.clone();
    let (next, ()) = build_with_fallback(start, &path, &retry(), |config| {
        if config.switcher == accepted_cells {
            Err("boom".into())
        } else {
            Ok(())
        }
    })
    .unwrap();
    assert_eq!(rejected_files(&path).len(), 0, "decided, not yet committed");
    let next = commit_fallback(next, &path).unwrap();
    assert!(next.boot_fallback.unwrap().reason.contains("boom"));
    assert_eq!(rejected_files(&path).len(), 1);
    assert_eq!(
        std::fs::read_to_string(&rejected_files(&path)[0]).unwrap(),
        text
    );
    // #324: a copy, not a move — the accepted state is untouched.
    assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
}

/// Review (lens 1, high): an admission failure whose last-good then fails to
/// BUILD (a global fault) must not move the operator's state aside.
#[tokio::test]
async fn admission_fallback_whose_build_fails_moves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (path, boot) = install(dir.path()).await;
    mark_started(&path).await;
    make_six_cell(&path);
    let before = std::fs::read(desired(&path)).unwrap();
    let start = resolve_start_config(boot, BOOT.to_owned(), &path).unwrap();
    let failed = build_with_fallback(start, &path, &retry(), |_| {
        Err::<(), _>("encoder missing".into())
    });
    assert!(failed.unwrap_err().contains("does not build either"));
    assert_eq!(std::fs::read(desired(&path)).unwrap(), before, "untouched");
    assert_eq!(rejected_files(&path).len(), 0);
}

/// Review (lens 1, high): an unreadable accepted state (an I/O error) is not
/// evidence it is bad: fail as before, never roll back over it.
#[cfg(unix)]
#[tokio::test]
async fn an_unreadable_accepted_state_does_not_roll_back() {
    let dir = tempfile::tempdir().unwrap();
    let (path, boot) = install(dir.path()).await;
    mark_started(&path).await;
    // A directory where the file should be: read_to_string fails with an I/O
    // error that is not NotFound.
    std::fs::remove_file(desired(&path)).unwrap();
    std::fs::create_dir(desired(&path)).unwrap();
    let error = start_ok(boot, &path).expect_err("fails, does not fall back");
    assert!(error.contains("cannot be read"), "{error}");
    assert_eq!(rejected_files(&path).len(), 0);
}

/// #324: identical bytes are kept once (dedupe), so each round here rejects
/// DISTINCT contents to exercise the bound.
#[tokio::test]
async fn kept_rejected_states_are_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let (path, boot) = install(dir.path()).await;
    mark_started(&path).await;
    for round in 0..(MAX_REJECTED + 3) {
        make_six_cell_variant(&path, &format!("_{round}"));
        start_ok(boot.clone(), &path).unwrap();
    }
    assert_eq!(rejected_files(&path).len(), MAX_REJECTED);
}

fn run_binary(path: &Path) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_multiview"))
        .args(["run", "--software", "--ticks", "2"])
        .arg(path)
        .env("MULTIVIEW_CONTROL_TOKEN", "test-secret")
        .output()
        .expect("the multiview binary runs")
}

/// Today's restart, end to end through the real binary: the saved 6-cell state
/// with one M/E and a last-good copy → the process starts and runs; without a
/// last-good copy → it fails with the clear reason (and moves nothing).
#[tokio::test]
async fn the_real_binary_restarts_from_last_good_and_fails_clearly_without_it() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _) = install(dir.path()).await;
    make_six_cell(&path);
    let failed = run_binary(&path);
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(!failed.status.success(), "no last-good: fails");
    assert!(
        stderr.contains("multi-cell layouts are not replaced"),
        "{stderr}"
    );

    let dir = tempfile::tempdir().unwrap();
    let (path, _) = install(dir.path()).await;
    mark_started(&path).await;
    make_six_cell(&path);
    let saved = std::fs::read(desired(&path)).unwrap();
    let started = run_binary(&path);
    assert!(
        started.status.success(),
        "falls back and runs: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    assert_eq!(rejected_files(&path).len(), 1, "the rejected state is kept");
    // #324: the saved state is untouched and the fallback is persisted even by
    // a bounded `--ticks` run (no control plane to show a banner).
    assert_eq!(std::fs::read(desired(&path)).unwrap(), saved);
    assert!(marker_exists(&path));
    // `multiview validate` names the pending warning.
    let (_, report) = validate(&path);
    assert!(
        report.contains("start-up fallback is pending acknowledgement"),
        "{report}"
    );
    // `multiview acknowledge-fallback` clears it; a second call says so.
    let ack = acknowledge_binary(&path);
    assert!(ack.status.success());
    assert!(
        String::from_utf8_lossy(&ack.stdout).contains("acknowledged"),
        "{}",
        String::from_utf8_lossy(&ack.stdout)
    );
    assert!(!marker_exists(&path));
    let again = acknowledge_binary(&path);
    assert!(again.status.success());
    assert!(
        String::from_utf8_lossy(&again.stdout).contains("no start-up fallback warning is pending")
    );
    assert_eq!(
        std::fs::read(desired(&path)).unwrap(),
        saved,
        "ack never touches it"
    );
}

fn acknowledge_binary(path: &Path) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_multiview"))
        .arg("acknowledge-fallback")
        .arg(path)
        .output()
        .expect("the multiview binary runs")
}

/// Review (lens 3): the production tick signal — the engine publisher's
/// state-slot sequence — drives the recorder: frozen → nothing written; one
/// published state → written.
#[tokio::test]
async fn the_recorder_follows_the_engine_state_slot() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _) = install(dir.path()).await;
    let accepted = multiview_control::boot_model::load_desired(&path)
        .unwrap()
        .unwrap();
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(8));
    let stop = StopSignal::new();
    let observed = Arc::clone(&publisher);
    let task = spawn_recorder(
        path.clone(),
        accepted,
        move || observed.state.sequence(),
        stop.clone(),
    );
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        !last_good_path_for(&path).exists(),
        "frozen clock: nothing written"
    );
    publisher.publish_state(serde_json::json!({"tick": 1}));
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .expect("recorder finishes after the first tick")
        .unwrap();
    assert!(last_good_path_for(&path).exists(), "first tick: written");
}

fn validate(path: &Path) -> (i32, String) {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_multiview"))
        .arg("validate")
        .arg(path)
        .output()
        .expect("the multiview binary runs");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

/// Review (lens 2): `multiview validate` says WHICH outcome a start would have
/// (fall back vs fail); both exit non-zero.
#[tokio::test]
async fn validate_distinguishes_fall_back_from_fail() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _) = install(dir.path()).await;
    make_six_cell(&path);
    let (code, text) = validate(&path);
    assert_ne!(code, 0, "{text}");
    assert!(
        text.contains("would refuse to start (no usable last-good copy"),
        "{text}"
    );

    let dir = tempfile::tempdir().unwrap();
    let (path, _) = install(dir.path()).await;
    mark_started(&path).await;
    make_six_cell(&path);
    let (code, text) = validate(&path);
    assert_ne!(code, 0, "{text}");
    assert!(
        text.contains("start from the last-good copy instead"),
        "{text}"
    );
    assert!(!text.contains("would refuse to start"), "{text}");
    assert!(
        text.contains("multi-cell layouts are not replaced"),
        "{text}"
    );
}

/// Review round 2 (lens 2): a last-good copy that itself cannot start is
/// reported as "fails", naming why, never as a fallback.
#[tokio::test]
async fn validate_reports_an_unusable_last_good_as_a_failure() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _) = install(dir.path()).await;
    make_six_cell(&path);
    // The "last-good" copy is the same unstartable state.
    std::fs::copy(desired(&path), last_good_path_for(&path)).unwrap();
    let (code, text) = validate(&path);
    assert_ne!(code, 0, "{text}");
    assert!(
        text.contains("would refuse to start (no usable last-good copy"),
        "{text}"
    );
}

/// Review (lens 2): the boot model names the fallback (`resume_fallback`).
#[tokio::test]
async fn the_boot_model_names_the_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let (path, boot) = install(dir.path()).await;
    mark_started(&path).await;
    make_six_cell(&path);
    let start = start_ok(boot, &path).unwrap();
    let model = start.to_boot_model(&path);
    assert!(model
        .resume_fallback()
        .is_some_and(|reason| reason.contains("started from the last configuration that ran")));
}

// ---------------------------------------------------------------------------
// #324: environment vs state, keep-in-place, persisted + acknowledgeable alarm.
// ---------------------------------------------------------------------------

/// An admissible accepted state that differs from last-good (its preview
/// source changed), so a build failure on it COULD fall back.
fn make_different_admissible(path: &Path) -> String {
    let file = desired(path);
    let text = std::fs::read_to_string(&file)
        .unwrap()
        .replace("\"preview_source\":\"in_b\"", "\"preview_source\":\"in_a\"");
    std::fs::write(&file, &text).unwrap();
    text
}

/// #324 finding 1: an environment-class error (a port in use) on the accepted
/// state is retried on the SAME state and then fails start-up — it never falls
/// back to last-good and never writes anything.
#[tokio::test]
async fn an_environment_error_never_rolls_back_the_accepted_state() {
    let dir = tempfile::tempdir().unwrap();
    let (path, boot) = install(dir.path()).await;
    mark_started(&path).await;
    let text = make_different_admissible(&path);
    let start = resolve_start_config(boot, BOOT.to_owned(), &path).unwrap();
    let accepted = start.running.switcher.clone();
    let attempts = std::sync::atomic::AtomicUsize::new(0);
    let policy = retry();
    let failed = build_with_fallback(start, &path, &policy, |config| {
        assert_eq!(
            config.switcher, accepted,
            "only the accepted state is tried"
        );
        attempts.fetch_add(1, Ordering::SeqCst);
        Err::<(), _>(BuildFailure::environment(
            "Address already in use (os error 98)",
        ))
    });
    let error = failed.expect_err("fails, does not fall back");
    assert!(error.contains("environment error"), "{error}");
    assert!(error.contains("after 4 attempts"), "{error}");
    assert!(error.contains("Address already in use"), "{error}");
    assert!(
        error.contains("saved configuration was not changed"),
        "{error}"
    );
    assert_eq!(attempts.load(Ordering::SeqCst), policy.attempts());
    assert_eq!(std::fs::read_to_string(desired(&path)).unwrap(), text);
    assert_eq!(rejected_files(&path).len(), 0);
    assert!(!marker_exists(&path));
}

/// #324: a transient environment error followed by success starts from the
/// ACCEPTED state, with nothing written.
#[tokio::test]
async fn a_transient_environment_error_retries_and_starts_from_the_accepted_state() {
    let dir = tempfile::tempdir().unwrap();
    let (path, boot) = install(dir.path()).await;
    mark_started(&path).await;
    let text = make_different_admissible(&path);
    let start = resolve_start_config(boot, BOOT.to_owned(), &path).unwrap();
    let accepted = start.running.switcher.clone();
    let attempts = std::sync::atomic::AtomicUsize::new(0);
    let (start, built) = build_with_fallback(start, &path, &retry(), |config| {
        if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            Err(BuildFailure::environment(
                "Resource temporarily unavailable",
            ))
        } else {
            Ok(config.switcher.clone())
        }
    })
    .unwrap();
    assert_eq!(built, accepted, "built from the accepted state");
    assert!(start.pending_fallback.is_none());
    let start = commit_fallback(start, &path).unwrap();
    assert!(start.boot_fallback.is_none());
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(std::fs::read_to_string(desired(&path)).unwrap(), text);
    assert_eq!(rejected_files(&path).len(), 0);
    assert!(!marker_exists(&path));
}

/// #324: an environment error on the LAST-GOOD build (after a state-class
/// failure on the accepted state) fails as an environment error too: nothing
/// is committed.
#[tokio::test]
async fn an_environment_error_on_the_last_good_build_commits_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (path, boot) = install(dir.path()).await;
    mark_started(&path).await;
    let text = make_different_admissible(&path);
    let start = resolve_start_config(boot, BOOT.to_owned(), &path).unwrap();
    let accepted = start.running.switcher.clone();
    let error = build_with_fallback(start, &path, &retry(), |config| {
        if config.switcher == accepted {
            Err::<(), _>(BuildFailure::state("bad state"))
        } else {
            Err(BuildFailure::environment("No space left on device"))
        }
    })
    .expect_err("fails");
    assert!(error.contains("environment error"), "{error}");
    assert_eq!(std::fs::read_to_string(desired(&path)).unwrap(), text);
    assert_eq!(rejected_files(&path).len(), 0);
    assert!(!marker_exists(&path));
}

fn publisher() -> EnginePublisher<EngineStateSnapshot, Event> {
    EnginePublisher::<EngineStateSnapshot, Event>::new(16)
}

/// Drain one `accepted-state-rejected` raise, if one was published.
fn raised(events: &mut multiview_engine::EventSubscription<Event>) -> Option<String> {
    while let Ok(event) = events.try_recv() {
        if let Event::HealthWarningRaised(w) = event.event.as_ref() {
            if w.code == WarningCode::AcceptedStateRejected {
                return Some(w.message.clone());
            }
        }
    }
    None
}

/// #324 findings 1-2, the six-cell state: start from last-good, desired.json
/// byte-identical, one kept copy, a persisted marker; the control plane serves
/// last-good; the warning is raised on this start AND on a later clean-looking
/// restart (crash-restart), until acknowledged.
#[tokio::test]
async fn a_state_fault_keeps_the_saved_state_and_persists_the_warning_until_acknowledged() {
    let dir = tempfile::tempdir().unwrap();
    let (path, boot) = install(dir.path()).await;
    mark_started(&path).await;
    let last_good = std::fs::read_to_string(last_good_path_for(&path)).unwrap();
    make_six_cell(&path);
    let saved = std::fs::read(desired(&path)).unwrap();
    let start = start_ok(boot.clone(), &path).unwrap();
    assert_eq!(std::fs::read(desired(&path)).unwrap(), saved);
    let kept = rejected_files(&path);
    assert_eq!(kept.len(), 1);
    assert_eq!(std::fs::read(&kept[0]).unwrap(), saved);
    assert!(marker_exists(&path));
    // The control plane agrees with what runs, not with the file.
    let model = start.to_boot_model(&path);
    let accepted = model.accepted().unwrap().unwrap();
    assert_eq!(serde_json::to_string(accepted.as_ref()).unwrap(), last_good);
    // This start raises it.
    let publisher = publisher();
    let mut events = publisher.subscribe();
    assert!(multiview_cli::last_good::announce(
        &path, &start, &publisher
    ));
    assert!(raised(&mut events)
        .unwrap()
        .contains("multi-cell layouts are not replaced"));
    // A restart whose start does NOT itself fall back (simulated: a start
    // config without a fallback) still raises it from the marker.
    let plain = resolve_start_config(boot.clone(), BOOT.to_owned(), &path).unwrap();
    let plain = StartConfig {
        boot_fallback: None,
        ..plain
    };
    let mut events = publisher.subscribe();
    assert!(multiview_cli::last_good::announce(
        &path, &plain, &publisher
    ));
    assert!(raised(&mut events)
        .unwrap()
        .contains("multi-cell layouts are not replaced"));
    // Acknowledged (the CLI path): gone, and the saved state still untouched.
    let cleared = multiview_cli::last_good::acknowledge(&path).unwrap();
    assert!(cleared.is_some());
    assert!(!marker_exists(&path));
    let mut events = publisher.subscribe();
    assert!(!multiview_cli::last_good::announce(
        &path, &plain, &publisher
    ));
    assert!(raised(&mut events).is_none());
    assert_eq!(multiview_cli::last_good::acknowledge(&path).unwrap(), None);
    assert_eq!(std::fs::read(desired(&path)).unwrap(), saved);
}

/// #324: a writer that fails like a full disk.
fn enospc(_: &Path, _: &str) -> Result<(), AtomicWriteError> {
    Err(AtomicWriteError::BeforeRename(
        std::io::Error::from_raw_os_error(28),
    ))
}

/// #324: `ENOSPC` while keeping the copy / writing the marker: the start
/// still proceeds from last-good (desired.json untouched, nothing lost) and
/// the fallback names the persist error.
#[tokio::test]
async fn enospc_while_recording_the_fallback_still_starts_from_last_good() {
    let dir = tempfile::tempdir().unwrap();
    let (path, boot) = install(dir.path()).await;
    mark_started(&path).await;
    make_six_cell(&path);
    let saved = std::fs::read(desired(&path)).unwrap();
    let start = resolve_start_config(boot, BOOT.to_owned(), &path).unwrap();
    let (start, ()) = build_with_fallback(start, &path, &retry(), |_| Ok(())).unwrap();
    let start = commit_fallback_with(start, &path, &enospc).expect("starts anyway");
    assert_eq!(start.running.cells.len(), 1, "from last-good");
    let fallback = start.boot_fallback.clone().unwrap();
    assert!(fallback.kept_at.is_none());
    let error = fallback.persist_error.clone().expect("named");
    assert!(error.contains("os error 28"), "{error}");
    assert!(fallback.message().contains("will not survive a restart"));
    assert_eq!(std::fs::read(desired(&path)).unwrap(), saved);
    assert!(!marker_exists(&path));
    // The in-memory warning is still raised on this start.
    let publisher = publisher();
    let mut events = publisher.subscribe();
    assert!(multiview_cli::last_good::announce(
        &path, &start, &publisher
    ));
    assert!(raised(&mut events).unwrap().contains("os error 28"));
}

/// #324: a read-only state directory (copy + marker cannot be written) — the
/// same as `ENOSPC`, through the real writer.
#[cfg(unix)]
#[tokio::test]
async fn a_read_only_state_dir_still_starts_from_last_good() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let (path, boot) = install(dir.path()).await;
    mark_started(&path).await;
    make_six_cell(&path);
    let saved = std::fs::read(desired(&path)).unwrap();
    let state = path.parent().unwrap().join(".multiview");
    let original = std::fs::metadata(&state).unwrap().permissions();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o500)).unwrap();
    if std::fs::write(state.join("probe"), b"x").is_ok() {
        let _ = std::fs::remove_file(state.join("probe"));
        std::fs::set_permissions(&state, original).unwrap();
        eprintln!("skipping: this process can write a 0500 directory (root)");
        return;
    }
    let result = start_ok(boot, &path);
    std::fs::set_permissions(&state, original).unwrap();
    let start = result.expect("starts from last-good anyway");
    assert_eq!(start.running.cells.len(), 1);
    let fallback = start.boot_fallback.unwrap();
    assert!(fallback.kept_at.is_none());
    assert!(fallback.persist_error.is_some());
    assert_eq!(std::fs::read(desired(&path)).unwrap(), saved);
    assert!(!marker_exists(&path));
}

/// #324: a crash after the kept copy but before the marker: the next start
/// makes the same decision, REUSES the kept copy, and writes the marker.
#[tokio::test]
async fn a_crash_between_the_kept_copy_and_the_marker_converges_on_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (path, boot) = install(dir.path()).await;
    mark_started(&path).await;
    make_six_cell(&path);
    let saved = std::fs::read_to_string(desired(&path)).unwrap();
    let kept = multiview_cli::last_good::keep_rejected_copy(&path, &saved, 1).unwrap();
    assert!(!marker_exists(&path));
    let start = start_ok(boot, &path).unwrap();
    assert_eq!(start.boot_fallback.unwrap().kept_at, Some(kept.clone()));
    assert_eq!(rejected_files(&path), vec![kept]);
    assert!(marker_exists(&path));
    assert_eq!(std::fs::read_to_string(desired(&path)).unwrap(), saved);
}

/// #324 finding 3: an `AfterRename` failure (the bytes are in place, only the
/// directory fsync failed) counts as kept; one whose bytes are NOT in place
/// does not.
#[tokio::test]
async fn an_after_rename_failure_with_the_bytes_in_place_counts_as_kept() {
    let dir = tempfile::tempdir().unwrap();
    let (path, boot) = install(dir.path()).await;
    mark_started(&path).await;
    make_six_cell(&path);
    let saved = std::fs::read(desired(&path)).unwrap();
    let after_rename = |p: &Path, content: &str| {
        multiview_control::boot_model::write_atomic(p, content).unwrap();
        Err(AtomicWriteError::AfterRename(std::io::Error::other(
            "injected directory sync fault",
        )))
    };
    let start = resolve_start_config(boot.clone(), BOOT.to_owned(), &path).unwrap();
    let (start, ()) = build_with_fallback(start, &path, &retry(), |_| Ok(())).unwrap();
    let start = commit_fallback_with(start, &path, &after_rename).unwrap();
    let fallback = start.boot_fallback.unwrap();
    let kept = fallback.kept_at.expect("treated as kept");
    assert_eq!(std::fs::read(&kept).unwrap(), saved);
    assert!(
        fallback.persist_error.is_none(),
        "{:?}",
        fallback.persist_error
    );
    assert!(marker_exists(&path), "the marker too");
    // Not in place: an error, never reported as kept.
    std::fs::remove_dir_all(path.parent().unwrap().join(".multiview").join(REJECTED_DIR)).unwrap();
    std::fs::remove_file(boot_fallback_marker_path(&path)).unwrap();
    let lost =
        |_: &Path, _: &str| Err(AtomicWriteError::AfterRename(std::io::Error::other("gone")));
    let start = resolve_start_config(boot, BOOT.to_owned(), &path).unwrap();
    let (start, ()) = build_with_fallback(start, &path, &retry(), |_| Ok(())).unwrap();
    let fallback = commit_fallback_with(start, &path, &lost)
        .unwrap()
        .boot_fallback
        .unwrap();
    assert!(fallback.kept_at.is_none());
    assert!(fallback.persist_error.is_some());
}

/// #324: the equal-content loop guard holds — last-good bytes == accepted
/// bytes and a state-class build error → the original error, nothing written.
#[tokio::test]
async fn an_unchanged_state_that_fails_to_build_returns_the_original_error_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (path, boot) = install(dir.path()).await;
    mark_started(&path).await;
    let saved = std::fs::read(desired(&path)).unwrap();
    let start = resolve_start_config(boot, BOOT.to_owned(), &path).unwrap();
    let error = build_with_fallback(start, &path, &retry(), |_| {
        Err::<(), _>(BuildFailure::state("encoder missing"))
    })
    .expect_err("nothing different to fall back to");
    assert_eq!(error, "encoder missing");
    assert_eq!(std::fs::read(desired(&path)).unwrap(), saved);
    assert_eq!(rejected_files(&path).len(), 0);
    assert!(!marker_exists(&path));
}

/// The boot document plus a read-only, an operator and a SCOPED operator
/// API key (their secrets are set by [`set_key_secrets`]).
fn boot_with_keys() -> String {
    format!(
        "{BOOT}[[api.keys]]\nkey_id = \"ro\"\nsecret_env = \"BOOT_FALLBACK_TEST_RO\"\nrole = \"read_only\"\n[[api.keys]]\nkey_id = \"op\"\nsecret_env = \"BOOT_FALLBACK_TEST_OP\"\nrole = \"operator\"\n[[api.keys]]\nkey_id = \"sc\"\nsecret_env = \"BOOT_FALLBACK_TEST_SC\"\nrole = \"operator\"\nscoped_object_ids = [\"in_a\"]\n"
    )
}

fn set_key_secrets() {
    std::env::set_var("MULTIVIEW_CONTROL_TOKEN", "test-secret");
    std::env::set_var("BOOT_FALLBACK_TEST_RO", "ro-secret");
    std::env::set_var("BOOT_FALLBACK_TEST_OP", "op-secret");
    std::env::set_var("BOOT_FALLBACK_TEST_SC", "sc-secret");
}

async fn http(addr: std::net::SocketAddr, method: &str, uri: &str, token: &str) -> (u16, String) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let request = format!(
        "{method} {uri} HTTP/1.0\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let raw = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap();
    (status, body.to_owned())
}

/// What [`serve`] hands back: the address, the stop trigger, the server task
/// and the command receiver (held so the bus stays open).
type Served = (
    std::net::SocketAddr,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<std::io::Result<()>>,
    multiview_control::CommandReceiver,
);

/// Serve the real control plane for `start` on `[::1]:0`, the way the binary
/// does (the boot model from the start resolution).
async fn serve(
    start: &StartConfig,
    path: &Path,
    publisher: &Arc<EnginePublisher<EngineStateSnapshot, Event>>,
) -> Served {
    let model = Arc::new(start.to_boot_model(path));
    let (commands, command_rx) = command_bus(64);
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let (addr, server, _state) = multiview_cli::control::bind_and_serve(
        "[::1]:0",
        &start.running,
        Arc::clone(publisher),
        commands,
        multiview_control::no_preview(),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        multiview_control::LiveApplyCaps::default(),
        multiview_control::LiveSourceCapability::synthetic_only(),
        Some(model),
        None,
        async move {
            let _ = stopped.await;
        },
    )
    .await
    .unwrap();
    (addr, stop, server, command_rx)
}

/// #324 finding 2, over the real router: a fallback committed by a process
/// WITHOUT a control plane (only the marker survives) is shown by a later
/// start on `GET /api/v1/health`, and acknowledged by
/// `POST /api/v1/health/accepted-state-rejected/ack` (write role).
#[tokio::test]
async fn the_persisted_warning_is_listed_and_acknowledged_over_the_api() {
    set_key_secrets();
    let text = boot_with_keys();
    let dir = tempfile::tempdir().unwrap();
    let (path, boot) = install_text(dir.path(), &text).await;
    mark_started(&path).await;
    make_six_cell(&path);
    let saved = std::fs::read(desired(&path)).unwrap();
    // Run 1 (e.g. `--ticks`): decide, build, commit — no control plane.
    let first = resolve_start_config(boot.clone(), text.clone(), &path).unwrap();
    let (first, ()) = build_with_fallback(first, &path, &retry(), |_| Ok(())).unwrap();
    commit_fallback(first, &path).unwrap();
    assert!(marker_exists(&path));
    // Run 2: a start whose own fallback is not what raises it (simulates a
    // healed engine), so the marker alone must.
    let start = resolve_start_config(boot, text, &path).unwrap();
    let start = StartConfig {
        boot_fallback: None,
        ..start
    };
    let publisher = Arc::new(publisher());
    let (addr, stop, server, _commands) = serve(&start, &path, &publisher).await;
    assert!(multiview_cli::last_good::announce(
        &path,
        &start,
        publisher.as_ref()
    ));
    let mut listed = String::new();
    for _ in 0..50 {
        let (status, body) = http(addr, "GET", "/api/v1/health", "ro.ro-secret").await;
        assert_eq!(status, 200, "{body}");
        listed = body;
        if listed.contains("accepted-state-rejected") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(listed.contains("accepted-state-rejected"), "{listed}");
    // Read-only: forbidden; nothing changes.
    let (status, _) = http(
        addr,
        "POST",
        "/api/v1/health/accepted-state-rejected/ack",
        "ro.ro-secret",
    )
    .await;
    assert_eq!(status, 403);
    assert!(marker_exists(&path));
    // #324 review: a SCOPED operator may not silence a whole-system alarm.
    let (status, _) = http(
        addr,
        "POST",
        "/api/v1/health/accepted-state-rejected/ack",
        "sc.sc-secret",
    )
    .await;
    assert_eq!(status, 403);
    assert!(marker_exists(&path), "a refused ack leaves the marker");
    // Another code: not acknowledgeable.
    let (status, _) = http(
        addr,
        "POST",
        "/api/v1/health/config-file-invalid/ack",
        "op.op-secret",
    )
    .await;
    assert_eq!(status, 404);
    // Operator (write): acknowledged.
    let (status, body) = http(
        addr,
        "POST",
        "/api/v1/health/accepted-state-rejected/ack",
        "op.op-secret",
    )
    .await;
    assert_eq!(status, 204, "{body}");
    assert!(!marker_exists(&path));
    let (_, body) = http(addr, "GET", "/api/v1/health", "ro.ro-secret").await;
    assert!(!body.contains("accepted-state-rejected"), "{body}");
    // Nothing pending any more: 404.
    let (status, _) = http(
        addr,
        "POST",
        "/api/v1/health/accepted-state-rejected/ack",
        "admin.test-secret",
    )
    .await;
    assert_eq!(status, 404);
    assert_eq!(
        std::fs::read(desired(&path)).unwrap(),
        saved,
        "never touched"
    );
    let _ = stop.send(());
    let _ = server.await;
}

/// A fallback committed from the six-cell state, the way run 1 leaves it;
/// returns the boot path, the boot config and the boot text with API keys.
async fn fallen_back(dir: &Path) -> (PathBuf, MultiviewConfig, String) {
    set_key_secrets();
    let text = boot_with_keys();
    let (path, boot) = install_text(dir, &text).await;
    mark_started(&path).await;
    make_six_cell(&path);
    let first = resolve_start_config(boot.clone(), text.clone(), &path).unwrap();
    let (first, ()) = build_with_fallback(first, &path, &retry(), |_| Ok(())).unwrap();
    commit_fallback(first, &path).unwrap();
    assert!(marker_exists(&path));
    (path, boot, text)
}

/// #324 review B: once the saved state starts again (admitted AND built, no
/// fallback), a pending marker is stale — its "started from the last
/// configuration that ran" is no longer true. The start removes it, raises
/// nothing, and `validate` stops warning.
#[tokio::test]
async fn a_clean_start_from_the_accepted_state_clears_a_stale_marker() {
    let dir = tempfile::tempdir().unwrap();
    let (path, boot) = install(dir.path()).await;
    mark_started(&path).await;
    let good = std::fs::read(desired(&path)).unwrap();
    make_six_cell(&path);
    start_ok(boot.clone(), &path).unwrap();
    assert!(marker_exists(&path));
    // The operator fixes the saved state (here: back to one that admits).
    std::fs::write(desired(&path), &good).unwrap();
    let start = start_ok(boot, &path).unwrap();
    assert!(start.boot_fallback.is_none());
    assert!(!marker_exists(&path), "the stale marker is settled");
    let publisher = publisher();
    let mut events = publisher.subscribe();
    assert!(!multiview_cli::last_good::announce(
        &path, &start, &publisher
    ));
    assert!(raised(&mut events).is_none());
    let (_, report) = validate(&path);
    assert!(!report.contains("start-up fallback is pending"), "{report}");
}

/// #324 review B: a start with NO accepted state proves nothing about the
/// saved configuration, so it leaves a pending marker alone.
#[tokio::test]
async fn a_start_without_an_accepted_state_leaves_the_marker_alone() {
    let dir = tempfile::tempdir().unwrap();
    let (path, boot) = install(dir.path()).await;
    mark_started(&path).await;
    make_six_cell(&path);
    start_ok(boot.clone(), &path).unwrap();
    assert!(marker_exists(&path));
    std::fs::remove_file(desired(&path)).unwrap();
    let start = start_ok(boot, &path).unwrap();
    assert!(start.accepted_text.is_none());
    assert!(marker_exists(&path), "not settled by a boot-document start");
}

/// #324 review E: credentials in the build error never reach the marker,
/// the warning or the message.
#[tokio::test]
async fn secrets_in_the_build_error_are_redacted_before_persisting() {
    let dir = tempfile::tempdir().unwrap();
    let (path, boot) = install(dir.path()).await;
    mark_started(&path).await;
    make_different_admissible(&path);
    let start = resolve_start_config(boot, BOOT.to_owned(), &path).unwrap();
    let accepted = start.running.switcher.clone();
    let (start, ()) = build_with_fallback(start, &path, &retry(), |config| {
        if config.switcher == accepted {
            Err(BuildFailure::state(
                "opening srt://h:1?passphrase=secret123456 failed; pushing rtmp://u:p@h/app/key refused",
            ))
        } else {
            Ok(())
        }
    })
    .unwrap();
    let start = commit_fallback(start, &path).unwrap();
    let fallback = start.boot_fallback.clone().unwrap();
    let marker = std::fs::read_to_string(boot_fallback_marker_path(&path)).unwrap();
    let publisher = publisher();
    let mut events = publisher.subscribe();
    assert!(multiview_cli::last_good::announce(
        &path, &start, &publisher
    ));
    let warning = raised(&mut events).unwrap();
    for text in [&fallback.reason, &fallback.message(), &marker, &warning] {
        assert!(!text.contains("secret123456"), "{text}");
        assert!(!text.contains("passphrase="), "{text}");
        assert!(!text.contains("u:p@"), "{text}");
        assert!(text.contains("srt://h:1"), "the host stays useful: {text}");
    }
}

/// #324 review E: a huge build error is bounded in the marker and messages.
#[tokio::test]
async fn a_huge_build_error_is_bounded_in_the_marker_and_the_warning() {
    let dir = tempfile::tempdir().unwrap();
    let (path, boot) = install(dir.path()).await;
    mark_started(&path).await;
    make_different_admissible(&path);
    let start = resolve_start_config(boot, BOOT.to_owned(), &path).unwrap();
    let accepted = start.running.switcher.clone();
    // Short words: redaction (which masks long token runs) cannot shrink it.
    let huge = "ab ".repeat((1 << 20) / 3);
    let (start, ()) = build_with_fallback(start, &path, &retry(), |config| {
        if config.switcher == accepted {
            Err(BuildFailure::state(huge.clone()))
        } else {
            Ok(())
        }
    })
    .unwrap();
    let start = commit_fallback(start, &path).unwrap();
    let max = multiview_control::MAX_WARNING_TEXT;
    let fallback = start.boot_fallback.clone().unwrap();
    assert!(fallback.reason.len() <= max, "{}", fallback.reason.len());
    assert!(fallback.message().len() <= max);
    let marker = std::fs::read(boot_fallback_marker_path(&path)).unwrap();
    assert!(marker.len() <= 3 * max, "{} bytes", marker.len());
    let publisher = publisher();
    let mut events = publisher.subscribe();
    assert!(multiview_cli::last_good::announce(
        &path, &start, &publisher
    ));
    assert!(raised(&mut events).unwrap().len() <= max);
}

/// #324 review F: the rejected/ bound holds on a dedupe hit too, and the
/// reused copy is never the one pruned.
#[tokio::test]
async fn the_rejected_bound_holds_when_a_kept_copy_is_reused() {
    let dir = tempfile::tempdir().unwrap();
    let (path, boot) = install(dir.path()).await;
    mark_started(&path).await;
    make_six_cell(&path);
    let saved = std::fs::read(desired(&path)).unwrap();
    let rejected = path.parent().unwrap().join(".multiview").join(REJECTED_DIR);
    std::fs::create_dir_all(&rejected).unwrap();
    // The OLDEST file holds the matching bytes: plain oldest-first pruning
    // would remove it.
    let matching = rejected.join("desired-0000000000000-000.json");
    std::fs::write(&matching, &saved).unwrap();
    for i in 1..(MAX_REJECTED + 3) {
        std::fs::write(
            rejected.join(format!("desired-{i:013}-000.json")),
            format!("other {i}"),
        )
        .unwrap();
    }
    let start = start_ok(boot, &path).unwrap();
    assert_eq!(start.boot_fallback.unwrap().kept_at, Some(matching.clone()));
    let left = rejected_files(&path);
    assert_eq!(left.len(), MAX_REJECTED, "{left:?}");
    assert!(left.contains(&matching));
}

/// #324 review D: an unreadable (here: unparsable, then oversized) marker is
/// still pending — the acknowledge removes it and answers 204.
#[tokio::test]
async fn an_unreadable_marker_is_acknowledged_and_removed() {
    let dir = tempfile::tempdir().unwrap();
    let (path, boot, text) = fallen_back(dir.path()).await;
    let start = resolve_start_config(boot, text, &path).unwrap();
    let publisher = Arc::new(publisher());
    let (addr, stop, server, _commands) = serve(&start, &path, &publisher).await;
    let oversized = vec![b' '; 128 * 1024];
    for garbage in [b"{not json".to_vec(), oversized] {
        std::fs::write(boot_fallback_marker_path(&path), &garbage).unwrap();
        let (status, body) = http(
            addr,
            "POST",
            "/api/v1/health/accepted-state-rejected/ack",
            "op.op-secret",
        )
        .await;
        assert_eq!(status, 204, "{body}");
        assert!(!marker_exists(&path));
    }
    let _ = stop.send(());
    let _ = server.await;
}
