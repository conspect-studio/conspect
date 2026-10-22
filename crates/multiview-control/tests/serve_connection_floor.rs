//! The control-plane serve loop is **HTTP/1-only** and enforces an accept-level
//! connection population cap (SEC-14 #126 R2 / [ADR-W031](../../docs/decisions/ADR-W031.md)).
//!
//! These are the round-2 hardening tests on top of the header-read timeout
//! (`serve_header_timeout.rs`):
//!
//! * **F1 — HTTP/2 is refused.** The header-read timeout only bounds HTTP/1 header
//!   reads; hyper's HTTP/2 server has no equivalent, so an HTTP/2 connection (which
//!   the old `auto` builder would negotiate on the h2 preface) could pin a slot
//!   forever. Serving on `hyper::server::conn::http1::Builder` bounds every slow-header
//!   connection by the header-read timeout, and the h2 preface — a complete but invalid
//!   HTTP/1 request line — is rejected immediately by the parser, so no slot is ever
//!   pinned open past the deadline.
//! * **F2 — accept-level population cap.** A flood of half-open connections is bounded
//!   at accept (global + per-IP), before any request headers parse.
//! * **F3 — bounded drain.** At shutdown, tracked (non-upgraded) connection tasks are
//!   aborted after a ceiling, so no tracked task outlives `serve`; an upgraded WebSocket
//!   is a detached task that drains cooperatively via the shutdown-aware transport, not
//!   joined.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use http_body::{Body as HttpBody, Frame};
use multiview_control::{
    command_bus, ApiKeyStore, AppState, EngineStateSnapshot, InMemoryRepository, ServeOptions,
};
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The HTTP/2 connection preface a client sends to begin an h2 session over cleartext
/// (RFC 9113 §3.4). The old `auto` builder negotiates HTTP/2 on seeing this; an
/// HTTP/1-only serve loop treats it as a malformed request line instead.
const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

fn test_state() -> AppState {
    let engine = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let (commands, _rx) = command_bus(8);
    AppState::new(
        engine,
        commands,
        Arc::new(InMemoryRepository::new()),
        Arc::new(ApiKeyStore::new(b"serve-connection-floor-pepper".to_vec())),
    )
}

/// Bind an ephemeral IPv6 loopback listener and serve the control plane with the
/// given [`ServeOptions`], returning the address + a shutdown sender + the join handle.
async fn serve_with(
    options: ServeOptions,
) -> (
    std::net::SocketAddr,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<std::io::Result<()>>,
) {
    let listener = TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    assert!(addr.is_ipv6(), "control plane must bind IPv6 loopback");
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
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

/// F3 (panel round-2): an HTTP/2 client (one that sends the h2 preface, then stalls
/// without ever completing an HTTP/1 header block) must be rejected **immediately by the
/// HTTP/1-only parser** — the preface's first line, `PRI * HTTP/2.0`, is a complete but
/// invalid HTTP/1 request line, so it is rejected on parse, well **before** the
/// header-read deadline (never negotiated into a timeout-free HTTP/2 session that pins the
/// slot open). A deliberately LONG header-read deadline makes the distinction observable:
/// a prompt close proves parser-rejection, whereas a close only near the deadline would
/// mean the preface was (wrongly) treated as a slow, still-arriving header block.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_http2_preface_is_rejected_promptly_by_the_http1_parser() {
    // A deliberately LONG header-read deadline: a parser-rejection closes in
    // milliseconds, so a close well within a small fraction of this proves the preface
    // was rejected on parse, not merely dropped at the deadline.
    const LONG_HEADER_READ_TIMEOUT: Duration = Duration::from_secs(4);
    // The close must land well inside the deadline (a quarter of it). If the preface were
    // (wrongly) treated as an incomplete header block it would close at
    // ~LONG_HEADER_READ_TIMEOUT; if h2 were negotiated the read would never resolve.
    const PROMPT_BOUND: Duration = Duration::from_secs(1);

    let options = ServeOptions::default().with_header_read_timeout(Some(LONG_HEADER_READ_TIMEOUT));
    let (addr, shutdown_tx, server) = serve_with(options).await;

    // Send the h2 connection preface and nothing else — an h2 server would reply with a
    // SETTINGS frame and then wait for the client's frames indefinitely (no header-read
    // timeout applies to h2), pinning the connection. The HTTP/1-only parser rejects the
    // preface's invalid request line immediately instead.
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(H2_PREFACE).await.unwrap();

    let mut buf = Vec::new();
    let start = std::time::Instant::now();
    let closed = tokio::time::timeout(PROMPT_BOUND, stream.read_to_end(&mut buf)).await;
    let elapsed = start.elapsed();

    // Rejected PROMPTLY: the read resolves well within the (long) deadline. Under the old
    // `auto` builder the preface negotiates a live h2 session that never closes
    // (`closed.is_err()`); if the preface were treated as a slow header block it would
    // only close at ~LONG_HEADER_READ_TIMEOUT, also tripping this bound.
    assert!(
        closed.is_ok(),
        "the HTTP/2 preface was not rejected promptly (within {PROMPT_BOUND:?}) — it was \
         either negotiated into a timeout-free h2 session or held to the \
         {LONG_HEADER_READ_TIMEOUT:?} header-read deadline instead of rejected on parse. \
         elapsed={elapsed:?}"
    );
    assert!(
        elapsed < PROMPT_BOUND,
        "the HTTP/2 preface close took {elapsed:?} — not the prompt parser-rejection a \
         complete-but-invalid HTTP/1 request line must get (deadline was \
         {LONG_HEADER_READ_TIMEOUT:?})"
    );
    // And no working exchange happened: no HTTP/1 success and no live h2 session.
    let response = String::from_utf8_lossy(&buf);
    assert!(
        !response.contains("200"),
        "the server completed an exchange for an HTTP/2 preface: {response:?}"
    );

    shutdown_tx.send(()).unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
}

