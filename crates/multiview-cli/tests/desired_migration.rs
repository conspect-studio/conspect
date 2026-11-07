//! First-boot migration of an existing install to `desired.json` (release gate,
//! `docs/architecture/live-durable-desired-state.md`).
//!
//! An install from before the envelope has `active.toml` and `layouts.db` and no
//! `desired.json`. These tests drive the real boot path, `resolve_start_config`
//! then `bind_and_serve`, against a real `SQLite` file and a real socket: every
//! resource keeps its name, id and `ETag`, the applied layout rebinds, a restart
//! is identical, and a stale `If-Match` is refused. A corrupt envelope stops the
//! real binary with a clear error and is never reset.
#![cfg(feature = "sqlite")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use multiview_cli::boot::resolve_start_config;
use multiview_cli::config_watch::WatchOptions;
use multiview_cli::control;
use multiview_config::MultiviewConfig;
use multiview_control::boot_model::load_desired;
use multiview_control::sqlite::SqliteRepository;
use multiview_control::{
    command_bus, AppState, EngineStateSnapshot, LayoutInput, LiveApplyCaps, LiveSourceCapability,
    Repository,
};
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TOKEN: &str = "admin.test-secret";
/// Enough outputs that their ids sort differently from their positions.
const OUTPUTS: usize = 11;

const CANVAS: &str = r##"schema_version = 1
[canvas]
width = 64
height = 64
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
"##;

const GRID: &str = r#"[layout]
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]
"#;

/// The original boot file: what the install started from long ago.
fn boot_doc() -> String {
    format!(
        r##"{CANVAS}{GRID}[control]
listen = "[::1]:0"
start = "resume"
[[sources]]
id = "in_a"
kind = "solid"
color = "#101418"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"
[[outputs]]
kind = "hls"
path = "/tmp/desired-migration-boot.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##
    )
}

/// The persisted Running state: every family, moved on from the boot file.
fn active_doc() -> String {
    let mut doc = format!(
        r##"{CANVAS}{GRID}[control]
listen = "[::1]:0"
start = "resume"
[audio]
sample_rate_hz = 48000
[[audio.routes]]
input_id = "in_a"
channels = {{ kind = "stereo" }}
target_track = "trk_a"
language = "eng"
title = "Camera A"
include_in_program_bus = true
gain_db = -3.0
mute = false
[[sources]]
id = "in_a"
kind = "solid"
color = "#a0b0c0"
display_name = "Camera A"
[[sources]]
id = "in_b"
kind = "bars"
[[cells]]
id = "cell_b"
area = "a"
[cells.source]
input_id = "in_b"
[[overlays]]
id = "ov_clock"
kind = "clock"
target = "canvas"
[[probes]]
id = "probe-1"
cell = "cell_b"
kind = "black"
luma_threshold = 16
severity = "Major"
latched = false
[probes.dwell]
up_ms = 2000
down_ms = 1000
[[devices]]
id = "dev-left"
display_name = "Left decoder"
driver = "zowietek"
address = "http://[fd00:db8::41]"
desired_mode = "decoder"
[[sync_groups]]
id = "wall"
target_skew_ms = 50
members = [{{ device = "dev-left" }}]
[[salvos]]
id = "wide"
[[salvos.tally]]
cell = "cell_b"
color = "Red"
[[tally_profiles]]
id = "main"
bit_colors = [{{ bit = 0, color = "Red" }}]
index_cells = [{{ index = 0, cell = "cell_b" }}]
"##
    );
    for index in 0..OUTPUTS {
        write!(
            doc,
            "[[outputs]]\nkind = \"hls\"\npath = \"/tmp/desired-migration-{index}.m3u8\"\n\
             codec = \"mpeg2video\"\nsegment_ms = 1000\n"
        )
        .unwrap();
    }
    doc
}

/// The `{canvas, layout, cells}` of a parsed document, as the editor saves it.
fn layout_body(config: &MultiviewConfig) -> Value {
    json!({"canvas": config.canvas, "layout": config.layout, "cells": config.cells})
}

fn db_url(boot_path: &Path) -> String {
    let path = boot_path.parent().unwrap().join(".multiview/layouts.db");
    format!("sqlite://{}?mode=rwc", path.display())
}

