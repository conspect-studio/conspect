//! The Boot / Loaded / Running configuration model (ADR-W024).
//!
//! * **Loaded** is the immutable boot snapshot held in memory (and persisted
//!   as `loaded.toml`); **Running** is Loaded + every live change, debounced-
//!   persisted to `active.toml` (atomic rename, machine-written, NEVER
//!   watched), composed by the SAME document composition the export uses.
//! * `POST /api/v1/config/revert-to-start` applies `diff(running, loaded)`
//!   through the ONE ADR-W020 apply machinery (live where live, honest
//!   restart warnings elsewhere).
//! * `POST /api/v1/config/promote` writes Running to the BOOT file path via
//!   the watcher's `expect_write()` suppression seam — promoting must NOT
//!   re-trigger a file-watch apply.
//! * `GET /api/v1/config/boot-model` reports the per-section divergence the
//!   UI indicator shows.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use multiview_config::{ConfigDiff, MultiviewConfig, StartMode};
use multiview_control::boot_model::{
    finish_running_persist, load_resume_config, persist_loaded, persist_running_now,
    spawn_running_persist, write_active_serialized, write_atomic, BootModel,
};
use multiview_control::config_watch::{
    apply_document_diff, spawn as spawn_watch, ManualPoll, WatchOptions,
};
use multiview_control::{
    command_bus, run_warning_ingest, AppState, Command, CommandReceiver, EngineStateSnapshot,
    InMemoryRepository, InMemoryWarningStore, WarningFilter, WarningRepository,
};
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use support::{body_json, get, put_json, send, ADMIN_TOKEN, OPERATOR_TOKEN, VIEWER_TOKEN};

/// The fast poll the watcher-interplay tests run at (real default is 1 s).
const TEST_POLL: Duration = Duration::from_millis(80);

/// How long assertions wait for a task to act.
const SETTLE: Duration = Duration::from_secs(10);

/// The boot document: one solid source, one bars source, two cells, one
/// output, and a control listener (so the composed Running document carries a
/// `[control]` block like a real deployment's).
const BOOT_DOC: &str = r##"schema_version = 1
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
[control]
listen = "[::1]:0"
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
path = "/tmp/boot-config-model.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;

/// The whole rig: a temp config dir holding the boot file, the shared
/// control-plane state (stores seeded from the boot doc + the boot model),
/// the router, and the engine-side command receiver.
struct Rig {
    /// Held for its `Drop` (removes the temp dir).
    _dir: tempfile::TempDir,
    boot_path: PathBuf,
    config: MultiviewConfig,
    state: AppState,
    router: Router,
    commands: CommandReceiver,
    warnings: Arc<dyn WarningRepository>,
}

/// Build the rig from `doc` written to a temp boot file, with the stores
/// seeded from it and a [`BootModel`] whose Loaded snapshot is the parsed
/// document (boot-mode start).
fn rig(doc: &str) -> Rig {
    rig_with(doc, 64)
}

/// [`rig`] with a chosen command-bus `capacity` (the shed tests run at 1).
fn rig_with(doc: &str, capacity: usize) -> Rig {
    let dir = tempfile::tempdir().expect("temp dir");
    let boot_path = dir.path().join("multiview.toml");
    std::fs::write(&boot_path, doc).expect("write boot config");
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
        publisher,
        sender,
        Arc::new(InMemoryRepository::new()),
        Arc::new(support::seeded_keys()),
    )
    .with_seeded_resources(seeded)
    .with_base_document(serde_json::to_value(&config).expect("boot doc to JSON"))
    .with_boot_model(Arc::new(BootModel::new(
        boot_path.clone(),
        config.clone(),
        config.clone(),
        StartMode::Boot,
        false,
        None,
    )));
    let router = multiview_control::router(state.clone());
    Rig {
        _dir: dir,
        boot_path,
        config,
        state,
        router,
        commands,
        warnings,
    }
}

/// Whether the active warning list carries `code`.
fn has_active_warning(warnings: &Arc<dyn WarningRepository>, code: &str) -> bool {
    warnings
        .list(&WarningFilter::active_only())
        .expect("list warnings")
        .iter()
        .any(|w| w.code.as_str() == code)
}

