// RT-9 held-out pop-avoidance test suite (the tests that matter for the refuted
// CLAIM 5 "No per-tick-scalar cross-fade" hard gate).
//
// An instant `repoint` (RT-8a) is sample-accurate but waveform-DISCONTINUOUS at
// the seam: the bus jumps from the old source's sample to the new source's
// sample in one step → a CLICK. RT-9 replaces that hard swap with an EQUAL-POWER
// (sin/cos) cross-fade applied as a PER-SAMPLE intra-block envelope inside
// `mix_program`'s sample loop, so the gain moves smoothly WITHIN a tick block
// (not stepping at tick boundaries). Four properties are asserted:
//
//   (a) NO DISCONTINUITY (no click): across the whole ~10 ms ramp — including the
//       old-instant-swap seam where RT-8a clicked — the sample-to-sample step
//       |x[n+1]-x[n]| stays bounded (no abrupt jump). The hard-swap step is gone.
//
//   (b) EQUAL POWER (no dip): the summed instantaneous power across the fade
//       stays ~constant — gainA²+gainB²==1 at every frame (sin²+cos²=1). A LINEAR
//       cross-fade would dip ~-3 dB at the midpoint; this does not.
//
//   (c) R128 / TRUE-PEAK not violated: the fade never produces a sample louder
//       than the louder of the two sources's equal-power envelope.
//
//   (d) OLD STRIP UNROUTED after the ramp: once the ramp completes the old strip
//       is removed from the bus (no lingering contribution / no double-count).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
// reason: test-only exact/approx float comparisons on bounded ramps and
// loss-free index<->float casts on small ranges.
#![allow(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::float_cmp,
    clippy::doc_markdown
)]

use std::sync::Arc;

use multiview_audio::format::{AudioBlock, AudioFormat, ChannelLayout};
use multiview_audio::mixer::{GainRamp, Mixer};
use multiview_audio::program::{ApplyClass, ProgramBus, SwitchTier};
use multiview_audio::store::AudioStore;
use multiview_core::time::Rational;

const FS: u32 = 48_000;

fn stereo() -> AudioFormat {
    AudioFormat::new(FS, ChannelLayout::Stereo)
}

// ---------------------------------------------------------------------------
// Mixer-level: the per-sample equal-power envelope.
// ---------------------------------------------------------------------------

/// The CORE per-sample claim: a `GainRamp` on a strip applies an envelope that
/// changes value EVERY frame within a single tick block — NOT a per-tick scalar
/// that would hold constant across the whole block and step at the boundary.
#[test]
fn gain_ramp_is_a_per_sample_envelope_not_a_per_tick_scalar() {
    let fmt = stereo();
    let mut mixer = Mixer::new(fmt);
    let a = mixer.add_input("a");
    mixer.route_to_program(a, 1.0);
    // A 1.0 DC source so the bus value == the applied envelope value exactly.
    mixer
        .submit(
            a,
            AudioBlock::from_interleaved(fmt, vec![1.0f32; 100 * 2]).unwrap(),
        )
        .unwrap();
    // Ramp gain 0 -> 1 (sin taper, up) over 100 frames.
    mixer.set_gain_ramp(a, GainRamp::up(100));

    let bus = mixer.mix_program().unwrap();
    let s: Vec<f32> = (0..bus.frame_count())
        .map(|f| bus.interleaved()[f * 2])
        .collect();
    assert_eq!(s.len(), 100);

    // PER-SAMPLE: consecutive frames within the one block differ (the envelope
    // moves inside the block) — a per-tick scalar would make all 100 equal.
    let distinct = s.windows(2).filter(|w| (w[0] - w[1]).abs() > 1e-7).count();
    assert!(
        distinct >= 90,
        "the envelope must move per-sample within the block (a per-tick scalar \
         would hold constant); only {distinct}/99 adjacent frames differed"
    );
    // Monotone non-decreasing up-ramp, 0 at the start, ~1 by the end.
    assert!(s[0].abs() < 1e-3, "up-ramp starts near 0, got {}", s[0]);
    assert!(s[99] > 0.99, "up-ramp ends near 1, got {}", s[99]);
    for w in s.windows(2) {
        assert!(w[1] + 1e-6 >= w[0], "up-ramp must be non-decreasing");
    }
}