/// Lay out an install as an older release left it.
fn legacy_install(active: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let boot_path = dir.path().join("multiview.toml");
    std::fs::write(&boot_path, boot_doc()).unwrap();
    let state_dir = dir.path().join(".multiview");
    std::fs::create_dir_all(&state_dir).unwrap();
    std::fs::write(state_dir.join("active.toml"), active).unwrap();
    (dir, boot_path)
}

/// Save `name` in the legacy library at `version`, as past edits left it.
fn saved(repo: &SqliteRepository, id: &str, name: &str, body: &Value, version: u64) {
    let input = |name: String| LayoutInput {
        name,
        body: body.clone(),
    };
    repo.create_layout(id, input(name.to_owned())).unwrap();
    for _ in 1..version {
        repo.update_layout(id, input(name.to_owned())).unwrap();
    }
}

struct Run {
    addr: SocketAddr,
    state: AppState,
    /// Held so the bus stays open for the life of the run.
    _commands: multiview_control::CommandReceiver,
    /// Held so the manually driven watcher stays parked.
    _poll: multiview_cli::config_watch::ManualPoll,
    watch: multiview_cli::config_watch::ConfigWatchHandle,
    stop: tokio::sync::oneshot::Sender<()>,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
}

/// One process start through the same two calls the binary makes.
async fn boot(boot_path: &Path) -> Run {
    std::env::set_var("MULTIVIEW_CONTROL_TOKEN", "test-secret");
    let text = std::fs::read_to_string(boot_path).unwrap();
    let loaded = MultiviewConfig::load_from_toml(&text).unwrap();
    let start = resolve_start_config(loaded, text, boot_path).unwrap();
    let model = Arc::new(start.to_boot_model(boot_path));
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let (commands, command_rx) = command_bus(64);
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let (addr, server, state) = control::bind_and_serve(
        "[::1]:0",
        &start.running,
        publisher,
        commands,
        multiview_control::no_preview(),
        None,
        None,
        None,
        None,
        None,
        LiveApplyCaps::default(),
        LiveSourceCapability::synthetic_only(),
        Some(model),
        None,
        async move {
            let _ = stopped.await;
        },
    )
    .await
    .unwrap();
    // The watcher the binary spawns, seeded the same way. It settles on the
    // unchanged boot file, which differs from Running on purpose: a migrating
    // install must not read that gap as an external edit.
    let (options, mut poll) = WatchOptions::default()
        .with_initial_observed(start.boot_text.clone())
        .with_handle(state.watch_handle().expect("installed by bind_and_serve"))
        .with_manual_poll();
    let watch = multiview_cli::config_watch::spawn(
        boot_path.to_path_buf(),
        start.running.clone(),
        state.clone(),
        options,
    );
    assert!(poll.poll_once().await && poll.poll_once().await);
    assert_eq!(state.config_watch.snapshot().applied_count, 0);
    Run {
        addr,
        state,
        _commands: command_rx,
        _poll: poll,
        watch,
        stop,
        server,
    }
}

impl Run {
    async fn shutdown(self) {
        self.watch.stop();
        let _ = self.stop.send(());
        let _ = self.server.await;
    }
}

struct Reply {
    status: u16,
    etag: Option<String>,
    body: Value,
}

async fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    if_match: Option<&str>,
    body: Option<&Value>,
) -> Reply {
    let payload = body.map(Value::to_string).unwrap_or_default();
    let precondition = if_match.map_or_else(String::new, |tag| format!("If-Match: {tag}\r\n"));
    let request = format!(
        "{method} {path} HTTP/1.0\r\nHost: {addr}\r\nAuthorization: Bearer {TOKEN}\r\n\
         {precondition}Content-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{payload}",
        payload.len()
    );
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let raw = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    Reply {
        status: head
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap(),
        etag: head.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("etag")
                .then(|| value.trim().to_owned())
        }),
        body: serde_json::from_str(body).unwrap_or(Value::Null),
    }
}

/// Every served resource: `uri -> (ETag, document)`.
async fn snapshot(addr: SocketAddr) -> BTreeMap<String, (Option<String>, Value)> {
    let mut seen = BTreeMap::new();
    for family in [
        "sources",
        "outputs",
        "overlays",
        "probes",
        "devices",
        "sync-groups",
        "salvos",
        "tally/profiles",
        "media/players",
        "layouts",
    ] {
        let listing = http(addr, "GET", &format!("/api/v1/{family}"), None, None).await;
        assert_eq!(listing.status, 200, "{family}");
        let items = listing.body.as_array().expect("a list").clone();
        for item in items {
            let id = item["id"].as_str().expect("an id");
            let uri = format!("/api/v1/{family}/{id}");
            let one = http(addr, "GET", &uri, None, None).await;
            assert_eq!(one.status, 200, "{uri}");
            seen.insert(uri, (one.etag, one.body));
        }
    }
    let audio = http(addr, "GET", "/api/v1/audio-routing", None, None).await;
    assert_eq!(audio.status, 200);
    seen.insert("/api/v1/audio-routing".into(), (audio.etag, audio.body));
    seen
}