/// Build a bodyless `POST` with a Bearer token and an `Idempotency-Key`.
fn post_idem(path: &str, token: &str, key: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"));
    if let Some(key) = key {
        builder = builder.header("Idempotency-Key", key);
    }
    builder.body(Body::empty()).expect("request should build")
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

/// Re-colour `in_a` through the REAL sources route (GET → `ETag` → PUT with
/// `If-Match`), exactly as the UI mutates Running.
async fn recolor_in_a(r: &Rig, color: &str) {
    let resp = send(&r.router, get("/api/v1/sources/in_a", OPERATOR_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK, "in_a must be readable");
    let etag = support::etag(&resp).expect("source carries an ETag");
    let resp = send(
        &r.router,
        put_json(
            "/api/v1/sources/in_a",
            OPERATOR_TOKEN,
            Some(&etag),
            &serde_json::json!({
                "name": "in_a",
                "body": { "id": "in_a", "kind": "solid", "color": color }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "the live edit must land");
}

/// The stored colour of `in_a` in the sources store.
fn stored_color(state: &AppState) -> Option<String> {
    state
        .sources
        .get("in_a")
        .ok()
        .and_then(|v| v.resource.body.get("color").cloned())
        .and_then(|v| v.as_str().map(str::to_owned))
}

/// `GET /config/boot-model` reports no divergence at start, then names the
/// changed section (vs Loaded AND vs the untouched boot file) after a live
/// API edit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boot_model_status_reports_per_section_divergence() {
    let r = rig(BOOT_DOC);

    let resp = send(&r.router, get("/api/v1/config/boot-model", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["modeled"], serde_json::json!(true));
    assert_eq!(
        body["boot_path"],
        serde_json::json!(r.boot_path.display().to_string())
    );
    assert_eq!(body["start"], serde_json::json!("boot"));
    assert_eq!(body["resumed"], serde_json::json!(false));
    assert_eq!(
        body["diverged_from_loaded"],
        serde_json::json!([]),
        "a fresh run has not diverged from the Loaded snapshot"
    );
    assert_eq!(
        body["diverged_from_boot_file"],
        serde_json::json!([]),
        "a fresh run matches the boot file it started from"
    );

    recolor_in_a(&r, "#f0f0f0").await;

    let resp = send(&r.router, get("/api/v1/config/boot-model", VIEWER_TOKEN)).await;
    let body = body_json(resp).await;
    let loaded_divergence = body["diverged_from_loaded"]
        .as_array()
        .expect("diverged_from_loaded is an array")
        .clone();
    assert!(
        loaded_divergence.contains(&serde_json::json!("sources")),
        "the live source edit must show as a sources divergence, got {loaded_divergence:?}"
    );
    let file_divergence = body["diverged_from_boot_file"]
        .as_array()
        .expect("diverged_from_boot_file is an array")
        .clone();
    assert!(
        file_divergence.contains(&serde_json::json!("sources")),
        "the untouched boot file must also differ, got {file_divergence:?}"
    );
}

/// A store-only deployment (no boot model) is honest: `modeled: false`, and
/// both actions refuse with a problem document.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_boot_model_the_surface_is_honest() {
    let h = support::harness();

    let resp = send(&h.router, get("/api/v1/config/boot-model", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["modeled"], serde_json::json!(false));

    let resp = send(
        &h.router,
        post_idem("/api/v1/config/revert-to-start", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "revert without a boot model must refuse with 409"
    );

    let resp = send(
        &h.router,
        post_idem("/api/v1/config/promote", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "promote without a boot model must refuse with 409"
    );
}

/// Revert-to-start is a write action: a read-only key is forbidden.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revert_and_promote_require_the_write_role() {
    let r = rig(BOOT_DOC);
    let resp = send(
        &r.router,
        post_idem("/api/v1/config/revert-to-start", VIEWER_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let resp = send(
        &r.router,
        post_idem("/api/v1/config/promote", VIEWER_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// THE REVERT SLICE: after a live API edit, `revert-to-start` puts Running
/// back to Loaded THROUGH the one apply machinery — an `UpsertSource`
/// carrying the Loaded document rides the bounded bus, the store resyncs to
/// the Loaded value, and the action is audited under the requesting
/// principal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revert_to_start_restores_loaded_live() {
    let mut r = rig(BOOT_DOC);
    recolor_in_a(&r, "#f0f0f0").await;
    assert_eq!(stored_color(&r.state).as_deref(), Some("#f0f0f0"));
    // The live edit itself rides UpsertSource (ADR-W018); clear it so the
    // assertions below see only what the REVERT submits.
    let _ = r.commands.try_drain();

    let resp = send(
        &r.router,
        post_idem("/api/v1/config/revert-to-start", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    assert_eq!(body["reverted"], serde_json::json!(true));
    assert!(
        body["operation_id"].as_str().is_some_and(|s| !s.is_empty()),
        "the 202 carries an operation id"
    );
    let summary = body["summary"].as_array().expect("summary array").clone();
    assert!(
        summary
            .iter()
            .any(|p| p.as_str().is_some_and(|s| s.contains("in_a"))),
        "the per-section summary names the reverted source, got {summary:?}"
    );

    // Running := Loaded — the store mirrors the boot colour again…
    assert_eq!(
        stored_color(&r.state).as_deref(),
        Some("#101418"),
        "the sources store must resync to the Loaded snapshot"
    );
    // …and the SAME live machinery carried it: an UpsertSource with the
    // Loaded document's colour.
    let drained = r.commands.try_drain();
    let reverted = drained.iter().any(|c| match c {
        Command::UpsertSource { source, .. } => {
            source.id == "in_a"
                && serde_json::to_value(source.as_ref())
                    .ok()
                    .and_then(|v| v.get("color").and_then(|c| c.as_str().map(str::to_owned)))
                    .as_deref()
                    == Some("#101418")
        }
        _ => false,
    });
    assert!(
        reverted,
        "revert must ride UpsertSource with the Loaded colour, got {drained:?}"
    );

    // The action is audited under the requesting principal.
    let entries = r.state.audit.list(None).expect("audit list");
    assert!(
        entries
            .iter()
            .any(|e| e.actor == "operator-key" && e.object_id == "revert-to-start"),
        "revert must be audited under the principal"
    );
}

/// Reverting when Running already equals Loaded applies nothing and says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revert_with_no_divergence_is_an_honest_noop() {
    let mut r = rig(BOOT_DOC);
    let resp = send(
        &r.router,
        post_idem("/api/v1/config/revert-to-start", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    assert_eq!(
        body["reverted"],
        serde_json::json!(false),
        "an undiverged Running must report reverted: false"
    );
    assert!(
        r.commands.try_drain().is_empty(),
        "an undiverged revert must enqueue no engine commands"
    );
}

/// Watch-interplay pin (c): revert AFTER a file-watch apply returns to
/// **Loaded**, not to the current (edited) boot file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revert_after_a_file_watch_apply_returns_to_loaded() {
    let mut r = rig(BOOT_DOC);
    let (options, mut poll) = WatchOptions::default()
        .with_poll_interval(TEST_POLL)
        .with_manual_poll();
    let watch = spawn_watch(
        r.boot_path.clone(),
        r.config.clone(),
        r.state.clone(),
        options,
    );
    settle_initial(&mut poll, &r.state).await;

    // External boot-file edit: in_a goes near-white; the watcher applies it.
    // Driven by manual ticks (no wall-clock race): one poll registers the
    // candidate, the second crosses the debounce and applies.
    external_edit(&r.boot_path, &BOOT_DOC.replace("#101418", "#f0f0f0"));
    drive_polls(&mut poll, 2).await;
    assert_eq!(
        r.state.config_watch.snapshot().applied_count,
        1,
        "the watcher must apply the external edit"
    );
    assert_eq!(stored_color(&r.state).as_deref(), Some("#f0f0f0"));
    let _ = r.commands.try_drain();

    // Revert: Running goes back to the LOADED snapshot (#101418), even though
    // the boot file on disk now says #f0f0f0.
    let resp = send(
        &r.router,
        post_idem("/api/v1/config/revert-to-start", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_eq!(
        stored_color(&r.state).as_deref(),
        Some("#101418"),
        "revert must target Loaded, not the edited boot file"
    );
    let drained = r.commands.try_drain();
    assert!(
        drained.iter().any(|c| matches!(
            c,
            Command::UpsertSource { source, .. } if source.id == "in_a"
        )),
        "the revert must ride the live machinery, got {drained:?}"
    );
    watch.stop();
}

/// THE PROMOTE SLICE: promote writes the current Running document to the
/// boot file path (valid TOML carrying the live edit), commits a `boot`
/// config revision, audits, and reports the written path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn promote_writes_running_to_the_boot_file_and_versions_it() {
    let r = rig(BOOT_DOC);
    recolor_in_a(&r, "#e0e0e0").await;

    let resp = send(
        &r.router,
        post_idem("/api/v1/config/promote", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        body["path"],
        serde_json::json!(r.boot_path.display().to_string())
    );
    assert!(
        body["revision"].as_u64().is_some_and(|n| n >= 1),
        "promote must commit a config revision, got {body:?}"
    );

    // The boot file now IS the Running document: parses, validates, carries
    // the live edit.
    let text = std::fs::read_to_string(&r.boot_path).expect("read promoted boot file");
    let promoted = MultiviewConfig::load_from_toml(&text).expect("promoted file parses");
    promoted.validate().expect("promoted file validates");
    let color = promoted
        .sources
        .iter()
        .find(|s| s.id == "in_a")
        .and_then(|s| {
            serde_json::to_value(s)
                .ok()
                .and_then(|v| v.get("color").and_then(|c| c.as_str().map(str::to_owned)))
        });
    assert_eq!(
        color.as_deref(),
        Some("#e0e0e0"),
        "the promoted file must carry the live edit"
    );

    // Versioned + audited.
    let history = r
        .state
        .config_versions
        .history("boot")
        .expect("boot history");
    assert_eq!(history.len(), 1, "one promote = one committed revision");
    let entries = r.state.audit.list(None).expect("audit list");
    assert!(
        entries
            .iter()
            .any(|e| e.actor == "operator-key" && e.object_id == "promote"),
        "promote must be audited under the principal"
    );

    // After promote the Running document matches the boot FILE (the Loaded
    // snapshot divergence remains — promote moves Boot, not Loaded).
    let resp = send(&r.router, get("/api/v1/config/boot-model", VIEWER_TOKEN)).await;
    let body = body_json(resp).await;
    assert_eq!(
        body["diverged_from_boot_file"],
        serde_json::json!([]),
        "after promote, Running == the boot file"
    );
    let loaded_divergence = body["diverged_from_loaded"]
        .as_array()
        .expect("array")
        .clone();
    assert!(
        loaded_divergence.contains(&serde_json::json!("sources")),
        "the Loaded snapshot still differs (promote does not move Loaded)"
    );
}

/// A replayed `Idempotency-Key` answers without rewriting the file or
/// committing another revision.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn promote_idempotency_replay_does_not_rewrite() {
    let r = rig(BOOT_DOC);
    recolor_in_a(&r, "#d0d0d0").await;

    let resp = send(
        &r.router,
        post_idem("/api/v1/config/promote", OPERATOR_TOKEN, Some("promote-1")),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let first = body_json(resp).await;
    assert_eq!(first["replayed"], serde_json::json!(false));
    let written = std::fs::read_to_string(&r.boot_path).expect("read boot file");

    // A later live edit makes Running differ again; the REPLAY must not
    // re-promote it.
    recolor_in_a(&r, "#0a0a0a").await;
    let resp = send(
        &r.router,
        post_idem("/api/v1/config/promote", OPERATOR_TOKEN, Some("promote-1")),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let replay = body_json(resp).await;
    assert_eq!(replay["replayed"], serde_json::json!(true));
    assert_eq!(
        replay["operation_id"], first["operation_id"],
        "the replay echoes the original operation id"
    );
    assert_eq!(
        std::fs::read_to_string(&r.boot_path).expect("read boot file"),
        written,
        "a replay must not rewrite the boot file"
    );
    let history = r
        .state
        .config_versions
        .history("boot")
        .expect("boot history");
    assert_eq!(history.len(), 1, "a replay must not commit a new revision");
}

/// Watch-interplay pin (a): a promote does NOT re-trigger a file-watch apply
/// — the server-side write is suppressed via `expect_write()` and adopted as
/// the watcher's new baseline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn promote_does_not_retrigger_a_file_watch_apply() {
    let mut r = rig(BOOT_DOC);
    let (options, mut poll) = WatchOptions::default()
        .with_poll_interval(TEST_POLL)
        .with_manual_poll();
    let watch = spawn_watch(
        r.boot_path.clone(),
        r.config.clone(),
        r.state.clone(),
        options,
    );
    settle_initial(&mut poll, &r.state).await;

    recolor_in_a(&r, "#c0c0c0").await;
    let _ = r.commands.try_drain();
    let resp = send(
        &r.router,
        post_idem("/api/v1/config/promote", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Several DETERMINISTIC poll iterations: the watcher must adopt (not apply)
    // the promote's own write via the `expect_write` suppression seam. Two
    // ticks cross the debounce and consume the banked token; the extra ticks
    // prove the adoption is stable (the apply count never moves), with no
    // wall-clock window to race.
    drive_polls(&mut poll, 4).await;
    assert_eq!(
        r.state.config_watch.snapshot().applied_count,
        0,
        "a promote must not count as a file-watch apply"
    );
    assert!(
        r.commands.try_drain().is_empty(),
        "a promote's own write must enqueue no watcher commands"
    );
    assert_eq!(
        stored_color(&r.state).as_deref(),
        Some("#c0c0c0"),
        "the stores keep the promoted Running state untouched"
    );

    // Watching resumes against the ADOPTED baseline: a real external edit
    // still applies (candidate on the first poll, applied on the second).
    let text = std::fs::read_to_string(&r.boot_path).expect("read promoted file");
    external_edit(&r.boot_path, &text.replace("#c0c0c0", "#101418"));
    drive_polls(&mut poll, 2).await;
    assert_eq!(
        r.state.config_watch.snapshot().applied_count,
        1,
        "an external edit after a promote must still hot-apply"
    );
    watch.stop();
}

/// Watch-interplay pin (b): in a RESUMED run (the watcher's baseline is the
/// resumed Running document, the file still holds the boot document) an
/// external boot-file edit still hot-applies — and the diff is computed
/// against the RESUMED baseline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_external_boot_edit_during_a_resumed_run_still_hot_applies() {
    // The resumed Running state: in_a near-white (differs from the boot doc).
    let active_doc = BOOT_DOC.replace("#101418", "#f0f0f0");
    let dir = tempfile::tempdir().expect("temp dir");
    let boot_path = dir.path().join("multiview.toml");
    std::fs::write(&boot_path, BOOT_DOC).expect("write boot file");
    let boot_config = MultiviewConfig::load_from_toml(BOOT_DOC).expect("parse boot");
    let active_config = MultiviewConfig::load_from_toml(&active_doc).expect("parse active");

    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let (sender, mut commands) = command_bus(64);
    // Stores seeded from the RESUMED document (it is the starting Running
    // state), Loaded stays the BOOT snapshot.
    let seeded = multiview_control::seed_resources(&active_config).expect("seed stores");
    let state = AppState::new(
        publisher,
        sender,
        Arc::new(InMemoryRepository::new()),
        Arc::new(support::seeded_keys()),
    )
    .with_seeded_resources(seeded)
    .with_base_document(serde_json::to_value(&active_config).expect("active doc to JSON"))
    .with_boot_model(Arc::new(BootModel::new(
        boot_path.clone(),
        boot_config,
        active_config.clone(),
        StartMode::Resume,
        true,
        None,
    )));
    // The watcher's baseline is the RESUMED Running document.
    let (options, mut poll) = WatchOptions::default()
        .with_poll_interval(TEST_POLL)
        .with_manual_poll();
    let watch = spawn_watch(boot_path.clone(), active_config, state.clone(), options);

    // External edit to the BOOT file: add a new source (in_a stays at the
    // boot colour, which DIFFERS from the resumed baseline's). The edit is in
    // place before any poll, so two deterministic ticks (candidate then apply)
    // diff it against the RESUMED baseline and apply it — no wall-clock race.
    std::fs::write(
        &boot_path,
        format!("{BOOT_DOC}[[sources]]\nid = \"live9\"\nkind = \"bars\"\n"),
    )
    .expect("edit boot file");
    drive_polls(&mut poll, 2).await;
    assert_eq!(
        state.config_watch.snapshot().applied_count,
        1,
        "the boot-file edit must hot-apply during a resumed run"
    );
    let drained = commands.try_drain();
    assert!(
        drained.iter().any(|c| matches!(
            c,
            Command::UpsertSource { source, .. } if source.id == "live9"
        )),
        "the added source must ride UpsertSource, got {drained:?}"
    );
    assert!(
        drained.iter().any(|c| matches!(
            c,
            Command::UpsertSource { source, .. } if source.id == "in_a"
        )),
        "in_a must be re-applied: the file's colour differs from the RESUMED baseline, got {drained:?}"
    );
    watch.stop();
}

/// Review m4 — the resume no-op rewrite guard: in a RESUMED run the watcher
/// observes the UNCHANGED boot file (its content differs from the resumed
/// baseline, but it is exactly what the run loaded at boot); it must NOT
/// re-apply the boot document over the resumed Running state. A REAL edit —
/// content differing from the last observed content — still hot-applies,
/// diffed against the RESUMED baseline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resumed_run_does_not_reapply_the_unchanged_boot_file() {
    // The resumed Running state: in_a near-white (differs from the boot doc).
    let active_doc = BOOT_DOC.replace("#101418", "#f0f0f0");
    let dir = tempfile::tempdir().expect("temp dir");
    let boot_path = dir.path().join("multiview.toml");
    std::fs::write(&boot_path, BOOT_DOC).expect("write boot file");
    let boot_config = MultiviewConfig::load_from_toml(BOOT_DOC).expect("parse boot");
    let active_config = MultiviewConfig::load_from_toml(&active_doc).expect("parse active");

    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let (sender, mut commands) = command_bus(64);
    let seeded = multiview_control::seed_resources(&active_config).expect("seed stores");
    let state = AppState::new(
        publisher,
        sender,
        Arc::new(InMemoryRepository::new()),
        Arc::new(support::seeded_keys()),
    )
    .with_seeded_resources(seeded)
    .with_base_document(serde_json::to_value(&active_config).expect("active doc to JSON"))
    .with_boot_model(Arc::new(BootModel::new(
        boot_path.clone(),
        boot_config,
        active_config.clone(),
        StartMode::Resume,
        true,
        None,
    )));
    // The watcher starts exactly as a resumed run wires it: the baseline is
    // the RESUMED document, and the boot-load observed the boot file's text.
    let (options, mut poll) = WatchOptions::default()
        .with_poll_interval(TEST_POLL)
        .with_initial_observed(BOOT_DOC.to_owned())
        .with_manual_poll();
    let watch = spawn_watch(boot_path.clone(), active_config, state.clone(), options);

    // Several DETERMINISTIC poll iterations against the UNCHANGED boot file:
    // its content equals the seeded last-observed text, so the watcher adopts
    // it as the baseline fingerprint WITHOUT applying — and stays there across
    // further ticks (no wall-clock window to race a spurious apply).
    drive_polls(&mut poll, 4).await;
    assert_eq!(
        state.config_watch.snapshot().applied_count,
        0,
        "the unchanged boot file must not clobber the resumed Running state"
    );
    assert!(
        commands.try_drain().is_empty(),
        "no engine commands for an unchanged boot file"
    );
    assert_eq!(
        stored_color(&state).as_deref(),
        Some("#f0f0f0"),
        "the stores keep the resumed values"
    );

    // A REAL edit still hot-applies, diffed against the RESUMED baseline
    // (candidate on the first poll, applied on the second).
    external_edit(&boot_path, &BOOT_DOC.replace("#101418", "#123456"));
    drive_polls(&mut poll, 2).await;
    assert_eq!(
        state.config_watch.snapshot().applied_count,
        1,
        "a real boot-file edit during a resumed run must still hot-apply"
    );
    let drained = commands.try_drain();
    assert!(
        drained.iter().any(|c| match c {
            Command::UpsertSource { source, .. } => {
                source.id == "in_a"
                    && serde_json::to_value(source.as_ref())
                        .ok()
                        .and_then(|v| v.get("color").and_then(|c| c.as_str().map(str::to_owned)))
                        .as_deref()
                        == Some("#123456")
            }
            _ => false,
        }),
        "the real edit must ride UpsertSource with the edited colour, got {drained:?}"
    );
    watch.stop();
}

/// Pin (d): the persisted `active.toml` round-trips
/// `MultiviewConfig::load_from_toml` + `validate`, and carries the live edit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_toml_round_trips_validate() {
    let r = rig(BOOT_DOC);
    recolor_in_a(&r, "#b0b0b0").await;

    persist_running_now(&r.state)
        .await
        .expect("persist Running");

    let active_path = r
        .state
        .boot_model
        .as_ref()
        .expect("boot model")
        .active_path();
    let text = std::fs::read_to_string(&active_path).expect("read active.toml");
    let active = MultiviewConfig::load_from_toml(&text).expect("active.toml parses");
    active.validate().expect("active.toml validates");
    let color = active
        .sources
        .iter()
        .find(|s| s.id == "in_a")
        .and_then(|s| {
            serde_json::to_value(s)
                .ok()
                .and_then(|v| v.get("color").and_then(|c| c.as_str().map(str::to_owned)))
        });
    assert_eq!(
        color.as_deref(),
        Some("#b0b0b0"),
        "active.toml must carry the live Running state"
    );
}

/// Pins (e)/(f) at the loader: a valid `active.toml` loads as the resume
/// state; a corrupt one is an error the caller falls back on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn load_resume_config_loads_valid_and_rejects_corrupt_active() {
    let dir = tempfile::tempdir().expect("temp dir");
    let boot_path = dir.path().join("multiview.toml");
    std::fs::write(&boot_path, BOOT_DOC).expect("write boot file");
    let state_dir = dir.path().join(".multiview");
    std::fs::create_dir_all(&state_dir).expect("state dir");

    // Missing active: an error (the caller warns + falls back to boot).
    assert!(load_resume_config(&boot_path).is_err());

    // Valid active: loads as the starting Running state.
    std::fs::write(
        state_dir.join("active.toml"),
        BOOT_DOC.replace("#101418", "#f0f0f0"),
    )
    .expect("write active");
    let resumed = load_resume_config(&boot_path).expect("valid active loads");
    let color = resumed
        .sources
        .iter()
        .find(|s| s.id == "in_a")
        .and_then(|s| {
            serde_json::to_value(s)
                .ok()
                .and_then(|v| v.get("color").and_then(|c| c.as_str().map(str::to_owned)))
        });
    assert_eq!(color.as_deref(), Some("#f0f0f0"));

    // Corrupt active: an error naming the failure.
    std::fs::write(state_dir.join("active.toml"), "this is [not toml").expect("corrupt active");
    let err = load_resume_config(&boot_path).expect_err("corrupt active must be rejected");
    assert!(
        err.contains("parse") || err.contains("TOML") || err.contains("read"),
        "the fallback reason should be actionable, got: {err}"
    );
}

/// The Running persister: an audit-recorded mutation (the ONE choke point)
/// leads — debounced — to an atomic `active.toml` write that follows further
/// changes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_running_persister_writes_active_toml_on_audited_changes() {
    let r = rig(BOOT_DOC);
    let active_path = r
        .state
        .boot_model
        .as_ref()
        .expect("boot model")
        .active_path();
    let task = spawn_running_persist(r.state.clone(), Duration::from_millis(50));

    // The choke point: a real route mutation records an audit entry, which
    // triggers the debounced persist.
    recolor_in_a(&r, "#a0a0a0").await;
    assert!(
        wait_until(SETTLE, || {
            std::fs::read_to_string(&active_path)
                .ok()
                .is_some_and(|t| t.contains("#a0a0a0"))
        })
        .await,
        "the persister must write active.toml carrying the audited change"
    );

    // A further change follows within another debounce window.
    recolor_in_a(&r, "#909090").await;
    assert!(
        wait_until(SETTLE, || {
            std::fs::read_to_string(&active_path)
                .ok()
                .is_some_and(|t| t.contains("#909090"))
        })
        .await,
        "the persister must follow subsequent changes"
    );
    // Atomic write hygiene: no temp residue next to the state files.
    let residue: Vec<String> = std::fs::read_dir(active_path.parent().expect("state dir"))
        .expect("read state dir")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != "active.toml" && n != "loaded.toml")
        .collect();
    assert!(
        residue.is_empty(),
        "atomic writes must leave no temp residue, got {residue:?}"
    );
    task.abort();
}

/// Review M2 (delta round) — write serialization is ticket-ordered: a STALE
/// composition (an older ticket, e.g. a persist write left running detached
/// on the blocking pool by an aborted task) is skipped once newer content
/// landed — `active.toml` content is monotonic, never regressing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_persist_write_never_overwrites_newer_content() {
    let r = rig(BOOT_DOC);
    let model = r.state.boot_model.as_ref().expect("boot model");
    let early = model.next_write_ticket();
    let late = model.next_write_ticket();

    assert!(
        write_active_serialized(model, late, "newer = 2\n").expect("newer write"),
        "the newer ticket writes"
    );
    assert!(
        !write_active_serialized(model, early, "stale = 1\n").expect("stale write call"),
        "the stale ticket must be SKIPPED once newer content landed"
    );
    assert_eq!(
        std::fs::read_to_string(model.active_path()).expect("read active.toml"),
        "newer = 2\n",
        "active.toml keeps the newer content"
    );
}

/// Review M2 — the teardown ordering: `finish_running_persist` aborts the
/// persister, AWAITS its termination, and only then runs the final persist,
/// capturing changes younger than the debounce even when the task is parked
/// deep inside a long debounce window — with no temp residue (the
/// deterministic `.tmp` stays single-writer through the teardown).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finish_running_persist_captures_changes_younger_than_the_debounce() {
    let r = rig(BOOT_DOC);
    let active_path = r
        .state
        .boot_model
        .as_ref()
        .expect("boot model")
        .active_path();
    // A debounce far longer than the test: the change below stays younger
    // than the debounce throughout, so ONLY the ordered teardown persist can
    // capture it.
    let task = spawn_running_persist(r.state.clone(), Duration::from_secs(600));
    assert!(
        wait_until(SETTLE, || active_path.exists()).await,
        "the startup persist writes the starting Running state"
    );

    // The audited change fires the notify; the task then sleeps its 600 s
    // debounce — parked, with the change unpersisted.
    recolor_in_a(&r, "#717171").await;

    finish_running_persist(task, &r.state).await;

    let text = std::fs::read_to_string(&active_path).expect("read active.toml");
    assert!(
        text.contains("#717171"),
        "the final persist must capture the change younger than the debounce"
    );
    let residue: Vec<String> = std::fs::read_dir(active_path.parent().expect("state dir"))
        .expect("read state dir")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != "active.toml" && n != "loaded.toml")
        .collect();
    assert!(
        residue.is_empty(),
        "the ordered teardown leaves no temp residue, got {residue:?}"
    );
}

/// Fail-soft: a state whose stores do not compose (no working layout) warns
/// and writes nothing — the persister never crashes and never wedges.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_running_persister_is_fail_soft_when_composition_fails() {
    let dir = tempfile::tempdir().expect("temp dir");
    let boot_path = dir.path().join("multiview.toml");
    std::fs::write(&boot_path, BOOT_DOC).expect("write boot file");
    let config = MultiviewConfig::load_from_toml(BOOT_DOC).expect("parse");

    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let (sender, _commands) = command_bus(64);
    // NO seeded resources: there is no working layout, so composition fails.
    let state = AppState::new(
        publisher,
        sender,
        Arc::new(InMemoryRepository::new()),
        Arc::new(support::seeded_keys()),
    )
    .with_boot_model(Arc::new(BootModel::new(
        boot_path,
        config.clone(),
        config,
        StartMode::Boot,
        false,
        None,
    )));
    let active_path = state.boot_model.as_ref().expect("boot model").active_path();
    let task = spawn_running_persist(state.clone(), Duration::from_millis(50));

    state.audit(
        "test",
        multiview_control::AuditAction::Update,
        "source",
        "in_a",
        None,
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !active_path.exists(),
        "a failed composition must not write active.toml"
    );
    assert!(
        !task.is_finished(),
        "the persister must survive a composition failure (fail-soft)"
    );
    task.abort();
}

/// MAJOR-B (panel, correctness): a SHED file-watch apply must NOT yield an
/// `active.toml` that resumes as adopted state. The watcher's `apply_change`
/// optimistically resyncs the stores (which audits → fires `running_changed`),
/// but when an engine command is shed it returns `Retry` WITHOUT advancing the
/// baseline — the engine never adopted the change. The debounced persister must
/// not snapshot that mutated-but-unadopted store state: a crash before the
/// retry completes would otherwise `start="resume"` into a configuration the
/// engine never ran. `active.toml` must reflect only adopted state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shed_file_watch_apply_is_not_persisted_as_adopted() {
    let r = rig_with(BOOT_DOC, 1);
    let model = r.state.boot_model.clone().expect("rig wires a boot model");
    let active_path = model.active_path();

    // Spawn the file-watcher (baseline = the boot config) driven by MANUAL
    // ticks (deterministic), and the debounced persister, sharing the one
    // AppState.
    let (options, mut poll) = WatchOptions::default()
        .with_poll_interval(TEST_POLL)
        .with_manual_poll();
    let watch = spawn_watch(
        r.boot_path.clone(),
        r.config.clone(),
        r.state.clone(),
        options,
    );
    let persist = spawn_running_persist(r.state.clone(), Duration::from_millis(40));

    // Occupy the capacity-1 command bus so the watcher's UpsertSource is SHED.
    // (A bare submit the engine never drains keeps the slot full for the test.)
    assert!(
        r.state
            .commands
            .try_submit(Command::UpsertSource {
                op: multiview_control::OperationId::new(),
                source: Box::new(r.config.sources[0].clone()),
            })
            .is_ok(),
        "the first submit fills the capacity-1 bus"
    );

    // Edit the BOOT file: change in_a to a SYNTHETIC source colour the watcher
    // applies via UpsertSource — which sheds on the full bus (apply_change ->
    // Retry), leaving the sources store optimistically mutated to #5e5e5e.
    let edited = BOOT_DOC.replace("#101418", "#5e5e5e");
    std::fs::write(&r.boot_path, &edited).expect("edit boot file");

    // Two deterministic ticks settle + apply the edit; the SHED happens within
    // the apply (no poll/debounce race). The incomplete WARNING then surfaces
    // through the async event-ingest pipeline (publish → `run_warning_ingest` →
    // store), a separate hop from the watcher poll, so it is awaited (briefly —
    // the apply already ran) rather than asserted synchronously.
    drive_polls(&mut poll, 2).await;
    assert!(
        wait_until(SETTLE, || has_active_warning(
            &r.warnings,
            "config-file-apply-incomplete"
        ))
        .await,
        "the shed file-watch apply must raise the incomplete warning"
    );

    // Give the persister several debounce windows to (wrongly) write the
    // unadopted state (the persister runs on its own timer, not the watcher's).
    tokio::time::sleep(TEST_POLL * 12).await;

    // active.toml must NOT carry the shed (unadopted) #5e5e5e — the engine
    // never adopted it.
    if let Ok(text) = std::fs::read_to_string(&active_path) {
        assert!(
            !text.contains("#5e5e5e"),
            "active.toml must not persist the shed/unadopted file-watch change as adopted state"
        );
    }

    watch.stop();
    persist.abort();
}

/// MAJOR-B / B-1 (panel): the adopted snapshot must never STICK. A applied →
/// edit to B while the bus is full (B sheds, the engine never adopts it) → edit
/// the file back to A: the watcher reaches the already-applied content, so the
/// engine runs A again. `active.toml` must reflect the adopted A throughout and
/// NEVER the shed B — never frozen stale because a settle-to-A path forgot to
/// re-adopt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_persist_gate_clears_when_a_shed_edit_reverts_to_adopted() {
    let r = rig_with(BOOT_DOC, 1);
    let model = r.state.boot_model.clone().expect("rig wires a boot model");
    let active_path = model.active_path();
    let (options, mut poll) = WatchOptions::default()
        .with_poll_interval(TEST_POLL)
        .with_manual_poll();
    let watch = spawn_watch(
        r.boot_path.clone(),
        r.config.clone(),
        r.state.clone(),
        options,
    );
    let persist = spawn_running_persist(r.state.clone(), Duration::from_millis(40));

    // Let A (the boot config) persist first (the persister's startup write,
    // independent of the watcher).
    assert!(
        wait_until(SETTLE, || std::fs::read_to_string(&active_path)
            .ok()
            .is_some_and(|t| t.contains("#101418")))
        .await,
        "the initial adopted state A must persist"
    );

    // Occupy the capacity-1 bus so the file edit to B sheds.
    assert!(
        r.state
            .commands
            .try_submit(Command::UpsertSource {
                op: multiview_control::OperationId::new(),
                source: Box::new(r.config.sources[0].clone()),
            })
            .is_ok(),
        "fill the bus"
    );
    // Edit to B: the watcher's UpsertSource sheds → the gate freezes. Two
    // deterministic ticks settle + apply (shedding); the incomplete warning
    // then surfaces through the async event-ingest pipeline (a separate hop
    // from the watcher poll), so it is awaited briefly.
    std::fs::write(&r.boot_path, BOOT_DOC.replace("#101418", "#7b7b7b")).expect("edit to B");
    drive_polls(&mut poll, 2).await;
    assert!(
        wait_until(SETTLE, || has_active_warning(
            &r.warnings,
            "config-file-apply-incomplete"
        ))
        .await,
        "the shed edit to B raises the incomplete warning (B is not adopted)"
    );
    // While B is shed-pending, active.toml must never carry the unadopted B
    // (the persister runs on its own timer; give it windows to (wrongly) act).
    tokio::time::sleep(TEST_POLL * 6).await;
    if let Ok(text) = std::fs::read_to_string(&active_path) {
        assert!(
            !text.contains("#7b7b7b"),
            "active.toml must never carry the shed/unadopted B"
        );
    }

    // Now edit the file BACK to A. The watcher reaches the already-applied
    // content; the snapshot re-adopts A (clearing the incomplete gate) and a
    // later adopted edit still persists (the snapshot never sticks). Two ticks
    // settle + re-adopt; re-adopting A is the EMPTY-diff path (no engine
    // command, so the still-full bus is irrelevant) and it fires
    // `running_changed`, so the persister re-converges active.toml to A.
    std::fs::write(&r.boot_path, BOOT_DOC).expect("edit back to A");
    drive_polls(&mut poll, 2).await;
    assert!(
        wait_until(SETTLE, || {
            std::fs::read_to_string(&active_path)
                .ok()
                .is_some_and(|t| t.contains("#101418") && !t.contains("#7b7b7b"))
        })
        .await,
        "active.toml must reflect the adopted A (never the shed B) and not be frozen"
    );
    // active.toml is A, and never the unadopted B.
    let text = std::fs::read_to_string(&active_path).expect("read active");
    assert!(
        text.contains("#101418"),
        "active.toml reflects the adopted A"
    );
    assert!(
        !text.contains("#7b7b7b"),
        "active.toml must never carry the shed/unadopted B"
    );

    watch.stop();
    persist.abort();
}

/// MAJOR-B round 2 / B-2 (panel): the original defect via REST. A REST source
/// upsert that SHEDS (the engine bus is full) updates the store + audits BEFORE
/// the bus, so without the unified gate the persister would write the
/// requested-but-unadopted source state to `active.toml`. The gate must freeze
/// persistence: `active.toml` must NOT carry the shed REST change.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shed_rest_source_upsert_is_not_persisted_as_adopted() {
    let mut r = rig_with(BOOT_DOC, 1);
    let model = r.state.boot_model.clone().expect("rig wires a boot model");
    let active_path = model.active_path();
    let persist = spawn_running_persist(r.state.clone(), Duration::from_millis(40));

    // Occupy the capacity-1 bus so the REST upsert's own UpsertSource sheds.
    assert!(
        r.state
            .commands
            .try_submit(Command::UpsertSource {
                op: multiview_control::OperationId::new(),
                source: Box::new(r.config.sources[0].clone()),
            })
            .is_ok(),
        "fill the bus"
    );

    // ACTUALLY perform a REST recolor of in_a to a SYNTHETIC colour (a
    // live-appliable kind) over the full bus. The route commits the store +
    // audits (firing running_changed) BUT its UpsertSource sheds →
    // ApplyMode::Restart, so the engine never adopts #3c3c3c. (F3, rule 19:
    // the prior version only filled the bus and asserted a value that was never
    // submitted was absent — a tautology that passed without exercising the
    // shed-REST path at all. This drives the real mutation so the assertion
    // genuinely guards the gate.)
    recolor_in_a(&r, "#3c3c3c").await;
    assert_eq!(
        stored_color(&r.state).as_deref(),
        Some("#3c3c3c"),
        "the shed REST edit still COMMITS the store (ADR-W018) — only the engine \
         did not adopt it, so this is a real requested-but-unadopted state"
    );

    // Give the persister several windows to (wrongly) write the unadopted state.
    tokio::time::sleep(Duration::from_millis(300)).await;
    if let Ok(text) = std::fs::read_to_string(&active_path) {
        assert!(
            !text.contains("#3c3c3c"),
            "active.toml must not persist the shed/unadopted REST source state as adopted"
        );
    }

    // Drain the bus + a follow-up adopted edit catches the gate up and persists.
    let _ = r.commands.try_drain();
    recolor_in_a(&r, "#4d4d4d").await;
    assert!(
        wait_until(SETTLE, || std::fs::read_to_string(&active_path)
            .ok()
            .is_some_and(|t| t.contains("#4d4d4d")))
        .await,
        "a later adopted REST mutation must catch the gate up and persist"
    );
    persist.abort();
}

/// `write_atomic` replaces the destination in one rename and leaves no
/// temp-file residue.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_atomic_replaces_and_leaves_no_residue() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("state.toml");
    write_atomic(&path, "first = 1\n").expect("first write");
    write_atomic(&path, "second = 2\n").expect("replace write");
    assert_eq!(
        std::fs::read_to_string(&path).expect("read"),
        "second = 2\n"
    );
    let entries: Vec<String> = std::fs::read_dir(dir.path())
        .expect("read dir")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, vec!["state.toml".to_owned()]);
}

/// The file mode of `path` (permission bits only).
#[cfg(unix)]
fn mode_of(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).expect("stat").permissions().mode() & 0o7777
}

