//! Handlers cannot pin a request permit or a task forever (#358 review
//! invariants F3), handshakes are capped separately (F2), and the session
//! close runs on the connection's runtime (F1).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::{Buf, Bytes};
use common::*;
use multiview_h3::capsule::{Capsule, CapsuleParser};
use multiview_h3::{
    AdmitToken, BoxFuture, Counters, H3Limits, RequestBody, ResponseBody, Site, WtHandler,
    WtSession,
};
use tokio::sync::mpsc;
use web_transport_trait::Session as _;

/// `/never` never answers; `/big` answers 8 MiB.
struct SlowSite;

impl Site for SlowSite {
    fn call(
        &self,
        req: http::Request<RequestBody>,
        _peer: SocketAddr,
    ) -> BoxFuture<'static, http::Response<ResponseBody>> {
        let path = req.uri().path().to_owned();
        Box::pin(async move {
            match path.as_str() {
                "/never" => std::future::pending().await,
                "/big" => http::Response::new(full_body(vec![7u8; 8 << 20])),
                _ => http::Response::new(full_body(Bytes::from_static(b"ok"))),
            }
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn site_that_never_answers_is_reset_and_frees_its_permit() {
    let limits = H3Limits {
        max_concurrent_requests: 1,
        response_head_timeout: Duration::from_millis(500),
        ..H3Limits::default()
    };
    let server = start(Arc::new(SlowSite), None, limits, None).await;
    let mut client = h3_client(&server).await;
    let started = Instant::now();
    let stuck = async {
        let mut stream = client
            .send
            .send_request(request(http::Method::GET, "/never"))
            .await
            .unwrap();
        stream.finish().await.unwrap();
        stream.recv_response().await
    };
    let err = tokio::time::timeout(TIMEOUT, stuck)
        .await
        .expect("the stuck request was never reset")
        .expect_err("the stuck request was answered");
    assert!(format!("{err:?}").contains("REQUEST_REJECTED"), "{err:?}");
    assert!(started.elapsed() >= Duration::from_millis(400));
    assert_eq!(
        Counters::get(&server.endpoint.counters().response_timeouts),
        1
    );
    // The only permit is free again: the next request is served (retry
    // briefly: the permit is released when the server task finishes).
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let fut = async {
            let mut stream = client
                .send
                .send_request(request(http::Method::GET, "/ok"))
                .await
                .ok()?;
            stream.finish().await.ok()?;
            stream.recv_response().await.ok()
        };
        if let Some(resp) = fut.await {
            assert_eq!(resp.status(), 200);
            break;
        }
        assert!(Instant::now() < deadline, "the permit was never released");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn response_write_without_progress_is_reset() {
    let limits = H3Limits {
        response_stall_timeout: Duration::from_millis(500),
        ..H3Limits::default()
    };
    let server = start(Arc::new(SlowSite), None, limits, None).await;
    let mut client = h3_client(&server).await;
    let mut stream = client
        .send
        .send_request(request(http::Method::GET, "/big"))
        .await
        .unwrap();
    stream.finish().await.unwrap();
    let resp = tokio::time::timeout(TIMEOUT, stream.recv_response())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status(), 200);
    // Do not read: the server runs out of flow-control credit, then resets.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let drained = tokio::time::timeout(TIMEOUT, async {
        let mut total = 0usize;
        loop {
            match stream.recv_data().await {
                Ok(Some(mut chunk)) => {
                    total += chunk.remaining();
                    chunk.advance(chunk.remaining());
                }
                Ok(None) => return Ok(total),
                Err(err) => return Err((total, err)),
            }
        }
    })
    .await
    .expect("the stalled response neither ended nor was reset");
    let (total, err) = drained.expect_err("the full 8 MiB arrived: no stall happened");
    assert!(total < 8 << 20, "{total}");
    assert!(format!("{err:?}").contains("REQUEST_CANCELLED"), "{err:?}");
}

/// Hands every session out; the handler future itself ignores `closed()`
/// and only ends when dropped (observed via `dropped`).
struct StubbornHandler {
    sessions: mpsc::UnboundedSender<WtSession>,
    dropped: Arc<AtomicBool>,
    hold_session: bool,
}

struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl WtHandler for StubbornHandler {
    fn admit(
        &self,
        _req: &http::request::Parts,
        _peer: SocketAddr,
    ) -> Result<AdmitToken, http::StatusCode> {
        Ok(Box::new(()))
    }

    fn accept(
        &self,
        session: WtSession,
        _token: AdmitToken,
        _req: http::request::Parts,
        _peer: SocketAddr,
    ) -> BoxFuture<'static, ()> {
        let flag = DropFlag(Arc::clone(&self.dropped));
        let _ = self.sessions.send(session.clone());
        let hold = self.hold_session;
        Box::pin(async move {
            let _flag = flag;
            if hold {
                let _session = session;
                std::future::pending::<()>().await;
            }
        })
    }

