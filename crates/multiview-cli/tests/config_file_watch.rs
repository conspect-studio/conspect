//! Config-file watch (ADR-W020): an external write to the boot config file
//! hot-reloads the IMPACTED parts through the SAME apply machinery the API
//! uses — IF the whole document validates; an invalid file changes NOTHING
//! (warn + health event, keep running, never falter).
//!
//! These tests drive the real watcher task over a real temp file at a short
//! poll interval, with the real control-plane `AppState` (stores + bounded
//! command bus + warning ingest) and — where the engine matters — the real
//! frame-boundary drain + `LiveSourceHub`, mirroring `live_source_apply.rs`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use multiview_cli::config_watch::{spawn as spawn_watch, ConfigWatchHandle, WatchOptions};
use multiview_cli::control::command_drain_with_live_sources;
use multiview_cli::live_sources::{shared_stores, stop_registry, LiveSourceHub};
use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_config::MultiviewConfig;
use multiview_control::{
    command_bus, run_warning_ingest, ApiKeyStore, AppState, Command, CommandReceiver,
    EngineStateSnapshot, InMemoryRepository, InMemoryWarningStore, WarningFilter,
    WarningRepository,
};
use multiview_engine::{CompositorDrive, EnginePublisher};
use multiview_events::Event;
use multiview_framestore::TileStore;

/// The fast poll the tests run the watcher at (real default is 1 s).
const TEST_POLL: Duration = Duration::from_millis(80);

/// How long assertions wait for the watcher/hub to act.
const SETTLE: Duration = Duration::from_secs(10);

/// The boot document: `in_a` a dark solid, `in_b` bars, two cells, one output.
const INITIAL_DOC: &str = r##"schema_version = 1
[canvas]
width = 64
height = 64
fps = "25/1"
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
kind = "solid"
color = "#101418"
[[sources]]
id = "in_b"
kind = "bars"
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
kind = "hls"
path = "/tmp/config-file-watch.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;

/// The whole watcher rig: temp file + control-plane state + watch handle.
struct Rig {
    /// Held for its Drop (removes the temp dir).
    _dir: tempfile::TempDir,
    path: PathBuf,
    config: MultiviewConfig,
    state: AppState,
    commands: CommandReceiver,
    warnings: Arc<dyn WarningRepository>,
    handle: ConfigWatchHandle,
}

/// Build the rig: write `doc` to a temp config file, seed the control-plane
/// stores from it, start the warning ingest, and spawn the watcher at the
/// fast test poll.
fn rig(doc: &str) -> Rig {
    rig_with(doc, 64, |_| {})
}

/// [`rig`] with a chosen command-bus `capacity` and a `boot_window` hook that
/// runs AFTER the baseline document is loaded but BEFORE the watcher is
/// spawned — exactly the real boot window between `load_validated` in
/// `run_run` and `serve_control_plane`'s spawn (ADR-W020 review M2).
fn rig_with(doc: &str, capacity: usize, boot_window: impl FnOnce(&std::path::Path)) -> Rig {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("multiview.toml");
    std::fs::write(&path, doc).expect("write boot config");
    let config = MultiviewConfig::load_from_toml(doc).expect("parse boot config");
    config.validate().expect("boot config validates");

    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let (sender, commands) = command_bus(capacity);
    let warnings: Arc<dyn WarningRepository> = Arc::new(InMemoryWarningStore::new());
    tokio::spawn(run_warning_ingest(
        publisher.subscribe(),
        Arc::clone(&warnings),
    ));
    let seeded = multiview_control::seed_resources(&config).expect("seed stores");
    let state = AppState::new(
        Arc::clone(&publisher),
        sender,
        Arc::new(InMemoryRepository::new()),
        Arc::new(ApiKeyStore::new(b"watch-test-pepper".to_vec())),
    )
    .with_seeded_resources(seeded);

    boot_window(&path);

    let handle = spawn_watch(
        path.clone(),
        config.clone(),
        state.clone(),
        WatchOptions::default().with_poll_interval(TEST_POLL),
    );
    Rig {
        _dir: dir,
        path,
        config,
        state,
        commands,
        warnings,
        handle,
    }
}