/// Review M3: `write_atomic` must preserve the DESTINATION's mode — a
/// `chmod 600` boot/state file must not silently widen to the umask default
/// when it is atomically replaced.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_atomic_preserves_the_destination_mode() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("state.toml");
    write_atomic(&path, "first = 1\n").expect("first write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod 600");
    write_atomic(&path, "second = 2\n").expect("replace write");
    assert_eq!(
        std::fs::read_to_string(&path).expect("read"),
        "second = 2\n"
    );
    assert_eq!(
        mode_of(&path),
        0o600,
        "an atomic replace must preserve the destination's mode (chmod-600 stays 600)"
    );
}

/// Review M3 at the route: a promote over a `chmod 600` boot file keeps the
/// boot file at mode 600 (credentials in the config must not leak to the
/// umask default through the temp-file + rename).
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn promote_preserves_the_boot_file_mode() {
    use std::os::unix::fs::PermissionsExt as _;
    let r = rig(BOOT_DOC);
    std::fs::set_permissions(&r.boot_path, std::fs::Permissions::from_mode(0o600))
        .expect("chmod 600 the boot file");
    recolor_in_a(&r, "#e8e8e8").await;

    let resp = send(
        &r.router,
        post_idem("/api/v1/config/promote", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let text = std::fs::read_to_string(&r.boot_path).expect("read promoted file");
    assert!(
        text.contains("#e8e8e8"),
        "the promoted file carries the live edit"
    );
    assert_eq!(
        mode_of(&r.boot_path),
        0o600,
        "promote must preserve the boot file's mode (chmod-600 stays 600)"
    );
}

/// MAJOR-A (panel, security): `write_atomic` must create a NEW file at mode
/// 0600 — not the umask default (typically 0644). `loaded.toml`/`active.toml`
/// carry the composed Running config WITH secrets intact (WebRTC ICE password,
/// `static_auth_secret`, WHIP bearer tokens), so a first write at 0644 leaves
/// those credentials group/world-readable. The existing mode test only covers
/// PRESERVING an already-0600 destination; this covers the first create.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_atomic_creates_a_new_file_at_mode_600() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("secret-state.toml");
    write_atomic(&path, "token = \"super-secret\"\n").expect("first write");
    assert_eq!(
        mode_of(&path),
        0o600,
        "a first-create atomic write must be 0600, not the umask default — \
         the state files carry plaintext credentials"
    );
}

/// MAJOR-A at the persistence layer: a FIRST persist of the Running snapshot
/// (`active.toml`) and the Loaded snapshot (`loaded.toml`) on a fresh deployment
/// must produce 0600 files inside a 0700 `.multiview` state dir. Without a
/// pre-existing destination there is nothing to preserve, so the writer itself
/// must tighten the mode before any secret bytes land.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_persist_writes_state_files_0600_in_a_0700_dir() {
    let r = rig(BOOT_DOC);
    // A fresh deployment: no .multiview dir yet.
    let model = r.state.boot_model.clone().expect("rig wires a boot model");
    persist_loaded(&model).expect("persist loaded");
    persist_running_now(&r.state)
        .await
        .expect("persist running");

    let state_dir = model.state_dir();
    let loaded = model.loaded_path();
    let active = model.active_path();
    assert!(loaded.exists(), "loaded.toml is written");
    assert!(active.exists(), "active.toml is written");
    assert_eq!(
        mode_of(&loaded),
        0o600,
        "loaded.toml (plaintext secrets) must be 0600 on first write"
    );
    assert_eq!(
        mode_of(&active),
        0o600,
        "active.toml (plaintext secrets) must be 0600 on first write"
    );
    assert_eq!(
        mode_of(&state_dir),
        0o700,
        "the .multiview state dir must be 0700 (its contents carry credentials)"
    );
}

