//! The peer's control and QPACK streams are bounded (#358 review abuse F1):
//! oversized control frames, a SETTINGS that never arrives, a control frame
//! trickled forever and chatty QPACK streams all close the connection, and
//! other connections are unaffected.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::time::{Duration, Instant};

use common::*;
use multiview_h3::{varint, Counters, H3Limits};

fn limits() -> H3Limits {
    H3Limits {
        settings_deadline: Duration::from_millis(800),
        control_frame_deadline: Duration::from_millis(800),
        ..H3Limits::default()
    }
}

/// Waits for the server to close `conn`; returns the application code.
async fn closed_code(conn: &noq::Connection) -> u64 {
    match tokio::time::timeout(TIMEOUT, conn.closed())
        .await
        .expect("the server never closed the connection")
    {
        noq::ConnectionError::ApplicationClosed(close) => close.error_code.into_inner(),
        other => panic!("unexpected close: {other:?}"),
    }
}

fn frame_header(ty: u64, len: u64) -> Vec<u8> {
    let mut out = Vec::new();
    varint::encode(ty, &mut out);
    varint::encode(len, &mut out);
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_settings_closes_the_connection_others_unaffected() {
    let server = start(TestSite::new(), None, limits(), None).await;
    let mut good = h3_client(&server).await;

    let conn = quic_connect(&server, b"h3").await.unwrap();
    let mut control = conn.open_uni().await.unwrap();
    let mut bytes = vec![0x00];
    bytes.extend(frame_header(0x04, 16 * 1024)); // SETTINGS declaring 16 KiB
    bytes.extend_from_slice(&[0x01, 0x00]);
    control.write_all(&bytes).await.unwrap();
    let started = Instant::now();
    assert_eq!(closed_code(&conn).await, 0x107); // H3_EXCESSIVE_LOAD
    assert!(started.elapsed() < Duration::from_millis(700));
    assert!(Counters::get(&server.endpoint.counters().control_stream_rejections) >= 1);

    let (resp, _) = fetch(&mut good.send, request(http::Method::GET, "/other")).await;
    assert_eq!(resp.status(), 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trickled_control_frame_is_cut_at_the_deadline() {
    let server = start(TestSite::new(), None, limits(), None).await;
    let mut good = h3_client(&server).await;

    let conn = quic_connect(&server, b"h3").await.unwrap();
    let mut control = conn.open_uni().await.unwrap();
    // A valid empty SETTINGS (so the settings deadline is met), then a
    // grease frame of 900 bytes (under its cap) trickled one byte at a time.
    let mut bytes = vec![0x00];
    bytes.extend(frame_header(0x04, 0));
    bytes.extend(frame_header(0x21, 900));
    control.write_all(&bytes).await.unwrap();
    let started = Instant::now();
    let trickle = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if control.write_all(&[0]).await.is_err() {
                break;
            }
        }
    });
    assert_eq!(closed_code(&conn).await, 0x107);
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(700), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
    trickle.abort();

    let (resp, _) = fetch(&mut good.send, request(http::Method::GET, "/other")).await;
    assert_eq!(resp.status(), 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_settings_closes_the_connection() {
    let server = start(TestSite::new(), None, limits(), None).await;
    // No control stream at all.
    let conn = quic_connect(&server, b"h3").await.unwrap();
    let started = Instant::now();
    assert_eq!(closed_code(&conn).await, 0x10a); // H3_MISSING_SETTINGS
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(500), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");

    // A control stream that sends only a partial SETTINGS header.
    let conn = quic_connect(&server, b"h3").await.unwrap();
    let mut control = conn.open_uni().await.unwrap();
    control.write_all(&[0x00, 0x04]).await.unwrap();
    let code = closed_code(&conn).await;
    assert!(code == 0x10a || code == 0x107, "{code:#x}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chatty_qpack_stream_closes_the_connection() {
    let server = start(TestSite::new(), None, limits(), None).await;
    let mut client = h3_client(&server).await;
    let (resp, _) = fetch(&mut client.send, request(http::Method::GET, "/")).await;
    assert_eq!(resp.status(), 200);
    // The h3 client already opened its encoder stream (type 0x02); a second
    // one is a connection error, so open a decoder-type stream on a fresh
    // raw connection instead.
    let conn = quic_connect(&server, b"h3").await.unwrap();
    let mut control = conn.open_uni().await.unwrap();
    let mut bytes = vec![0x00];
    bytes.extend(frame_header(0x04, 0));
    control.write_all(&bytes).await.unwrap();
    let mut decoder = conn.open_uni().await.unwrap();
    let mut bytes = vec![0x03];
    bytes.extend(std::iter::repeat_n(0u8, 200));
    decoder.write_all(&bytes).await.unwrap();
    assert_eq!(closed_code(&conn).await, 0x202); // QPACK_DECODER_STREAM_ERROR
                                                 // The other connection is unaffected.
    let (resp, _) = fetch(&mut client.send, request(http::Method::GET, "/after")).await;
    assert_eq!(resp.status(), 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_uni_stream_is_stopped() {
    let server = start(TestSite::new(), None, limits(), None).await;
    let mut client = h3_client(&server).await;
    let mut uni = client.conn.open_uni().await.unwrap();
    uni.write_all(&[0x21, 1, 2, 3]).await.unwrap(); // grease type 0x21
    let stopped = tokio::time::timeout(TIMEOUT, uni.stopped())
        .await
        .expect("the unknown stream was held open")
        .unwrap();
    assert_eq!(stopped.map(noq::VarInt::into_inner), Some(0x103));
    let (resp, _) = fetch(&mut client.send, request(http::Method::GET, "/")).await;
    assert_eq!(resp.status(), 200);
}
