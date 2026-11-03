//! #162: a bounded run (`--ticks`/`--duration`) never starts the control
//! plane (ADR-W020 §9 / ADR-W024 §8 smoke-run semantics unchanged), so a
//! configured `[control]` section — and the `/hls/{output-id}/` HLS delivery
//! mounts it would carry — goes silently unserved for the run's whole
//! duration. [`multiview_cli::run::warn_if_control_plane_configured_on_bounded_run`]
//! is the operator-facing surfacing of that fact: it must warn loudly when
//! `[control]` is configured, and stay silent for a store-only config (no
//! `[control]`), so a genuine output-clock smoke test is not nagged.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Mutex};

use multiview_cli::run::warn_if_control_plane_configured_on_bounded_run;
use multiview_config::MultiviewConfig;
use multiview_telemetry::{LogCaptureLayer, LogRing};
use tracing_subscriber::layer::SubscriberExt as _;

/// A minimal valid config carrying a `[control]` section (listen address
/// irrelevant — [`warn_if_control_plane_configured_on_bounded_run`] never
/// binds anything, it only inspects the parsed config).
fn config_with_control() -> MultiviewConfig {
    let doc = r##"schema_version = 1
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
[control]
listen = "[::]:8080"
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
path = "/tmp/bounded-run-control-warning.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;
    MultiviewConfig::load_from_toml(doc).expect("parse config with [control]")
}

/// The same config with NO `[control]` section (a pure store-only smoke run).
fn config_without_control() -> MultiviewConfig {
    let doc = r##"schema_version = 1
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
path = "/tmp/bounded-run-control-warning-none.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;
    MultiviewConfig::load_from_toml(doc).expect("parse config without [control]")
}

/// Run `body` under a `tracing` subscriber that captures every event into a
/// fresh [`LogRing`], returning the captured records.
fn capture<F: FnOnce()>(body: F) -> Vec<multiview_telemetry::LogRecord> {
    let ring = Arc::new(LogRing::new(16));
    let layer = LogCaptureLayer::new(Arc::clone(&ring));
    let subscriber = tracing_subscriber::registry().with(layer);
    tracing::subscriber::with_default(subscriber, body);
    ring.snapshot()
}

#[test]
fn warns_once_when_control_is_configured_on_a_bounded_run() {
    let config = config_with_control();
    let records = capture(|| {
        warn_if_control_plane_configured_on_bounded_run(&config, 2700);
    });

    assert_eq!(records.len(), 1, "exactly one warning record: {records:?}");
    let rec = &records[0];
    assert_eq!(rec.level, multiview_telemetry::LogLevel::Warn);
    assert!(
        rec.message.contains("bounded run")
            && rec.message.contains("control plane")
            && rec.message.contains("/hls/"),
        "the warning must name the bounded-run / control-plane / HLS-mount \
         gap so an operator can act on it; got {:?}",
        rec.message
    );
}

#[test]
fn stays_silent_for_a_store_only_config_with_no_control_section() {
    let config = config_without_control();
    let records = capture(|| {
        warn_if_control_plane_configured_on_bounded_run(&config, 2700);
    });

    assert!(
        records.is_empty(),
        "a config with no [control] section is an honest pure smoke test — \
         it must not be warned about; got {records:?}"
    );
}

/// Guard against a dead-lock/no-op refactor: the function must be callable
/// repeatedly (e.g. a future multi-program run) without panicking or hanging,
/// and keeps emitting the same warning shape every call.
#[test]
fn is_idempotent_across_repeated_bounded_calls() {
    let config = config_with_control();
    let count = Arc::new(Mutex::new(0usize));
    let count_clone = Arc::clone(&count);
    let records = {
        let ring = Arc::new(LogRing::new(16));
        let layer = LogCaptureLayer::new(Arc::clone(&ring)).with_on_record(move |_| {
            *count_clone.lock().unwrap() += 1;
        });
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            warn_if_control_plane_configured_on_bounded_run(&config, 1);
            warn_if_control_plane_configured_on_bounded_run(&config, 2);
        });
        ring.snapshot()
    };
    assert_eq!(records.len(), 2);
    assert_eq!(*count.lock().unwrap(), 2);
}