/// A real `CompositorDrive` over the config's solved layout with one empty
/// registered store per declared source (mirrors `live_source_apply.rs`).
fn test_drive(config: &MultiviewConfig) -> CompositorDrive<Nv12Image> {
    let layout = config.solve_layout().expect("solve layout");
    let canvas_color = CanvasColor::default();
    let nosignal = Nv12Image::solid(
        config.canvas.width,
        config.canvas.height,
        16,
        128,
        128,
        canvas_color.output_tag(),
    )
    .expect("nosignal card");
    let mut stores = HashMap::new();
    for source in &config.sources {
        stores.insert(
            source.id.clone(),
            Arc::new(TileStore::<Nv12Image>::with_defaults(source.id.clone())),
        );
    }
    CompositorDrive::new(
        Arc::new(layout),
        stores,
        nosignal,
        canvas_color,
        LinearRgba::opaque(0.0, 0.0, 0.0),
    )
    .expect("build drive")
}

/// Await `predicate` (polled every 20 ms) until it holds or `deadline` lapses.
async fn wait_until(deadline: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let end = std::time::Instant::now() + deadline;
    while std::time::Instant::now() < end {
        if predicate() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    predicate()
}

/// The number of successful file applies the watcher has recorded.
fn applied_count(state: &AppState) -> u64 {
    state.config_watch.snapshot().applied_count
}

/// Whether the active warning list carries `code`.
fn has_active_warning(warnings: &Arc<dyn WarningRepository>, code: &str) -> bool {
    warnings
        .list(&WarningFilter::active_only())
        .expect("list warnings")
        .iter()
        .any(|w| w.code.as_str() == code)
}

/// THE VERTICAL SLICE: editing the solid colour and adding a bars source in
/// the file reaches the running engine — the SAME `UpsertSource` machinery the
/// API uses — and reseeds the control stores (the UI's truth), audit-logged as
/// actor `config-file`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_file_edit_changes_the_engine_picture_and_the_stores() {
    let mut r = rig(INITIAL_DOC);
    let registry = stop_registry();
    let preview = shared_stores(HashMap::new());
    let hub = LiveSourceHub::start(Arc::clone(&registry), Arc::clone(&preview));
    let mut drain = command_drain_with_live_sources(
        std::mem::replace(&mut r.commands, command_bus(1).1),
        r.config.clone(),
        Arc::clone(&r.state.engine),
        hub.handle(),
    );
    let mut drive = test_drive(&r.config);

    // External edit: in_a goes near-white, and a NEW bars source live9 appears.
    let updated = format!(
        "{}[[sources]]\nid = \"live9\"\nkind = \"bars\"\n",
        INITIAL_DOC.replace("#101418", "#f0f0f0")
    );
    std::fs::write(&r.path, &updated).expect("rewrite config");

    assert!(
        wait_until(SETTLE, || applied_count(&r.state) >= 1).await,
        "the watcher must apply the valid rewrite"
    );
    // Frame boundary: the drain registers the new source + swaps the producer.
    drain(&mut drive);

    let live9 = drive.store("live9").cloned();
    assert!(
        live9.is_some(),
        "the file-added source must register a frame store at the frame boundary"
    );
    let in_a = drive.store("in_a").cloned().expect("in_a store");
    // The hub-swapped producer publishes the NEW near-white solid into the
    // reused in_a store — the picture proof (read the newest published frame;
    // the store's slate policy is irrelevant to what the producer wrote).
    let bright = wait_until(SETTLE, || {
        in_a.slot()
            .load()
            .is_some_and(|frame| frame.sample(8, 8).is_some_and(|(y, _, _)| y > 150))
    })
    .await;
    assert!(
        bright,
        "the edited solid colour must reach the engine picture (luma > 150)"
    );

    // The control stores follow the file (the UI's truth).
    let live9_stored = r.state.sources.get("live9").expect("live9 in the store");
    assert_eq!(
        live9_stored
            .resource
            .body
            .get("kind")
            .and_then(|v| v.as_str()),
        Some("bars")
    );
    let in_a_stored = r.state.sources.get("in_a").expect("in_a in the store");
    assert_eq!(
        in_a_stored
            .resource
            .body
            .get("color")
            .and_then(|v| v.as_str()),
        Some("#f0f0f0"),
        "the sources store mirrors the edited colour"
    );

    // Every store change is audit-logged with the config-file actor.
    let entries = r.state.audit.list(None).expect("audit list");
    assert!(
        entries.iter().any(|e| e.actor == "config-file"),
        "file-applied changes must be audited as actor config-file"
    );
    r.handle.stop();
    hub.shutdown();
}

