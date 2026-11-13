//! Streams the adapter holds while classifying them (#358 review m1): a
//! silent bidi stream or a uni stream with an incomplete type varint never
//! stalls the connection and is retired at the header deadline.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::time::{Duration, Instant};

use common::*;
use multiview_h3::H3Limits;

fn short_deadline() -> H3Limits {
    H3Limits {
        header_deadline: Duration::from_millis(800),
        ..H3Limits::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn silent_bidi_stream_does_not_stall_requests_and_is_reset() {
    let server = start(TestSite::new(), None, short_deadline(), None).await;
    let mut client = h3_client(&server).await;
    let (resp, _) = fetch(&mut client.send, request(http::Method::GET, "/")).await;
    assert_eq!(resp.status(), 200);

    // Open a bidi stream and send nothing on it. QUIC only announces it once
    // a later stream is used (streams open in order), which the GET does.
    let (silent_send, _silent_recv) = client.conn.open_bi().await.unwrap();
    let started = Instant::now();
    let (resp, body) = fetch(&mut client.send, request(http::Method::GET, "/next")).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(body, b"hello /next");
    assert!(
        started.elapsed() < Duration::from_millis(600),
        "the GET waited for the silent stream ({:?})",
        started.elapsed()
    );
    // The silent stream is retired at the deadline: STOP_SENDING
    // H3_REQUEST_REJECTED.
    let stopped = tokio::time::timeout(TIMEOUT, silent_send.stopped())
        .await
        .expect("the silent stream was never rejected")
        .unwrap();
    assert_eq!(stopped.map(noq::VarInt::into_inner), Some(0x10b));
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(700), "{elapsed:?}");
    // Still usable afterwards.
    let (resp, _) = fetch(&mut client.send, request(http::Method::GET, "/after")).await;
    assert_eq!(resp.status(), 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uni_stream_with_incomplete_type_is_stopped_at_the_deadline() {
    let server = start(TestSite::new(), None, short_deadline(), None).await;
    let mut client = h3_client(&server).await;
    let (resp, _) = fetch(&mut client.send, request(http::Method::GET, "/")).await;
    assert_eq!(resp.status(), 200);

    let mut uni = client.conn.open_uni().await.unwrap();
    // The first byte of a two-byte varint, and nothing more.
    uni.write_all(&[0x40]).await.unwrap();
    let started = Instant::now();
    let (resp, _) = fetch(&mut client.send, request(http::Method::GET, "/next")).await;
    assert_eq!(resp.status(), 200);
    assert!(started.elapsed() < Duration::from_millis(600));
    let stopped = tokio::time::timeout(TIMEOUT, uni.stopped())
        .await
        .expect("the incomplete uni stream was never stopped")
        .unwrap();
    // H3_STREAM_CREATION_ERROR.
    assert_eq!(stopped.map(noq::VarInt::into_inner), Some(0x103));
    assert!(started.elapsed() >= Duration::from_millis(700));
    let (resp, _) = fetch(&mut client.send, request(http::Method::GET, "/after")).await;
    assert_eq!(resp.status(), 200);
}

/// #382: the header deadline covers only the request HEADERS. A
/// WebTransport CONNECT stream (and its session) must outlive it: before
/// the fix the guard re-armed the deadline on the next pending read, so
/// every session was reset with `H3_REQUEST_REJECTED` `header_deadline`
/// after accept (10 s in production: `MoQ` played 10 s, then fell back).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webtransport_session_outlives_the_header_deadline() {
    let (tx, mut sessions) = tokio::sync::mpsc::unbounded_channel();
    let server = start(
        TestSite::new(),
        Some(std::sync::Arc::new(Holder(tx))),
        short_deadline(),
        None,
    )
    .await;
    let mut client = h3_client(&server).await;
    let mut connect = client
        .send
        .send_request(connect_request("/moq/x", Some(r#""moqt-16""#)))
        .await
        .unwrap();
    let resp = tokio::time::timeout(TIMEOUT, connect.recv_response())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let session = tokio::time::timeout(TIMEOUT, sessions.recv())
        .await
        .unwrap()
        .unwrap();
    // Three header deadlines with an idle CONNECT stream.
    let ended = tokio::time::timeout(Duration::from_millis(2400), {
        let session = session.clone();
        async move { web_transport_trait::Session::closed(&session).await }
    })
    .await;
    assert!(ended.is_err(), "the session ended: {ended:?}");
    // The session still carries streams both ways.
    let mut uni = web_transport_trait::Session::open_uni(&session)
        .await
        .unwrap();
    web_transport_trait::SendStream::write(&mut uni, b"alive")
        .await
        .unwrap();
    web_transport_trait::SendStream::finish(&mut uni).unwrap();
    let (_, mut recv, prefix) = tokio::time::timeout(TIMEOUT, client.uni.recv())
        .await
        .unwrap()
        .unwrap();
    let mut got = prefix.map(|p| p.to_vec()).unwrap_or_default();
    got.extend(recv.read_to_end(64).await.unwrap());
    assert_eq!(got, b"alive");
    assert_eq!(
        multiview_h3::Counters::get(&server.endpoint.counters().header_deadline_rejections),
        0
    );
}

/// #382: a request body may take longer than the header deadline once the
/// HEADERS frame is complete (an upload on a slow link).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_body_may_outlast_the_header_deadline() {
    let server = start(TestSite::new(), None, short_deadline(), None).await;
    let mut client = h3_client(&server).await;
    let fut = async {
        let mut stream = client
            .send
            .send_request(request(http::Method::POST, "/upload"))
            .await
            .unwrap();
        for piece in [&b"slow "[..], b"body ", b"sent"] {
            stream
                .send_data(bytes::Bytes::from_static(piece))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(700)).await;
        }
        stream.finish().await.unwrap();
        let resp = stream.recv_response().await.unwrap();
        let body = read_body(&mut stream).await;
        (resp, body)
    };
    let (resp, body) = tokio::time::timeout(TIMEOUT, fut).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(body, b"slow body sent");
}

struct Holder(tokio::sync::mpsc::UnboundedSender<multiview_h3::WtSession>);

impl multiview_h3::WtHandler for Holder {
    fn admit(
        &self,
        _req: &http::request::Parts,
        _peer: std::net::SocketAddr,
    ) -> Result<multiview_h3::AdmitToken, http::StatusCode> {
        Ok(Box::new(()))
    }

    fn accept(
        &self,
        session: multiview_h3::WtSession,
        _token: multiview_h3::AdmitToken,
        _req: http::request::Parts,
        _peer: std::net::SocketAddr,
    ) -> multiview_h3::BoxFuture<'static, ()> {
        let _ = self.0.send(session.clone());
        Box::pin(async move {
            let _ = web_transport_trait::Session::closed(&session).await;
        })
    }

    fn protocols(&self) -> &[&'static str] {
        &["moqt-16"]
    }
}