/// F2: the accept-level global connection cap bounds the *population* of connections
/// the serve loop will hold at all — before any request headers parse. With the cap at
/// 2, two slow-header connections occupy both slots and a third is dropped promptly at
/// accept, rather than being admitted and held until its own header-read deadline (the
/// unbounded-population failure the request-level caps miss).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_global_connection_cap_drops_over_cap_connections_at_accept() {
    // Generous header-read timeout so the two occupying connections stay held for the
    // whole test; the per-IP cap is set high so the GLOBAL cap of 2 is the binding one.
    let options = ServeOptions::default()
        .with_header_read_timeout(Some(Duration::from_secs(2)))
        .with_max_connections(Some(2))
        .with_max_connections_per_ip(Some(100));
    let (addr, shutdown_tx, server) = serve_with(options).await;

    // Occupy both global slots with slow-header connections (admitted at accept, then
    // held reading their unfinished header block).
    let mut held = Vec::new();
    for _ in 0..2 {
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(b"GET /api/v1/openapi.json HTTP/1.1\r\nHost: x\r\n")
            .await
            .unwrap();
        held.push(s);
    }
    // Let the sequential accept loop admit + spawn both.
    tokio::time::sleep(Duration::from_millis(150)).await;

    // The third connection is over the global cap of 2. The loop accepts the TCP
    // connection then immediately drops it (no admission), so the client sees a prompt
    // EOF — NOT a slot held open until the 2 s header deadline.
    let mut third = TcpStream::connect(addr).await.unwrap();
    let mut buf = Vec::new();
    let start = std::time::Instant::now();
    let read = tokio::time::timeout(Duration::from_millis(500), third.read_to_end(&mut buf)).await;
    let elapsed = start.elapsed();
    assert!(
        read.is_ok(),
        "the over-cap connection was still open after {elapsed:?} (held, not dropped at accept) — \
         the accept-level global cap is not enforced"
    );
    assert!(
        buf.is_empty(),
        "a connection dropped at accept returns no bytes; got {buf:?}"
    );

    drop(held);
    shutdown_tx.send(()).unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
}

/// A response body that never yields a frame and never ends — an in-flight response the
/// graceful drain cannot complete, forcing the abort path.
struct PendingBody;

impl HttpBody for PendingBody {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        Poll::Pending
    }

    fn is_end_stream(&self) -> bool {
        false
    }
}

/// Bind an ephemeral IPv6 loopback listener and serve an arbitrary [`Router`] with the
/// given [`ServeOptions`].
async fn serve_router_with_opts(
    app: Router,
    options: ServeOptions,
) -> (
    std::net::SocketAddr,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<std::io::Result<()>>,
) {
    let listener = TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(multiview_control::serve_router_with(
        listener,
        app,
        options,
        async move {
            let _ = shutdown_rx.await;
        },
    ));
    (addr, shutdown_tx, server)
}

