//! End-to-end (SEC-14): the *served* control plane installs the management-plane
//! request-concurrency + rate caps and keys the per-IP limit on the real peer address.
//!
//! The middleware behaviour is unit-tested in-process (`src/limits.rs`); this test
//! adds the real TCP bind + `into_make_service_with_connect_info` wiring, so it
//! proves `ConnectInfo` actually reaches the per-IP guard over a genuine socket
//! and that `router()` installs the layers when `control.limits` is enabled.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::Arc;
use std::time::Duration;

use multiview_config::limits::ManagementLimits;
use multiview_control::{
    command_bus, ApiKeyStore, AppState, EngineStateSnapshot, InMemoryRepository,
};
use multiview_engine::EnginePublisher;
use multiview_events::{Alert, AlertSeverity, Event};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A real `AppState` with the SEC-14 limits enabled and a per-IP burst of exactly
/// one, so the second request from the same loopback client is rejected.
fn limited_state() -> AppState {
    let engine = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let (commands, _rx) = command_bus(8);
    let mut limits = ManagementLimits::default();
    limits.per_ip.burst = 1;
    limits.per_ip.refill_per_sec = 1;
    AppState::new(
        engine,
        commands,
        Arc::new(InMemoryRepository::new()),
        Arc::new(ApiKeyStore::new(b"limits-e2e-pepper".to_vec())),
    )
    .with_limits(&limits)
}