fn tag(version: u64) -> String {
    format!("W/\"{version}\"")
}

/// Wait for the boot or commit repair to finish, then return `layouts.db`.
async fn settled_projection(state: &AppState) -> Vec<(String, String)> {
    for _ in 0..500 {
        if !state.layout_projection.status().retrying {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let status = state.layout_projection.status();
    assert!(!status.degraded && !status.retrying, "{status:?}");
    state
        .repository
        .list_layouts()
        .unwrap()
        .into_iter()
        .map(|entry| (entry.layout.id, entry.layout.name))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_existing_install_migrates_without_losing_identity() {
    let active = active_doc();
    let running = MultiviewConfig::load_from_toml(&active).unwrap();
    let (_dir, boot_path) = legacy_install(&active);
    let original = MultiviewConfig::load_from_toml(&boot_doc()).unwrap();
    {
        // Past edits: the applied layout is a saved one, at version 3, whose
        // authored body spells out only the canvas geometry. The working entry
        // kept the old boot canvas, as a legacy database does.
        let repo = SqliteRepository::connect(&db_url(&boot_path))
            .await
            .unwrap();
        let mut applied = layout_body(&running);
        applied["canvas"] = json!({"width": 64, "height": 64});
        saved(&repo, "studio", "Studio", &applied, 3);
        saved(&repo, "schema_v1", "schema_v1", &layout_body(&original), 2);
        saved(&repo, "alternate", "Alternate", &layout_body(&original), 5);
    }
    assert!(load_desired(&boot_path).unwrap().is_none());

    // First boot: the legacy state is served, and nothing has been rewritten.
    let first = boot(&boot_path).await;
    assert!(first.state.boot_model.as_ref().unwrap().resumed());
    let before = snapshot(first.addr).await;
    assert!(load_desired(&boot_path).unwrap().is_none());
    // 2 sources, the outputs, 1 each of overlay/probe/device/sync group/salvo/
    // tally profile, 3 layouts and the audio singleton.
    assert_eq!(before.len(), 2 + OUTPUTS + 6 + 3 + 1, "{:?}", before.keys());
    let source = &before["/api/v1/sources/in_a"];
    assert_eq!(source.0, Some(tag(1)));
    assert_eq!(source.1["name"], "Camera A");
    assert_eq!(source.1["body"]["color"], "#a0b0c0", "active.toml must win");
    assert_eq!(before["/api/v1/devices/dev-left"].1["name"], "Left decoder");
    assert_eq!(before["/api/v1/layouts/studio"].0, Some(tag(3)));
    assert_eq!(before["/api/v1/layouts/alternate"].0, Some(tag(5)));

    // The first commit is an output whose id sorts away from its position, so
    // a composition from the id-sorted store would scramble every other output.
    let uri = "/api/v1/outputs/output-2";
    let mut changed = before[uri].1.clone();
    changed["body"]["segment_ms"] = json!(2000);
    let stale = before[uri].0.clone().unwrap();
    let put = http(first.addr, "PUT", uri, Some(&stale), Some(&changed)).await;
    assert_eq!(put.status, 200, "{:?}", put.body);
    assert_eq!(put.etag, Some(tag(2)));

    // The envelope now exists and every other resource is untouched.
    let desired = load_desired(&boot_path).unwrap().expect("the first commit");
    assert_eq!(
        desired.selected_layout_id, "studio",
        "selection must rebind"
    );
    assert_eq!(desired.selected_body, layout_body(&running));
    assert_eq!(desired.config.cells, running.cells);
    assert_eq!(desired.output_ids.len(), OUTPUTS);
    for (index, output) in desired.output_ids.iter().enumerate() {
        assert_eq!(*output, format!("output-{index}"));
    }
    let mut expected = before.clone();
    expected.insert(uri.to_owned(), (put.etag.clone(), changed.clone()));
    let migrated = snapshot(first.addr).await;
    for (key, value) in &expected {
        assert_eq!(migrated.get(key), Some(value), "{key} changed in migration");
    }
    assert_eq!(migrated.len(), expected.len());
    assert_eq!(migrated[uri].1["body"]["segment_ms"], 2000);
    assert_eq!(
        settled_projection(&first.state).await,
        [
            ("alternate".to_owned(), "Alternate".to_owned()),
            ("schema_v1".to_owned(), "schema_v1".to_owned()),
            ("studio".to_owned(), "Studio".to_owned()),
        ]
    );
    // A tag older than the migrated version is refused: the output this commit
    // advanced, and a saved layout whose legacy counter must have carried over.
    let stale_put = http(first.addr, "PUT", uri, Some(&stale), Some(&changed)).await;
    assert_eq!(stale_put.status, 412);
    let ghost = json!({"name": "stale", "body": migrated["/api/v1/layouts/studio"].1["body"]});
    let older = http(
        first.addr,
        "PUT",
        "/api/v1/layouts/studio",
        Some("W/\"2\""),
        Some(&ghost),
    )
    .await;
    assert_eq!(older.status, 412);
    first.shutdown().await;

    // Two restarts are byte-identical, and the same tags stay stale.
    let on_disk = std::fs::read(boot_path.parent().unwrap().join(".multiview/desired.json"));
    for _ in 0..2 {
        let next = boot(&boot_path).await;
        assert_eq!(snapshot(next.addr).await, migrated);
        let status = http(next.addr, "GET", "/api/v1/config/boot-model", None, None).await;
        assert_eq!(status.body["layout_projection"]["degraded"], false);
        assert_eq!(
            next.state
                .boot_model
                .as_ref()
                .unwrap()
                .selected_layout_id()
                .as_deref(),
            Some("studio")
        );
        let refused = http(next.addr, "PUT", uri, Some(&stale), Some(&changed)).await;
        assert_eq!(refused.status, 412);
        let gone = http(next.addr, "GET", "/api/v1/sources/nope", None, None).await;
        assert_eq!(gone.status, 404);
        next.shutdown().await;
        assert_eq!(
            std::fs::read(boot_path.parent().unwrap().join(".multiview/desired.json")).unwrap(),
            *on_disk.as_ref().unwrap(),
            "a restart must not rewrite accepted intent"
        );
    }
}

/// Nothing in the library shows the running canvas: selection falls back to the
/// working layout, and the library entry is created rather than lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_install_with_no_applied_layout_rebinds_to_the_working_entry() {
    let active = active_doc();
    let (_dir, boot_path) = legacy_install(&active);
    let original = MultiviewConfig::load_from_toml(&boot_doc()).unwrap();
    {
        let repo = SqliteRepository::connect(&db_url(&boot_path))
            .await
            .unwrap();
        saved(&repo, "alternate", "Alternate", &layout_body(&original), 2);
    }
    let run = boot(&boot_path).await;
    let uri = "/api/v1/sources/in_b";
    let current = http(run.addr, "GET", uri, None, None).await;
    let body = json!({"name": "B", "body": {"id": "in_b", "kind": "solid", "color": "#123456"}});
    let put = http(run.addr, "PUT", uri, current.etag.as_deref(), Some(&body)).await;
    assert_eq!(put.status, 200, "{:?}", put.body);
    let desired = load_desired(&boot_path).unwrap().unwrap();
    assert_eq!(desired.selected_layout_id, "schema_v1");
    assert!(desired
        .layouts
        .iter()
        .any(|layout| layout.id == "alternate"));
    run.shutdown().await;
}

/// A boot repairs a projection that a previous run left behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn boot_repairs_a_projection_left_stale_by_the_last_run() {
    let active = active_doc();
    let running = MultiviewConfig::load_from_toml(&active).unwrap();
    let (_dir, boot_path) = legacy_install(&active);
    {
        let repo = SqliteRepository::connect(&db_url(&boot_path))
            .await
            .unwrap();
        saved(&repo, "studio", "Studio", &layout_body(&running), 3);
    }
    let run = boot(&boot_path).await;
    let uri = "/api/v1/sources/in_b";
    let current = http(run.addr, "GET", uri, None, None).await;
    let body = json!({"name": "B", "body": {"id": "in_b", "kind": "bars"}});
    let put = http(run.addr, "PUT", uri, current.etag.as_deref(), Some(&body)).await;
    assert_eq!(put.status, 200, "{:?}", put.body);
    let served = snapshot(run.addr).await;
    run.shutdown().await;
    {
        // The state a fault leaves: a lost rename and a row the envelope deleted.
        let repo = SqliteRepository::connect(&db_url(&boot_path))
            .await
            .unwrap();
        let drifted = LayoutInput {
            name: "stale name".into(),
            body: layout_body(&running),
        };
        repo.update_layout("studio", drifted.clone()).unwrap();
        repo.create_layout("ghost", drifted).unwrap();
    }
    let next = boot(&boot_path).await;
    assert_eq!(
        settled_projection(&next.state).await,
        [
            ("schema_v1".to_owned(), "schema_v1".to_owned()),
            ("studio".to_owned(), "Studio".to_owned()),
        ]
    );
    assert_eq!(snapshot(next.addr).await, served, "drift reached the API");
    next.shutdown().await;
}

fn run_binary(boot_path: &Path) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_multiview"))
        .args(["run", "--software", "--ticks", "1"])
        .arg(boot_path)
        .env("MULTIVIEW_CONTROL_TOKEN", "test-secret")
        .output()
        .expect("the multiview binary runs")
}

