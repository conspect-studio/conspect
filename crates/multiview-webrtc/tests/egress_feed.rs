//! Failing-first, **offline** tests for the WebRTC program-output egress feed
//! ([`multiview_webrtc::transport::egress`]) — the bounded, drop-oldest seam the
//! `webrtc` (WHEP-serve) and `whip_push` outputs pull the **already-encoded**
//! program access units through (ADR-0049 §5, invariants #7 / #10).
//!
//! These prove the pure half (no socket, no str0m): the feed is bounded
//! drop-oldest (a stalled WHEP player / WHIP target can never grow memory or
//! back-pressure the encode-once fan-out), it preserves the keyframe flag + the
//! RTP timestamp + the media kind, and the same access units fan to two
//! independent feeds (the encode-once proof — one encode, two WebRTC consumers).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions
)]

use multiview_webrtc::egress::{egress_feed, EgressMedia, EgressSample, MAX_EGRESS};

fn video(ts: u32, keyframe: bool) -> EgressSample {
    EgressSample {
        media: EgressMedia::Video,
        rtp_timestamp: ts,
        keyframe,
        data: vec![0x00, 0x00, 0x00, 0x01, if keyframe { 0x65 } else { 0x61 }],
    }
}

fn audio(ts: u32) -> EgressSample {
    EgressSample {
        media: EgressMedia::Audio,
        rtp_timestamp: ts,
        keyframe: false,
        data: vec![0xFC, 0xAB],
    }
}

#[test]
fn feed_is_fifo_and_bounded_drop_oldest() {
    let (sink, feed) = egress_feed();
    // Push past the cap; the oldest are evicted, the newest survive in order.
    let count = u32::try_from(MAX_EGRESS).unwrap_or(u32::MAX) + 50;
    for i in 0..count {
        let _ = sink.push(video(i, i == 0));
    }
    assert!(feed.len() <= MAX_EGRESS, "feed must be bounded");
    assert_eq!(feed.dropped(), 50, "exactly the overflow was dropped");
    // Drop-oldest kept the newest tail: the first surviving sample is the 50th.
    let first = feed.pop().expect("a sample is buffered");
    assert_eq!(first.rtp_timestamp, 50, "drop-oldest kept the newest tail");
}

#[test]
fn feed_preserves_keyframe_kind_and_timestamp() {
    let (sink, feed) = egress_feed();
    let _ = sink.push(video(90_000, true));
    let _ = sink.push(audio(48_000));
    let v = feed.pop().expect("video sample");
    assert_eq!(v.media, EgressMedia::Video);
    assert!(v.keyframe, "the IDR flag survives the feed");
    assert_eq!(v.rtp_timestamp, 90_000);
    let a = feed.pop().expect("audio sample");
    assert_eq!(a.media, EgressMedia::Audio);
    assert!(!a.keyframe);
    assert_eq!(a.rtp_timestamp, 48_000);
    assert!(feed.pop().is_none(), "drained");
}

#[test]
fn the_same_access_units_fan_to_two_independent_feeds() {
    // Encode-once-mux-many (invariant #7): a `webrtc` and a `whip_push` output
    // each own a feed; the bake consumer fans the SAME encoded program AU to
    // both. Pushing one AU to two sinks yields the identical bytes in each — one
    // encode, N WebRTC consumers, packetization-only marginal cost.
    let (sink_a, feed_a) = egress_feed();
    let (sink_b, feed_b) = egress_feed();
    let au = video(90_000, true);
    let _ = sink_a.push(au.clone());
    let _ = sink_b.push(au.clone());
    let a = feed_a.pop().expect("feed a");
    let b = feed_b.pop().expect("feed b");
    assert_eq!(a.data, b.data, "both feeds carry the identical access unit");
    assert_eq!(a.data, au.data);
    assert_eq!(a.rtp_timestamp, b.rtp_timestamp);
    assert!(a.keyframe && b.keyframe);
}

#[test]
fn a_stalled_consumer_never_blocks_the_producer() {
    // Invariant #10: the producer (the bake consumer fanning packets) must never
    // block on a slow/stalled WHEP player or WHIP target. Even with a consumer
    // that NEVER pops, every push returns immediately and memory stays bounded.
    let (sink, feed) = egress_feed();
    for i in 0..10_000u32 {
        // Each push is wait-free; a real test would time this, but the bounded
        // length below is the load-bearing isolation evidence.
        let _ = sink.push(video(i, false));
    }
    assert!(
        feed.len() <= MAX_EGRESS,
        "a never-draining consumer cannot grow memory (drop-oldest)"
    );
    assert!(feed.dropped() > 0, "the stalled consumer lost its packets");
}

#[test]
fn close_then_drain_is_end_of_stream() {
    let (sink, feed) = egress_feed();
    let _ = sink.push(video(1, true));
    sink.close();
    assert!(!feed.is_ended(), "not ended while a sample remains");
    assert!(feed.pop().is_some());
    assert!(feed.is_ended(), "ended once closed and drained");
}
