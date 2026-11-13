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