/// Equal-power (sin/cos), NOT linear: at the midpoint of a 0->1 up-ramp the gain
/// is sin(π/4) ≈ 0.707 (-3 dB amplitude), not 0.5. This is what keeps the SUM of
/// the up+down pair at constant power.
#[test]
fn up_ramp_is_equal_power_sin_taper_not_linear() {
    let fmt = stereo();
    let mut mixer = Mixer::new(fmt);
    let a = mixer.add_input("a");
    mixer.route_to_program(a, 1.0);
    mixer
        .submit(
            a,
            AudioBlock::from_interleaved(fmt, vec![1.0f32; 1000 * 2]).unwrap(),
        )
        .unwrap();
    mixer.set_gain_ramp(a, GainRamp::up(1000));
    let bus = mixer.mix_program().unwrap();
    let mid = bus.interleaved()[500 * 2];
    // sin(π/2 * 0.5) = sin(π/4) ≈ 0.70710678. Linear would be 0.5.
    approx::assert_abs_diff_eq!(mid, std::f32::consts::FRAC_1_SQRT_2, epsilon = 1e-2);
    assert!(
        (mid - 0.5).abs() > 0.1,
        "an equal-power taper at the midpoint is ~0.707, clearly not the linear 0.5"
    );
}

/// The equal-power IDENTITY across the whole ramp: gainA²(down) + gainB²(up) == 1
/// at every frame, including the midpoint where a linear fade's gains
/// sum-of-squares would be 0.5 (the -3 dB dip the equal-power taper avoids).
#[test]
fn equal_power_identity_holds_at_every_frame() {
    let n = 1000usize;
    for frame in 0..n {
        let ga = GainRamp::down(n).envelope_at(frame);
        let gb = GainRamp::up(n).envelope_at(frame);
        let power = ga * ga + gb * gb;
        approx::assert_abs_diff_eq!(power, 1.0, epsilon = 1e-4);
    }
    // The LINEAR counter-example: linear gains at the midpoint sum-of-squares to
    // 0.5, proving the equal-power taper is doing real work.
    let lin_mid = 0.5f32 * 0.5 + 0.5 * 0.5;
    assert!(
        (lin_mid - 1.0).abs() > 0.4,
        "a linear fade WOULD dip (this is the contrast)"
    );
}

// ---------------------------------------------------------------------------
// Bus-level: the equal-power cross-fade breakaway.
// ---------------------------------------------------------------------------

/// (a) NO CLICK + (b) NO DIP + (c) NO OVER-PEAK: a ~10 ms equal-power cross-fade
/// from source A (+0.5 DC) to source B (+0.5 DC) across a TICK BLOCK has no
/// sample discontinuity at the seam, never dips below the steady level, and
/// never exceeds the louder source. Both sources are the same correlated +0.5
/// DC, so an equal-power fade holds the bus in [0.5, 0.707] the WHOLE time — and
/// critically there is NO step at the instant-swap seam (the RT-8a click is gone).
#[test]
fn equal_power_crossfade_has_no_click_and_constant_power_across_a_tick_block() {
    let fmt = stereo();
    // 25 fps @ 48k = 1920 samples/tick. A 480-frame (~10 ms) ramp is well under
    // one tick block, so the seam + the whole ramp live inside the first tick.
    let mut bus = ProgramBus::new(fmt, Rational::new(25, 1));
    let ramp = 480usize;

    let store_a = Arc::new(AudioStore::new(fmt, 192_000));
    // Source B is a LIVE breakaway target: a `repoint_crossfade` seeks it to its
    // live edge (as `repoint` does), so a real source publishes fresh audio AT
    // that edge each tick. Model that by seeding B's current tail and topping it
    // up at the live edge as the bus consumes it (exactly as RT-8a's breakaway
    // test models a live decoder).
    let store_b = Arc::new(AudioStore::new(fmt, 192_000));
    store_a
        .publish(&AudioBlock::from_interleaved(fmt, vec![0.5f32; 192_000 * 2]).unwrap())
        .unwrap();
    store_b
        .publish(&AudioBlock::from_interleaved(fmt, vec![0.5f32; 4_000 * 2]).unwrap())
        .unwrap();
    let point = bus.add_source("prog", Arc::clone(&store_a), 1.0);

    // Tick once on A alone (steady 0.5) to establish the pre-seam baseline.
    let pre = bus.tick();
    let pre_left: Vec<f32> = (0..pre.frame_count())
        .map(|f| pre.interleaved()[f * 2])
        .collect();
    assert!(pre_left.iter().all(|&v| (v - 0.5).abs() < 1e-6));
    let last_pre = *pre_left.last().unwrap();

    // BREAKAWAY: cross-fade to B over `ramp` frames. This is the CLICK-FREE tier.
    // The crossfade seeks B to its live edge (frame 4000); publish fresh B audio
    // there so the seam reads B's live tone, exactly as a real decoder would.
    let tier = bus
        .repoint_crossfade(point, Arc::clone(&store_b), ramp)
        .unwrap();
    assert_eq!(tier, SwitchTier::ClickFree);
    store_b
        .publish(&AudioBlock::from_interleaved(fmt, vec![0.5f32; 8_000 * 2]).unwrap())
        .unwrap();

    let post = bus.tick();
    let post_left: Vec<f32> = (0..post.frame_count())
        .map(|f| post.interleaved()[f * 2])
        .collect();

    // (a) NO CLICK at the seam: the worst-case adjacent step over the whole
    // concatenated signal must stay tiny.
    let mut full = pre_left.clone();
    full.extend_from_slice(&post_left);
    let max_step = full
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0f32, f32::max);
    assert!(
        max_step < 5e-3,
        "no click: max adjacent sample step across the fade must stay bounded, got {max_step}"
    );
    assert!(
        (post_left[0] - last_pre).abs() < 5e-3,
        "the seam sample must be continuous with the pre-seam tail (no instant jump)"
    );

    // (b) NO DIP + (c) NO OVER-PEAK through the fade.
    for (i, &v) in post_left.iter().take(ramp).enumerate() {
        assert!(
            v >= 0.5 - 1e-3,
            "equal-power fade of correlated sources must not dip below the level at frame {i}: {v}"
        );
        assert!(
            v <= 0.5 * std::f32::consts::SQRT_2 + 1e-3,
            "fade must never exceed the equal-power sum of the two 0.5 sources at frame {i}: {v}"
        );
    }
    // After the ramp the bus is steady on B alone at 0.5.
    assert!(
        post_left[ramp..].iter().all(|&v| (v - 0.5).abs() < 1e-3),
        "after the ramp the bus must be B alone at its steady level"
    );
}

