//! Issue #519 review (lens 1): an explicit source DELETE whose commit is
//! indeterminate (the rename landed, the directory sync failed) returns
//! before the route's audit entry; the journal must still name the caller.
//!
//! Its own test binary: it installs a tracing subscriber, and callsite
//! interest is cached process-wide, so it must not share a process with
//! tests that hit the same callsites concurrently.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use multiview_config::{MultiviewConfig, StartMode};
use multiview_control::boot_model::{load_desired, BootModel};
use serde_json::json;
use support::{
    delete_if_match, etag, get, harness_with, post_json, send, ADMIN_TOKEN, OPERATOR_TOKEN,
};

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
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]
[[sources]]
id = "cam-a"
kind = "hls"
url = "http://[::1]:9/a.m3u8"
[[sources]]
id = "youtube-germnay"
kind = "hls"
url = "http://[::1]:9/germany.m3u8"
[[cells]]
id = "pgm"
area = "a"
[cells.source]
input_id = "cam-a"
[[outputs]]
kind = "hls"
path = "/tmp/accepted-source-indeterminate/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;

/// Captures every event's fields into one string.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<String>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        struct V<'a>(&'a mut String);
        impl tracing::field::Visit for V<'_> {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                use std::fmt::Write as _;
                let _ = write!(self.0, " {}={:?}", field.name(), value);
            }
        }
        let mut out = self.0.lock().unwrap();
        event.record(&mut V(&mut out));
        out.push('\n');
    }
}

#[tokio::test]
async fn indeterminate_delete_still_journals_the_caller() {
    use tracing_subscriber::layer::SubscriberExt as _;
    let capture = Capture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let _guard = tracing::subscriber::set_default(subscriber);

    let config = MultiviewConfig::load_from_toml(DOC).unwrap();
    let seeded = multiview_control::seed_resources(&config).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("conspect.toml");
    std::fs::write(&path, DOC).unwrap();
    let model = Arc::new(BootModel::new(
        path.clone(),
        config.clone(),
        config.clone(),
        StartMode::Boot,
        false,
        None,
    ));
    let shared = Arc::clone(&model);
    let h = harness_with(move |state| {
        state
            .with_seeded_resources(seeded)
            .with_base_document(serde_json::to_value(&config).unwrap())
            .with_boot_model(shared)
            .with_live_sources(multiview_control::LiveSourceCapability::synthetic_and_network())
    });
    // A first commit so an accepted envelope exists.
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/sources/solid-1",
            OPERATOR_TOKEN,
            &json!({"name": "Solid", "body": {"id": "solid-1", "kind": "solid", "color": "#202020"}}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let current = send(
        &h.router,
        get("/api/v1/sources/youtube-germnay", ADMIN_TOKEN),
    )
    .await;
    let tag = etag(&current).unwrap();
    model.fail_next_dir_sync();
    let resp = send(
        &h.router,
        delete_if_match("/api/v1/sources/youtube-germnay", ADMIN_TOKEN, Some(&tag)),
    )
    .await;
    assert!(
        resp.status().is_server_error(),
        "an indeterminate commit is reported: {}",
        resp.status()
    );
    let persisted = load_desired(&path).unwrap().unwrap();
    assert!(
        !persisted
            .config
            .sources
            .iter()
            .any(|s| s.id == "youtube-germnay"),
        "the rename landed"
    );
    let logged = capture.0.lock().unwrap().clone();
    let line = logged
        .lines()
        .find(|l| l.contains("removed by an explicit delete") && l.contains("youtube-germnay"))
        .unwrap_or_else(|| panic!("no explicit-delete line: {logged}"));
    assert!(line.contains("admin-key"), "the caller is named: {line}");
    assert!(
        !logged.contains("accepted-state-implicit-removal"),
        "not reported as implicit: {logged}"
    );
}
