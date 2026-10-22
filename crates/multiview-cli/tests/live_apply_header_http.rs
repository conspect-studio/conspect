//! ADR-W018 level 2 — the run-path live-apply **capability signal** over the
//! real bound server: `bind_and_serve` threads the kinds the running engine
//! can live-ingest into the control plane, and the `X-Multiview-Apply` header
//! on a network-source mutation answers per that capability — `live` only when
//! a real ingest spawner backs the claim, `restart` on the software run path.
//!
//! This pins the cli wiring end-to-end (HTTP through the real router), the
//! honesty keystone of the network live-add lane: the header must NEVER claim
//! `live` for a kind the running engine cannot ingest.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use multiview_cli::control;
use multiview_config::MultiviewConfig;
use multiview_control::{command_bus, EngineStateSnapshot, LiveSourceCapability};
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A minimal valid config with a `[control]` block (the listen address the
/// binary would use is irrelevant here — the test binds `[::1]:0` directly).
fn config() -> MultiviewConfig {
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
kind = "bars"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"
[[outputs]]
kind = "hls"
path = "/tmp/live-apply-header.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;
    MultiviewConfig::load_from_toml(doc).expect("parse config")
}

/// POST an rtsp source over a raw HTTP/1.0 connection and return the full
/// response text (status line + headers + body).
async fn post_rtsp_source(addr: std::net::SocketAddr, token: &str) -> String {
    let body =
        r#"{"name":"Cam","body":{"id":"cam1","kind":"rtsp","url":"rtsp://[2001:db8::1]/cam1"}}"#;
    let req = format!(
        "POST /api/v1/sources/cam1 HTTP/1.0\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    String::from_utf8_lossy(&buf).into_owned()
}

/// The `x-multiview-apply` header value in a raw HTTP response, if present.
fn apply_header(response: &str) -> Option<String> {
    response
        .lines()
        .take_while(|l| !l.trim().is_empty())
        .find_map(|l| {
            let (name, value) = l.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("x-multiview-apply")
                .then(|| value.trim().to_owned())
        })
}

/// Bind a server with `capability`, POST an rtsp source, return the header.
async fn header_for(capability: LiveSourceCapability) -> Option<String> {
    // A stable admin secret so the test can authenticate (the env var is set
    // process-wide; this test file is its own test binary, so no other test's
    // bootstrap-token path races it).
    std::env::set_var("MULTIVIEW_CONTROL_TOKEN", "test-secret");
    let cfg = config();
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(8));
    let (commands, _command_rx) = command_bus(8);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let (addr, server, _state) = control::bind_and_serve(
        "[::1]:0",
        &cfg,
        publisher,
        commands,
        multiview_control::no_preview(),
        None, // whep
        None, // whip
        None, // whep_output
        None, // licence
        None, // mesh
        multiview_control::LiveApplyCaps::default(),
        capability,
        None, // boot_model (ADR-W024)
        async move {
            let _ = shutdown_rx.await;
        },
    )
    .await
    .expect("control server binds");
    let response = post_rtsp_source(addr, "admin.test-secret").await;
    assert!(
        response.starts_with("HTTP/1.0 201") || response.starts_with("HTTP/1.1 201"),
        "the rtsp source create must succeed, got: {}",
        response.lines().next().unwrap_or_default()
    );
    let header = apply_header(&response);
    let _ = shutdown_tx.send(());
    let _ = server.await;
    header
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn network_header_is_live_only_when_an_ingest_spawner_is_wired() {
    // The full-pipeline (ffmpeg) run wires a real ingest spawner into the hub
    // and declares network kinds live-appliable → the header claims live.
    let live = header_for(LiveSourceCapability::synthetic_and_network()).await;
    assert_eq!(
        live.as_deref(),
        Some("live"),
        "with the network capability an rtsp create must declare live"
    );

    // The software run wires no ingest spawner → the header must stay honest.
    let restart = header_for(LiveSourceCapability::synthetic_only()).await;
    assert_eq!(
        restart.as_deref(),
        Some("restart"),
        "without the network capability an rtsp create must declare restart"
    );
}
