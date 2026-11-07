// ProgramBus (AUD-3): the per-output-tick program-bus mix driven by the
// SampleClock. Each tick pulls exactly `samples_per_tick` frames from every
// routed source's last-good store and mixes them, so the program audio rides the
// output clock with an exact, gap-free sample budget — the audio analogue of
// "exactly N frames for N ticks". A dropped/stalled source contributes silence
// (the store is silence-filling), never a gap, never an absent block.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::Arc;

use multiview_audio::format::{AudioBlock, AudioFormat, ChannelLayout};
use multiview_audio::program::ProgramBus;
use multiview_audio::store::AudioStore;
use multiview_core::time::Rational;

fn stereo48() -> AudioFormat {
    AudioFormat::new(48_000, ChannelLayout::Stereo)
}

#[test]
fn no_sources_emits_exact_gap_free_silence_budget() {
    // With no audio sources the program bus is still continuous: exactly
    // samples_per_tick frames of silence each tick, summing to the exact NTSC
    // long-run total (2000 ticks at 30000/1001 = 2000 * 1601.6 = 3_203_200).
    let fmt = stereo48();
    let mut bus = ProgramBus::new(fmt, Rational::new(30_000, 1001));
    let mut total = 0usize;
    for _ in 0..2000 {
        let b = bus.tick();
        assert_eq!(b.format(), fmt);
        assert!(
            b.frame_count() == 1601 || b.frame_count() == 1602,
            "each tick is the NTSC per-tick budget, got {}",
            b.frame_count()
        );
        assert!(
            b.interleaved().iter().all(|&s| s == 0.0),
            "no source => gap-free silence"
        );
        total += b.frame_count();
    }
    assert_eq!(total, 3_203_200);
}

#[test]
fn a_routed_source_is_mixed_into_the_program_bus() {
    let fmt = stereo48();
    let store = Arc::new(AudioStore::new(fmt, 48_000));
    // Publish 4800 frames of a constant 0.5 (stereo -> 9600 interleaved samples).
    let block = AudioBlock::from_interleaved(fmt, vec![0.5f32; 4800 * 2]).unwrap();
    store.publish(&block).unwrap();

    let mut bus = ProgramBus::new(fmt, Rational::new(25, 1)); // 1920 samples/tick
    bus.add_source("in_a", Arc::clone(&store), 1.0);
    let b = bus.tick();
    assert_eq!(b.frame_count(), 1920);
    assert!(
        b.interleaved().iter().all(|&s| (s - 0.5).abs() < 1e-6),
        "the routed unity-gain 0.5 source must reach the program bus"
    );
}

#[test]
fn synthetic_tone_ignores_paused_producer_and_keeps_fractional_phase() {
    let fmt = stereo48();
    let tone = Arc::new(AudioStore::line_up_tone(fmt));
    let mut bus = ProgramBus::new(fmt, Rational::new(30_000, 1001));
    let point = bus.add_source("bars", Arc::clone(&tone), 1.0);
    // The obsolete producer could be paused forever. No write is made here.
    for tick in [0_u64, 1, 2, 13, 29, 300] {
        let start = tone.read_cursor();
        let rendered = bus.tick_to(tick + 1);
        let mut reference = multiview_audio::ToneGenerator::line_up(fmt);
        reference.seek_to_frame(u64::try_from(start).unwrap());
        let expected = reference.next_block(rendered.frame_count()).unwrap();
        assert_eq!(
            rendered.interleaved(),
            expected.interleaved(),
            "sample phase at tick {tick}"
        );
        assert!(rendered
            .interleaved()
            .iter()
            .any(|sample| sample.abs() > 0.01));
        assert_eq!(tone.read_cursor(), bus.position_at(tick + 1));
    }
    bus.set_program_gain_ramped(point, 0.0, 0).unwrap();
    let muted = bus.tick();
    assert!(muted.interleaved().iter().all(|sample| *sample == 0.0));
    assert!(
        bus.input_tracks()[0]
            .1
            .interleaved()
            .iter()
            .any(|sample| sample.abs() > 0.01),
        "pre-route meter stays audible"
    );
    bus.set_program_gain_ramped(point, 1.0, 0).unwrap();
    assert!(bus
        .tick()
        .interleaved()
        .iter()
        .any(|sample| sample.abs() > 0.01));
    assert!(bus.remove_source(point));
    assert!(bus.tick().interleaved().iter().all(|sample| *sample == 0.0));
}

#[test]
fn a_dropped_source_yields_silence_never_absent() {
    // A source whose decode stalled (an empty store, nothing ever published)
    // still yields an exact-budget block of silence every tick — never absent.
    let fmt = stereo48();
    let store = Arc::new(AudioStore::new(fmt, 48_000));
    let mut bus = ProgramBus::new(fmt, Rational::new(30, 1)); // 1600 samples/tick
    bus.add_source("dead", store, 1.0);
    for _ in 0..50 {
        let b = bus.tick();
        assert_eq!(b.frame_count(), 1600);
        assert!(b.interleaved().iter().all(|&s| s == 0.0));
    }
}