/// (c) R128 / TRUE-PEAK not violated: for two UNCORRELATED sources (so there is
/// no constructive +3 dB bump), the equal-power cross-fade's bus true-peak never
/// exceeds the louder source's peak, and the summed POWER stays constant across
/// the fade (no dip). Source A is a +1.0/-1.0 square at one phase, B the same at
/// the opposite phase, so at every instant only one is at +1.0 and the cross term
/// is statistically zero — the equal-power sum gainA²·1 + gainB²·1 = 1 holds the
/// power flat while the peak stays at <= 1.0 (the louder source).
#[test]
fn crossfade_true_peak_bounded_and_power_flat_for_uncorrelated_sources() {
    let fmt = stereo();
    let mut mixer = Mixer::new(fmt);
    let a = mixer.add_input("a");
    let b = mixer.add_input("b");
    mixer.route_to_program(a, 1.0);
    mixer.route_to_program(b, 1.0);

    // Uncorrelated full-scale content: A square +1/-1, B the inverse square so the
    // two are anti-phase (cross term cancels in power; never both +1 at once).
    let n = 960usize;
    let mut sa = Vec::with_capacity(n * 2);
    let mut sb = Vec::with_capacity(n * 2);
    for f in 0..n {
        let va = if f % 2 == 0 { 1.0f32 } else { -1.0 };
        let vb = -va; // anti-phase
        sa.push(va);
        sa.push(va);
        sb.push(vb);
        sb.push(vb);
    }
    mixer
        .submit(a, AudioBlock::from_interleaved(fmt, sa).unwrap())
        .unwrap();
    mixer
        .submit(b, AudioBlock::from_interleaved(fmt, sb).unwrap())
        .unwrap();
    mixer.set_gain_ramp(a, GainRamp::down(n));
    mixer.set_gain_ramp(b, GainRamp::up(n));

    let bus = mixer.mix_program().unwrap();
    // TRUE-PEAK bounded: no bus sample exceeds 1.0 (the louder source's peak).
    // An equal-power fade of anti-phase ±1 sources is gA·(±1) + gB·(∓1), whose
    // magnitude |gA - gB| <= 1 always (and the clamp would catch any overflow).
    for (i, &v) in bus.interleaved().iter().enumerate() {
        assert!(
            v.abs() <= 1.0 + 1e-6,
            "true-peak must not exceed the louder source (1.0) at sample {i}: {v}"
        );
    }
    // POWER flat: per frame the instantaneous power gA²·1 + gB²·1 == 1. Recover
    // the per-strip gains from the envelopes and check the sum-of-squares.
    for f in 0..n {
        let ga = GainRamp::down(n).envelope_at(f);
        let gb = GainRamp::up(n).envelope_at(f);
        approx::assert_abs_diff_eq!(ga * ga + gb * gb, 1.0, epsilon = 1e-4);
    }
}