    fn protocols(&self) -> &[&'static str] {
        &[]
    }
}

async fn wt_server(
    limits: H3Limits,
    hold_session: bool,
) -> (Server, mpsc::UnboundedReceiver<WtSession>, Arc<AtomicBool>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let dropped = Arc::new(AtomicBool::new(false));
    let handler = Arc::new(StubbornHandler {
        sessions: tx,
        dropped: Arc::clone(&dropped),
        hold_session,
    });
    let server = start(TestSite::new(), Some(handler), limits, None).await;
    (server, rx, dropped)
}

async fn connect(client: &mut Client) -> ClientStream {
    let fut = async {
        let mut stream = client
            .send
            .send_request(connect_request("/moq/x", None))
            .await
            .unwrap();
        let resp = stream.recv_response().await.unwrap();
        assert_eq!(resp.status(), 200);
        stream
    };
    tokio::time::timeout(TIMEOUT, fut).await.expect("CONNECT")
}

/// Reads the CONNECT stream until a CLOSE capsule (or its end).
async fn recv_close(stream: &mut ClientStream) -> Option<(u32, String)> {
    let mut parser = CapsuleParser::new();
    let mut out = Vec::new();
    let fut = async {
        while let Ok(Some(mut chunk)) = stream.recv_data().await {
            let bytes = chunk.copy_to_bytes(chunk.remaining());
            parser.feed(&bytes, &mut out).ok()?;
            for c in out.drain(..) {
                if let Capsule::Close { code, reason } = c {
                    return Some((code, reason));
                }
            }
        }
        None
    };
    tokio::time::timeout(TIMEOUT, fut)
        .await
        .expect("no capsule")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handler_ignoring_close_is_dropped_after_the_grace() {
    let limits = H3Limits {
        wt_close_grace: Duration::from_millis(400),
        ..H3Limits::default()
    };
    let (server, mut sessions, dropped) = wt_server(limits, true).await;
    let mut client = h3_client(&server).await;
    let mut stream = connect(&mut client).await;
    let session = tokio::time::timeout(TIMEOUT, sessions.recv())
        .await
        .unwrap()
        .unwrap();
    drop(session);
    // The client closes the session.
    stream
        .send_data(Bytes::from(multiview_h3::capsule::encode_close(3, "bye")))
        .await
        .unwrap();
    stream.finish().await.unwrap();
    let closed_at = Instant::now();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        !dropped.load(Ordering::SeqCst),
        "dropped before the grace ran out"
    );
    let deadline = Instant::now() + TIMEOUT;
    while !dropped.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the handler was never dropped");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(closed_at.elapsed() >= Duration::from_millis(350));
    // The connection keeps serving pages.
    let (resp, _) = fetch(&mut client.send, request(http::Method::GET, "/")).await;
    assert_eq!(resp.status(), 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_from_another_runtime_still_sends_the_capsule() {
    let (server, mut sessions, _dropped) = wt_server(H3Limits::default(), false).await;
    let mut client = h3_client(&server).await;
    let mut stream = connect(&mut client).await;
    let session = tokio::time::timeout(TIMEOUT, sessions.recv())
        .await
        .unwrap()
        .unwrap();
    // A task on a different (current-thread) runtime closes the session and
    // that runtime is gone right after: nothing may depend on it.
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            session.close(42, "elsewhere");
        });
        drop(rt);
    })
    .join()
    .unwrap();
    assert_eq!(
        recv_close(&mut stream).await,
        Some((42, "elsewhere".to_owned()))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn last_handle_dropped_without_a_runtime_still_closes() {
    let (server, mut sessions, _dropped) = wt_server(H3Limits::default(), false).await;
    let mut client = h3_client(&server).await;
    let mut stream = connect(&mut client).await;
    let session = tokio::time::timeout(TIMEOUT, sessions.recv())
        .await
        .unwrap()
        .unwrap();
    // The handler returned and dropped its clone; this is the last one.
    std::thread::spawn(move || drop(session)).join().unwrap();
    assert_eq!(recv_close(&mut stream).await, Some((0, String::new())));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handshake_cap_refuses_without_touching_connections() {
    let limits = H3Limits {
        max_handshakes: 0,
        ..H3Limits::default()
    };
    let server = start(TestSite::new(), None, limits, None).await;
    assert!(quic_connect(&server, b"h3").await.is_err());
    assert_eq!(
        Counters::get(&server.endpoint.counters().handshakes_refused),
        1
    );
    assert_eq!(
        Counters::get(&server.endpoint.counters().connections_refused),
        0
    );
}
