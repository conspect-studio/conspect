// RT-8a held-out lip-sync test (the one that matters per the refuted CLAIM 5).
//
// The program bus must keep audio lip-synced to the output tick index even when
// video frames are dropped (DropOnOverload) and a mid-stream audio breakaway
// (`repoint`) swaps which warm store the program bus reads. Two properties are
// asserted:
//
//   (a) NO DRIFT: cumulative emitted samples after driving the bus to absolute
//       tick index T equals the SampleClock ideal `floor(T * rate * den / num)`,
//       regardless of how many ticks were SKIPPED in between (the catch-up that
//       inv #3 demands under DropOnOverload). A surviving-frame-paced `tick()`
//       would trail by exactly the dropped ticks' samples — this test forbids
//       that.
//
//   (b) SAMPLE-ALIGNED SEAM: a mid-stream `repoint` onto a warm store that has
//       been buffering (base far ahead, drop-oldest) must read from the LIVE
//       EDGE at the seam — no silence gap, no climb-from-zero through evicted
//       history. The first post-repoint block carries the new source's live
//       audio, not silence.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
// reason: test-only exact float comparisons on integer-valued ramps and
// loss-free index<->float casts on small bounded ranges; `doc_markdown` relaxed
// for the prose constant names in test-helper doc comments.
#![allow(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::float_cmp,
    clippy::doc_markdown
)]

use std::sync::Arc;

use multiview_audio::format::{AudioBlock, AudioFormat, ChannelLayout};
use multiview_audio::program::ProgramBus;
use multiview_audio::store::AudioStore;
use multiview_core::time::Rational;

const FS: u32 = 48_000;

fn stereo() -> AudioFormat {
    AudioFormat::new(FS, ChannelLayout::Stereo)
}

/// The SampleClock ideal cumulative sample count after `ticks` ticks at
/// `rate` Hz / `num`/`den` fps — `floor(ticks * rate * den / num)`.
fn ideal_total(ticks: u64, rate: u64, num: u64, den: u64) -> u64 {
    (ticks * rate * den) / num
}

/// Driving the bus by ABSOLUTE tick index keeps cumulative emitted samples equal
/// to the SampleClock ideal even when whole ranges of ticks are SKIPPED
/// (simulating DropOnOverload gaps where the consumer only sees surviving
/// frames). The bus must CATCH UP across each gap — no drift.
#[test]
fn tick_index_driven_bus_never_drifts_across_dropped_ticks() {
    let fmt = stereo();
    let (rate, num, den) = (48_000_u64, 30_000_u64, 1001_u64); // NTSC, fractional
    let mut bus = ProgramBus::new(fmt, Rational::new(30_000, 1001));

    // A single steady source so the emitted block lengths are exercised.
    let store = Arc::new(AudioStore::new(fmt, 96_000));
    store.publish(&AudioBlock::silence(fmt, 96_000)).unwrap();
    bus.add_source("a", Arc::clone(&store), 1.0);

    // A tick sequence with GAPS: we only emit at these absolute tick indices,
    // skipping the ticks in between (as if every other / several frames were
    // dropped under overload). The cumulative emitted samples must still equal
    // the ideal for the LAST tick index reached.
    let ticks: [u64; 8] = [1, 2, 5, 9, 10, 50, 137, 500];
    let mut cumulative = 0u64;
    for &t in &ticks {
        let block = bus.tick_to(t);
        cumulative += u64::try_from(block.frame_count()).unwrap();
        let ideal = ideal_total(t, rate, num, den);
        assert_eq!(
            cumulative, ideal,
            "cumulative emitted samples must equal the SampleClock ideal at tick {t} \
             (no drift across the skipped ticks)"
        );
    }
}

/// `seek_to_live_edge` lands the read cursor at the live edge of a warm store —
/// the next read returns the freshly-published tail, NOT silence and NOT a climb
/// from frame 0 through evicted history.
#[test]
fn seek_to_live_edge_lands_at_the_live_edge_not_frame_zero() {
    let store = AudioStore::new(stereo(), 1_000);
    // Publish far more than capacity so the live window is way ahead of frame 0
    // and the head is well past where a fresh cursor (frame 0) would read.
    // 5000 frames in chunks of 250; only the last 1000 survive.
    let mut written = 0usize;
    let channels = 2;
    while written < 5_000 {
        let mut s = Vec::with_capacity(250 * channels);
        for i in 0..(250 * channels) {
            s.push((written * channels + i) as f32);
        }
        store
            .publish(&AudioBlock::from_interleaved(stereo(), s).unwrap())
            .unwrap();
        written += 250;
    }

    // A fresh cursor at frame 0 would read silence (evicted) climbing toward the
    // surviving tail. Seek to the live edge instead.
    store.seek_to_live_edge();

    // The next read begins at the head (frame 5000): nothing is published THERE
    // yet, so it is silence — but the cursor is at the live edge, not frame 0.
    assert_eq!(
        store.read_cursor(),
        5_000,
        "seek_to_live_edge must put the cursor at the write head (live edge)"
    );

    // Now the producer publishes the next live frames; the very first read after
    // the seam returns them with no silence climb from frame 0.
    let mut s = Vec::with_capacity(10 * channels);
    for i in 0..(10 * channels) {
        s.push((5_000 * channels + i) as f32);
    }
    store
        .publish(&AudioBlock::from_interleaved(stereo(), s).unwrap())
        .unwrap();
    let out = store.read(10);
    assert_eq!(out.frame_count(), 10);
    for (i, &v) in out.interleaved().iter().enumerate() {
        assert_eq!(
            v,
            (5_000 * channels + i) as f32,
            "post-seek read must be the live edge, not evicted history / silence"
        );
    }
}