/// A cell rebinding in the file rides the SAME resolve+solve+ApplyLayout path
/// as `POST /commands/apply-layout`: the active layout swaps at the frame
/// boundary.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_cell_rebinding_swaps_the_active_layout() {
    let mut r = rig(INITIAL_DOC);
    let registry = stop_registry();
    let preview = shared_stores(HashMap::new());
    let hub = LiveSourceHub::start(Arc::clone(&registry), Arc::clone(&preview));
    let mut drain = command_drain_with_live_sources(
        std::mem::replace(&mut r.commands, command_bus(1).1),
        r.config.clone(),
        Arc::clone(&r.state.engine),
        hub.handle(),
    );
    let mut drive = test_drive(&r.config);
    assert_eq!(
        drive.layout().cells.first().and_then(|c| c.source.clone()),
        Some("in_a".to_owned())
    );

    // Rebind cell_a from in_a to in_b in the FILE.
    let updated = INITIAL_DOC.replace(
        "[[cells]]\nid = \"cell_a\"\narea = \"a\"\n[cells.source]\ninput_id = \"in_a\"",
        "[[cells]]\nid = \"cell_a\"\narea = \"a\"\n[cells.source]\ninput_id = \"in_b\"",
    );
    std::fs::write(&r.path, updated).expect("rewrite config");

    assert!(
        wait_until(SETTLE, || applied_count(&r.state) >= 1).await,
        "the watcher must apply the cell rebinding"
    );
    drain(&mut drive);
    assert_eq!(
        drive.layout().cells.first().and_then(|c| c.source.clone()),
        Some("in_b".to_owned()),
        "the file's cell rebinding must swap the active layout at the frame boundary"
    );
    r.handle.stop();
    hub.shutdown();
}

/// An INVALID rewrite changes NOTHING: no commands, stores untouched, baseline
/// kept — and the operator sees a `config-file-invalid` health warning that a
/// subsequent valid apply clears.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_invalid_rewrite_changes_nothing_and_raises_a_warning() {
    let mut r = rig(INITIAL_DOC);

    std::fs::write(&r.path, "this is [not the multiview schema\n").expect("rewrite garbage");
    assert!(
        wait_until(SETTLE, || {
            r.state.config_watch.snapshot().last_rejected.is_some()
        })
        .await,
        "the watcher must record the rejected load"
    );
    assert!(
        wait_until(SETTLE, || has_active_warning(
            &r.warnings,
            "config-file-invalid"
        ))
        .await,
        "an invalid file must raise the config-file-invalid health warning"
    );
    // NOTHING changed: no engine commands, the stores still hold the boot doc.
    assert!(
        r.commands.try_drain().is_empty(),
        "an invalid file must enqueue no engine commands"
    );
    let in_a = r.state.sources.get("in_a").expect("in_a still stored");
    assert_eq!(
        in_a.resource.body.get("color").and_then(|v| v.as_str()),
        Some("#101418"),
        "an invalid file must not touch the stores"
    );
    assert_eq!(applied_count(&r.state), 0, "nothing applied");

    // A subsequent VALID rewrite applies and clears the invalid warning.
    std::fs::write(&r.path, INITIAL_DOC.replace("#101418", "#f0f0f0")).expect("fix config");
    assert!(
        wait_until(SETTLE, || applied_count(&r.state) >= 1).await,
        "the corrected file must apply"
    );
    assert!(
        wait_until(SETTLE, || !has_active_warning(
            &r.warnings,
            "config-file-invalid"
        ))
        .await,
        "a valid apply must clear the config-file-invalid warning"
    );
    let drained = r.commands.try_drain();
    assert!(
        drained
            .iter()
            .any(|c| matches!(c, Command::UpsertSource { source, .. } if source.id == "in_a")),
        "the corrected colour edit must ride the UpsertSource machinery, got {drained:?}"
    );
    r.handle.stop();
}