/// Accepted intent that cannot be read stops the run. It never falls back to
/// `active.toml` or the boot file, and the record is left for the operator.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_corrupt_envelope_stops_the_real_binary_and_is_never_reset() {
    let active = active_doc();
    let running = MultiviewConfig::load_from_toml(&active).unwrap();
    let (_dir, boot_path) = legacy_install(&active);
    let run = boot(&boot_path).await;
    let uri = "/api/v1/sources/in_b";
    let current = http(run.addr, "GET", uri, None, None).await;
    let body = json!({"name": "B", "body": {"id": "in_b", "kind": "bars"}});
    let put = http(run.addr, "PUT", uri, current.etag.as_deref(), Some(&body)).await;
    assert_eq!(put.status, 200, "{:?}", put.body);
    run.shutdown().await;
    let path = boot_path.parent().unwrap().join(".multiview/desired.json");
    let valid = std::fs::read_to_string(&path).unwrap();
    let mut document: Value = serde_json::from_str(&valid).unwrap();

    let truncated = valid[..valid.len() / 2].to_owned();
    document["version"] = json!(2);
    let future = document.to_string();
    document["version"] = json!(1);
    document["selected_layout_id"] = json!("no-such-layout");
    let dangling = document.to_string();
    document["selected_layout_id"] = json!("studio");
    document["output_ids"] = json!([]);
    let unmapped = document.to_string();

    for (case, corrupt, reason) in [
        ("truncated", truncated.as_str(), "parsing"),
        (
            "unknown version",
            future.as_str(),
            "unsupported desired-state version 2",
        ),
        (
            "dangling selection",
            dangling.as_str(),
            "selected layout is absent",
        ),
        (
            "lost output identities",
            unmapped.as_str(),
            "output identity map",
        ),
    ] {
        std::fs::write(&path, corrupt).unwrap();
        let output = run_binary(&boot_path);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{case}: the run must not start");
        assert!(
            stderr.contains("accepted desired state cannot be read")
                && stderr.contains("desired.json")
                && stderr.contains(reason),
            "{case}: unclear startup error: {stderr}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            corrupt,
            "{case}: the record must be left untouched"
        );
        // The library path cannot skip the check: an embedder fails closed too.
        let config = running.clone();
        let model = Arc::new(multiview_control::boot_model::BootModel::new(
            boot_path.clone(),
            config.clone(),
            config.clone(),
            multiview_config::StartMode::Resume,
            true,
            None,
        ));
        let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(8));
        let (commands, _rx) = command_bus(8);
        let bound = control::bind_and_serve(
            "[::1]:0",
            &config,
            publisher,
            commands,
            multiview_control::no_preview(),
            None,
            None,
            None,
            None,
            None,
            LiveApplyCaps::default(),
            LiveSourceCapability::synthetic_only(),
            Some(model),
            None,
            async {},
        )
        .await;
        assert!(
            bound.is_err(),
            "{case}: bind_and_serve must refuse to serve"
        );
    }
    // Restoring the record recovers the run, with nothing lost.
    std::fs::write(&path, &valid).unwrap();
    assert!(load_desired(&boot_path).unwrap().is_some());
}
