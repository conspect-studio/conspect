//! DEV-D1 end-to-end: `multiview run`'s control listener serves every
//! configured HLS/LL-HLS output's directory under `/hls/{output-id}/` with the
//! ADR-0032 §6 header contract, so a Cast receiver (a browser app on a Google
//! origin) or any browser player can fetch playlists/segments **cross-origin**
//! over a real socket.
//!
//! Drives [`multiview_cli::control::bind_and_serve`] against a genuine
//! `[::1]` TCP socket (the crate's existing real-socket serve test pattern):
//! cross-origin GET reflects the Origin with `Vary: Origin`, preflight
//! `OPTIONS` answers `204`, a no-Origin request gets no CORS headers, and a
//! cross-origin Range GET still serves `206` + `Content-Range`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::Arc;

use multiview_cli::control::bind_and_serve;
use multiview_config::MultiviewConfig;
use multiview_control::EngineStateSnapshot;
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Segment fixture bytes (16 bytes so range maths are easy to eyeball).
const SEGMENT_BYTES: &[u8] = b"0123456789abcdef";

/// The cross-origin caller used throughout.
const ORIGIN: &str = "https://receiver.example";

/// A minimal valid config carrying TWO HLS-family outputs: one with a clean
/// explicit id and one whose id needs URL-segment sanitisation (`aux out/2` →
/// mount segment `aux-out-2`).
fn hls_config(program_dir: &std::path::Path, aux_dir: &std::path::Path) -> MultiviewConfig {
    let doc = format!(
        r##"schema_version = 1
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
kind = "rtsp"
url = "rtsp://x/a"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"
[[outputs]]
kind = "hls"
id = "program"
path = "{program}/multiview.m3u8"
codec = "h264"
[[outputs]]
kind = "ll_hls"
id = "aux out/2"
path = "{aux}/low.m3u8"
codec = "h264"
"##,
        program = program_dir.display(),
        aux = aux_dir.display(),
    );
    MultiviewConfig::load_from_toml(&doc).expect("parse HLS config")
}

/// Raw HTTP/1.0 request over a real socket; returns (head, body) split at the
/// blank line. `Connection: close` ends the read at EOF without framing.
async fn http_request(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> (String, Vec<u8>) {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut req = format!("{method} {path} HTTP/1.0\r\nHost: {addr}\r\nConnection: close\r\n");
    for (name, value) in headers {
        req.push_str(name);
        req.push_str(": ");
        req.push_str(value);
        req.push_str("\r\n");
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("response head/body split");
    let head = String::from_utf8_lossy(&buf[..split]).into_owned();
    (head, buf[split + 4..].to_vec())
}

/// The status code in an HTTP response head.
fn status_code(head: &str) -> Option<&str> {
    head.lines().next()?.split_whitespace().nth(1)
}

/// Case-insensitive single-line header lookup in a response head.
fn header_value<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

/// Stage the two HLS output directories (playlists + a segment) and build the
/// config naming them. The `TempDir`s are returned so they outlive the run.
fn staged_fixture() -> (tempfile::TempDir, tempfile::TempDir, MultiviewConfig) {
    let program_dir = tempfile::tempdir().unwrap();
    let aux_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        program_dir.path().join("multiview.m3u8"),
        "#EXTM3U\n#EXT-X-VERSION:7\n",
    )
    .unwrap();
    std::fs::write(program_dir.path().join("seg0.ts"), SEGMENT_BYTES).unwrap();
    std::fs::write(aux_dir.path().join("low.m3u8"), "#EXTM3U\n").unwrap();
    let config = hls_config(program_dir.path(), aux_dir.path());
    (program_dir, aux_dir, config)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn control_listener_serves_hls_outputs_with_cors() {
    let (_program_dir, _aux_dir, config) = staged_fixture();
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let (commands, _rx) = multiview_control::command_bus(8);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    // IPv6-first: the control plane binds `[::1]` (conventions §10).
    let (addr, handle) = bind_and_serve(
        "[::1]:0",
        &config,
        publisher,
        commands,
        multiview_control::no_preview(),
        None,
        async move {
            let _ = shutdown_rx.await;
        },
    )
    .await
    .expect("bind + serve should start");

    // 1. Cross-origin playlist GET: reflected Origin + Vary, right Content-Type.
    let (head, body) = http_request(
        addr,
        "GET",
        "/hls/program/multiview.m3u8",
        &[("Origin", ORIGIN)],
    )
    .await;
    assert_eq!(status_code(&head), Some("200"), "head: {head}");
    assert_eq!(
        header_value(&head, "access-control-allow-origin"),
        Some(ORIGIN),
        "must reflect the request Origin, head: {head}"
    );
    assert_eq!(header_value(&head, "vary"), Some("Origin"));
    assert_eq!(
        header_value(&head, "content-type"),
        Some("application/vnd.apple.mpegurl")
    );
    assert!(body.starts_with(b"#EXTM3U"));

    // 2. Preflight OPTIONS on a segment: 204 + the allow set.
    let (head, _) = http_request(
        addr,
        "OPTIONS",
        "/hls/program/seg0.ts",
        &[("Origin", ORIGIN), ("Access-Control-Request-Method", "GET")],
    )
    .await;
    assert_eq!(status_code(&head), Some("204"), "head: {head}");
    let methods = header_value(&head, "access-control-allow-methods").unwrap_or_default();
    assert!(
        methods.contains("GET") && methods.contains("OPTIONS"),
        "preflight must allow GET/OPTIONS, head: {head}"
    );

    // 3. No Origin → no CORS headers (a normal player).
    let (head, _) = http_request(addr, "GET", "/hls/program/multiview.m3u8", &[]).await;
    assert_eq!(status_code(&head), Some("200"));
    assert_eq!(
        header_value(&head, "access-control-allow-origin"),
        None,
        "a no-Origin request must carry no CORS headers, head: {head}"
    );

    // 4. Cross-origin Range GET still range-serves: 206 + Content-Range + CORS.
    let (head, body) = http_request(
        addr,
        "GET",
        "/hls/program/seg0.ts",
        &[("Origin", ORIGIN), ("Range", "bytes=2-5")],
    )
    .await;
    assert_eq!(status_code(&head), Some("206"), "head: {head}");
    assert_eq!(header_value(&head, "content-range"), Some("bytes 2-5/16"));
    assert_eq!(
        header_value(&head, "access-control-allow-origin"),
        Some(ORIGIN)
    );
    let exposed = header_value(&head, "access-control-expose-headers").unwrap_or_default();
    assert!(
        exposed.contains("Content-Range"),
        "Content-Range must be CORS-exposed, head: {head}"
    );
    assert_eq!(body, b"2345");

    // 5. The second output's id is sanitised into a URL segment:
    //    `aux out/2` mounts at `/hls/aux-out-2/`.
    let (head, body) = http_request(
        addr,
        "GET",
        "/hls/aux-out-2/low.m3u8",
        &[("Origin", ORIGIN)],
    )
    .await;
    assert_eq!(status_code(&head), Some("200"), "head: {head}");
    assert!(body.starts_with(b"#EXTM3U"));

    // 6. The management API is untouched by the HLS mounts.
    let (head, _) = http_request(addr, "GET", "/api/v1/openapi.json", &[]).await;
    assert_eq!(status_code(&head), Some("200"));

    shutdown_tx.send(()).unwrap();
    let joined = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
        .await
        .expect("serve should return within 5s of shutdown");
    joined
        .expect("serve task panicked")
        .expect("serve returned an I/O error");
}