/// The whole RT-8a contract end to end: under a SKIPPED-tick sequence (overload
/// gaps) with a mid-stream `repoint` to a warm store, the program bus stays
/// drift-free AND the breakaway is sample-aligned at the seam — the new source's
/// live audio appears immediately, no silence gap, no climb-from-zero.
#[test]
fn breakaway_under_overload_is_drift_free_and_sample_aligned() {
    let fmt = stereo();
    let (rate, num, den) = (48_000_u64, 30_000_u64, 1001_u64);
    let mut bus = ProgramBus::new(fmt, Rational::new(30_000, 1001));

    // Source A: a constant +0.25 tone, plenty of headroom.
    let store_a = Arc::new(AudioStore::new(fmt, 192_000));
    store_a
        .publish(&AudioBlock::from_interleaved(fmt, vec![0.25f32; 192_000 * 2]).unwrap())
        .unwrap();
    let point = bus.add_source("prog", Arc::clone(&store_a), 1.0);

    // Source B: a warm breakaway target carrying a constant +0.75 tone. It has
    // been buffering for a long time (drop-oldest), so its write head is far
    // ahead of frame 0 — a fresh frame-0 cursor would read evicted silence
    // climbing up. Capacity comfortably exceeds the largest single catch-up read
    // below, so the live tail is always within the surviving window.
    let store_b = Arc::new(AudioStore::new(fmt, 48_000));
    let mut written = 0usize;
    while written < 40_000 {
        store_b
            .publish(&AudioBlock::from_interleaved(fmt, vec![0.75f32; 2_000 * 2]).unwrap())
            .unwrap();
        written += 2_000;
    }

    // Drive a gappy tick sequence (DropOnOverload drops several ticks at a time).
    // Confirm A is on the bus and there is no drift up to the breakaway tick.
    let mut cumulative = 0u64;
    for &t in &[1u64, 2, 4, 7, 8, 12, 20] {
        let block = bus.tick_to(t);
        cumulative += u64::try_from(block.frame_count()).unwrap();
        assert_eq!(
            cumulative,
            ideal_total(t, rate, num, den),
            "pre-seam drift at {t}"
        );
        assert!(
            block.interleaved().iter().all(|&s| (s - 0.25).abs() < 1e-6),
            "source A (+0.25) must be on the bus before the breakaway"
        );
    }

    // BREAKAWAY at tick 20: repoint the SAME route point onto the warm store B.
    // This seeks B to its live edge so the seam reads B's live tone, not silence
    // / evicted history.
    bus.repoint(point, Arc::clone(&store_b)).unwrap();

    // Past the seam: B is a live source, so before each (catch-up) tick it
    // publishes exactly the samples that tick will consume — keeping its live
    // edge ahead of the cursor exactly as a real-time decoder would. The tick
    // sequence keeps skipping ticks (overload). Both properties must hold every
    // tick.
    let mut prev = 20u64;
    for &t in &[21u64, 22, 25, 29, 30, 34, 42, 60] {
        let catch_up =
            usize::try_from(ideal_total(t, rate, num, den) - ideal_total(prev, rate, num, den))
                .unwrap();
        store_b
            .publish(&AudioBlock::from_interleaved(fmt, vec![0.75f32; catch_up * 2]).unwrap())
            .unwrap();
        let block = bus.tick_to(t);
        cumulative += u64::try_from(block.frame_count()).unwrap();
        // (a) NO DRIFT across the seam + skipped ticks: cumulative emitted
        // samples still equal the SampleClock ideal for the tick count.
        assert_eq!(
            cumulative,
            ideal_total(t, rate, num, den),
            "post-seam cumulative must still equal the ideal at tick {t} (no drift)"
        );
        // (b) SAMPLE-ALIGNED SEAM: the breakaway source B (+0.75) is on the bus
        // immediately — no silence gap, no climb from zero through evicted
        // history.
        assert!(
            block.interleaved().iter().all(|&s| (s - 0.75).abs() < 1e-6),
            "breakaway must be sample-aligned at the live edge: expected the +0.75 \
             source B, got a silence gap / stale climb at tick {t}"
        );
        prev = t;
    }
}