/// MAJOR-A round 2 (panel, security): a PRE-EXISTING world-readable temp file
/// at the deterministic `.<name>.tmp` path (a prior crash / old binary) must
/// NOT receive the plaintext secret bytes. With an exclusive, randomly-named
/// temp the planted file is simply ignored; the real destination ends up 0600
/// and the planted 0644 file never holds our content.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_atomic_ignores_a_preexisting_world_readable_temp() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("active.toml");
    // An attacker / a prior crash left the deterministic temp name at 0644.
    let planted = dir.path().join(".active.toml.tmp");
    std::fs::write(&planted, "stale = 0\n").expect("plant temp");
    std::fs::set_permissions(&planted, std::fs::Permissions::from_mode(0o644)).expect("chmod 644");

    write_atomic(&path, "token = \"super-secret\"\n").expect("write");

    assert_eq!(
        mode_of(&path),
        0o600,
        "the destination must be 0600 regardless of any planted temp file"
    );
    // If the planted 0644 file still exists, it must NOT carry our secret.
    if let Ok(text) = std::fs::read_to_string(&planted) {
        assert!(
            !text.contains("super-secret"),
            "the secret must never be written into a pre-existing world-readable temp inode"
        );
    }
}

/// MAJOR-A round 2 (panel, security): a SYMLINK planted at the temp path in a
/// shared `.multiview` must NOT be followed — `open()` through it would write
/// the secrets to the link target and the rename would install the symlink as
/// `active.toml`. An exclusive, randomly-named temp defeats the attack: the
/// planted symlink at the deterministic name is untouched, and the real file is
/// a regular 0600 file carrying the content.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_atomic_does_not_follow_a_symlink_planted_at_the_temp_path() {
    let dir = tempfile::tempdir().expect("temp dir");
    let target = dir.path().join("attacker-target");
    std::fs::write(&target, "").expect("create attacker target");
    let path = dir.path().join("active.toml");
    // The attacker pre-plants the deterministic temp name as a symlink.
    let planted = dir.path().join(".active.toml.tmp");
    std::os::unix::fs::symlink(&target, &planted).expect("plant symlink");

    write_atomic(&path, "token = \"super-secret\"\n").expect("write");

    // The real destination is a REGULAR file (not the symlink) at 0600.
    let meta = std::fs::symlink_metadata(&path).expect("stat dest");
    assert!(
        meta.file_type().is_file(),
        "active.toml must be a regular file, never an installed symlink"
    );
    assert_eq!(mode_of(&path), 0o600, "the destination must be 0600");
    // The attacker target must NOT have received the secret through the link.
    let target_text = std::fs::read_to_string(&target).expect("read target");
    assert!(
        !target_text.contains("super-secret"),
        "the secret must never be written through a planted symlink"
    );
}

/// MAJOR-A round 2 (panel, security): persistence must FAIL-CLOSED when the
/// `.multiview` state dir is group/world-writable (an attacker could swap the
/// state files). Refusing to persist does NOT take output off air (the output
/// clock is untouched — control-plane persistence only). A first persist into a
/// 0777 state dir must write nothing and surface an error.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persist_refuses_a_group_or_world_writable_state_dir() {
    use std::os::unix::fs::PermissionsExt as _;
    let r = rig(BOOT_DOC);
    let model = r.state.boot_model.clone().expect("rig wires a boot model");
    let state_dir = model.state_dir();
    std::fs::create_dir_all(&state_dir).expect("create state dir");
    // An insecure, world-writable state dir.
    std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o777))
        .expect("chmod 777 the state dir");

    let result = persist_running_now(&r.state).await;
    assert!(
        result.is_err(),
        "persistence must fail-closed on a group/world-writable state dir"
    );
    assert!(
        !model.active_path().exists(),
        "no active.toml (with plaintext secrets) may be written into an insecure dir"
    );
}

