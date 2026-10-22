//! ST 2110 receive-bridge **genuine drop-oldest** regression (ADR-0033 §7).
//!
//! The `channel_bridge` seam between the async NIC receive task and the sync
//! `St2110Producer` must, when its bounded queue is full, drop the **oldest**
//! queued packet and retain the **newest** — for live media the freshest data
//! is what matters, and a stalled reader must never back-pressure the receive
//! task (invariant #10). ADR-0033 §7 records that the original bridge dropped
//! the *newest* on a full channel (an mpsc `try_send` failure discarded the
//! just-arrived unit); this test pins the corrected behaviour.
//!
//! Gated behind `st2110` (the receive-transport feature that owns the bridge).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
#![cfg(feature = "st2110")]

use multiview_input::st2110::transport::{ChannelPacketSource, PacketSource, St2110Packet};

/// A minimal packet unit carrying its sequence number (the field the test keys
/// on); the payload is irrelevant to the bridge's queueing.
fn unit(seq: u16) -> St2110Packet {
    St2110Packet {
        marker: false,
        timestamp: u32::from(seq),
        sequence: seq,
        ssrc: 0,
        payload_type: 96,
        payload: Vec::new(),
    }
}

#[test]
fn channel_bridge_retains_newest_drops_oldest_on_overflow() {
    // A capacity-2 bounded ring, pushed past capacity with sequences 1..=4.
    let (sink, mut source) = ChannelPacketSource::bounded(2);
    for seq in 1..=4u16 {
        sink.push(unit(seq));
    }

    // Genuine drop-oldest: the ring holds the two NEWEST (seq 3, 4) in order;
    // the two oldest (seq 1, 2) were evicted.
    let first = source
        .poll_packet()
        .expect("poll never faults")
        .expect("ring holds the newest packet");
    let second = source
        .poll_packet()
        .expect("poll never faults")
        .expect("ring holds the second-newest packet");
    assert_eq!(
        (first.sequence, second.sequence),
        (3, 4),
        "a full bounded ring must retain the NEWEST packets and drop the oldest"
    );

    // Nothing more is buffered, and the drop is accounted for as telemetry.
    assert!(
        source.poll_packet().expect("poll never faults").is_none(),
        "only capacity-many packets survive"
    );
    assert_eq!(
        source.dropped(),
        2,
        "the two evicted oldest packets are counted as dropped"
    );
}