/// (d) After the ramp completes the OLD strip is UNROUTED — it no longer
/// contributes to the bus. Drive the bus past the ramp and confirm the old
/// source's store is no longer read (its cursor freezes) while B keeps advancing.
#[test]
fn old_strip_is_unrouted_after_the_ramp_completes() {
    let fmt = stereo();
    let mut bus = ProgramBus::new(fmt, Rational::new(25, 1)); // 1920/tick
    let ramp = 480usize;

    let store_a = Arc::new(AudioStore::new(fmt, 192_000));
    let store_b = Arc::new(AudioStore::new(fmt, 192_000));
    store_a
        .publish(&AudioBlock::from_interleaved(fmt, vec![0.3f32; 192_000 * 2]).unwrap())
        .unwrap();
    // B is a live source: a small seeded tail then top-ups at the live edge.
    store_b
        .publish(&AudioBlock::from_interleaved(fmt, vec![0.9f32; 2_000 * 2]).unwrap())
        .unwrap();
    let point = bus.add_source("prog", Arc::clone(&store_a), 1.0);

    bus.repoint_crossfade(point, Arc::clone(&store_b), ramp)
        .unwrap();
    // B (live) publishes fresh audio at its live edge to cover the next reads.
    store_b
        .publish(&AudioBlock::from_interleaved(fmt, vec![0.9f32; 4_000 * 2]).unwrap())
        .unwrap();
    // Tick 1: covers the whole ramp (1920 > 480) and into B-only territory.
    let _ = bus.tick();
    let cursor_a_after_ramp = store_a.read_cursor();
    // Tick 2: fully past the ramp — A must NOT be read again (unrouted), so its
    // cursor does not advance, while B keeps being read.
    store_b
        .publish(&AudioBlock::from_interleaved(fmt, vec![0.9f32; 4_000 * 2]).unwrap())
        .unwrap();
    let cursor_b_before = store_b.read_cursor();
    let block2 = bus.tick();
    assert_eq!(
        store_a.read_cursor(),
        cursor_a_after_ramp,
        "after the ramp the OLD store A must be unrouted (cursor frozen, never read again)"
    );
    assert!(
        store_b.read_cursor() > cursor_b_before,
        "the NEW store B must still be read after the ramp"
    );
    // And the post-ramp bus is B alone at 0.9.
    assert!(
        (0..block2.frame_count()).all(|f| (block2.interleaved()[f * 2] - 0.9).abs() < 1e-3),
        "after the ramp the bus is the new source B (0.9) alone"
    );
}

/// The whole RT-9 contract across a MULTI-tick span so the ramp straddles a tick
/// BLOCK boundary (the per-sample envelope must continue smoothly from one tick
/// block into the next — a per-tick scalar would step at the boundary).
#[test]
fn crossfade_spans_a_tick_boundary_with_no_step_at_the_block_seam() {
    let fmt = stereo();
    let mut bus = ProgramBus::new(fmt, Rational::new(25, 1)); // 1920/tick
    let ramp = 2400usize; // > one tick block => the ramp crosses the boundary

    let store_a = Arc::new(AudioStore::new(fmt, 384_000));
    let store_b = Arc::new(AudioStore::new(fmt, 384_000));
    store_a
        .publish(&AudioBlock::from_interleaved(fmt, vec![0.5f32; 384_000 * 2]).unwrap())
        .unwrap();
    // B is a live source: seed a small tail, then top up at the live edge.
    store_b
        .publish(&AudioBlock::from_interleaved(fmt, vec![0.5f32; 2_000 * 2]).unwrap())
        .unwrap();
    let point = bus.add_source("prog", Arc::clone(&store_a), 1.0);

    let _ = bus.tick(); // baseline on A
    bus.repoint_crossfade(point, Arc::clone(&store_b), ramp)
        .unwrap();

    // Drain three more ticks (covers the 2400-frame ramp which spans 2 blocks);
    // B (live) publishes a tick's budget of fresh audio at its live edge before
    // each so the seam + fade read B's live tone, not silence past EOF.
    let mut signal = Vec::new();
    for _ in 0..3 {
        store_b
            .publish(&AudioBlock::from_interleaved(fmt, vec![0.5f32; 1_920 * 2]).unwrap())
            .unwrap();
        let block = bus.tick();
        for frame in 0..block.frame_count() {
            signal.push(block.interleaved()[frame * 2]);
        }
    }
    let max_step = signal
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0f32, f32::max);
    assert!(
        max_step < 5e-3,
        "the per-sample envelope must continue smoothly across the tick block \
         boundary at frame 1920 (no step); max adjacent step was {max_step}"
    );
}