/// A canvas geometry change is Class-2 (pinned canvas, ADR-R004): the watcher
/// never applies it live — it warns `config-file-requires-restart` naming the
/// section, and submits NO layout command.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_canvas_change_is_held_as_restart_required() {
    let mut r = rig(INITIAL_DOC);

    std::fs::write(
        &r.path,
        INITIAL_DOC
            .replace("width = 64", "width = 128")
            .replace("height = 64", "height = 128"),
    )
    .expect("rewrite canvas");

    assert!(
        wait_until(SETTLE, || applied_count(&r.state) >= 1).await,
        "the rewrite still applies (as a restart-pending change)"
    );
    let drained = r.commands.try_drain();
    assert!(
        !drained
            .iter()
            .any(|c| matches!(c, Command::ApplyLayout { .. })),
        "a Class-2 canvas change must never submit a live layout apply, got {drained:?}"
    );
    assert!(
        wait_until(SETTLE, || has_active_warning(
            &r.warnings,
            "config-file-requires-restart"
        ))
        .await,
        "a canvas change must raise the requires-restart warning"
    );
    let pending = r.state.config_watch.snapshot().restart_pending;
    assert!(
        pending.iter().any(|s| s == "canvas"),
        "the restart-pending sections must name canvas, got {pending:?}"
    );
    r.handle.stop();
}

/// A restart-only section (outputs) still reseeds its control store — the UI
/// reflects the file immediately — and warns requires-restart naming it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_output_change_reseeds_the_store_and_warns_restart() {
    let mut r = rig(INITIAL_DOC);

    std::fs::write(
        &r.path,
        INITIAL_DOC.replace("segment_ms = 1000", "segment_ms = 2000"),
    )
    .expect("rewrite output");

    assert!(
        wait_until(SETTLE, || applied_count(&r.state) >= 1).await,
        "the output rewrite applies (store reseed + restart warning)"
    );
    let output = r.state.outputs.get("output-0").expect("seeded output");
    assert_eq!(
        output
            .resource
            .body
            .get("segment_ms")
            .and_then(serde_json::Value::as_i64),
        Some(2000),
        "the outputs store must follow the file"
    );
    assert!(
        wait_until(SETTLE, || has_active_warning(
            &r.warnings,
            "config-file-requires-restart"
        ))
        .await
    );
    let pending = r.state.config_watch.snapshot().restart_pending;
    assert!(
        pending.iter().any(|s| s == "outputs"),
        "the restart-pending sections must name outputs, got {pending:?}"
    );
    assert!(
        r.commands.try_drain().is_empty(),
        "an outputs-only change has no live engine path yet"
    );
    r.handle.stop();
}

/// Editor multi-writes settle to ONE apply carrying the FINAL content (the
/// debounce: the watcher acts only on a stable fingerprint).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn editor_multi_writes_settle_to_one_apply() {
    let r = rig(INITIAL_DOC);

    // Two quick writes (an editor's truncate+write churn): an intermediate
    // colour, then the final one, inside a single poll window.
    std::fs::write(&r.path, INITIAL_DOC.replace("#101418", "#202428")).expect("write A");
    tokio::time::sleep(Duration::from_millis(20)).await;
    std::fs::write(
        &r.path,
        format!("{}\n# settled\n", INITIAL_DOC.replace("#101418", "#f0f0f0")),
    )
    .expect("write B");

    assert!(
        wait_until(SETTLE, || applied_count(&r.state) >= 1).await,
        "the settled write must apply"
    );
    // Let several further polls elapse: no second apply may appear.
    tokio::time::sleep(TEST_POLL * 6).await;
    assert_eq!(
        applied_count(&r.state),
        1,
        "the multi-write burst must settle to exactly one apply"
    );
    let in_a = r.state.sources.get("in_a").expect("in_a stored");
    assert_eq!(
        in_a.resource.body.get("color").and_then(|v| v.as_str()),
        Some("#f0f0f0"),
        "the single apply must carry the FINAL content"
    );
    r.handle.stop();
}

/// An atomic write-temp + rename(2) — the safe-editor idiom — is detected
/// (the watcher stats the PATH, not a held fd).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_atomic_rename_is_detected() {
    let r = rig(INITIAL_DOC);

    let tmp = r.path.with_extension("toml.tmp");
    std::fs::write(&tmp, INITIAL_DOC.replace("#101418", "#f0f0f0")).expect("write temp");
    std::fs::rename(&tmp, &r.path).expect("atomic rename onto the config path");

    assert!(
        wait_until(SETTLE, || applied_count(&r.state) >= 1).await,
        "an atomic rename must be detected and applied"
    );
    let in_a = r.state.sources.get("in_a").expect("in_a stored");
    assert_eq!(
        in_a.resource.body.get("color").and_then(|v| v.as_str()),
        Some("#f0f0f0")
    );
    r.handle.stop();
}