/// F3: a tracked in-flight connection whose response never completes (a pending body, the
/// non-upgraded analogue of a long-lived SSE stream) must be ABORTED at the drain ceiling,
/// so no tracked connection task outlives `serve`. Under the old detached-`tokio::spawn` loop the
/// task is abandoned (the connection stays alive) when the ceiling is reached; the
/// `JoinSet` + `abort_all` fix drops it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_in_flight_connection_is_aborted_at_the_drain_ceiling() {
    let app = Router::new().route(
        "/pending",
        get(|| async { Response::new(Body::new(PendingBody)) }),
    );
    let ceiling = Duration::from_millis(300);
    let options = ServeOptions::default()
        // Generous header-read timeout: this test is about the DRAIN, not the header
        // deadline — the request completes, only the response body pends.
        .with_header_read_timeout(Some(Duration::from_secs(5)))
        .with_graceful_shutdown_ceiling(ceiling);
    let (addr, shutdown_tx, server) = serve_router_with_opts(app, options).await;

    // Send a COMPLETE request for the never-ending response, then give the server a
    // moment to route it and enter the pending-body state (the connection is now
    // in-flight and will not drain gracefully).
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(b"GET /pending HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Signal shutdown: the in-flight connection cannot complete, so the loop must abort
    // it after the ceiling.
    shutdown_tx.send(()).unwrap();

    // serve() must return within the ceiling + slack (not hang on the stuck connection).
    let serve_returned = tokio::time::timeout(ceiling + Duration::from_secs(3), server).await;
    assert!(
        serve_returned.is_ok(),
        "serve() did not return within the drain ceiling after shutdown"
    );

    // The in-flight connection must be CLOSED shortly after the ceiling — abort_all
    // dropped its task. Under the leaked-detached-task bug the task keeps the pending
    // body alive and this read never resolves.
    let mut rest = Vec::new();
    let closed = tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut rest)).await;
    assert!(
        closed.is_ok(),
        "the in-flight connection was not aborted at the drain ceiling — a connection task \
         outlived serve() (F3 leak)"
    );
}

// ---------------------------------------------------------------------------
// F1 (panel round-2): a live, UPGRADED WebSocket must keep counting against the
// accept-level population caps AND drain when serve() shuts down.
//
// hyper's `UpgradeableConnection` future completes at the HTTP/1 Upgrade handshake
// (not at the WebSocket's end), and axum runs the upgraded socket as a DETACHED
// task. So an accept-level `ConnectionGuard` held only by the connection task drops
// at upgrade — the live WebSocket then (i) stops occupying its per-IP population
// slot (sequential upgrades bypass the per-IP cap) and (ii) is invisible to the
// shutdown drain, outliving serve(). These tests pin both. RED before the
// guard-rides-the-IO + shutdown-aware `TrackedStream` fix (+ the cooperative
// `socket.recv()` arm in `run_ws_session`); GREEN after.
// ---------------------------------------------------------------------------

/// Like [`test_state`] but with auth disabled, so a credential-less WebSocket
/// handshake upgrades (as `local_admin`) — these tests exercise the accept-level
/// per-IP population cap + the shutdown drain, not authentication.
fn test_state_auth_disabled() -> AppState {
    test_state().with_auth_disabled(true)
}

/// Bind an ephemeral IPv6 loopback listener and serve the full control-plane for
/// `state` with the given [`ServeOptions`] (so `/api/v1/ws` upgrades through the
/// hand-rolled loop under the accept-level caps).
async fn serve_control_plane_with(
    state: AppState,
    options: ServeOptions,
) -> (
    std::net::SocketAddr,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<std::io::Result<()>>,
) {
    let listener = TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    assert!(addr.is_ipv6(), "control plane must bind IPv6 loopback");
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(multiview_control::serve_with(
        listener,
        state,
        options,
        async move {
            let _ = shutdown_rx.await;
        },
    ));
    (addr, shutdown_tx, server)
}

/// Open a raw `/api/v1/ws` upgrade handshake (no `Origin`, no credential — accepted
/// as `local_admin` under `auth_disabled`), read the response status code, and RETURN
/// the still-open socket so the caller holds the session open. `"101"` means the
/// socket upgraded and its detached session task is now live.
async fn ws_upgrade(addr: std::net::SocketAddr) -> (String, TcpStream) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    // RFC 6455 example key; the server's accept value is irrelevant to these tests.
    let req = format!(
        "GET /api/v1/ws HTTP/1.1\r\nHost: {addr}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
         Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).await.unwrap();
    // Read just the status line; any WS frame the session sends after `101` stays
    // buffered (we never drain it), keeping the socket open.
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

