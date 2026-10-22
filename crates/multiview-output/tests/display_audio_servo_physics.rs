//! Servo→resampler **application physics** tests (DEV-B4 / display-out §5).
//!
//! The servo speaks in "drain ppm" (positive = consume FIFO content faster);
//! the [`AdaptiveResampler`](multiview_audio::AdaptiveResampler) speaks in
//! "output-frames-per-input ppm" (positive = MORE device frames per content
//! frame = content plays *slower*). At a fixed device sample rate the two are
//! **reciprocal**: draining input faster means emitting *fewer* device frames
//! per input frame. Feeding the servo's ppm straight into the resampler is a
//! sign error that turns the control loop into positive feedback (a too-full
//! FIFO would fill further until it saturates at the clamp, dropping forever).
//! These tests pin the reciprocal mapping ([`drain_ratio`]), the skew
//! measurement ([`skew_ms`], positive = audio AHEAD of scanout), the
//! end-to-end sign chain, and — over the REAL `AudioFifo` + `BufferServo` +
//! `AdaptiveResampler` — that the closed loop *converges* under a drifting
//! engine crystal instead of saturating.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::float_cmp
)]

use multiview_audio::adaptive::RatioPpm;
use multiview_audio::{AdaptiveResampler, AudioBlock, AudioFormat, ChannelLayout};
use multiview_output::display::audio::{drain_ratio, skew_ms, AudioFifo, BufferServo};

#[test]
fn drain_ratio_is_the_reciprocal_of_the_servo_demand() {
    // +1000 ppm drain demand => the resampler must emit FEWER output frames per
    // input frame: ppm_resampler = (1/(1+p) - 1)·1e6 ≈ -999.0 for p = 1e-3.
    let applied = drain_ratio(RatioPpm::from_ppm(1_000.0));
    assert!(
        applied.ppm() < 0.0,
        "a positive drain demand must map to a NEGATIVE resampler ppm, got {}",
        applied.ppm()
    );
    assert!(
        (applied.ppm() + 999.000_999).abs() < 0.01,
        "reciprocal must be exact, not just negated: got {}",
        applied.ppm()
    );
    // The reciprocal of the reciprocal is the original ratio (no drift through
    // the mapping).
    let servo = RatioPpm::from_ppm(437.0);
    let round_trip = drain_ratio(drain_ratio(servo));
    assert!((round_trip.ppm() - servo.ppm()).abs() < 1e-6);
    // Unity is a fixed point.
    assert_eq!(drain_ratio(RatioPpm::ZERO).ppm(), 0.0);
}

#[test]
fn skew_is_audio_minus_scanout_elapsed() {
    // Audio has played 48000 frames @ 48 kHz = 1.000 s while scanout advanced
    // 0.995 s => audio is 5 ms AHEAD => +5.0.
    let s = skew_ms(48_000, 48_000, 995_000_000);
    assert!((s - 5.0).abs() < 1e-6, "expected +5 ms, got {s}");
    // Audio behind scanout => negative.
    assert!(skew_ms(47_760, 48_000, 1_000_000_000) < 0.0);
    // Degenerate inputs never divide by zero / panic.
    assert_eq!(skew_ms(0, 0, 0), 0.0);
}

#[test]
fn audio_ahead_of_scanout_is_slowed_down() {
    // The full sign chain: audio AHEAD (positive skew) must end as a POSITIVE
    // resampler ppm (more device frames per content frame = audio plays slower
    // and scanout catches it back up); audio behind, the opposite.
    let mut servo = BufferServo::new();
    let ahead = drain_ratio(servo.correction(0.5, 5.0));
    let mut servo2 = BufferServo::new();
    let behind = drain_ratio(servo2.correction(0.5, -5.0));
    assert!(
        ahead.ppm() > 0.0,
        "audio ahead must slow down (positive resampler ppm), got {}",
        ahead.ppm()
    );
    assert!(
        behind.ppm() < 0.0,
        "audio behind must speed up (negative resampler ppm), got {}",
        behind.ppm()
    );
}

#[test]
fn closed_loop_with_the_real_parts_converges_and_stops_dropping() {
    // The whole drain loop in miniature, over the REAL components: the engine
    // crystal runs +300 ppm fast relative to the device sample clock; per
    // device-paced iteration the loop pops a fixed 480-frame quantum, resamples
    // at drain_ratio(servo), and the engine pushes 480·(1+300ppm) scaled by the
    // device time the written frames consumed. The fill must settle around the
    // setpoint and the FIFO must stop dropping after warm-up — with the sign
    // applied directly (no reciprocal) this loop is positive feedback and
    // saturates full, dropping every iteration.
    let format = AudioFormat::new(48_000, ChannelLayout::Stereo);
    let cap_frames = 8_192usize;
    let mut fifo = AudioFifo::new(cap_frames, 2);
    let mut servo = BufferServo::new();
    let mut resampler = AdaptiveResampler::new(format);

    let engine_ratio = 1.0 + 300.0 / 1_000_000.0; // engine crystal vs device
    let quantum = 480usize;
    let mut inflow_accum = 0.0_f64;
    let mut scratch = vec![0.0f32; quantum * 2];
    // Start half-full so the loop begins at the setpoint, then must HOLD it
    // against the drift.
    fifo.push(&vec![0.1f32; (cap_frames / 2) * 2]);

    let mut drops_late = 0u64;
    let mut fills = Vec::new();
    let mut drops_before = fifo.dropped_frames();
    for i in 0..4_000 {
        let frac = fifo.fill_fraction();
        resampler.set_ratio(drain_ratio(servo.correction(frac, 0.0)));
        let _ = fifo.pop_into(&mut scratch);
        let block = AudioBlock::from_interleaved(format, scratch.clone()).unwrap();
        let out_frames = resampler.process(&block).frame_count();
        // The device consumes `out_frames` at 48 kHz — that wall time elapses,
        // during which the (fast) engine produced this much content:
        inflow_accum += (out_frames as f64) * engine_ratio;
        let whole = inflow_accum.floor() as usize;
        inflow_accum -= whole as f64;
        fifo.push(&vec![0.1f32; whole * 2]);
        if i >= 2_000 {
            fills.push(fifo.fill_fraction());
            drops_late += fifo.dropped_frames() - drops_before;
        }
        drops_before = fifo.dropped_frames();
    }
    let avg_fill = fills.iter().sum::<f64>() / (fills.len() as f64);
    assert!(
        (avg_fill - 0.5).abs() < 0.2,
        "the loop must hold the fill near the setpoint under a +300ppm engine \
         crystal; settled average was {avg_fill:.3}"
    );
    assert_eq!(
        drops_late, 0,
        "a converged loop must not drop after warm-up (fill saturating at the \
         clamp means the servo sign is positive feedback)"
    );
}
