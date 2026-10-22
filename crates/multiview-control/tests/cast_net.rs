//! The CASTV2 wire codec behind the off-by-default `cast` feature (DEV-D2,
//! ADR-M011): 4-byte big-endian length prefix + protobuf `CastMessage`
//! (schema from the BSD-3-Clause Chromium Open Screen sources), exercised
//! over an in-memory duplex — no socket, no device.
#![cfg(feature = "cast")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::time::Duration;

use multiview_control::devices::cast::net::{read_frame, write_frame, MAX_FRAME_LEN};
use multiview_control::devices::cast::protocol::{CastFrame, NS_HEARTBEAT};

#[tokio::test]
async fn frames_round_trip_over_the_wire_codec() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (mut read_half, _w) = tokio::io::split(server);
    let (_r, mut write_half) = tokio::io::split(client);

    let frame = CastFrame {
        namespace: NS_HEARTBEAT.to_owned(),
        source: "sender-0".to_owned(),
        destination: "receiver-0".to_owned(),
        payload: "{\"type\":\"PING\"}".to_owned(),
    };
    write_frame(&mut write_half, &frame)
        .await
        .expect("the frame writes");
    let decoded = read_frame(&mut read_half).await.expect("the frame reads");
    assert_eq!(decoded, frame);
}

#[tokio::test]
async fn oversized_frames_are_rejected_not_buffered() {
    // Bounded memory: a length prefix beyond MAX_FRAME_LEN is an error — the
    // reader never allocates an attacker-controlled buffer.
    use tokio::io::AsyncWriteExt;

    let (client, server) = tokio::io::duplex(1024);
    let (mut read_half, _w) = tokio::io::split(server);
    let (_r, mut write_half) = tokio::io::split(client);

    let huge = u32::try_from(MAX_FRAME_LEN)
        .expect("fits u32")
        .saturating_add(1);
    write_half
        .write_all(&huge.to_be_bytes())
        .await
        .expect("the bogus prefix writes");
    let err = read_frame(&mut read_half)
        .await
        .expect_err("an oversized frame is rejected");
    assert!(
        err.to_string().contains("frame"),
        "the error names the frame bound: {err}"
    );
}

#[tokio::test(start_paused = true)]
async fn write_frame_is_bounded_on_a_wedged_peer() {
    // A device that accepts the TLS session but stops draining its socket
    // must not wedge the actor's send path forever (the steady loop can only
    // service heartbeats and control commands between sends): the write is
    // bounded.
    let (client, server) = tokio::io::duplex(64);
    let (_r, mut write_half) = tokio::io::split(client);

    let frame = CastFrame {
        namespace: NS_HEARTBEAT.to_owned(),
        source: "sender-0".to_owned(),
        destination: "receiver-0".to_owned(),
        // Larger than the duplex buffer, so the write pends on the peer.
        payload: "x".repeat(4 * 1024),
    };
    // The outer guard makes a regression fail fast instead of hanging the
    // suite; the inner write_frame must resolve well before it.
    let result = tokio::time::timeout(
        Duration::from_secs(120),
        write_frame(&mut write_half, &frame),
    )
    .await
    .expect("write_frame resolves in bounded time on a wedged peer");
    let err = result.expect_err("a peer that never drains is a channel error, not a hang");
    assert!(
        err.to_string().contains("timed out"),
        "the error names the write timeout: {err}"
    );
    // The peer stays alive (wedged, not gone) for the whole write attempt.
    drop(server);
}

#[tokio::test]
async fn truncated_and_garbage_bodies_are_errors_not_hangs() {
    use tokio::io::AsyncWriteExt;

    // A length prefix promising more bytes than the peer ever sends (the
    // connection then drops mid-frame) is an error. The whole client stream
    // is dropped so the reader sees EOF mid-body.
    let (mut client, server) = tokio::io::duplex(1024);
    let (mut read_half, _w) = tokio::io::split(server);
    client
        .write_all(&10_u32.to_be_bytes())
        .await
        .expect("the prefix writes");
    client
        .write_all(&[0_u8; 4])
        .await
        .expect("the truncated body writes");
    drop(client);
    let err = read_frame(&mut read_half)
        .await
        .expect_err("a truncated frame body is an error");
    assert!(!err.to_string().is_empty());

    // A full-length body that is not a CastMessage protobuf is a decode
    // error — tolerated as a dead channel, never a panic.
    let (mut client, server) = tokio::io::duplex(1024);
    let (mut read_half, _w) = tokio::io::split(server);
    client
        .write_all(&5_u32.to_be_bytes())
        .await
        .expect("the prefix writes");
    client
        .write_all(&[0xFF_u8; 5])
        .await
        .expect("the garbage body writes");
    let err = read_frame(&mut read_half)
        .await
        .expect_err("a non-protobuf body is an error");
    assert!(!err.to_string().is_empty());
    drop(client);
}
