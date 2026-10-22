//! The control-plane serve loop enforces a **header-read timeout** (SEC-14 /
//! [ADR-W028](../../docs/decisions/ADR-W028.md) F-A): a slow-header ("slowloris")
//! client that opens a connection and dribbles — or never finishes — its request
//! header block is dropped when the deadline elapses, rather than pinning the
//! connection (a hyper task + socket + buffers) open indefinitely.
//!
//! The SEC-14 concurrency + rate caps engage only *after* headers are parsed, so
//! they do not bound this half-open state; the header-read timeout is what closes
//! the hole in-process. A complete request that beats the deadline is served
//! normally — the timeout only fires on a stalled header block.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::Arc;
use std::time::Duration;

use multiview_control::{
    command_bus, ApiKeyStore, AppState, EngineStateSnapshot, InMemoryRepository, ServeOptions,
};
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A short header-read deadline keeps the test fast; the production default is
/// [`multiview_control::DEFAULT_HEADER_READ_TIMEOUT`] (20 s).
const TEST_HEADER_READ_TIMEOUT: Duration = Duration::from_millis(500);

/// Build a minimal but real `AppState` (mirrors `tests/serve.rs`).
fn test_state() -> AppState {
    let engine = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let (commands, _rx) = command_bus(8);
    AppState::new(
        engine,
        commands,
        Arc::new(InMemoryRepository::new()),
        Arc::new(ApiKeyStore::new(b"serve-header-timeout-pepper".to_vec())),
    )
}

/// Bind an ephemeral IPv6 loopback listener and serve the control plane with the
/// short test header-read timeout, returning the address and a shutdown handle +
/// join handle so each test drives graceful shutdown itself.
async fn serve_with_short_timeout() -> (
    std::net::SocketAddr,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<std::io::Result<()>>,
) {
    let listener = TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    assert!(addr.is_ipv6(), "control plane must bind IPv6 loopback");

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let options = ServeOptions::default().with_header_read_timeout(Some(TEST_HEADER_READ_TIMEOUT));
    let server = tokio::spawn(multiview_control::serve_with(
        listener,
        test_state(),
        options,
        async move {
            let _ = shutdown_rx.await;
        },
    ));
    (addr, shutdown_tx, server)
}

/// Positive control: a client that sends its full header block at once (well under
/// the deadline) is served normally. Guards against a "close every connection"
/// regression that would trivially satisfy the slowloris assertion below.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_complete_request_is_served_within_the_header_read_timeout() {
    let (addr, shutdown_tx, server) = serve_with_short_timeout().await;

    let mut stream = TcpStream::connect(addr).await.unwrap();
    let req =
        format!("GET /api/v1/openapi.json HTTP/1.0\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf))
        .await
        .expect("a complete request must be answered, not timed out")
        .unwrap();
    let response = String::from_utf8_lossy(&buf);
    let status_code = response
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .nth(1);
    assert_eq!(
        status_code,
        Some("200"),
        "a fast, complete request must be served with 200, got: {response:?}"
    );

    shutdown_tx.send(()).unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
}

/// A slow-header client that sends a partial request head and then stalls — never
/// terminating its header block — must have its connection dropped by the server
/// once the header-read deadline elapses, not held open forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_header_client_is_dropped_when_it_stalls_past_the_deadline() {
    let (addr, shutdown_tx, server) = serve_with_short_timeout().await;

    // Open a connection and send the request line + one header, but NEVER the
    // terminating blank line — the classic slowloris shape — then stall.
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(b"GET /api/v1/openapi.json HTTP/1.1\r\nHost: multiview\r\n")
        .await
        .unwrap();

    // The server must give up on the unfinished header block at the deadline and
    // close the connection. Measure WHEN it closes and WHAT (if anything) it returns,
    // so the drop is provably attributable to the header-read timeout — not a
    // reject-every-connection bug (too early), an unrelated eventual termination (too
    // late), or a completed exchange.
    let mut buf = Vec::new();
    let start = std::time::Instant::now();
    let closed = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf)).await;
    let elapsed = start.elapsed();

    // (1) The connection was actually closed (the read resolved) rather than held open
    // until the 5 s outer bound — the slowloris-unbounded failure.
    assert!(
        closed.is_ok(),
        "the server held a stalled slow-header connection open past the \
         {TEST_HEADER_READ_TIMEOUT:?} header-read deadline (slowloris not bounded); it must close \
         the connection when the deadline elapses"
    );
    // (2) The close lands NEAR the deadline: not well before it (which would mean the
    // server rejects connections outright, trivially satisfying (1)) and not well
    // after it (an eventual termination, not the guard). A generous [0.5x, 3x] window
    // absorbs scheduler/CI jitter while still pinning the drop to the timeout.
    assert!(
        elapsed >= TEST_HEADER_READ_TIMEOUT / 2,
        "the connection was dropped in {elapsed:?}, well before the {TEST_HEADER_READ_TIMEOUT:?} \
         header-read deadline — not attributable to the timeout (is the server closing every \
         connection?)"
    );
    assert!(
        elapsed <= TEST_HEADER_READ_TIMEOUT * 3,
        "the connection survived {elapsed:?}, far past the {TEST_HEADER_READ_TIMEOUT:?} header-read \
         deadline — the drop is an eventual termination, not the header-read timeout firing"
    );
    // (3) The never-completed request was NOT served: no success response came back (a
    // bare close/reset, or at most a timeout status, is expected — never the requested
    // resource). Proves the close is the guard firing, not a completed exchange.
    let response = String::from_utf8_lossy(&buf);
    assert!(
        !response.contains("200"),
        "the server returned a success response to an incomplete header block: {response:?}"
    );

    shutdown_tx.send(()).unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
}