#[test]
fn repoint_swaps_an_existing_points_store_replace_not_append() {
    // `repoint` REPLACES the Arc<AudioStore> bound to an existing route point —
    // distinct from `add_source`'s append. After a repoint, the same route point
    // reads the new store, and the bus still has exactly ONE source on it (the
    // old store is no longer mixed in — replace, not append).
    let fmt = stereo48();
    let store_a = Arc::new(AudioStore::new(fmt, 48_000));
    store_a
        .publish(&AudioBlock::from_interleaved(fmt, vec![0.25f32; 4800 * 2]).unwrap())
        .unwrap();
    let store_b = Arc::new(AudioStore::new(fmt, 48_000));
    store_b
        .publish(&AudioBlock::from_interleaved(fmt, vec![0.75f32; 4800 * 2]).unwrap())
        .unwrap();

    let mut bus = ProgramBus::new(fmt, Rational::new(25, 1)); // 1920 samples/tick
    let point = bus.add_source("in", Arc::clone(&store_a), 1.0);

    let before = bus.tick();
    assert!(
        before
            .interleaved()
            .iter()
            .all(|&s| (s - 0.25).abs() < 1e-6),
        "store A (+0.25) must be on the bus before the repoint"
    );

    // Record store A's read cursor before the repoint. The first tick advanced
    // it by one tick's budget (1920); after the repoint A is DETACHED and must
    // never be read again (replace, not append-leaving-a-duplicate).
    let a_cursor_before = store_a.read_cursor();
    assert_eq!(a_cursor_before, 1920, "the first tick read A once");

    // Repoint the SAME point onto store B. This seeks B to its live edge
    // (frame 4800, its current head); a live source then keeps publishing past
    // the edge, so the next read returns B's fresh audio (replace), never A.
    bus.repoint(point, Arc::clone(&store_b)).unwrap();
    store_b
        .publish(&AudioBlock::from_interleaved(fmt, vec![0.75f32; 1920 * 2]).unwrap())
        .unwrap();

    let after = bus.tick();
    assert!(
        after.interleaved().iter().all(|&s| (s - 0.75).abs() < 1e-6),
        "after repoint the SAME point must read store B (+0.75), replacing A"
    );
    // Replace, not append: the detached store A must NOT be pulled on the
    // post-repoint tick — its cursor stays put. An append that left A in the
    // route list would advance A's cursor again here.
    assert_eq!(
        store_a.read_cursor(),
        a_cursor_before,
        "after repoint, the replaced store A must be detached (never read again)"
    );
}

#[test]
fn repoint_of_a_nonexistent_point_is_a_clean_error_no_panic() {
    // A repoint of a route point the bus never registered must be an honest
    // error / no-op — never a panic, never a silent wrong-source bind.
    let fmt = stereo48();
    let mut bus = ProgramBus::new(fmt, Rational::new(25, 1));
    let store = Arc::new(AudioStore::new(fmt, 48_000));
    // RoutePoint::input(7) was never added.
    let bogus = multiview_audio::RoutePoint::input(7);
    let err = bus.repoint(bogus, store).unwrap_err();
    assert!(
        matches!(err, multiview_audio::AudioError::UnknownInput(7)),
        "repointing an unknown point must be an UnknownInput error, got {err:?}"
    );
}

#[test]
fn input_tracks_expose_each_routed_sources_discrete_block_after_a_tick() {
    let fmt = AudioFormat::new(48_000, ChannelLayout::Stereo);
    let mut bus = ProgramBus::new(fmt, Rational::new(25, 1));
    assert!(
        bus.input_tracks().is_empty(),
        "nothing before the first mix"
    );
    let loud = Arc::new(AudioStore::new(fmt, 48_000));
    let quiet = Arc::new(AudioStore::new(fmt, 48_000));
    loud.publish(&AudioBlock::from_interleaved(fmt, vec![0.5; 2 * 4800]).unwrap())
        .unwrap();
    quiet
        .publish(&AudioBlock::from_interleaved(fmt, vec![0.1; 2 * 4800]).unwrap())
        .unwrap();
    bus.add_source("loud", loud, 1.0);
    bus.add_source("quiet", quiet, 1.0);
    let program = bus.tick();
    let tracks = bus.input_tracks();
    assert_eq!(tracks.len(), 2);
    assert_eq!(tracks[0].0, "loud");
    assert_eq!(tracks[1].0, "quiet");
    assert_eq!(tracks[0].1.frame_count(), program.frame_count());
    assert!(
        (tracks[0].1.interleaved()[0] - 0.5).abs() < 1e-6,
        "pre-mix, unaltered"
    );
    assert!((tracks[1].1.interleaved()[0] - 0.1).abs() < 1e-6);
}
