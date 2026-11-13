//! Oversized header blocks (acceptance): the declared-length guard and the
//! decoded field-section limit, on a real connection.

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn declared_huge_headers_frame_is_rejected_before_buffering() {
    let server = start(TestSite::new(), None, H3Limits::default(), None).await;
    let mut client = h3_client(&server).await;
    // h3 settings exchanged: a GET works.
    let (resp, _) = fetch(&mut client.send, request(http::Method::GET, "/")).await;
    assert_eq!(resp.status(), 200);

    let (mut send, mut recv) = client.conn.open_bi().await.unwrap();
    let mut frame = Vec::new();
    varint::encode(0x01, &mut frame); // HEADERS
    varint::encode(1 << 20, &mut frame); // declares 1 MiB
    frame.extend(std::iter::repeat_n(0u8, 4096)); // sends only 4 KiB
    let started = Instant::now();
    send.write_all(&frame).await.unwrap();
    // The server stops our sending side (STOP_SENDING H3_EXCESSIVE_LOAD)
    // although we never finished: it did not wait for the declared 1 MiB.
    let stopped = tokio::time::timeout(TIMEOUT, send.stopped())
        .await
        .expect("the server never stopped the stream")
        .unwrap();
    assert_eq!(stopped.map(noq::VarInt::into_inner), Some(0x107));
    assert!(started.elapsed() < Duration::from_secs(5));
    let read = tokio::time::timeout(TIMEOUT, recv.read_to_end(1024))
        .await
        .unwrap();
    assert!(
        matches!(read, Err(noq::ReadToEndError::Read(noq::ReadError::Reset(code))) if code.into_inner() == 0x107),
        "{read:?}"
    );
    assert_eq!(
        Counters::get(&server.endpoint.counters().header_guard_rejections),
        1
    );
    // The connection is still usable.
    let (resp, _) = fetch(&mut client.send, request(http::Method::GET, "/after")).await;
    assert_eq!(resp.status(), 200);
}

/// A QPACK block (static table only) for `GET https://localhost/` plus
/// `extra` empty fields named "a" (33 bytes of field-section size each).
fn header_block(extra: usize) -> Vec<u8> {
    let mut block = vec![0x00, 0x00]; // required insert count 0, base 0
    block.push(0xC0 | 0x11); // :method GET
    block.push(0xC0 | 0x17); // :scheme https
    block.push(0xC0 | 0x01); // :path /
    block.push(0x50); // :authority with literal value
    block.push(9);
    block.extend_from_slice(b"localhost");
    for _ in 0..extra {
        block.extend_from_slice(&[0x21, b'a', 0x00]); // literal name "a", empty value
    }
    block
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decoded_field_section_over_the_cap_is_refused() {
    let server = start(TestSite::new(), None, H3Limits::default(), None).await;
    let mut client = h3_client(&server).await;
    let (resp, _) = fetch(&mut client.send, request(http::Method::GET, "/")).await;
    assert_eq!(resp.status(), 200);

    // 600 fields * 33 = 19 800 > 16 384, in ~1.8 KB on the wire (below the
    // declared-length guard, so QPACK's field-section check is what fires).
    let block = header_block(600);
    assert!(block.len() < 4096);
    let (mut send, mut recv) = client.conn.open_bi().await.unwrap();
    let mut frame = Vec::new();
    varint::encode(0x01, &mut frame);
    varint::encode(u64::try_from(block.len()).unwrap(), &mut frame);
    frame.extend_from_slice(&block);
    send.write_all(&frame).await.unwrap();
    send.finish().unwrap();
    let read = tokio::time::timeout(TIMEOUT, recv.read_to_end(64 * 1024))
        .await
        .unwrap();
    match read {
        // 431 response: a HEADERS frame back, never a 200 from the site.
        Ok(bytes) => {
            let (ty, _) = varint::decode(&bytes).unwrap();
            assert_eq!(ty, 0x01, "not a HEADERS frame: {bytes:?}");
            assert!(
                !bytes.windows(5).any(|w| w == b"hello"),
                "the site answered"
            );
        }
        // Or a stream error.
        Err(noq::ReadToEndError::Read(noq::ReadError::Reset(_))) => {}
        Err(other) => panic!("unexpected: {other:?}"),
    }
    assert_eq!(Counters::get(&server.endpoint.counters().requests), 1);
    // The connection stays usable.
    let (resp, _) = fetch(&mut client.send, request(http::Method::GET, "/after")).await;
    assert_eq!(resp.status(), 200);

    // Control: the same block shape under the cap is served.
    let block = header_block(10);
    let (mut send, mut recv) = client.conn.open_bi().await.unwrap();
    let mut frame = Vec::new();
    varint::encode(0x01, &mut frame);
    varint::encode(u64::try_from(block.len()).unwrap(), &mut frame);
    frame.extend_from_slice(&block);
    send.write_all(&frame).await.unwrap();
    send.finish().unwrap();
    let bytes = tokio::time::timeout(TIMEOUT, recv.read_to_end(64 * 1024))
        .await
        .unwrap()
        .unwrap();
    assert!(
        bytes.windows(7).any(|w| w == b"hello /"),
        "control request not served"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn header_deadline_rejects_a_stalled_request() {
    let limits = H3Limits {
        header_deadline: Duration::from_millis(300),
        ..H3Limits::default()
    };
    let server = start(TestSite::new(), None, limits, None).await;
    let mut client = h3_client(&server).await;
    let (resp, _) = fetch(&mut client.send, request(http::Method::GET, "/")).await;
    assert_eq!(resp.status(), 200);
    let (mut send, _recv) = client.conn.open_bi().await.unwrap();
    let mut frame = Vec::new();
    varint::encode(0x01, &mut frame);
    varint::encode(100, &mut frame);
    frame.extend_from_slice(&[0u8; 10]); // and then nothing
    send.write_all(&frame).await.unwrap();
    let stopped = tokio::time::timeout(TIMEOUT, send.stopped())
        .await
        .expect("the stalled request was never rejected")
        .unwrap();
    assert_eq!(stopped.map(noq::VarInt::into_inner), Some(0x10b));
    assert_eq!(
        Counters::get(&server.endpoint.counters().header_deadline_rejections),
        1
    );
}