/// `expect_write(content)` suppresses exactly one reload of that content (the
/// promote-to-boot flow writes the file server-side and must not re-trigger),
/// while a LATER external edit still applies against the adopted baseline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expect_write_suppresses_one_reload_then_watching_resumes() {
    let mut r = rig(INITIAL_DOC);

    let promoted = INITIAL_DOC.replace("#101418", "#303438");
    r.handle.expect_write(&promoted);
    std::fs::write(&r.path, &promoted).expect("server-side write");

    // Several polls elapse: the expected write is adopted silently.
    tokio::time::sleep(TEST_POLL * 8).await;
    assert_eq!(
        applied_count(&r.state),
        0,
        "an expected (self) write must not re-trigger an apply"
    );
    assert!(
        r.commands.try_drain().is_empty(),
        "an expected write must enqueue nothing"
    );
    let in_a = r.state.sources.get("in_a").expect("in_a stored");
    assert_eq!(
        in_a.resource.body.get("color").and_then(|v| v.as_str()),
        Some("#101418"),
        "an expected write must not reseed the stores (the writer already did)"
    );

    // Watching resumes, diffing against the ADOPTED baseline: a later external
    // edit applies, and its diff is vs the promoted content.
    std::fs::write(&r.path, INITIAL_DOC.replace("#101418", "#f0f0f0")).expect("external edit");
    assert!(
        wait_until(SETTLE, || applied_count(&r.state) >= 1).await,
        "watching must resume after a suppressed write"
    );
    let drained = r.commands.try_drain();
    assert!(
        drained
            .iter()
            .any(|c| matches!(c, Command::UpsertSource { source, .. } if source.id == "in_a")),
        "the later external edit must apply, got {drained:?}"
    );
    r.handle.stop();
}

/// REVIEW M1 (the shed corner): a valid edit whose engine command is SHED on
/// a full bus must NOT advance the baseline or claim "applied" — the status
/// records a partial-apply rejection, an interim health warning is raised,
/// and the watcher RE-APPLIES idempotently on a later poll once the bus
/// drains; the warning then clears.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shed_apply_is_retried_and_never_claims_applied() {
    // A capacity-1 bus, pre-filled with a dummy command so the watcher's
    // UpsertSource submission is shed (Full).
    let mut r = rig_with(INITIAL_DOC, 1, |_| {});
    r.state
        .commands
        .try_submit(Command::Start {
            op: multiview_control::OperationId::new(),
        })
        .expect("fill the capacity-1 bus");

    std::fs::write(&r.path, INITIAL_DOC.replace("#101418", "#f0f0f0")).expect("rewrite config");

    // The shed attempt is surfaced: a partial-apply rejection naming the shed
    // (NOT a claimed apply) + an interim health warning.
    assert!(
        wait_until(SETTLE, || {
            r.state
                .config_watch
                .snapshot()
                .last_rejected
                .is_some_and(|stamp| stamp.detail.contains("shed"))
        })
        .await,
        "a shed submission must record a partial-apply rejection naming the shed"
    );
    assert_eq!(
        applied_count(&r.state),
        0,
        "the status must never claim applied while a command was shed"
    );
    assert!(
        wait_until(SETTLE, || has_active_warning(
            &r.warnings,
            "config-file-apply-incomplete"
        ))
        .await,
        "a shed apply raises the interim apply-incomplete warning"
    );

    // Drain the bus (the dummy goes away): the watcher's retry now lands.
    let first = r.commands.try_drain();
    assert!(
        first.iter().any(|c| matches!(c, Command::Start { .. })),
        "the dummy filled the bus, got {first:?}"
    );
    assert!(
        wait_until(SETTLE, || applied_count(&r.state) >= 1).await,
        "the shed edit must be re-applied once the bus drains"
    );
    let drained = r.commands.try_drain();
    assert!(
        drained
            .iter()
            .any(|c| matches!(c, Command::UpsertSource { source, .. } if source.id == "in_a")),
        "the retried apply must submit the shed UpsertSource, got {drained:?}"
    );
    assert!(
        wait_until(SETTLE, || !has_active_warning(
            &r.warnings,
            "config-file-apply-incomplete"
        ))
        .await,
        "a completed retry clears the apply-incomplete warning"
    );
    r.handle.stop();
}