/// F1(i): a live upgraded WebSocket must occupy its source IP's population slot for
/// its whole life. With the per-IP cap at 1, a held-open WebSocket must cause a SECOND
/// connection from the same IP to be refused at accept. Under the upgrade-escape bug
/// the guard drops at the HTTP/1 Upgrade, freeing the slot, so the second connection
/// is (wrongly) admitted and served — sequential upgrades would bypass the per-IP cap
/// entirely.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_upgraded_websocket_counts_against_the_per_ip_connection_cap() {
    let options = ServeOptions::default()
        .with_header_read_timeout(Some(Duration::from_secs(5)))
        .with_max_connections(Some(64))
        .with_max_connections_per_ip(Some(1));
    let (addr, shutdown_tx, server) =
        serve_control_plane_with(test_state_auth_disabled(), options).await;

    // WS #1: upgrade and HOLD it open — its detached session must hold the per-IP slot.
    let (status, ws1) = ws_upgrade(addr).await;
    assert_eq!(status, "101", "the first WebSocket must upgrade (101)");
    // Let the accept loop settle the guard across the upgrade.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // A SECOND connection from the same IP (::1) must be refused at accept: the loop
    // accepts the TCP connection then immediately drops it (over the per-IP cap of 1),
    // so a COMPLETE request gets a prompt EOF with NO response bytes. `Connection:
    // close` means the admitted (buggy) path also resolves `read_to_end` (response +
    // close) rather than hanging on keep-alive — so `buf.is_empty()` cleanly
    // discriminates refused-at-accept from admitted-and-served.
    let mut second = TcpStream::connect(addr).await.unwrap();
    second
        .write_all(b"GET /api/v1/auth/status HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut buf = Vec::new();
    let read = tokio::time::timeout(Duration::from_millis(500), second.read_to_end(&mut buf)).await;
    assert!(
        read.is_ok(),
        "the second same-IP connection neither closed nor responded within 500ms"
    );
    assert!(
        buf.is_empty(),
        "the second same-IP connection received a response — the live WebSocket's per-IP \
         population slot was freed at the HTTP/1 upgrade (guard dropped at upgrade); got: {}",
        String::from_utf8_lossy(&buf)
    );

    drop(ws1);
    shutdown_tx.send(()).unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
}

/// F1(ii): a live upgraded WebSocket must terminate promptly when `serve()` is shut
/// down — the shutdown signal must reach the detached socket. Under the escape bug the
/// upgraded socket is a detached task invisible to the drain, and (its engine
/// broadcast still held by its own `AppState` clone) it never observes `serve()`'s
/// shutdown, so it outlives `serve()`: the client socket never reaches EOF.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_upgraded_websocket_is_drained_when_serve_shuts_down() {
    let ceiling = Duration::from_millis(500);
    let options = ServeOptions::default()
        .with_header_read_timeout(Some(Duration::from_secs(5)))
        .with_graceful_shutdown_ceiling(ceiling);
    let (addr, shutdown_tx, server) =
        serve_control_plane_with(test_state_auth_disabled(), options).await;

    let (status, mut ws) = ws_upgrade(addr).await;
    assert_eq!(status, "101", "the WebSocket must upgrade (101)");
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Shut down serve().
    shutdown_tx.send(()).unwrap();

    // serve() must return (its tracked, non-upgraded connections drain within the
    // ceiling). This alone does NOT prove the WS drained — the upgraded socket is a
    // detached task serve() does not await — so the close assertion below is the
    // load-bearing one.
    let serve_returned = tokio::time::timeout(ceiling + Duration::from_secs(3), server).await;
    assert!(
        serve_returned.is_ok(),
        "serve() did not return after shutdown"
    );

    // The client must observe the WebSocket close promptly (its socket reaches EOF):
    // the shutdown-aware transport returns EOF into the upgraded socket and the
    // cooperative session ends. Under the bug the detached session lives on and this
    // read never EOFs.
    let mut rest = Vec::new();
    let closed = tokio::time::timeout(Duration::from_secs(3), ws.read_to_end(&mut rest)).await;
    assert!(
        closed.is_ok(),
        "the live WebSocket was not drained when serve() shut down — it outlived serve() \
         (F1 shutdown-escape: the upgraded socket never saw the shutdown signal)"
    );
}
