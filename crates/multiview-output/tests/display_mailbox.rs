//! Display-sink mailbox tests (DEV-B1 / ADR-0044, invariants #1 + #10): the
//! wait-free latest-frame mailbox between the engine's canvas publish and the
//! display sink thread. The writer must NEVER block — regardless of whether the
//! reader drains — and the reader always observes the newest published frame.
//! Pure Rust, no hardware.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::Arc;
use std::thread;

use multiview_output::display::frame_mailbox;

/// A tiny stand-in payload (the production payload is the NV12 canvas).
#[derive(Debug, PartialEq, Eq)]
struct Payload(u64);

#[test]
fn empty_mailbox_reads_none() {
    let (_publisher, reader) = frame_mailbox::<Payload>();
    assert!(reader.latest().is_none());
}

#[test]
fn reader_sees_the_latest_published_frame() {
    let (publisher, reader) = frame_mailbox::<Payload>();
    publisher.publish(Payload(1));
    publisher.publish(Payload(2));
    publisher.publish(Payload(3));
    let (frame, seq) = reader.latest().expect("a frame is present");
    assert_eq!(*frame, Payload(3));
    assert_eq!(seq, 3, "three publishes => sequence 3");
}

#[test]
fn sequence_is_strictly_increasing_and_stamped_on_the_frame() {
    let (publisher, reader) = frame_mailbox::<Payload>();
    let s1 = publisher.publish(Payload(10));
    let s2 = publisher.publish(Payload(20));
    assert!(s2 > s1, "publish sequence must strictly increase");
    let (frame, seq) = reader.latest().expect("frame");
    // The sequence travels WITH the frame (stamped atomically inside the slot
    // value), so a reader can never observe frame N paired with sequence N+1.
    assert_eq!(seq, s2);
    assert_eq!(*frame, Payload(20));
}

#[test]
fn writer_never_blocks_when_nothing_drains() {
    // Conflation under a wedged consumer: publish a large burst with NO reads.
    // A blocking/bounded channel would deadlock or error here; the mailbox must
    // complete every publish (overwrite, newest wins) on this same thread.
    let (publisher, reader) = frame_mailbox::<Payload>();
    let writer = thread::spawn(move || {
        for i in 0..100_000u64 {
            publisher.publish(Payload(i));
        }
        publisher
    });
    let publisher = writer
        .join()
        .expect("writer thread completed (never blocked)");
    let (frame, seq) = reader.latest().expect("latest survives the burst");
    assert_eq!(*frame, Payload(99_999), "reader sees the newest frame");
    assert_eq!(seq, 100_000);
    drop(publisher);
}

#[test]
fn concurrent_reader_never_observes_older_than_seen() {
    // Newest-wins under concurrency: the sequence a reader observes never goes
    // backwards, and the payload always matches its stamped sequence.
    let (publisher, reader) = frame_mailbox::<Payload>();
    let publisher = Arc::new(publisher);
    let writer = {
        let publisher = Arc::clone(&publisher);
        thread::spawn(move || {
            for i in 1..=50_000u64 {
                publisher.publish(Payload(i));
            }
        })
    };
    let mut last_seen = 0u64;
    while last_seen < 50_000 {
        if let Some((frame, seq)) = reader.latest() {
            assert!(seq >= last_seen, "sequence regressed: {seq} < {last_seen}");
            assert_eq!(frame.0, seq, "payload must match its stamped sequence");
            last_seen = seq;
        }
    }
    writer.join().expect("writer completed");
}