/// REVIEW M2 (the startup window): an edit landing between the boot-time
/// config load and the watcher spawn must be DETECTED and applied — never
/// silently adopted as the baseline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edit_in_the_boot_window_is_applied_not_adopted() {
    // The boot_window hook writes the edit AFTER the baseline was loaded but
    // BEFORE the watcher spawns — the exact run_run → serve_control_plane gap.
    let mut r = rig_with(INITIAL_DOC, 64, |path| {
        std::fs::write(path, INITIAL_DOC.replace("#101418", "#f0f0f0"))
            .expect("edit in the boot window");
    });

    assert!(
        wait_until(SETTLE, || applied_count(&r.state) >= 1).await,
        "an edit landing in the boot window must be applied, not adopted"
    );
    let drained = r.commands.try_drain();
    assert!(
        drained
            .iter()
            .any(|c| matches!(c, Command::UpsertSource { source, .. } if source.id == "in_a")),
        "the boot-window edit must ride the UpsertSource machinery, got {drained:?}"
    );
    r.handle.stop();
}

/// REVIEW m2: stopping the watcher (or its task dying) must flip the
/// watch-status to inactive — the UI must never show "Watching" for a dead
/// watcher.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_the_watcher_marks_the_status_inactive() {
    let r = rig(INITIAL_DOC);
    assert!(
        r.state.config_watch.snapshot().active,
        "the spawned watcher reports active"
    );
    r.handle.stop();
    assert!(
        wait_until(SETTLE, || !r.state.config_watch.snapshot().active).await,
        "a stopped watcher must mark the watch-status inactive"
    );
}

/// REVIEW m3: an `expect_write` token is paired with the CONTENT the server
/// will write — a banked token must not eat a REAL external edit that lands
/// first; the expected content is still suppressed when it arrives later.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_banked_expect_token_does_not_eat_a_real_external_edit() {
    let mut r = rig(INITIAL_DOC);

    // The server banks a token for content it has NOT written yet.
    let promoted = INITIAL_DOC.replace("#101418", "#303438");
    r.handle.expect_write(&promoted);

    // A REAL external edit (different content) lands first: it must APPLY.
    std::fs::write(&r.path, INITIAL_DOC.replace("#101418", "#f0f0f0")).expect("external edit");
    assert!(
        wait_until(SETTLE, || applied_count(&r.state) >= 1).await,
        "a banked token must not suppress a real external edit"
    );
    let drained = r.commands.try_drain();
    assert!(
        drained
            .iter()
            .any(|c| matches!(c, Command::UpsertSource { source, .. } if source.id == "in_a")),
        "the external edit must ride the UpsertSource machinery, got {drained:?}"
    );

    // The promoted content arrives later: the banked token suppresses exactly
    // that write (no second apply, no commands).
    std::fs::write(&r.path, &promoted).expect("server-side write");
    tokio::time::sleep(TEST_POLL * 8).await;
    assert_eq!(
        applied_count(&r.state),
        1,
        "the expected content must be adopted silently when it finally lands"
    );
    assert!(
        r.commands.try_drain().is_empty(),
        "the expected content must enqueue nothing"
    );
    r.handle.stop();
}

/// REVIEW m5: a latched `config-file-invalid` warning (here: the file went
/// missing) must CLEAR when the file comes back at the already-applied
/// content (rename away, then rename back) — the invalid condition resolved
/// even though no new content needs applying.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rename_away_and_back_clears_the_latched_invalid_warning() {
    let r = rig(INITIAL_DOC);
    let parked = r.path.with_extension("toml.parked");

    std::fs::rename(&r.path, &parked).expect("rename the config away");
    assert!(
        wait_until(SETTLE, || has_active_warning(
            &r.warnings,
            "config-file-invalid"
        ))
        .await,
        "a persistently missing config file raises config-file-invalid"
    );

    std::fs::rename(&parked, &r.path).expect("rename the config back");
    assert!(
        wait_until(SETTLE, || !has_active_warning(
            &r.warnings,
            "config-file-invalid"
        ))
        .await,
        "the warning must clear when the file is back at the applied content"
    );
    assert_eq!(
        applied_count(&r.state),
        0,
        "coming back unchanged applies nothing"
    );
    r.handle.stop();
}