/// Minimal HTTP/1.0 client: one request, read to EOF, return the numeric status
/// code from the status line (`HTTP/<ver> <code> <reason>`).
async fn http_get_status(addr: std::net::SocketAddr, path: &str) -> Option<String> {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let req = format!("GET {path} HTTP/1.0\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf).into_owned();
    text.lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .map(str::to_owned)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_served_control_plane_rate_limits_per_source_ip() {
    // IPv6-first loopback bind (conventions §10).
    let listener = TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(multiview_control::serve(
        listener,
        limited_state(),
        async move {
            let _ = shutdown_rx.await;
        },
    ));

    // First request from the loopback client is within the per-IP burst of 1.
    assert_eq!(
        http_get_status(addr, "/api/v1/openapi.json")
            .await
            .as_deref(),
        Some("200"),
        "first request should be admitted"
    );
    // The second request (same source IP, immediately) exceeds the burst → 429.
    // Proves ConnectInfo reached the per-IP guard over a real socket.
    assert_eq!(
        http_get_status(addr, "/api/v1/openapi.json")
            .await
            .as_deref(),
        Some("429"),
        "second request from the same IP should be rate-limited"
    );

    shutdown_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("serve returned within 5s of shutdown")
        .expect("serve task did not panic")
        .expect("serve returned no I/O error");
}

/// A real `AppState` with auth disabled (so a no-`Origin`, no-credential WebSocket
/// upgrade reaches the handler as `local_admin`) and the concurrency cap set to
/// `max_concurrent`, with generous per-IP / per-key rates so only the concurrency cap
/// bites in this test. Returns the engine publisher too, so the test can emit an event
/// to wake the sessions (an idle WebSocket session only notices a gone client on its
/// next write).
fn concurrency_capped_state(
    max_concurrent: usize,
) -> (AppState, Arc<EnginePublisher<EngineStateSnapshot, Event>>) {
    let engine = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let (commands, _rx) = command_bus(8);
    let mut limits = ManagementLimits::default();
    limits.max_concurrent_requests = max_concurrent;
    // Generous rate limits: all handshakes are from one loopback IP, so a tight per-IP
    // burst would shed them BEFORE the concurrency cap — which is what we want to test.
    limits.per_ip.burst = 1_000;
    limits.per_ip.refill_per_sec = 1_000;
    limits.per_api_key.burst = 1_000;
    limits.per_api_key.refill_per_sec = 1_000;
    let state = AppState::new(
        Arc::clone(&engine),
        commands,
        Arc::new(InMemoryRepository::new()),
        Arc::new(ApiKeyStore::new(b"ws-cap-e2e-pepper".to_vec())),
    )
    .with_auth_disabled(true)
    .with_limits(&limits);
    (state, engine)
}

/// An `alert.raised` event, used only to wake the WebSocket sessions so one whose
/// client has gone notices the disconnect on its (failing) write and tears down.
fn wake_event() -> Event {
    Event::AlertRaised(Alert {
        key: "sec14-wake".to_owned(),
        severity: AlertSeverity::Warning,
        title: "wake".to_owned(),
        detail: None,
        active: true,
    })
}

/// Open a raw WebSocket upgrade handshake (no `Origin`, no credential — accepted as
/// `local_admin` under `auth_disabled`), read the response status line, and RETURN the
/// still-open socket so the caller can hold the session open. A `101` means the socket
/// upgraded (and its session task now holds a concurrency permit); a `503` means the
/// cap shed it.
async fn ws_upgrade_status(addr: std::net::SocketAddr) -> (String, TcpStream) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    // RFC 6455 example key; the server's accept value is irrelevant to this test.
    let req = format!(
        "GET /api/v1/ws HTTP/1.1\r\nHost: {addr}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
         Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).await.unwrap();
    // Read until we have the status line (the first `\n`). Any WS frame the session
    // sends after a `101` stays buffered — we never drain it, keeping the socket open.
    let mut acc = Vec::new();
    let mut buf = [0u8; 256];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
            .await
            .expect("read the handshake response within 5s")
            .unwrap();
        if n == 0 {
            break;
        }
        acc.extend_from_slice(&buf[..n]);
        if acc.contains(&b'\n') {
            break;
        }
    }
    let text = String::from_utf8_lossy(&acc).into_owned();
    let status = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or_default()
        .to_owned();
    (status, stream)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_served_control_plane_caps_concurrent_websocket_connections() {
    // The concurrency cap must bound LIVE WebSocket connections, not just handshake
    // handler executions: axum spawns the upgraded socket as a detached task, so the
    // permit has to ride the session (SEC-14 F1). Cap = 2 ⇒ two held-open sockets
    // occupy both permits and the third upgrade is shed 503.
    let listener = TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (state, engine) = concurrency_capped_state(2);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(multiview_control::serve(listener, state, async move {
        let _ = shutdown_rx.await;
    }));

    // Two WebSocket sessions, held open, occupy both concurrency permits.
    let (s1, hold1) = ws_upgrade_status(addr).await;
    assert_eq!(s1, "101", "the first WebSocket upgrade is admitted");
    let (s2, _hold2) = ws_upgrade_status(addr).await;
    assert_eq!(s2, "101", "the second WebSocket upgrade is admitted");

    // A third upgrade finds no permit — the two live sockets still hold theirs — so it
    // is shed `503`. This is the WebSocket half of F1: a live socket keeps its slot.
    let (s3, s3_socket) = ws_upgrade_status(addr).await;
    assert_eq!(
        s3, "503",
        "a third concurrent WebSocket upgrade is shed while two are held open"
    );
    drop(s3_socket);

    // Close the first socket: its session detects the gone client on its next write and
    // ends, dropping its concurrency permit — so a fresh upgrade is admitted. Each loop
    // publishes an event so the idle session actually attempts that write (the second,
    // still-connected session absorbs the event and keeps its permit).
    drop(hold1);
    let mut admitted_after_close = None;
    for _ in 0..50 {
        let _ = engine.publish_event(wake_event());
        let (status, _s) = ws_upgrade_status(addr).await;
        if status == "101" {
            admitted_after_close = Some(status);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        admitted_after_close.as_deref(),
        Some("101"),
        "closing a held WebSocket frees its permit for a new upgrade"
    );

    shutdown_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("serve returned within 5s of shutdown")
        .expect("serve task did not panic")
        .expect("serve returned no I/O error");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_limits_falls_back_to_secure_defaults_on_an_invalid_config() {
    // F-D: the PUBLIC `with_limits` runtime API must not install an INVALID limits
    // config. A zero concurrency cap is invalid — installed verbatim it builds a
    // zero-permit `Semaphore` that sheds EVERY request `503` forever (a self-inflicted
    // outage). The CLI path validates at config load, but an embedder calling
    // `with_limits` directly bypassed that. `with_limits` now validates and falls back
    // to the secure defaults (fail-SAFE, never panic), so the served plane still admits
    // requests.
    let engine = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let (commands, _rx) = command_bus(8);
    let mut invalid = ManagementLimits::default();
    invalid.max_concurrent_requests = 0;
    let state = AppState::new(
        engine,
        commands,
        Arc::new(InMemoryRepository::new()),
        Arc::new(ApiKeyStore::new(b"with-limits-validate-pepper".to_vec())),
    )
    .with_limits(&invalid);

    let listener = TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(multiview_control::serve(listener, state, async move {
        let _ = shutdown_rx.await;
    }));

    // Installed verbatim, the zero cap sheds this request `503` (the closed semaphore
    // rejects everything). With the fail-safe fallback to the secure defaults (a 256
    // cap) it is admitted `200`.
    assert_eq!(
        http_get_status(addr, "/api/v1/openapi.json")
            .await
            .as_deref(),
        Some("200"),
        "with_limits must fall back to the secure defaults on an invalid config, not \
         install a permanently-closed concurrency cap"
    );

    shutdown_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("serve returned within 5s of shutdown")
        .expect("serve task did not panic")
        .expect("serve returned no I/O error");
}
