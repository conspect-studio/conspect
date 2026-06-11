//! `multiview_control::serve` binds a real socket, serves the control router over
//! HTTP, and shuts down gracefully when its shutdown future resolves.
//!
//! This is the seam that lets `multiview run` expose the management API and the
//! embedded web UI:
//! `router()` is already covered in-process via `tower::oneshot`; what `serve`
//! adds is the real TCP bind + graceful-shutdown drive, so the test exercises a
//! genuine client socket against an unauthenticated endpoint and then asserts the
//! server future returns cleanly once signalled.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::Arc;
use std::time::Duration;

use multiview_control::{
    command_bus, ApiKeyStore, AppState, EngineStateSnapshot, InMemoryRepository,
};
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Build a minimal but real `AppState`: a wait-free engine publisher, the
/// non-blocking command bus, an in-memory repository, and an empty key store.
fn test_state() -> AppState {
    let engine = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let (commands, _rx) = command_bus(8);
    AppState::new(
        engine,
        commands,
        Arc::new(InMemoryRepository::new()),
        Arc::new(ApiKeyStore::new(b"serve-test-pepper".to_vec())),
    )
}

/// Minimal HTTP/1.0 client: write one request, read the whole response to EOF.
/// `Connection: close` makes the server close after the response, so
/// `read_to_end` terminates without parsing framing.
async fn http_get(addr: std::net::SocketAddr, path: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let req = format!("GET {path} HTTP/1.0\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    String::from_utf8_lossy(&buf).into_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_binds_responds_and_shuts_down_gracefully() {
    // Bind an ephemeral IPv6 loopback port and learn the actual address
    // (IPv6-first: the control plane must serve over `[::1]`, not just IPv4).
    let listener = TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    assert!(addr.is_ipv6(), "control plane must bind IPv6 loopback");

    // Drive the control plane on its own task; resolve the shutdown future on a
    // oneshot so the test controls when graceful shutdown begins.
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(multiview_control::serve(
        listener,
        test_state(),
        async move {
            let _ = shutdown_rx.await;
        },
    ));

    // A genuine client hits the unauthenticated OpenAPI document (default
    // `openapi` feature). Proves the socket is bound and the router is serving.
    let response = http_get(addr, "/api/v1/openapi.json").await;
    let status_line = response.lines().next().unwrap_or_default();
    // Status line is `HTTP/<ver> <code> <reason>`; assert the code is 200
    // version-agnostically (hyper echoes the request's HTTP version, e.g. 1.0).
    let status_code = status_line.split_whitespace().nth(1);
    assert_eq!(
        status_code,
        Some("200"),
        "expected a 200 status code, got status line: {status_line:?}"
    );
    assert!(
        response.contains("openapi"),
        "expected an OpenAPI document in the response body"
    );

    // Signal shutdown; the server future must resolve cleanly (no in-flight
    // connections to drain) well within a generous bound.
    shutdown_tx.send(()).unwrap();
    let joined = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("serve did not return within 5s of shutdown (graceful shutdown wedged)");
    joined
        .expect("serve task panicked")
        .expect("serve returned an I/O error");
}