/// Review B1 interleaving (1) — the settle-window race: promote writes W and
/// banks its expect token, but an external edit E lands BEFORE W settles, so
/// the watcher's first settled observation is E. E is a REAL external change:
/// it must be APPLIED through the one machinery, never adopted against the
/// banked (W-content) token.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_promote_racing_an_external_edit_applies_the_edit() {
    let mut r = rig(BOOT_DOC);
    let (options, mut poll) = WatchOptions::default()
        .with_poll_interval(TEST_POLL)
        .with_manual_poll();
    let watch = spawn_watch(
        r.boot_path.clone(),
        r.config.clone(),
        r.state.clone(),
        options,
    );

    recolor_in_a(&r, "#c0c0c0").await;
    let _ = r.commands.try_drain();
    let resp = send(
        &r.router,
        post_idem("/api/v1/config/promote", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // The external edit lands after the promote's write but BEFORE the watcher
    // polls at all — so the first SETTLED observation is E, not the promote's
    // W (the deterministic expression of "E supersedes W inside the settle
    // window"). Two ticks (candidate then apply) apply E through the one
    // machinery; the banked W-content token never matches E.
    std::fs::write(&r.boot_path, BOOT_DOC.replace("#101418", "#123456"))
        .expect("external edit superseding the promote write before any poll");

    drive_polls(&mut poll, 2).await;
    assert_eq!(
        r.state.config_watch.snapshot().applied_count,
        1,
        "the racing external edit must be APPLIED, not adopted against the promote token"
    );
    assert_eq!(
        stored_color(&r.state).as_deref(),
        Some("#123456"),
        "the stores must follow the external edit"
    );
    let drained = r.commands.try_drain();
    assert!(
        drained.iter().any(|c| match c {
            Command::UpsertSource { source, .. } => {
                source.id == "in_a"
                    && serde_json::to_value(source.as_ref())
                        .ok()
                        .and_then(|v| v.get("color").and_then(|c| c.as_str().map(str::to_owned)))
                        .as_deref()
                        == Some("#123456")
            }
            _ => false,
        }),
        "the edit must ride UpsertSource with E's colour, got {drained:?}"
    );
    watch.stop();
}

/// Review B1 interleaving (3) — the failed-write token leak: a promote that
/// banks its expect token and then FAILS to write the boot file must release
/// the token; a later REAL external edit that happens to carry the same
/// content must be APPLIED, never silently adopted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_promote_write_releases_the_banked_expect_token() {
    let mut r = rig(BOOT_DOC);
    let (options, mut poll) = WatchOptions::default()
        .with_poll_interval(TEST_POLL)
        .with_manual_poll();
    let watch = spawn_watch(
        r.boot_path.clone(),
        r.config.clone(),
        r.state.clone(),
        options,
    );
    recolor_in_a(&r, "#e0e0e0").await;
    let _ = r.commands.try_drain();

    // The exact content the promote will render: the export TOML (the same
    // `compose_running_config` + `to_toml` path the promote route uses).
    let resp = send(&r.router, get("/api/v1/config/export", OPERATOR_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let running_toml =
        String::from_utf8(support::body_bytes(resp).await).expect("export is UTF-8 TOML");

    // Force the atomic write to fail ROOT-PROOF: the secure writer
    // (`NamedTempFile`) uses an unpredictable temp name, so blocking a
    // deterministic temp name no longer works — instead replace the boot file
    // DESTINATION with a NON-EMPTY directory. The temp is created fine, but the
    // final `rename(2)` of a regular file over a non-empty directory fails
    // (`ENOTDIR`/`EEXIST`) regardless of uid.
    let before = std::fs::read_to_string(&r.boot_path).expect("read boot file");
    std::fs::remove_file(&r.boot_path).expect("remove the boot file");
    std::fs::create_dir(&r.boot_path).expect("replace the boot path with a directory");
    std::fs::write(r.boot_path.join("keep"), "x").expect("make the blocker dir non-empty");
    let resp = send(
        &r.router,
        post_idem("/api/v1/config/promote", OPERATOR_TOKEN, None),
    )
    .await;
    assert!(
        resp.status().is_server_error(),
        "the blocked write must fail the promote, got {}",
        resp.status()
    );
    // Restore the boot file to exactly its pre-promote content.
    std::fs::remove_file(r.boot_path.join("keep")).expect("clear the blocker dir");
    std::fs::remove_dir(&r.boot_path).expect("remove the blocker dir");
    std::fs::write(&r.boot_path, &before).expect("restore the boot file");

    // A REAL external edit now lands with exactly the content the failed
    // promote announced. The promote never wrote it, so it must be APPLIED
    // (UpsertSource for in_a's new colour) — a leaked token would eat it. No
    // poll has run yet; two ticks (candidate then apply) settle and apply it.
    std::fs::write(&r.boot_path, &running_toml).expect("external edit matching the failed write");
    drive_polls(&mut poll, 2).await;
    assert_eq!(
        r.state.config_watch.snapshot().applied_count,
        1,
        "the external edit must be APPLIED — the failed promote's token must not eat it"
    );
    let drained = r.commands.try_drain();
    assert!(
        drained.iter().any(|c| matches!(
            c,
            Command::UpsertSource { source, .. } if source.id == "in_a"
        )),
        "the matching-content edit must still ride UpsertSource, got {drained:?}"
    );
    watch.stop();
}

/// Review B1 interleaving (2) — the superseded-token leak: a promote whose
/// write LANDED is superseded by an external edit E before it ever settles
/// (the watcher only observes E — interleaving (1) applies it). The banked
/// token is now stale: a much later REAL edit that restores exactly the
/// promoted bytes (a `git checkout`, an editor undo) must be APPLIED, never
/// eaten by the leftover token.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_superseded_promote_token_does_not_eat_a_later_real_edit() {
    let mut r = rig(BOOT_DOC);
    let (options, mut poll) = WatchOptions::default()
        .with_poll_interval(TEST_POLL)
        .with_manual_poll();
    let watch = spawn_watch(
        r.boot_path.clone(),
        r.config.clone(),
        r.state.clone(),
        options,
    );
    recolor_in_a(&r, "#c0c0c0").await;
    let _ = r.commands.try_drain();

    // The promote WRITES W successfully: token banked AND the write landed.
    let resp = send(
        &r.router,
        post_idem("/api/v1/config/promote", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let w_text = std::fs::read_to_string(&r.boot_path).expect("read the promoted W");

    // E supersedes W before the watcher polls; the watcher's first settled
    // observation is E and it applies (interleaving (1), pinned elsewhere) —
    // and, not matching the banked W token, draining the stale LANDED token.
    std::fs::write(&r.boot_path, BOOT_DOC.replace("#101418", "#123456"))
        .expect("external edit superseding the promote");
    drive_polls(&mut poll, 2).await;
    assert_eq!(
        r.state.config_watch.snapshot().applied_count,
        1,
        "the superseding edit must apply"
    );
    let _ = r.commands.try_drain();

    // A later REAL edit restores exactly the promoted bytes. The stale token
    // (whose write was superseded without ever settling) must not eat it —
    // two more ticks settle and APPLY the restore.
    std::fs::write(&r.boot_path, &w_text).expect("real edit restoring the promoted bytes");
    drive_polls(&mut poll, 2).await;
    assert_eq!(
        r.state.config_watch.snapshot().applied_count,
        2,
        "the byte-identical REAL edit must be APPLIED, not eaten by the stale token"
    );
    let drained = r.commands.try_drain();
    assert!(
        drained.iter().any(|c| matches!(
            c,
            Command::UpsertSource { source, .. } if source.id == "in_a"
        )),
        "restoring the promoted bytes must ride UpsertSource, got {drained:?}"
    );
    assert_eq!(
        stored_color(&r.state).as_deref(),
        Some("#c0c0c0"),
        "the stores must follow the restored content"
    );
    watch.stop();
}

/// Review M4 follow-on — the retry after a shed revert must actually re-send
/// the shed engine commands and complete: a shed revert applies nothing
/// durable (the stores stay at the running state), so a retry once the bus
/// drains re-runs the whole revert — `UpsertSource` rides, the stores resync
/// to Loaded, the response claims the revert, and the
/// `config-file-apply-incomplete` warning clears.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retried_revert_re_sends_the_shed_engine_commands() {
    let mut r = rig_with(BOOT_DOC, 1);
    // The live edit's own UpsertSource fills the capacity-1 bus.
    recolor_in_a(&r, "#f0f0f0").await;

    let resp = send(
        &r.router,
        post_idem("/api/v1/config/revert-to-start", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    assert_eq!(body["reverted"], serde_json::json!(false));
    assert!(body["shed"].as_u64().is_some_and(|n| n >= 1));
    assert!(
        wait_until(SETTLE, || has_active_warning(
            &r.warnings,
            "config-file-apply-incomplete"
        ))
        .await,
        "the shed revert raises the incomplete warning"
    );

    // The engine catches up: draining the bus frees its capacity.
    let _ = r.commands.try_drain();

    // RETRY: it must re-send the shed engine command and complete.
    let resp = send(
        &r.router,
        post_idem("/api/v1/config/revert-to-start", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    assert_eq!(
        body["reverted"],
        serde_json::json!(true),
        "the retry must complete the revert, got {body:?}"
    );
    assert_eq!(body["shed"], serde_json::json!(0));
    let drained = r.commands.try_drain();
    assert!(
        drained.iter().any(|c| match c {
            Command::UpsertSource { source, .. } => {
                source.id == "in_a"
                    && serde_json::to_value(source.as_ref())
                        .ok()
                        .and_then(|v| v.get("color").and_then(|c| c.as_str().map(str::to_owned)))
                        .as_deref()
                        == Some("#101418")
            }
            _ => false,
        }),
        "the retried revert must re-send the shed UpsertSource with the Loaded colour, got {drained:?}"
    );
    assert_eq!(
        stored_color(&r.state).as_deref(),
        Some("#101418"),
        "the completed retry resyncs the stores to Loaded"
    );
    assert!(
        wait_until(SETTLE, || !has_active_warning(
            &r.warnings,
            "config-file-apply-incomplete"
        ))
        .await,
        "the completed retry clears the incomplete warning"
    );
}

/// Review M4 — revert 202 honesty under a full bus: when engine command(s)
/// are SHED the response must NOT claim a full revert (`reverted: false`,
/// `shed` count surfaced, partial summary) and the
/// `config-file-apply-incomplete` warning path fires so the operator sees it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shed_revert_reports_partial_and_raises_the_warning() {
    let r = rig_with(BOOT_DOC, 1);
    // The live edit's own UpsertSource fills the capacity-1 bus; the revert's
    // submission below is shed.
    recolor_in_a(&r, "#f0f0f0").await;

    let resp = send(
        &r.router,
        post_idem("/api/v1/config/revert-to-start", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    assert_eq!(
        body["reverted"],
        serde_json::json!(false),
        "a shed revert must NOT claim reverted: true, got {body:?}"
    );
    assert!(
        body["shed"].as_u64().is_some_and(|n| n >= 1),
        "the shed count must be surfaced, got {body:?}"
    );
    let summary = body["summary"].as_array().expect("summary array").clone();
    assert!(
        !summary.is_empty(),
        "the partial summary still names what was attempted"
    );
    // A shed revert applies nothing durable: the stores are rolled back to
    // the pre-revert Running state, so a retry's diff re-runs the whole
    // revert (the M4 follow-on contract).
    assert_eq!(stored_color(&r.state).as_deref(), Some("#f0f0f0"));
    // The operator is told the engine did not get every command.
    assert!(
        wait_until(SETTLE, || has_active_warning(
            &r.warnings,
            "config-file-apply-incomplete"
        ))
        .await,
        "a shed revert must raise the config-file-apply-incomplete warning"
    );
}

/// Review m3 — reservation release on failed compose: a revert/promote whose
/// Running composition FAILS (422) must release its `Idempotency-Key`
/// reservation, so a retry with the same key actually runs (never answers as
/// a replay of a request that did nothing).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_compose_releases_the_idempotency_reservation() {
    // A state with a boot model but NO seeded resources: there is no working
    // layout, so `compose_running_config` fails with 422.
    let dir = tempfile::tempdir().expect("temp dir");
    let boot_path = dir.path().join("multiview.toml");
    std::fs::write(&boot_path, BOOT_DOC).expect("write boot file");
    let config = MultiviewConfig::load_from_toml(BOOT_DOC).expect("parse");
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let (sender, _commands) = command_bus(64);
    let state = AppState::new(
        publisher,
        sender,
        Arc::new(InMemoryRepository::new()),
        Arc::new(support::seeded_keys()),
    )
    .with_boot_model(Arc::new(BootModel::new(
        boot_path,
        config.clone(),
        config,
        StartMode::Boot,
        false,
        None,
    )));
    let router = multiview_control::router(state);

    for (path, key) in [
        ("/api/v1/config/revert-to-start", "revert-key-1"),
        ("/api/v1/config/promote", "promote-key-1"),
    ] {
        for attempt in 1..=2_u8 {
            let resp = send(&router, post_idem(path, OPERATOR_TOKEN, Some(key))).await;
            assert_eq!(
                resp.status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{path} attempt {attempt} must fail compose with 422 — a replayed \
                 reservation would answer 2xx instead"
            );
        }
    }
}

/// ADR-W024 MAJOR-C2 (concurrency): two concurrent promotes must not lose a
/// suppression token. Each promote banks an expect token before writing; under
/// the shared promote/revert mutation serial they cannot interleave, so the
/// watcher never mistakes one promote's own write for an external edit (which
/// would re-apply the file). With the watcher running, `applied_count` must
/// stay 0 across two back-to-back promotes — neither write is applied.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_promotes_do_not_lose_a_suppression_token() {
    let mut r = rig(BOOT_DOC);
    let (options, mut poll) = WatchOptions::default()
        .with_poll_interval(TEST_POLL)
        .with_manual_poll();
    let watch = spawn_watch(
        r.boot_path.clone(),
        r.config.clone(),
        r.state.clone(),
        options,
    );

    // A live edit moves Running away from the boot file. Drain its own
    // UpsertSource off the bus now, so the post-promote drain below sees ONLY
    // commands the watcher would have wrongly enqueued (none, if suppression
    // holds).
    recolor_in_a(&r, "#c1c1c1").await;
    let _ = r.commands.try_drain();
    let router_a = r.router.clone();
    let router_b = r.router.clone();
    let (resp_a, resp_b) = tokio::join!(
        send(
            &router_a,
            post_idem("/api/v1/config/promote", OPERATOR_TOKEN, Some("promote-a")),
        ),
        send(
            &router_b,
            post_idem("/api/v1/config/promote", OPERATOR_TOKEN, Some("promote-b")),
        ),
    );
    assert_eq!(resp_a.status(), StatusCode::OK, "promote A succeeds");
    assert_eq!(resp_b.status(), StatusCode::OK, "promote B succeeds");

    // Several DETERMINISTIC poll iterations: the watcher must ADOPT both promote
    // writes (never apply either as an external edit). A lost token would let
    // the watcher re-apply the promoted file, bumping applied_count — the extra
    // ticks prove the count stays pinned at 0 with no wall-clock window to race.
    drive_polls(&mut poll, 4).await;
    assert_eq!(
        r.state.config_watch.snapshot().applied_count,
        0,
        "neither concurrent promote's own write may be applied as an external edit"
    );
    assert!(
        r.commands.try_drain().is_empty(),
        "a promote's own write must enqueue no watcher commands"
    );
    watch.stop();
}

/// ADR-W024 MAJOR-C3 (concurrency): a promote concurrent with a revert must not
/// produce a torn/stale commit. Under the shared mutation serial the two cannot
/// interleave, so the document promote WRITES to the boot file is exactly the
/// document it COMMITS as the `boot` revision, and the boot file is always a
/// valid config (never a half-applied mix). Run promote+revert concurrently and
/// assert the boot file parses+validates and equals the committed revision.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn promote_concurrent_with_revert_commits_a_consistent_snapshot() {
    let mut r = rig(BOOT_DOC);
    // Running diverges from Loaded so revert has work to do.
    recolor_in_a(&r, "#d2d2d2").await;

    let router_p = r.router.clone();
    let router_v = r.router.clone();
    let (promote_resp, revert_resp) = tokio::join!(
        send(
            &router_p,
            post_idem("/api/v1/config/promote", OPERATOR_TOKEN, Some("promote-x")),
        ),
        send(
            &router_v,
            post_idem(
                "/api/v1/config/revert-to-start",
                OPERATOR_TOKEN,
                Some("revert-x")
            ),
        ),
    );
    assert_eq!(promote_resp.status(), StatusCode::OK, "promote succeeds");
    assert_eq!(
        revert_resp.status(),
        StatusCode::ACCEPTED,
        "revert is accepted"
    );
    let promote_body = body_json(promote_resp).await;
    let committed_rev = promote_body["revision"]
        .as_u64()
        .expect("promote returns the committed boot revision");

    // The boot file promote wrote must be a VALID config (never a torn mix).
    let boot_text = std::fs::read_to_string(&r.boot_path).expect("read boot file");
    let boot_config =
        MultiviewConfig::load_from_toml(&boot_text).expect("the promoted boot file parses");
    boot_config
        .validate()
        .expect("the promoted boot file validates");

    // The committed `boot` revision must equal the document promote wrote to the
    // boot file (compose → write → commit was one atomic critical section; a
    // concurrent revert could not split it).
    let revision = r
        .state
        .config_versions
        .get(
            "boot",
            multiview_control::versioning::RevisionId::new(committed_rev),
        )
        .expect("the committed boot revision is retrievable");
    let committed_config: MultiviewConfig =
        serde_json::from_value(revision.document.clone()).expect("the committed doc is a config");
    let committed_toml = committed_config.to_toml().expect("render committed");
    let boot_canonical = boot_config.to_toml().expect("render boot file");
    assert_eq!(
        committed_toml, boot_canonical,
        "the committed boot revision must equal the document written to the boot file \
         (no stale/torn commit from a concurrent revert)"
    );
    let _ = r.commands.try_drain();
}

/// MAJOR-B round 4 — codex sequence 1 (over-adoption): a shed `DELETE
/// /sources/in_a` (bus full → the engine still runs `in_a`) followed by an
/// UNRELATED landed mutation must NOT drop `in_a` from `active.toml`. The
/// round-3 global counter let the unrelated landed mutation "adopt" the prior
/// shed's generation, persisting a config WITHOUT `in_a` though the engine ran
/// it. The round-4 adopted snapshot applies only the specific landed delta, so
/// `in_a` stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shed_delete_then_unrelated_landed_mutation_keeps_the_running_source() {
    let mut r = rig_with(BOOT_DOC, 1);
    let model = r.state.boot_model.clone().expect("rig wires a boot model");
    let active_path = model.active_path();
    let persist = spawn_running_persist(r.state.clone(), Duration::from_millis(40));

    // Occupy the capacity-1 bus so the DELETE's RemoveSource sheds.
    assert!(
        r.state
            .commands
            .try_submit(Command::UpsertSource {
                op: multiview_control::OperationId::new(),
                source: Box::new(r.config.sources[0].clone()),
            })
            .is_ok(),
        "fill the bus"
    );

    // DELETE /sources/in_a — the store drops in_a, but the RemoveSource SHEDS,
    // so the engine still runs in_a (ApplyMode::Restart). Read the ETag first.
    // DELETE requires the administer role (ADMIN_TOKEN).
    let resp = send(&r.router, get("/api/v1/sources/in_a", ADMIN_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let etag = support::etag(&resp).expect("source carries an ETag");
    let req = Request::builder()
        .method("DELETE")
        .uri("/api/v1/sources/in_a")
        .header(header::AUTHORIZATION, format!("Bearer {ADMIN_TOKEN}"))
        .header(header::IF_MATCH, etag)
        .body(Body::empty())
        .expect("delete request");
    let resp = send(&r.router, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::NO_CONTENT,
        "the delete succeeds (store)"
    );

    // Drain the bus so the next, UNRELATED mutation lands live.
    let _ = r.commands.try_drain();

    // An unrelated NEW synthetic source in_c — a live-appliable kind that LANDS
    // (POST creates a new resource).
    let resp = send(
        &r.router,
        support::post_json(
            "/api/v1/sources/in_c",
            OPERATOR_TOKEN,
            &serde_json::json!({
                "name": "in_c",
                "body": { "id": "in_c", "kind": "solid", "color": "#222222" }
            }),
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "the unrelated add lands"
    );

    // Several persist windows: active.toml must STILL carry in_a (the engine
    // runs it; the shed delete was never adopted) and gain in_c.
    assert!(
        wait_until(SETTLE, || std::fs::read_to_string(&active_path)
            .ok()
            .is_some_and(|t| t.contains("in_c")))
        .await,
        "active.toml must gain the landed in_c"
    );
    let text = std::fs::read_to_string(&active_path).expect("read active");
    assert!(
        text.contains("in_a"),
        "active.toml must KEEP in_a — the engine still runs it; an unrelated landed \
         mutation must never adopt the prior shed delete (round-3 over-adoption)"
    );
    persist.abort();
}

/// MAJOR-B round 4 — codex sequence 2 (mid-mutation race): the persister shares
/// the config-mutation lock with every live mutation, so it can never compose
/// `active.toml` mid-mutation. Driven DETERMINISTICALLY: the test holds the lock
/// (standing in for an in-flight unadopted mutation) and drives a persist; the
/// persist must BLOCK until the lock is released, then read settled state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_persister_blocks_on_the_mutation_lock() {
    let r = rig(BOOT_DOC);
    let model = r.state.boot_model.clone().expect("rig wires a boot model");
    let active_path = model.active_path();
    assert!(
        !active_path.exists(),
        "no active.toml before the first persist"
    );

    // Hold the mutation lock — as an in-flight mutate→submit→adopt would.
    let guard = r.state.lock_config_mutation().await;

    // Drive a persist concurrently; it must block on the lock (the same lock the
    // in-flight mutation holds), so nothing is written while we hold it.
    let persist_state = r.state.clone();
    let persisting = tokio::spawn(async move {
        persist_running_now(&persist_state).await.expect("persist");
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        !persisting.is_finished(),
        "the persist must BLOCK on the mutation lock while an in-flight mutation holds it"
    );
    assert!(
        !active_path.exists(),
        "no active.toml may be written while the mutation lock is held (no mid-mutation compose)"
    );

    // Release the lock: the persist now proceeds and writes adopted state.
    drop(guard);
    tokio::time::timeout(Duration::from_secs(5), persisting)
        .await
        .expect("the persist completes once the lock frees")
        .expect("persist task");
    assert!(
        active_path.exists(),
        "active.toml is written once the in-flight mutation releases the lock"
    );
}

/// MAJOR-B round 5 — codex sequence 3 (layout leak): `Command::ApplyLayout` is a
/// LIVE sheddable command. A landed `PUT /layouts/working` (the working-layout
/// store mutates) followed by a SHED `POST /commands/apply-layout` must NOT
/// persist the requested-but-unadopted canvas/layout/cells into `active.toml` —
/// it keeps the previously ADOPTED layout (same B-2 class as sources, for the
/// layout sections). The round-4 snapshot did not back layout/cells.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shed_apply_layout_does_not_persist_unadopted_layout() {
    let mut r = rig_with(BOOT_DOC, 1);
    let model = r.state.boot_model.clone().expect("rig wires a boot model");
    let active_path = model.active_path();
    let persist = spawn_running_persist(r.state.clone(), Duration::from_millis(40));
    let working = r
        .state
        .working_layout_id
        .clone()
        .expect("the rig seeds a working layout id");

    // Let the initial adopted layout persist: cell_a→in_a, cell_b→in_b.
    assert!(
        wait_until(SETTLE, || std::fs::read_to_string(&active_path)
            .ok()
            .is_some_and(|t| t.contains("cell_a")))
        .await,
        "the initial adopted layout must persist"
    );

    // Occupy the capacity-1 bus so the apply-layout SHEDS.
    assert!(
        r.state
            .commands
            .try_submit(Command::UpsertSource {
                op: multiview_control::OperationId::new(),
                source: Box::new(r.config.sources[0].clone()),
            })
            .is_ok(),
        "fill the bus"
    );

    // PUT a CHANGED working layout — same pinned 64x64 canvas + grid, but the
    // cells SWAP their sources (cell_a→in_b, cell_b→in_a). The store mutates.
    let changed = serde_json::json!({
        "canvas": { "width": 64, "height": 64, "fps": "25/1", "pixel_format": "nv12",
                    "background": "#101014", "color": { "profile": "sdr-bt709-limited" } },
        "layout": { "kind": "grid", "columns": ["1fr", "1fr"], "rows": ["1fr"], "areas": ["a b"] },
        "cells": [
            { "id": "cell_a", "area": "a", "source": { "input_id": "in_b" } },
            { "id": "cell_b", "area": "b", "source": { "input_id": "in_a" } }
        ]
    });
    let resp = send(
        &r.router,
        get(&format!("/api/v1/layouts/{working}"), OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "the working layout is readable"
    );
    let etag = support::etag(&resp).expect("layout carries an ETag");
    let resp = send(
        &r.router,
        put_json(
            &format!("/api/v1/layouts/{working}"),
            OPERATOR_TOKEN,
            Some(&etag),
            &serde_json::json!({ "name": working, "body": changed }),
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "the layout PUT lands (store)"
    );

    // POST apply-layout — the ApplyLayout SHEDS on the full bus → EngineBusy.
    let resp = send(
        &r.router,
        support::post_json(
            "/api/v1/commands/apply-layout",
            OPERATOR_TOKEN,
            &serde_json::json!({ "layout": working }),
        ),
    )
    .await;
    assert!(
        resp.status().is_server_error() || resp.status() == StatusCode::SERVICE_UNAVAILABLE,
        "the shed apply-layout must fail (engine busy), got {}",
        resp.status()
    );

    // Several persist windows: active.toml must keep the ADOPTED layout
    // (cell_a→in_a) and NEVER the unadopted swap (cell_a→in_b).
    tokio::time::sleep(Duration::from_millis(300)).await;
    let text = std::fs::read_to_string(&active_path).expect("read active");
    let config = MultiviewConfig::load_from_toml(&text).expect("active parses");
    let cell_a = config
        .cells
        .iter()
        .find(|c| c.id == "cell_a")
        .expect("cell_a present");
    let cell_a_src = serde_json::to_value(cell_a).ok().and_then(|v| {
        v.get("source")
            .and_then(|s| s.get("input_id"))
            .and_then(|i| i.as_str().map(str::to_owned))
    });
    assert_eq!(
        cell_a_src.as_deref(),
        Some("in_a"),
        "active.toml must keep the ADOPTED layout (cell_a→in_a); the shed apply-layout's \
         swap (cell_a→in_b) must NOT be persisted as adopted"
    );
    let _ = r.commands.try_drain();
    persist.abort();
}

/// MAJOR-B round 5 — codex sequence 4 (watcher over-adopt): a file-watch apply
/// that adds a RESTART-ONLY change (here a NETWORK source, which is restart-only
/// on a synthetic-only run) must NOT enter `active.toml` — the engine never
/// adopted it LIVE. The round-4 watcher copied the whole requested document into
/// the snapshot wholesale, leaking restart-only file edits; round 5 adopts only
/// the per-section deltas the engine applied live.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_only_file_watch_change_is_not_persisted_as_adopted() {
    let r = rig(BOOT_DOC);
    let model = r.state.boot_model.clone().expect("rig wires a boot model");
    let active_path = model.active_path();
    let (options, mut poll) = WatchOptions::default()
        .with_poll_interval(TEST_POLL)
        .with_manual_poll();
    let watch = spawn_watch(
        r.boot_path.clone(),
        r.config.clone(),
        r.state.clone(),
        options,
    );
    let persist = spawn_running_persist(r.state.clone(), Duration::from_millis(40));

    // The initial adopted state persists (the persister's startup write).
    assert!(
        wait_until(SETTLE, || std::fs::read_to_string(&active_path)
            .ok()
            .is_some_and(|t| t.contains("in_a")))
        .await,
        "the initial adopted state persists"
    );

    // Edit the BOOT file to ADD a NETWORK (rtsp) source — restart-only on this
    // synthetic-only run: the watcher reseeds the store + warns restart, but the
    // engine never ingests it live.
    // Insert the new source ONCE, after the existing `in_b` (bars) source —
    // `str::replace` would otherwise hit BOTH `[[cells]]` anchors and define
    // net1 twice (an invalid duplicate id the watcher would reject).
    let edited = BOOT_DOC.replace(
        "id = \"in_b\"\nkind = \"bars\"\n",
        "id = \"in_b\"\nkind = \"bars\"\n[[sources]]\nid = \"net1\"\nkind = \"rtsp\"\nurl = \"rtsp://[::1]:8554/x\"\n",
    );
    assert!(edited.contains("net1"), "the edit must insert net1 once");
    std::fs::write(&r.boot_path, &edited).expect("edit boot file: add a network source");

    // Two deterministic ticks settle + APPLY the edit. The store reseed
    // (`resync_store`) is a SYNCHRONOUS store write inside the apply, so the
    // restart-only network source is present in the store the instant the apply
    // tick completes — asserted directly (no wall-clock poll race).
    drive_polls(&mut poll, 2).await;
    assert!(
        r.state.sources.get("net1").is_ok(),
        "the watcher must reseed the restart-only source into the store"
    );

    // Several persist windows: active.toml must NOT carry the unadopted network
    // source (the engine never ingests it live on a synthetic-only run, so it is
    // not in the adopted snapshot). The persister runs on its own timer.
    tokio::time::sleep(TEST_POLL * 8).await;
    let text = std::fs::read_to_string(&active_path).expect("active.toml exists");
    assert!(
        !text.contains("net1"),
        "active.toml must not persist the restart-only/unadopted file-watch source as adopted, got:\n{text}"
    );
    watch.stop();
    persist.abort();
}

// ===========================================================================
// ADR-W024 round 6 — under-adoption fixes. The round-5 re-panel found three
// runtime-mutable sections whose adopted state `active.toml` failed to capture:
//   F1. salvos + tally_profiles are runtime-mutable through their definition
//       PUT/DELETE routes (pure CONTROL-PLANE store edits — no engine command,
//       so the store IS the adopted state), yet `compose_document_unredacted`
//       took them VERBATIM from the boot base document, so a runtime salvo /
//       tally-profile edit was LOST on resume.
//   F2. the file-watch `overlays` branch was restart-only (resync + restart
//       accounting) while the REST overlay routes submit `UpsertOverlay`/
//       `RemoveOverlay` + adopt per-section — so a live overlay edit arriving
//       via the watched boot file never entered `active.toml`.
// (F3 strengthens the previously-tautological shed-REST-source test in place.)
// ===========================================================================

/// A boot document that declares a salvo recall AND a tally profile (both
/// reference the `BOOT_DOC` cells), so the runtime-mutable salvo/tally stores
/// are seeded at start and a later REST edit can be checked for round-trip into
/// `active.toml`.
const BOOT_DOC_WITH_RECALLS: &str = r##"schema_version = 1
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
[control]
listen = "[::1]:0"
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
path = "/tmp/boot-config-model.m3u8"
codec = "mpeg2video"
segment_ms = 1000
[[salvos]]
id = "recall_a"
display_name = "Recall A"
[[salvos.tally]]
cell = "cell_a"
color = "Red"
[[tally_profiles]]
id = "tp1"
[[tally_profiles.bit_colors]]
bit = 0
color = "Red"
[[tally_profiles.index_cells]]
index = 0
cell = "cell_a"
"##;

/// ADR-W024 round 6 — F1 (salvos): a salvo definition is runtime-mutable
/// through `PUT /api/v1/salvos/{id}` (a pure control-plane store edit — no
/// engine command, so the store IS the adopted state). A runtime edit MUST
/// round-trip into `active.toml` so a `resume` does not lose it. The round-5
/// composition took salvos VERBATIM from the boot base document, dropping the
/// edit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_runtime_salvo_edit_persists_to_active_toml() {
    let r = rig(BOOT_DOC_WITH_RECALLS);
    let model = r.state.boot_model.clone().expect("rig wires a boot model");
    let active_path = model.active_path();
    let persist = spawn_running_persist(r.state.clone(), Duration::from_millis(40));

    // The seeded salvo persists at startup (it must be composed from the store,
    // which is seeded from the boot document).
    assert!(
        wait_until(SETTLE, || std::fs::read_to_string(&active_path)
            .ok()
            .is_some_and(|t| t.contains("recall_a")))
        .await,
        "the boot-seeded salvo must be composed into active.toml"
    );

    // Edit the salvo's display name through the REAL route (GET → ETag → PUT).
    let resp = send(&r.router, get("/api/v1/salvos/recall_a", OPERATOR_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK, "salvo is readable");
    let etag = support::etag(&resp).expect("salvo carries an ETag");
    let resp = send(
        &r.router,
        put_json(
            "/api/v1/salvos/recall_a",
            OPERATOR_TOKEN,
            Some(&etag),
            &serde_json::json!({
                "id": "recall_a",
                "display_name": "Recall A v2",
                "tally": [{ "cell": "cell_a", "color": "Green" }],
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "the salvo edit must land");

    // The runtime edit must round-trip into active.toml (a pure store edit is
    // adopted by construction — no engine command to shed).
    assert!(
        wait_until(SETTLE, || std::fs::read_to_string(&active_path)
            .ok()
            .is_some_and(|t| t.contains("Recall A v2")))
        .await,
        "the runtime salvo edit must persist into active.toml (it is lost on \
         resume otherwise)"
    );
    persist.abort();
}

/// ADR-W024 round 6 — F1 (`tally_profiles`): a tally profile is runtime-mutable
/// through `PUT /api/v1/tally/profiles/{id}` (a pure control-plane store edit).
/// A runtime edit MUST round-trip into `active.toml`. The round-5 composition
/// took `tally_profiles` VERBATIM from the boot base document.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_runtime_tally_profile_edit_persists_to_active_toml() {
    let r = rig(BOOT_DOC_WITH_RECALLS);
    let model = r.state.boot_model.clone().expect("rig wires a boot model");
    let active_path = model.active_path();
    let persist = spawn_running_persist(r.state.clone(), Duration::from_millis(40));

    // The seeded tally profile persists at startup (composed from the store).
    assert!(
        wait_until(SETTLE, || std::fs::read_to_string(&active_path)
            .ok()
            .is_some_and(|t| t.contains("tp1")))
        .await,
        "the boot-seeded tally profile must be composed into active.toml"
    );

    // Add a second index→cell binding through the REAL route. The new index `1`
    // is a value the seeded profile did not carry, so it is a recognisable edit.
    let resp = send(&r.router, get("/api/v1/tally/profiles/tp1", OPERATOR_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK, "tally profile is readable");
    let etag = support::etag(&resp).expect("tally profile carries an ETag");
    let resp = send(
        &r.router,
        put_json(
            "/api/v1/tally/profiles/tp1",
            OPERATOR_TOKEN,
            Some(&etag),
            &serde_json::json!({
                "id": "tp1",
                "bit_colors": [{ "bit": 0, "color": "Red" }, { "bit": 1, "color": "Green" }],
                "index_cells": [{ "index": 0, "cell": "cell_a" }, { "index": 1, "cell": "cell_b" }],
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "the tally edit must land");

    // The runtime edit must round-trip into active.toml. Assert the new index→
    // cell_b binding is present (the round-5 verbatim-from-base composition kept
    // only the boot profile, missing cell_b).
    assert!(
        wait_until(SETTLE, || {
            let Ok(text) = std::fs::read_to_string(&active_path) else {
                return false;
            };
            let Ok(config) = MultiviewConfig::load_from_toml(&text) else {
                return false;
            };
            config
                .tally_profiles
                .iter()
                .find(|p| p.id == "tp1")
                .is_some_and(|p| p.index_cells.iter().any(|c| c.cell == "cell_b"))
        })
        .await,
        "the runtime tally-profile edit must persist into active.toml"
    );
    persist.abort();
}

/// [`rig`] but with a binary-injected overlay live-apply capability (every
/// overlay renders), so the file-watch overlay branch CAN apply live — the
/// posture a full-pipeline run has. The default rig carries no capability
/// (honest restart-only), which is correct for a store-only run but cannot
/// exercise the watcher's live overlay path.
fn rig_with_overlay_live(doc: &str) -> Rig {
    rig_with_overlay_live_capacity(doc, 64)
}

/// [`rig_with_overlay_live`] with a chosen command-bus `capacity` — the
/// overlay-shed-retry test runs at 1 so the first `UpsertOverlay`/`RemoveOverlay`
/// sheds while the bus is occupied.
fn rig_with_overlay_live_capacity(doc: &str, capacity: usize) -> Rig {
    let dir = tempfile::tempdir().expect("temp dir");
    let boot_path = dir.path().join("multiview.toml");
    std::fs::write(&boot_path, doc).expect("write boot config");
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
        publisher,
        sender,
        Arc::new(InMemoryRepository::new()),
        Arc::new(support::seeded_keys()),
    )
    .with_seeded_resources(seeded)
    .with_base_document(serde_json::to_value(&config).expect("boot doc to JSON"))
    .with_live_apply(
        multiview_control::LiveApplyCaps::default()
            .with_overlays(multiview_control::OverlayLiveCapability::new(|_| true)),
    )
    .with_boot_model(Arc::new(BootModel::new(
        boot_path.clone(),
        config.clone(),
        config.clone(),
        StartMode::Boot,
        false,
        None,
    )));
    let router = multiview_control::router(state.clone());
    Rig {
        _dir: dir,
        boot_path,
        config,
        state,
        router,
        commands,
        warnings,
    }
}

/// ADR-W024 round 6 — F2 (watcher overlay parity): on an overlay-capable run a
/// file-watch edit that ADDS an overlay must be applied LIVE and adopted, so
/// `active.toml` carries it — exactly as the REST overlay route adopts a landed
/// `UpsertOverlay`. The round-5 watcher overlay branch was restart-only
/// (resync + restart accounting only), so a live overlay file-edit never
/// entered `active.toml`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_watch_overlay_add_is_adopted_on_an_overlay_capable_run() {
    let r = rig_with_overlay_live(BOOT_DOC);
    let model = r.state.boot_model.clone().expect("rig wires a boot model");
    let active_path = model.active_path();
    let (options, mut poll) = WatchOptions::default()
        .with_poll_interval(TEST_POLL)
        .with_manual_poll();
    let watch = spawn_watch(
        r.boot_path.clone(),
        r.config.clone(),
        r.state.clone(),
        options,
    );
    let persist = spawn_running_persist(r.state.clone(), Duration::from_millis(40));

    // The initial adopted state persists (the persister's startup write).
    assert!(
        wait_until(SETTLE, || std::fs::read_to_string(&active_path)
            .ok()
            .is_some_and(|t| t.contains("in_a")))
        .await,
        "the initial adopted state persists"
    );

    // Edit the BOOT file to ADD a clock overlay (a rendering kind on a capable
    // run). The watcher must submit UpsertOverlay + adopt it (like the REST
    // route), so it reaches active.toml.
    let edited = format!(
        "{BOOT_DOC}[[overlays]]\nid = \"ov_clock\"\nkind = \"clock\"\ntarget = \"canvas\"\n"
    );
    assert!(edited.contains("ov_clock"), "the edit must add the overlay");
    std::fs::write(&r.boot_path, &edited).expect("edit boot file: add an overlay");

    // Two deterministic ticks settle + APPLY the edit. The overlay store reseed
    // is a SYNCHRONOUS store write inside the apply, so the overlay is present
    // the instant the apply tick completes — asserted directly.
    drive_polls(&mut poll, 2).await;
    assert!(
        r.state.overlays.get("ov_clock").is_ok(),
        "the watcher must reseed the overlay into the store"
    );

    // The live overlay edit must be ADOPTED and persisted to active.toml — the
    // engine drew it (UpsertOverlay landed, firing `running_changed`), so the
    // persister writes it. The persist itself runs on its own debounce timer.
    assert!(
        wait_until(SETTLE, || std::fs::read_to_string(&active_path)
            .ok()
            .is_some_and(|t| t.contains("ov_clock")))
        .await,
        "a live file-watch overlay add must be adopted into active.toml (the \
         watcher must mirror the REST overlay adopt path)"
    );
    watch.stop();
    persist.abort();
}

/// ADR-W024 round 7 — F2 retry lost-update (concurrency BLOCKER). A file-watch
/// overlay ADD whose `UpsertOverlay` SHEDS on the first attempt (bus full) MUST,
/// on retry, STILL submit the command and end ADOPTED in `active.toml`.
///
/// The round-6 `apply_overlay_changes` computed the delta from the MUTATED
/// overlay store and reseeded the store to `next` BEFORE the retry resolved. On
/// a shed the file baseline does not advance, but the next retry saw the store
/// already == `next`, submitted NO `UpsertOverlay`, falsely reported
/// `all_landed`, cleared restart/incomplete, advanced the baseline — so the shed
/// overlay edit was silently dropped (never delivered, never adopted). The fix
/// derives the retry delta from the FILE BASELINE (the `ConfigDiff`, stable
/// across retries), exactly like the source path, so the retry re-submits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shed_file_watch_overlay_add_is_redelivered_and_adopted_on_retry() {
    // Capacity-1 bus so the overlay UpsertOverlay sheds while the bus is full.
    let mut r = rig_with_overlay_live_capacity(BOOT_DOC, 1);
    let model = r.state.boot_model.clone().expect("rig wires a boot model");
    let active_path = model.active_path();
    let (options, mut poll) = WatchOptions::default()
        .with_poll_interval(TEST_POLL)
        .with_manual_poll();
    let watch = spawn_watch(
        r.boot_path.clone(),
        r.config.clone(),
        r.state.clone(),
        options,
    );
    let persist = spawn_running_persist(r.state.clone(), Duration::from_millis(40));

    // Initial adopted state persists (no overlay yet — the persister's startup
    // write).
    assert!(
        wait_until(SETTLE, || std::fs::read_to_string(&active_path)
            .ok()
            .is_some_and(|t| t.contains("in_a")))
        .await,
        "the initial adopted state persists"
    );

    // Occupy the capacity-1 bus so the watcher's first UpsertOverlay sheds.
    assert!(
        r.state
            .commands
            .try_submit(Command::UpsertSource {
                op: multiview_control::OperationId::new(),
                source: Box::new(r.config.sources[0].clone()),
            })
            .is_ok(),
        "fill the bus"
    );

    // Edit the BOOT file to ADD a clock overlay. The watcher applies it, but the
    // UpsertOverlay SHEDS on the full bus → Retry (baseline NOT advanced). Two
    // deterministic ticks drive the (shed) first apply.
    let edited = format!(
        "{BOOT_DOC}[[overlays]]\nid = \"ov_clock\"\nkind = \"clock\"\ntarget = \"canvas\"\n"
    );
    std::fs::write(&r.boot_path, &edited).expect("edit boot file: add an overlay");
    drive_polls(&mut poll, 2).await;

    // The watcher reseeds the overlay store even on a shed (the UI mirror follows
    // the file) — the reseed is a SYNCHRONOUS store write in the apply, so it is
    // present the instant the (shed) apply tick completes (proof the attempt ran).
    assert!(
        r.state.overlays.get("ov_clock").is_ok(),
        "the watcher reseeds the overlay store on the (shed) first attempt"
    );

    // While the UpsertOverlay is shed-pending, active.toml must NOT yet carry the
    // overlay — the engine has not adopted it (a shed is not a false success).
    // The persister runs on its own timer; give it windows to (wrongly) act.
    tokio::time::sleep(TEST_POLL * 3).await;
    if let Ok(text) = std::fs::read_to_string(&active_path) {
        assert!(
            !text.contains("ov_clock"),
            "active.toml must NOT carry the overlay while its UpsertOverlay is shed-pending"
        );
    }

    // Drain the bus, then drive the RETRY (two more ticks): it must RE-DERIVE the
    // delta from the file baseline (stable), re-submit UpsertOverlay (now it
    // lands), adopt it, and fire `running_changed` so the persister writes it.
    // The round-6 bug saw the store already == next and dropped it.
    let _ = r.commands.try_drain();
    drive_polls(&mut poll, 2).await;
    assert!(
        wait_until(SETTLE, || std::fs::read_to_string(&active_path)
            .ok()
            .is_some_and(|t| t.contains("ov_clock")))
        .await,
        "after the bus drains, the retry MUST re-deliver the overlay and adopt it \
         into active.toml — a shed must never silently become a false success"
    );

    watch.stop();
    persist.abort();
}

/// ADR-W024 round 7 — F2 retry lost-update, REMOVAL direction. A file-watch
/// overlay REMOVE whose `RemoveOverlay` SHEDS on the first attempt MUST, on
/// retry, still submit the removal and end unadopted (dropped from
/// `active.toml`). Same root cause as the add case: a removal delta derived from
/// the already-reseeded store vanishes on retry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shed_file_watch_overlay_remove_is_redelivered_and_adopted_on_retry() {
    // Boot WITH an overlay so a file edit can REMOVE it.
    let boot_with_overlay = format!(
        "{BOOT_DOC}[[overlays]]\nid = \"ov_clock\"\nkind = \"clock\"\ntarget = \"canvas\"\n"
    );
    let mut r = rig_with_overlay_live_capacity(&boot_with_overlay, 1);
    let model = r.state.boot_model.clone().expect("rig wires a boot model");
    let active_path = model.active_path();
    let (options, mut poll) = WatchOptions::default()
        .with_poll_interval(TEST_POLL)
        .with_manual_poll();
    let watch = spawn_watch(
        r.boot_path.clone(),
        r.config.clone(),
        r.state.clone(),
        options,
    );
    let persist = spawn_running_persist(r.state.clone(), Duration::from_millis(40));

    // The boot overlay is adopted at start (seeded into the store, composed by
    // the persister's startup write).
    assert!(
        wait_until(SETTLE, || std::fs::read_to_string(&active_path)
            .ok()
            .is_some_and(|t| t.contains("ov_clock")))
        .await,
        "the boot overlay is adopted at start"
    );

    // Occupy the capacity-1 bus so the watcher's first RemoveOverlay sheds.
    assert!(
        r.state
            .commands
            .try_submit(Command::UpsertSource {
                op: multiview_control::OperationId::new(),
                source: Box::new(r.config.sources[0].clone()),
            })
            .is_ok(),
        "fill the bus"
    );

    // Edit the BOOT file to REMOVE the overlay (drop the [[overlays]] block).
    // Two deterministic ticks drive the (shed) first apply → RemoveOverlay sheds.
    std::fs::write(&r.boot_path, BOOT_DOC).expect("edit boot file: remove the overlay");
    drive_polls(&mut poll, 2).await;

    // The watcher reseeds the store (removes ov_clock) even on a shed — a
    // SYNCHRONOUS store write in the apply, present the instant the tick completes.
    assert!(
        r.state.overlays.get("ov_clock").is_err(),
        "the watcher reseeds the overlay store (removes it) on the (shed) first attempt"
    );

    // While the RemoveOverlay is shed-pending, active.toml must STILL carry the
    // overlay — the engine still renders it (the removal was not adopted). The
    // persister runs on its own timer; give it windows to (wrongly) act.
    tokio::time::sleep(TEST_POLL * 3).await;
    if let Ok(text) = std::fs::read_to_string(&active_path) {
        assert!(
            text.contains("ov_clock"),
            "active.toml must KEEP the overlay while its RemoveOverlay is shed-pending"
        );
    }

    // Drain the bus, then drive the RETRY (two more ticks): it must re-derive the
    // removal from the file baseline, re-submit RemoveOverlay (now it lands),
    // unadopt it, and fire `running_changed` so the persister drops it from
    // active.toml.
    let _ = r.commands.try_drain();
    drive_polls(&mut poll, 2).await;
    assert!(
        wait_until(SETTLE, || std::fs::read_to_string(&active_path)
            .ok()
            .is_some_and(|t| !t.contains("ov_clock")))
        .await,
        "after the bus drains, the retry MUST re-deliver the removal and drop the \
         overlay from active.toml — a shed removal must never silently become a false success"
    );

    watch.stop();
    persist.abort();
}

// ===========================================================================
// Deterministic watcher harness (task #131).
//
// The watcher's poll loop is driven by a MANUAL TICK
// (`WatchOptions::with_manual_poll`) instead of the wall clock. Every
// `ManualPoll::poll_once().await` runs EXACTLY one poll iteration and resolves
// only once that iteration has fully completed and the loop is parked again, so
// these tests assert the SAME retry/settle behaviour as their `wait_until(SETTLE)`
// counterparts WITHOUT any `tokio::time::sleep` — no poll/debounce wall-clock
// race, deterministic under arbitrary host contention. The watcher's debounce
// (a new fingerprint must be seen on TWO consecutive polls before it acts) is
// driven by firing two ticks, not by sleeping two poll intervals.
// ===========================================================================

/// Fire `n` deterministic watcher poll iterations, awaiting each to completion.
/// Panics if the loop has exited early (a `poll_once` that returns `false`
/// before `n` is reached is a real defect, not a flake).
async fn drive_polls(poll: &mut ManualPoll, n: usize) {
    for i in 0..n {
        assert!(
            poll.poll_once().await,
            "the watcher loop exited before poll {} of {n} — it must stay alive across the drive",
            i + 1
        );
    }
}

/// Settle the watcher's initial pass deterministically: the first settled poll
/// re-reads the file and diffs it against the boot baseline (review M2 in
/// `config_watch.rs`). Two ticks cross the two-poll debounce; for an UNCHANGED
/// boot file the diff is empty and is adopted with no commands and no apply
/// count. Returns once the loop is parked again. Asserts the unchanged settle
/// applied nothing (the deterministic equivalent of the `applied_count == 0`
/// boot-window invariant).
async fn settle_initial(poll: &mut ManualPoll, state: &AppState) {
    drive_polls(poll, 2).await;
    assert_eq!(
        state.config_watch.snapshot().applied_count,
        0,
        "the unchanged boot file must settle to an empty diff (no apply)"
    );
}

/// Force `path`'s modification time to `mtime`, leaving its inode and length
/// untouched (opens the existing file without truncating, then `set_modified`).
/// Lets a test REMOVE the filesystem's mtime granularity as a variable and pin
/// exactly which fingerprint axis distinguishes an edit from the already-applied
/// content — `config_watch.rs` fingerprints len+mtime+inode PLUS a hash of the
/// file's bytes, so with mtime (and inode) pinned only the bytes differ.
fn force_mtime(path: &Path, mtime: std::time::SystemTime) {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open the boot file to set its mtime");
    file.set_modified(mtime).expect("set the boot file mtime");
}

/// Simulate a real external boot-file edit: an ATOMIC temp-file save (write a
/// freshly-named sibling temp, then `rename(2)` it over the path), exactly how
/// an editor saves and precisely what the watcher's fingerprint is built to
/// detect (`config_watch.rs` module docs: "a write-temp + `rename(2)` lands as
/// a normal change (new inode)").
///
/// The rename gives the path a BRAND-NEW inode — the temp is created while the
/// old file is still linked, so its inode can never be the still-linked one —
/// so the fingerprint always differs from the previously-applied one regardless
/// of mtime granularity. This is the realistic editor-save model, so the
/// external-edit tests use it. A same-length in-place rewrite (same inode) is
/// NOW detected too — the watcher fingerprints the file's bytes, not just its
/// stat — and that path has its own coverage in
/// [`an_in_place_same_length_edit_after_promote_still_applies`].
fn external_edit(path: &Path, content: &str) {
    let dir = path.parent().expect("the boot path has a parent directory");
    let tmp = tempfile::NamedTempFile::new_in(dir).expect("create a temp for the atomic edit");
    std::fs::write(tmp.path(), content).expect("write the external-edit content to the temp");
    tmp.persist(path)
        .expect("atomically rename the external edit into place");
}

/// DETERMINISTIC equivalent of `revert_after_a_file_watch_apply_returns_to_loaded`'s
/// apply gate: an external boot-file edit hot-applies through the watcher, driven
/// by manual ticks with ZERO wall-clock waits. Proves the manual-poll seam
/// exercises the real settle → diff → apply path (task #131). The previous
/// version slept `TEST_POLL` and polled `wait_until(SETTLE=10s)` for
/// `applied_count >= 1`; this one fires exactly the ticks the debounce needs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_poll_drives_an_external_edit_apply_deterministically() {
    let mut r = rig(BOOT_DOC);
    let (options, mut poll) = WatchOptions::default()
        .with_poll_interval(TEST_POLL)
        .with_manual_poll();
    let watch = spawn_watch(
        r.boot_path.clone(),
        r.config.clone(),
        r.state.clone(),
        options,
    );

    // Settle the boot baseline (unchanged file ⇒ empty diff ⇒ no apply).
    settle_initial(&mut poll, &r.state).await;
    assert_eq!(
        stored_color(&r.state).as_deref(),
        Some("#101418"),
        "the store still holds the boot colour after the initial settle"
    );

    // External edit: in_a goes near-white. One tick registers the new
    // fingerprint as a candidate; the second crosses the debounce and applies.
    external_edit(&r.boot_path, &BOOT_DOC.replace("#101418", "#f0f0f0"));
    assert!(poll.poll_once().await, "first poll: candidate registered");
    assert_eq!(
        r.state.config_watch.snapshot().applied_count,
        0,
        "one poll only registers the candidate; the debounce has not settled yet"
    );
    assert!(
        poll.poll_once().await,
        "second poll: the settled edit applies"
    );

    assert_eq!(
        r.state.config_watch.snapshot().applied_count,
        1,
        "the settled external edit applies exactly once — deterministically"
    );
    assert_eq!(
        stored_color(&r.state).as_deref(),
        Some("#f0f0f0"),
        "the store mirrors the applied edit"
    );
    let drained = r.commands.try_drain();
    assert!(
        drained.iter().any(|c| matches!(
            c,
            Command::UpsertSource { source, .. } if source.id == "in_a"
        )),
        "the edit rides the live machinery (UpsertSource), got {drained:?}"
    );

    watch.stop();
}

/// Regression (task #7): the post-promote external-edit apply must NOT depend
/// on the filesystem's mtime granularity. The watcher fingerprints the boot
/// file and short-circuits without re-applying when the fingerprint matches the
/// already-applied one (`config_watch.rs`).
///
/// This test REMOVES mtime as a variable — it forces the edit's mtime equal to
/// the adopted file's — and drives an ATOMIC temp+rename edit (via
/// [`external_edit`]), which lands on a FRESH inode distinct from the
/// still-linked adopted one by construction, so the fingerprint always differs
/// and the edit hot-applies. The harder same-length IN-PLACE case (every stat
/// axis aliases; only the bytes differ) is covered by
/// [`an_in_place_same_length_edit_after_promote_still_applies`], which the
/// watcher now catches because the fingerprint folds a hash of the bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_edit_after_promote_applies_regardless_of_mtime_granularity() {
    let mut r = rig(BOOT_DOC);
    let (options, mut poll) = WatchOptions::default()
        .with_poll_interval(TEST_POLL)
        .with_manual_poll();
    let watch = spawn_watch(
        r.boot_path.clone(),
        r.config.clone(),
        r.state.clone(),
        options,
    );
    settle_initial(&mut poll, &r.state).await;

    // Promote: the watcher adopts (not applies) its own write.
    recolor_in_a(&r, "#c0c0c0").await;
    let _ = r.commands.try_drain();
    let resp = send(
        &r.router,
        post_idem("/api/v1/config/promote", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    drive_polls(&mut poll, 4).await;
    assert_eq!(
        r.state.config_watch.snapshot().applied_count,
        0,
        "the promote itself must not count as a file-watch apply"
    );

    // The fingerprint the watcher now holds as `applied` (the promote-adopted
    // boot file).
    let adopted_mtime = std::fs::metadata(&r.boot_path)
        .expect("stat the adopted boot file")
        .modified()
        .expect("adopted boot file mtime");

    // A genuine external edit: a same-width colour swap, in place (same inode,
    // same length) — the original flaky write path.
    let promoted = std::fs::read_to_string(&r.boot_path).expect("read the promoted file");
    let edited = promoted.replace("#c0c0c0", "#101418");
    external_edit(&r.boot_path, &edited);
    // Remove mtime as a discriminator — force the worst case the coarse fs
    // granularity produces by accident, so ONLY the inode can tell the edit
    // apart from the adopted content.
    force_mtime(&r.boot_path, adopted_mtime);

    drive_polls(&mut poll, 2).await;
    assert_eq!(
        r.state.config_watch.snapshot().applied_count,
        1,
        "a real external edit after a promote must hot-apply even when its \
         fingerprint mtime matches the adopted write (task #7)"
    );
    assert_eq!(
        stored_color(&r.state).as_deref(),
        Some("#101418"),
        "the store mirrors the applied external edit"
    );
    watch.stop();
}

/// Regression (task #7, the real production fix): the config watcher must detect
/// a legitimate same-length IN-PLACE rewrite — a shell `>` redirect, a
/// non-atomic editor, or a config-management tool rewriting the file in place —
/// even when it lands within the filesystem's mtime granularity so its `len`,
/// `mtime`, and `inode` ALL alias the already-applied write. This is the defect
/// behind the original flake: the watcher's fingerprint was stat-only
/// (len+mtime+inode) and the "already applied" short-circuit dropped such an
/// edit WITHOUT ever reading its content. The fix folds a hash of the file's
/// bytes into the fingerprint, so a same-length in-place content change is still
/// a distinct fingerprint and hot-applies.
///
/// The stimulus is the aliased worst case, forced deterministically (not left to
/// the tmpfs odds): an in-place `std::fs::write` (same inode by construction), a
/// same-WIDTH colour swap (same length), and `force_mtime` pinning the edit's
/// mtime equal to the adopted write's. Only the file's BYTES differ — every stat
/// axis aliases. Fails (`applied_count` stays 0) against a stat-only fingerprint;
/// passes once the content hash lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_in_place_same_length_edit_after_promote_still_applies() {
    let mut r = rig(BOOT_DOC);
    let (options, mut poll) = WatchOptions::default()
        .with_poll_interval(TEST_POLL)
        .with_manual_poll();
    let watch = spawn_watch(
        r.boot_path.clone(),
        r.config.clone(),
        r.state.clone(),
        options,
    );
    settle_initial(&mut poll, &r.state).await;

    // Promote: the watcher adopts (not applies) its own write.
    recolor_in_a(&r, "#c0c0c0").await;
    let _ = r.commands.try_drain();
    let resp = send(
        &r.router,
        post_idem("/api/v1/config/promote", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    drive_polls(&mut poll, 4).await;
    assert_eq!(
        r.state.config_watch.snapshot().applied_count,
        0,
        "the promote itself must not count as a file-watch apply"
    );

    // The adopted write's mtime + inode — the aliasing target.
    let adopted_meta = std::fs::metadata(&r.boot_path).expect("stat the adopted boot file");
    let adopted_mtime = adopted_meta.modified().expect("adopted boot file mtime");
    #[cfg(unix)]
    let adopted_inode = {
        use std::os::unix::fs::MetadataExt as _;
        adopted_meta.ino()
    };

    // A genuine external edit written IN PLACE (same inode), a same-width colour
    // swap (same length) — the path an atomic-rename editor would NOT take but a
    // shell redirect or config-management rewrite does.
    let promoted = std::fs::read_to_string(&r.boot_path).expect("read the promoted file");
    let edited = promoted.replace("#c0c0c0", "#101418");
    assert_eq!(
        edited.len(),
        promoted.len(),
        "the colour swap must be the same length to exercise the aliasing case"
    );
    std::fs::write(&r.boot_path, &edited).expect("rewrite the boot file in place");
    // Force the worst case the coarse fs granularity produces by accident: pin
    // the edit's mtime to the adopted write's, so len + mtime + inode ALL alias
    // and only the bytes differ.
    force_mtime(&r.boot_path, adopted_mtime);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let edited_inode = std::fs::metadata(&r.boot_path)
            .expect("stat the in-place edit")
            .ino();
        assert_eq!(
            edited_inode, adopted_inode,
            "an in-place rewrite must reuse the inode — the aliasing precondition"
        );
    }

    drive_polls(&mut poll, 2).await;
    assert_eq!(
        r.state.config_watch.snapshot().applied_count,
        1,
        "a same-length in-place edit whose len+mtime+inode alias the adopted \
         write must still hot-apply — the watcher fingerprints content, not just \
         stat (task #7)"
    );
    assert_eq!(
        stored_color(&r.state).as_deref(),
        Some("#101418"),
        "the store mirrors the applied in-place edit"
    );
    watch.stop();
}

/// RED→GREEN (task #7, cross-vendor finding 1 — the probe-vs-settled TOCTOU):
/// the fingerprint the watcher records as `applied` MUST be the fingerprint of
/// the content it actually applied. The poll probes the file (folding a content
/// hash into the fingerprint), debounces, and — on the settling poll — applies.
/// If a concurrent writer lands BETWEEN this poll's probe and its settled read,
/// a probe-then-reread applies the racer's bytes (B) yet records the PROBE's
/// fingerprint (A) — an incoherent `applied` that would then silently drop a
/// later edit restoring A (its fingerprint aliases the mis-recorded `applied`).
///
/// The [`WatchOptions::with_post_probe_interpose`] seam interposes exactly that
/// concurrent write in the probe→apply window, deterministically. The applied
/// content must be the PROBED snapshot A (whose fingerprint is what gets
/// recorded); the racer B is not lost — it is a fresh fingerprint picked up on a
/// later poll. Since `applied` is recorded as A's fingerprint in BOTH the buggy
/// and fixed builds, asserting the store holds A is exactly asserting
/// `applied == fingerprint-of-applied-content`: the reread build stores B and
/// FAILS; the single-read build stores A and passes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_write_racing_the_probe_never_desyncs_the_applied_fingerprint() {
    let mut r = rig(BOOT_DOC);
    let (options, interpose) = WatchOptions::default()
        .with_poll_interval(TEST_POLL)
        .with_post_probe_interpose();
    let (options, mut poll) = options.with_manual_poll();
    let watch = spawn_watch(
        r.boot_path.clone(),
        r.config.clone(),
        r.state.clone(),
        options,
    );
    settle_initial(&mut poll, &r.state).await;

    // The edit we expect applied (A): a same-width colour swap.
    let doc_a = BOOT_DOC.replace("#101418", "#aabbcc");
    external_edit(&r.boot_path, &doc_a);
    // Poll 1 registers A's fingerprint as the debounce candidate (no apply yet).
    assert!(poll.poll_once().await, "poll 1: candidate A registered");
    assert_eq!(
        r.state.config_watch.snapshot().applied_count,
        0,
        "one poll only registers the candidate; the debounce has not settled"
    );

    // Arm a concurrent writer (B, same width) to land in the probe→apply window
    // of the NEXT poll: the loop reads A for its fingerprint, THEN B appears on
    // disk, THEN the settled read/apply runs. Poll 2 crosses the debounce.
    let doc_b = BOOT_DOC.replace("#101418", "#ddeeff");
    interpose.interpose_next(&doc_b);
    assert!(
        poll.poll_once().await,
        "poll 2: A settles while B races the reread"
    );

    assert_eq!(
        r.state.config_watch.snapshot().applied_count,
        1,
        "the settled edit applies exactly once"
    );
    // The applied content must be the probed snapshot A — the fingerprint the
    // loop records as `applied` is A's, so applying B would desync it. A reread
    // records A's fingerprint but stores B (the defect); the single-read build
    // stores A.
    assert_eq!(
        stored_color(&r.state).as_deref(),
        Some("#aabbcc"),
        "the applied content matches the recorded fingerprint (A), not the racer B"
    );
    let drained = r.commands.try_drain();
    assert!(
        drained.iter().any(|c| matches!(
            c,
            Command::UpsertSource { source, .. } if source.id == "in_a"
        )),
        "the settled edit rides the live machinery, got {drained:?}"
    );

    watch.stop();
}

/// Characterisation (task #7, cross-vendor finding 2 — the typed unreadable
/// sentinel + recovery): a watched path that exists but whose BYTES cannot be
/// read this poll must NOT alias the already-applied content and must NOT apply.
/// Two such polls settle to the "cannot be read" reject; when the path becomes a
/// readable file again the watcher resumes and applies the new content. Models
/// the present-but-unreadable branch (`ContentFingerprint::Unreadable`)
/// deterministically and root-independently by swapping the file for a directory
/// at the same path — `stat` succeeds, but `read` returns `EISDIR` regardless of
/// privilege (a chmod-000 test would be a no-op under a root CI runner).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreadable_path_rejects_then_recovers_on_next_readable_content() {
    let r = rig(BOOT_DOC);
    let (options, mut poll) = WatchOptions::default()
        .with_poll_interval(TEST_POLL)
        .with_manual_poll();
    let watch = spawn_watch(
        r.boot_path.clone(),
        r.config.clone(),
        r.state.clone(),
        options,
    );
    settle_initial(&mut poll, &r.state).await;

    // Present-but-unreadable: replace the file with a directory at the SAME
    // path. No poll runs during the swap (manual tick), so the loop never sees a
    // transient ENOENT — it sees a path whose stat succeeds but whose byte read
    // fails (EISDIR). Two polls debounce it to the reject.
    std::fs::remove_file(&r.boot_path).expect("remove the boot file");
    std::fs::create_dir(&r.boot_path).expect("put a directory in its place");
    drive_polls(&mut poll, 2).await;
    assert_eq!(
        r.state.config_watch.snapshot().applied_count,
        0,
        "an unreadable path must never alias the applied content or apply"
    );
    // The reject publishes an event the warning-ingest task folds into the store
    // on a separate task, so await it (the debounce itself is deterministic).
    assert!(
        wait_until(SETTLE, || has_active_warning(
            &r.warnings,
            "config-file-invalid"
        ))
        .await,
        "two unreadable polls must settle to the cannot-be-read reject"
    );
    assert_eq!(
        stored_color(&r.state).as_deref(),
        Some("#101418"),
        "the running store is unchanged while the path is unreadable"
    );

    // Recovery: the path becomes a readable file again, carrying a new edit.
    std::fs::remove_dir(&r.boot_path).expect("remove the placeholder directory");
    let recovered = BOOT_DOC.replace("#101418", "#123456");
    std::fs::write(&r.boot_path, &recovered).expect("write the recovered content");
    drive_polls(&mut poll, 2).await;
    assert_eq!(
        r.state.config_watch.snapshot().applied_count,
        1,
        "once readable again the watcher resumes and applies the new content"
    );
    assert_eq!(
        stored_color(&r.state).as_deref(),
        Some("#123456"),
        "the recovered edit hot-applies"
    );
    watch.stop();
}

/// A boot document carrying two equal-`z` overlays (the reorder fixture).
const BOOT_DOC_TWO_OVERLAYS: &str = r##"schema_version = 1
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
color = "#103050"
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
[[overlays]]
id = "ov1"
kind = "clock"
target = "canvas"
z = 0
[[overlays]]
id = "ov2"
kind = "clock"
target = "canvas"
z = 0
[[outputs]]
kind = "hls"
path = "/tmp/reorder.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;

/// Task #130 (watcher path): on an overlay-capable run, a file-watch document
/// whose only change is a pure equal-`z` overlay REORDER (`[ov1, ov2]` →
/// `[ov2, ov1]`, identical documents) must ride the live machinery as a
/// `ReorderOverlays` command carrying the new order — so the engine actually
/// re-blends the draw order live. The id-keyed per-overlay delta is EMPTY for a
/// reorder, so without this the change would never reach the engine (the
/// `UpsertOverlay` no-op the live re-blend RED proves). Class-1: it rides the
/// same bounded bus, nothing restart-pending on a capable run.
#[tokio::test]
async fn a_file_watch_overlay_reorder_rides_reorder_overlays_command() {
    let mut r = rig_with_overlay_live(BOOT_DOC_TWO_OVERLAYS);
    let _ = r.commands.try_drain(); // ignore any seeding traffic

    // The running baseline is the boot doc; `next` swaps the two equal-z
    // overlays' declaration order, identical documents otherwise.
    let running = MultiviewConfig::load_from_toml(BOOT_DOC_TWO_OVERLAYS).expect("parse running");
    let reordered_doc = BOOT_DOC_TWO_OVERLAYS.replace(
        "[[overlays]]\nid = \"ov1\"\nkind = \"clock\"\ntarget = \"canvas\"\nz = 0\n[[overlays]]\nid = \"ov2\"\nkind = \"clock\"\ntarget = \"canvas\"\nz = 0\n",
        "[[overlays]]\nid = \"ov2\"\nkind = \"clock\"\ntarget = \"canvas\"\nz = 0\n[[overlays]]\nid = \"ov1\"\nkind = \"clock\"\ntarget = \"canvas\"\nz = 0\n",
    );
    let next = MultiviewConfig::load_from_toml(&reordered_doc).expect("parse reordered");

    let diff = ConfigDiff::between(&running, &next);
    assert!(
        diff.overlays_reordered,
        "precondition: the diff sees the reorder"
    );
    assert!(
        diff.overlays.is_empty(),
        "precondition: a pure reorder has no per-id overlay delta"
    );

    let outcome = apply_document_diff(&r.state, "config-file", &diff, &next);

    let drained = r.commands.try_drain();
    let reorder = drained.iter().find_map(|c| match c {
        Command::ReorderOverlays { order, .. } => Some(order.clone()),
        _ => None,
    });
    assert_eq!(
        reorder,
        Some(vec!["ov2".to_owned(), "ov1".to_owned()]),
        "the watcher must enqueue ReorderOverlays carrying the new draw order, \
         got {drained:?}"
    );
    assert!(
        !outcome.restart.contains("overlays"),
        "a landed reorder on a capable run is Class-1, not restart-pending; \
         restart = {:?}",
        outcome.restart
    );
    assert_eq!(outcome.shed, 0, "nothing shed on a capacity-64 bus");
}

/// Task #130 (source-leg honesty): a pure SOURCE reorder has NO live engine
/// seam — source declaration order affects only the cold-start test-pattern
/// palette index (`run.rs`), never a live render path (cells bind by `input_id`;
/// the live source stores are id-keyed and `list()` is id-sorted, so order is
/// not even representable in the store/`active.toml`). So it must be reported as
/// a real, RESTART-pending (Class-2) delta — NOT silently dropped, and NOT a
/// live `Command::ReorderSources` (there is no such command — that would be
/// theater). Before the fix `apply_document_diff` ignored a pure source reorder
/// entirely (block 1 only fires on `!diff.sources.is_empty()`) — an unapplied,
/// unreported no-op.
#[tokio::test]
async fn a_file_watch_source_reorder_is_reported_restart_pending() {
    let mut r = rig(BOOT_DOC_TWO_OVERLAYS);
    let _ = r.commands.try_drain();

    // Swap the two sources' declaration order; identical documents otherwise.
    let running = MultiviewConfig::load_from_toml(BOOT_DOC_TWO_OVERLAYS).expect("parse running");
    let reordered_doc = BOOT_DOC_TWO_OVERLAYS.replace(
        "[[sources]]\nid = \"in_a\"\nkind = \"solid\"\ncolor = \"#103050\"\n[[sources]]\nid = \"in_b\"\nkind = \"bars\"\n",
        "[[sources]]\nid = \"in_b\"\nkind = \"bars\"\n[[sources]]\nid = \"in_a\"\nkind = \"solid\"\ncolor = \"#103050\"\n",
    );
    let next = MultiviewConfig::load_from_toml(&reordered_doc).expect("parse reordered");
    assert_eq!(
        next.sources
            .iter()
            .map(|s| s.id.clone())
            .collect::<Vec<_>>(),
        vec!["in_b".to_owned(), "in_a".to_owned()],
        "the perturbed doc actually swapped the source order"
    );

    let diff = ConfigDiff::between(&running, &next);
    assert!(
        diff.sources_reordered,
        "precondition: the diff sees the source reorder"
    );
    assert!(
        diff.sources.is_empty(),
        "precondition: a pure source reorder has no per-id source delta"
    );

    let outcome = apply_document_diff(&r.state, "config-file", &diff, &next);

    // Reported as a real, restart-pending delta — never silently dropped.
    assert!(
        outcome.restart.contains("sources"),
        "a pure source reorder is restart-pending (Class-2; no live seam); \
         restart = {:?}",
        outcome.restart
    );
    // NO engine command: there is no live source-order seam (a ReorderSources
    // command would be theater — the engine consumes source order only at
    // cold-start for the test-pattern palette).
    let drained = r.commands.try_drain();
    assert!(
        drained.is_empty(),
        "a source reorder submits NO engine command, got {drained:?}"
    );
}