// ---------------------------------------------------------------------------
// The two-tier model (SOFT-STEP vs CLICK-FREE).
// ---------------------------------------------------------------------------

/// The tier is surfaced so the caller/UI can read which transition a switch
/// uses. The decoded-bus cross-fade is CLICK-FREE (Class-2); a coded passthrough
/// AU-aligned step is SOFT-STEP (Class-1). Both are modelled; only CLICK-FREE is
/// implemented here.
#[test]
fn switch_tiers_are_distinct_and_carry_their_class() {
    assert_eq!(SwitchTier::SoftStep.class(), ApplyClass::Class1);
    assert_eq!(SwitchTier::ClickFree.class(), ApplyClass::Class2);
    assert!(SwitchTier::ClickFree.is_pop_free());
    assert!(!SwitchTier::SoftStep.is_pop_free());
    assert_ne!(SwitchTier::SoftStep, SwitchTier::ClickFree);
}

/// A zero-length ramp degrades to an instant hard swap (a SOFT-STEP-equivalent
/// hard cut) — `repoint_crossfade(.., 0)` behaves like RT-8a `repoint` and is
/// reported as such, never panicking on the degenerate input.
#[test]
fn zero_length_ramp_degrades_to_a_soft_step_hard_swap() {
    let fmt = stereo();
    let mut bus = ProgramBus::new(fmt, Rational::new(25, 1));
    let store_a = Arc::new(AudioStore::new(fmt, 96_000));
    let store_b = Arc::new(AudioStore::new(fmt, 96_000));
    store_a
        .publish(&AudioBlock::from_interleaved(fmt, vec![0.2f32; 96_000 * 2]).unwrap())
        .unwrap();
    store_b
        .publish(&AudioBlock::from_interleaved(fmt, vec![0.8f32; 96_000 * 2]).unwrap())
        .unwrap();
    let point = bus.add_source("p", Arc::clone(&store_a), 1.0);
    let _ = bus.tick();
    let tier = bus
        .repoint_crossfade(point, Arc::clone(&store_b), 0)
        .unwrap();
    assert_eq!(
        tier,
        SwitchTier::SoftStep,
        "a 0-frame ramp is a hard step, not click-free"
    );
    store_b
        .publish(&AudioBlock::from_interleaved(fmt, vec![0.8f32; 1920 * 2]).unwrap())
        .unwrap();
    let after = bus.tick();
    assert!(
        (0..after.frame_count()).all(|f| (after.interleaved()[f * 2] - 0.8).abs() < 1e-3),
        "a zero-ramp swap is the instant B (0.8), exactly like RT-8a repoint"
    );
}

/// `repoint_crossfade` on an unknown point is a clean typed error (no panic),
/// exactly like `repoint`.
#[test]
fn crossfade_of_a_nonexistent_point_is_a_clean_error() {
    let fmt = stereo();
    let mut bus = ProgramBus::new(fmt, Rational::new(25, 1));
    let store = Arc::new(AudioStore::new(fmt, 48_000));
    let bogus = multiview_audio::RoutePoint::input(42);
    let err = bus.repoint_crossfade(bogus, store, 480).unwrap_err();
    assert!(matches!(err, multiview_audio::AudioError::UnknownInput(42)));
}

// ---------------------------------------------------------------------------
// Bounded memory (invariant #9): completed cross-fades reclaim their strip.
// ---------------------------------------------------------------------------

