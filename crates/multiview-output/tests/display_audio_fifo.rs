//! Bounded drop-oldest audio FIFO tests (DEV-B4 / invariants #1 + #10).
//!
//! The audio sink is a *consumer*: the engine (the audio program bus) pushes
//! blocks into this FIFO and **must never block** on a slow/wedged ALSA reader.
//! The FIFO is bounded and drops the OLDEST samples when full — it never grows
//! and the writer never waits. These tests prove that property without any
//! hardware: a writer that vastly out-paces the reader stays bounded, never
//! blocks, and the drop count is observable telemetry.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_output::display::audio::AudioFifo;

#[test]
fn fill_is_bounded_and_writer_never_blocks() {
    // Capacity is in frames (per channel). Push far more than capacity; the
    // FIFO must clamp to capacity and never panic/grow.
    let cap = 4_096usize;
    let mut fifo = AudioFifo::new(cap, 2);
    for _ in 0..100 {
        // each push is a 1024-frame stereo block => 100*1024 frames offered
        fifo.push(&vec![0.25f32; 1024 * 2]);
    }
    assert!(
        fifo.fill_frames() <= cap,
        "FIFO fill must never exceed capacity (got {} > {cap})",
        fifo.fill_frames()
    );
    assert!(fifo.dropped_frames() > 0, "over-fill must record drops");
}

#[test]
fn drops_oldest_keeps_newest() {
    // After overflow the FIFO holds the most-recent samples (drop-oldest), so a
    // late-joining reader gets fresh audio, not stale backlog.
    let mut fifo = AudioFifo::new(4, 1); // 4 frames, mono
    fifo.push(&[1.0, 2.0, 3.0, 4.0]); // fills exactly
    fifo.push(&[5.0, 6.0]); // overflow by 2 => oldest (1,2) dropped
    let mut out = vec![0.0f32; 4];
    let n = fifo.pop_into(&mut out);
    assert_eq!(n, 4);
    assert_eq!(out, [3.0, 4.0, 5.0, 6.0], "newest 4 frames survive");
}

#[test]
fn underrun_pop_zero_fills_and_reports_short() {
    // A reader asking for more than is buffered gets what's there + the rest
    // zero-filled (silence), and the short count is reported — the reader never
    // blocks waiting for the writer (the dual of the no-block-on-write rule).
    let mut fifo = AudioFifo::new(8, 1);
    fifo.push(&[1.0, 2.0]);
    let mut out = vec![9.9f32; 5];
    let got = fifo.pop_into(&mut out);
    assert_eq!(got, 2, "only 2 real frames were available");
    assert_eq!(out, [1.0, 2.0, 0.0, 0.0, 0.0], "remainder is silence");
}

#[test]
fn fill_fraction_is_reported_for_the_servo() {
    // The servo reads fill as a fraction of capacity to compute its correction;
    // an empty FIFO is 0.0, exactly full is 1.0.
    let mut fifo = AudioFifo::new(100, 2);
    assert!((fifo.fill_fraction() - 0.0).abs() < 1e-9);
    fifo.push(&vec![0.0f32; 50 * 2]);
    assert!(
        (fifo.fill_fraction() - 0.5).abs() < 1e-9,
        "half-full FIFO => 0.5, got {}",
        fifo.fill_fraction()
    );
}
