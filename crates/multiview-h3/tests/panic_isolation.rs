//! A panic in one request or one connection task ends only that request or
//! connection; the endpoint keeps serving everyone else and counts it
//! (issue #403).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use common::*;
use multiview_h3::{BoxFuture, Counters, H3Limits, PeersHook, RequestBody, ResponseBody, Site};

/// `/panic` panics inside the site's future (as a buggy handler, or a
/// panic in the h3 request path under it, would); everything else is 200.
struct PanickingSite;

impl Site for PanickingSite {
    fn call(
        &self,
        req: http::Request<RequestBody>,
        _peer: SocketAddr,
    ) -> BoxFuture<'static, http::Response<ResponseBody>> {
        let path = req.uri().path().to_owned();
        Box::pin(async move {
            assert!(path != "/panic", "injected request-handler panic");
            http::Response::new(full_body(Bytes::from(format!("ok {path}"))))
        })
    }
}

/// Sends `path` and returns whether a complete response arrived.
async fn try_fetch(client: &mut Client, path: &str) -> bool {
    let fut = async {
        let mut stream = client
            .send
            .send_request(request(http::Method::GET, path))
            .await
            .ok()?;
        let _ = stream.finish().await;
        let resp = stream.recv_response().await.ok()?;
        Some(resp.status())
    };
    matches!(
        tokio::time::timeout(TIMEOUT, fut).await,
        Ok(Some(status)) if status == 200
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_handler_panic_ends_only_that_request() {
    let limits = H3Limits {
        // One permit: a panic that leaked its permit would starve the
        // connection's next request.
        max_concurrent_requests: 1,
        ..H3Limits::default()
    };
    let server = start(Arc::new(PanickingSite), None, limits, None).await;
    let mut a = h3_client(&server).await;
    let mut b = h3_client(&server).await;
    assert!(try_fetch(&mut a, "/before").await);

    for _ in 0..3 {
        assert!(
            !try_fetch(&mut a, "/panic").await,
            "a panicked request answered 200"
        );
        // The same connection serves the next request (permit released)...
        let (resp, body) = fetch(&mut a.send, request(http::Method::GET, "/same")).await;
        assert_eq!(resp.status(), 200);
        assert_eq!(body, b"ok /same");
        // ...and so does every other connection.
        let (resp, _) = fetch(&mut b.send, request(http::Method::GET, "/other")).await;
        assert_eq!(resp.status(), 200);
    }
    // A fresh connection is accepted and served.
    let mut c = h3_client(&server).await;
    let (resp, _) = fetch(&mut c.send, request(http::Method::GET, "/new")).await;
    assert_eq!(resp.status(), 200);

    let counters = server.endpoint.counters();
    // Counted once per panic (the task is reaped by the connection's join
    // loop no later than the next accepted request).
    assert_eq!(Counters::get(&counters.request_panics), 3);
    assert_eq!(Counters::get(&counters.connection_panics), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection_task_panic_ends_only_that_connection() {
    // The peers hook runs inside the connection task: panic in it for the
    // first connection only.
    let armed = Arc::new(AtomicBool::new(true));
    let hook_armed = Arc::clone(&armed);
    // Live server-side connections, as the peers hook sees them: the guard's
    // "down" call runs when a connection task ends (also on unwind).
    let live = Arc::new(AtomicUsize::new(0));
    let hook_live = Arc::clone(&live);
    let hook: PeersHook = Arc::new(move |_remote, up| {
        // The panic fires inside the "up" call, before the server builds the
        // peer guard, so that connection is never counted live.
        assert!(
            !(up && hook_armed.swap(false, Ordering::SeqCst)),
            "injected connection-task panic"
        );
        if up {
            hook_live.fetch_add(1, Ordering::SeqCst);
        } else {
            hook_live.fetch_sub(1, Ordering::SeqCst);
        }
    });
    let limits = H3Limits {
        // One connection per source: a panic that leaked the cap slot would
        // refuse every later connection from [::1].
        max_connections_per_source: 1,
        ..H3Limits::default()
    };
    let server = start(TestSite::new(), None, limits, Some(hook)).await;

    // The first connection's task panics right after the handshake; the
    // QUIC connection is dropped with it.
    let doomed = quic_connect(&server, b"h3").await.unwrap();
    let closed = tokio::time::timeout(TIMEOUT, doomed.closed()).await;
    assert!(closed.is_ok(), "the panicked connection was never closed");
    assert!(!armed.load(Ordering::SeqCst));

    // The endpoint keeps accepting, and the cap slot was released.
    for i in 0..3 {
        let mut client = h3_client(&server).await;
        let path = format!("/after/{i}");
        let (resp, body) = fetch(&mut client.send, request(http::Method::GET, &path)).await;
        assert_eq!(resp.status(), 200);
        assert_eq!(body, format!("hello {path}").into_bytes());
        // With one connection per source, the next client is admitted only
        // once the server has ended this one: close it and wait for that,
        // rather than racing the close frame.
        client.conn.close(0u32.into(), b"done");
        drop(client);
        wait_until(|| live.load(Ordering::SeqCst) == 0).await;
    }
    let counters = server.endpoint.counters();
    assert_eq!(Counters::get(&counters.connection_panics), 1);
    assert_eq!(Counters::get(&counters.request_panics), 0);
}

/// Polls `cond` until it holds, failing after `TIMEOUT`.
async fn wait_until(cond: impl Fn() -> bool) {
    tokio::time::timeout(TIMEOUT, async {
        while !cond() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("condition not reached within TIMEOUT");
}