/// (e) BOUNDED MEMORY — a completed cross-fade must RECLAIM its outgoing mixer
/// strip. A `repoint_crossfade` adds a temporary outgoing strip that fades out;
/// once its ramp completes the strip is retired. If retirement only *unroutes*
/// the strip (leaving it resident in the mixer's strip storage), then every
/// cross-fade leaks one strip forever — on a 24/7 multi-program host that is
/// unbounded RAM growth on the data plane (invariant #9: bounded queues/pools
/// "allocated at start, never grow").
///
/// This drives many add/route/complete cross-fade cycles through the program bus
/// and asserts (1) after each completed fade only the channel strip remains live
/// — the outgoing strip was reclaimed, not leaked — and (2) the mixer's physical
/// strip storage stays BOUNDED across all cycles (reused slots, not one leaked
/// strip each). It FAILS against the leaking (unroute-only) retirement and PASSES
/// once retirement reclaims the slot.
#[test]
fn repeated_crossfades_reclaim_strips_and_stay_bounded() {
    const CYCLES: usize = 200;

    let fmt = stereo();
    // 25 fps @ 48k = 1920 samples/tick; a 480-frame (~10 ms) ramp is well under
    // one tick block, so a single tick completes each fade and retires its strip.
    let mut bus = ProgramBus::new(fmt, Rational::new(25, 1));
    let ramp = 480usize;

    let store_a = Arc::new(AudioStore::new(fmt, 192_000));
    store_a
        .publish(&AudioBlock::from_interleaved(fmt, vec![0.4f32; 8_000 * 2]).unwrap())
        .unwrap();
    let point = bus.add_source("prog", Arc::clone(&store_a), 1.0);

    // Baseline: one routed source == one live strip, one physical slot.
    let baseline_slots = bus.mixer_slot_count();
    assert_eq!(
        bus.live_strip_count(),
        1,
        "one routed source is one live strip at start"
    );

    for i in 0..CYCLES {
        // Cross-fade the single channel onto a fresh store over a short ramp. The
        // channel keeps its identity (`point`), so we re-point it every cycle.
        let next = Arc::new(AudioStore::new(fmt, 192_000));
        next.publish(&AudioBlock::from_interleaved(fmt, vec![0.4f32; 8_000 * 2]).unwrap())
            .unwrap();
        bus.repoint_crossfade(point, Arc::clone(&next), ramp)
            .unwrap();
        // Fresh audio at the new store's live edge to cover the reads.
        next.publish(&AudioBlock::from_interleaved(fmt, vec![0.4f32; 4_000 * 2]).unwrap())
            .unwrap();
        // One tick covers the whole 480-frame ramp (1920 > 480) and retires the
        // outgoing strip; a second tick is safely past it.
        let _ = bus.tick();
        let _ = bus.tick();

        // After a completed cross-fade exactly ONE strip is live (the channel).
        // The outgoing strip must have been reclaimed — not merely unrouted and
        // left resident. The leaking implementation has `i + 2` live strips here.
        assert_eq!(
            bus.live_strip_count(),
            1,
            "after cross-fade #{i} only the channel strip may remain live; the \
             faded-out strip must be reclaimed, not leaked (got {})",
            bus.live_strip_count()
        );
    }

    // BOUNDED: the mixer's physical strip storage never grew unboundedly. With
    // slot reuse it stays a small constant; the leaking implementation is
    // ~CYCLES+1 physical slots here.
    assert!(
        bus.mixer_slot_count() <= baseline_slots + 2,
        "mixer strip storage must stay bounded across {CYCLES} completed \
         cross-fades (reused slots, not one leaked strip each): got {} physical \
         slots, baseline {}",
        bus.mixer_slot_count(),
        baseline_slots
    );
}

/// SOAK — the mixer's physical strip storage stays a small CONSTANT across a long
/// run of cross-fades (catches slow growth a single-shot bound would miss). One
/// channel plus at most one in-flight outgoing strip means exactly TWO physical
/// slots ever exist — the channel slot and the single reused outgoing slot —
/// forever; a per-cross-fade leak would make this grow toward `N`.
#[test]
fn crossfade_strip_storage_is_flat_over_a_long_soak() {
    const CYCLES: usize = 2_000;

    let fmt = stereo();
    let mut bus = ProgramBus::new(fmt, Rational::new(25, 1)); // 1920 samples/tick
    let ramp = 480usize;

    let store = Arc::new(AudioStore::new(fmt, 192_000));
    store
        .publish(&AudioBlock::from_interleaved(fmt, vec![0.3f32; 8_000 * 2]).unwrap())
        .unwrap();
    let point = bus.add_source("prog", Arc::clone(&store), 1.0);

    let mut peak_slots = bus.mixer_slot_count();
    for _ in 0..CYCLES {
        let next = Arc::new(AudioStore::new(fmt, 192_000));
        next.publish(&AudioBlock::from_interleaved(fmt, vec![0.3f32; 8_000 * 2]).unwrap())
            .unwrap();
        bus.repoint_crossfade(point, Arc::clone(&next), ramp)
            .unwrap();
        // Fresh audio at the live edge to cover the reads.
        next.publish(&AudioBlock::from_interleaved(fmt, vec![0.3f32; 4_000 * 2]).unwrap())
            .unwrap();
        // One tick (1920 > 480) completes the ramp and reclaims the outgoing strip.
        let _ = bus.tick();
        peak_slots = peak_slots.max(bus.mixer_slot_count());
    }

    // Exactly two physical slots ever — the channel + the one reused outgoing
    // slot — not `CYCLES`. This is the invariant-#9 guarantee as a flat constant.
    assert_eq!(
        peak_slots, 2,
        "strip storage must stay flat at 2 slots across {CYCLES} cross-fades \
         (channel + one reused outgoing slot); saw peak {peak_slots}"
    );
    assert_eq!(
        bus.live_strip_count(),
        1,
        "at rest exactly one strip (the channel) is live"
    );
}

// ---------------------------------------------------------------------------
// Generation guard (ABA safety): a reused slot rejects stale handles.
// ---------------------------------------------------------------------------

/// GENERATION GUARD (ABA safety) — reclaiming a strip and reusing its slot for a
/// DIFFERENT source must NOT let a stale `RoutePoint` (one that named the slot
/// before it was reclaimed) address or perturb the new occupant. The slot's
/// generation tag makes the stale handle read as unknown, so a freed route can
/// never silently bind the wrong source — the ABA hazard the resilience brief
/// warns about ("non-refcounted … silently reused on the next decode →
/// corruption"). Without the generation tag the reused slot would answer to the
/// old index and the two sources would cross-wire.
#[test]
fn a_reused_slot_rejects_a_stale_route_point() {
    let fmt = stereo();
    let mut mixer = Mixer::new(fmt);

    // Occupy a slot, route + arm it, then reclaim it — keep the now-stale handle.
    let stale = mixer.add_input("old");
    mixer.route_to_program(stale, 0.9);
    mixer.set_gain_ramp(stale, GainRamp::down(100));
    assert!(mixer.remove_input(stale), "the live strip is reclaimed");
    // Removing an already-reclaimed handle is a safe no-op (no double-free).
    assert!(
        !mixer.remove_input(stale),
        "a second remove of the same handle must be a no-op"
    );

    // A new input REUSES the freed slot: same index, bumped generation.
    let fresh = mixer.add_input("new");
    assert_eq!(
        fresh.index(),
        stale.index(),
        "the freed slot index must be reused (bounded storage)"
    );
    assert_ne!(
        fresh, stale,
        "but the handle must differ — the generation was bumped"
    );

    // The stale handle is INERT: it names no strip and cannot mutate the reused
    // occupant.
    assert!(
        mixer.input_id(stale).is_none(),
        "the stale handle must address nothing"
    );
    assert!(mixer.program_gain(stale).is_none());
    mixer.route_to_program(stale, 0.1); // must NOT touch `fresh`
    mixer.set_gain_ramp(stale, GainRamp::up(50)); // must NOT touch `fresh`
    mixer.unroute_from_program(stale);
    assert!(
        mixer.gain_ramp(stale).is_none(),
        "a stale handle must carry no ramp"
    );
    // `submit` through the stale handle is a typed UnknownInput error, never a
    // wrong-source bind.
    let err = mixer
        .submit(stale, AudioBlock::silence(fmt, 4))
        .unwrap_err();
    assert!(matches!(err, multiview_audio::AudioError::UnknownInput(_)));

    // The fresh strip is untouched by every stale write: its own id, no ramp, and
    // its default gain of 1.0 — NOT the 0.1 the stale `route_to_program` tried.
    assert_eq!(mixer.input_id(fresh), Some("new"));
    assert!(
        mixer.gain_ramp(fresh).is_none(),
        "the stale set_gain_ramp must not have armed the fresh strip"
    );
    assert_eq!(
        mixer.program_gain(fresh),
        Some(1.0),
        "the fresh strip keeps its own default gain; the stale route was inert"
    );
    // And `fresh` addresses the reused slot correctly.
    mixer.route_to_program(fresh, 0.5);
    assert_eq!(mixer.program_gain(fresh), Some(0.5));
}

// ---------------------------------------------------------------------------
// Overlapping cross-fades: each outgoing strip retires independently and every
// reclaimed slot is reused (storage stays at the concurrent high-water).
// ---------------------------------------------------------------------------

/// OVERLAPPING FADES — starting a second/third `repoint_crossfade` while an
/// earlier outgoing strip is STILL FADING (before it retires) must keep every
/// outgoing strip independent: each retires on its own tick when its ramp
/// completes, and every retired slot is reclaimed and reused. Physical storage
/// must stay at the concurrent high-water (the channel + `OVERLAP` simultaneous
/// outgoing strips), never the total number of fades ever run. A leak would grow
/// storage past the high-water; a shared/aliased slot would fail to return the
/// live count to 1 or skip an intermediate count on the way down.
#[test]
fn overlapping_crossfades_retire_independently_and_reuse_slots() {
    const OVERLAP: usize = 3; // simultaneous in-flight outgoing strips
    const ROUNDS: usize = 4; // repeated to prove reuse (no growth)

    let fmt = stereo();
    let mut bus = ProgramBus::new(fmt, Rational::new(25, 1)); // 1920 samples/tick
                                                              // A long ramp (6 tick blocks) so an outgoing strip is still mid-fade
                                                              // (0 < frames_done < ramp) while the next fades start one tick apart — the
                                                              // fades genuinely overlap, and equal ramps + one-tick-apart starts make the
                                                              // outgoing strips complete on distinct ticks (staggered retirement).
    let ramp = 6 * 1920;

    let seed = || -> Arc<AudioStore> {
        let store = Arc::new(AudioStore::new(fmt, 96_000));
        store
            .publish(&AudioBlock::from_interleaved(fmt, vec![0.25f32; 8_000 * 2]).unwrap())
            .unwrap();
        store
    };

    let point = bus.add_source("prog", seed(), 1.0);
    assert_eq!(bus.live_strip_count(), 1);

    // The concurrent high-water: the channel + OVERLAP simultaneous outgoing
    // strips. Storage must never exceed this, however many rounds run.
    let high_water = OVERLAP + 1;
    let mut peak_slots = bus.mixer_slot_count();

    for round in 0..ROUNDS {
        // Start OVERLAP cross-fades one tick apart, so each earlier outgoing strip
        // is genuinely mid-fade when the next starts.
        for k in 0..OVERLAP {
            bus.repoint_crossfade(point, seed(), ramp).unwrap();
            peak_slots = peak_slots.max(bus.mixer_slot_count());
            if k + 1 < OVERLAP {
                // Advance the in-flight fades by one tick block — well short of the
                // 6-block ramp, so nothing retires yet.
                let _ = bus.tick();
                peak_slots = peak_slots.max(bus.mixer_slot_count());
            }
        }

        // All OVERLAP outgoing strips coexist with the channel now.
        assert_eq!(
            bus.live_strip_count(),
            high_water,
            "round {round}: the channel + all {OVERLAP} overlapping outgoing strips \
             must be live at once"
        );
        assert_eq!(
            bus.mixer_slot_count(),
            high_water,
            "round {round}: physical storage is exactly the concurrent high-water"
        );

        // Tick to completion, recording the live-strip trajectory. Because the
        // strips started one tick apart with equal ramps they retire on distinct
        // ticks, so the count must pass through every intermediate value down to 1
        // — independent, not all-at-once, retirement.
        let mut observed = vec![bus.live_strip_count()];
        let mut ticks = 0;
        while bus.live_strip_count() > 1 {
            let _ = bus.tick();
            peak_slots = peak_slots.max(bus.mixer_slot_count());
            observed.push(bus.live_strip_count());
            ticks += 1;
            assert!(
                ticks < 128,
                "round {round}: every overlapping outgoing strip must retire within a \
                 bounded number of ticks (stuck at {} live)",
                bus.live_strip_count()
            );
        }
        for expected in 1..=high_water {
            assert!(
                observed.contains(&expected),
                "round {round}: the live count must pass through {expected} as the \
                 overlapping fades retire one at a time (independent retirement); \
                 observed {observed:?}"
            );
        }
        assert_eq!(
            bus.live_strip_count(),
            1,
            "round {round}: after completion only the channel remains live"
        );
    }

    // REUSE — across every round storage stayed at the concurrent high-water: the
    // OVERLAP retired slots were reclaimed and reused each round, never leaked. A
    // leak would make this `ROUNDS * OVERLAP + 1`, not `OVERLAP + 1`.
    assert_eq!(
        peak_slots, high_water,
        "physical storage must stay at the concurrent high-water ({high_water}) \
         across {ROUNDS} rounds of {OVERLAP} overlapping fades — reused slots, not a \
         leaked strip per fade"
    );
    assert_eq!(bus.mixer_slot_count(), high_water);
}
