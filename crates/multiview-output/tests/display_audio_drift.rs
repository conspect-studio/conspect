//! Closed-loop device-clock drift tests for the display-audio servo
//! (DEV-B4 adversarial review MAJOR-1: the skew half of the three-clock servo
//! must be CLOSED-loop).
//!
//! These sims drive the REAL `AudioFifo` plus `BufferServo`,
//! `AdaptiveResampler` and `SkewTracker` exactly as `steady_state_drain` does
//! — same per-iteration order (observe skew → servo on the fill → reciprocal
//! ratio → pop a 480-frame quantum → resample → write → account) — with a
//! **device crystal that drifts in ppm against the display/flip clock**,
//! fast-forwarded over simulated hours. The resampler ratio must be able to
//! *cancel* the measured skew (content-position accounting): if the tracker
//! instead accumulates post-resample device frames, the device consumes those
//! at its own crystal rate regardless of the ratio, the measurement is
//! uncontrollable, the servo integrates into the clamp, the FIFO pegs, and
//! the loop late-drops forever. A frozen flip clock (wedged display) must
//! degrade to fill-only control — never peg the clamp or storm drops — and
//! re-anchor when flips resume.
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

use multiview_audio::{AdaptiveResampler, AudioBlock, AudioFormat, ChannelLayout, RatioPpm};
use multiview_output::display::audio::{drain_ratio, AudioFifo, BufferServo, SkewTracker};

/// The drain quantum the real loop pops per iteration (sink.rs `DRAIN_FRAMES`).
const QUANTUM: usize = 480;
/// The FIFO capacity the pipeline configures (`DISPLAY_AUDIO_FIFO_FRAMES`).
const CAPACITY: usize = 8_192;
/// The canonical content/device sample rate.
const RATE: f64 = 48_000.0;
/// The modelled constant device ring depth (`snd_pcm_delay`), in device frames.
const DELAY_FRAMES: i64 = 1_440;

/// How the simulated display flip clock behaves over true time.
struct FlipPlan {
    /// Flips stop advancing at this true-time second (wedged display)…
    freeze_at: f64,
    /// …and resume at this true-time second (`f64::INFINITY` = never).
    resume_at: f64,
}

impl FlipPlan {
    fn healthy() -> Self {
        Self {
            freeze_at: f64::INFINITY,
            resume_at: f64::INFINITY,
        }
    }

    /// The kernel flip timestamp at true time `t`: the last 60 Hz vsync edge,
    /// in ns. While wedged the last-flip value holds (exactly what
    /// `last_flip_ns` does when the pipe stops flipping); on resume it jumps
    /// to the current edge (the next real flip lands "now").
    fn flip_ns(&self, t: f64) -> u64 {
        let effective = if t >= self.freeze_at && t < self.resume_at {
            self.freeze_at
        } else {
            t
        };
        let edge = (effective * 60.0).floor() / 60.0;
        (edge * 1e9) as u64
    }
}

/// What the sim measured over its late (post-warm-up) window.
struct SimOutcome {
    /// Mean FIFO fill fraction across the late window.
    avg_late_fill: f64,
    /// Frames late-dropped at the FIFO (drop-oldest) inside the late window.
    late_drops: u64,
    /// Largest |applied resampler ppm| seen inside the late window.
    max_late_applied_ppm: f64,
}

/// Run the closed loop for `hours` of simulated true time with the device
/// crystal `dev_ppm` fast against the display clock, collecting stats over the
/// trailing `late_window_hours`.
fn run_sim(dev_ppm: f64, hours: f64, late_window_hours: f64, flips: &FlipPlan) -> SimOutcome {
    let format = AudioFormat::new(48_000, ChannelLayout::Mono);
    let mut fifo = AudioFifo::new(CAPACITY, 1);
    let mut servo = BufferServo::new();
    let mut resampler = AdaptiveResampler::new(format);
    let mut tracker = SkewTracker::new();

    // Start at the setpoint: the loop's job is to HOLD it against the drift.
    fifo.push(&vec![0.1f32; CAPACITY / 2]);

    let dev_rate = RATE * (1.0 + dev_ppm / 1e6); // device frames per true second
    let total_secs = hours * 3_600.0;
    let late_start = total_secs - late_window_hours * 3_600.0;

    let mut t = 0.0f64; // true seconds
    let mut inflow_accum = 0.0f64;
    let mut scratch = vec![0.0f32; QUANTUM];
    let content = vec![0.1f32; QUANTUM * 2];

    let mut late_drops = 0u64;
    let mut drops_before = fifo.dropped_frames();
    let mut fill_sum = 0.0f64;
    let mut fill_n = 0u64;
    let mut max_applied = 0.0f64;

    while t < total_secs {
        // --- One steady_state_drain iteration, verbatim order. ---
        let flip_ns = flips.flip_ns(t);
        let skew = tracker.skew_input(flip_ns, Some(DELAY_FRAMES), resampler.ratio(), 48_000);
        let fill = fifo.fill_fraction();
        resampler.set_ratio(drain_ratio(servo.correction(fill, skew)));
        let _ = fifo.pop_into(&mut scratch);
        let block = AudioBlock::from_interleaved(format, scratch.clone()).unwrap();
        let out_frames = resampler.process(&block).frame_count();
        tracker.on_written(out_frames as u64, resampler.ratio());

        // The device plays the written frames at ITS crystal; that much true
        // time elapses, during which the engine pushes content at exactly the
        // nominal rate (engine and display share the true timebase here — the
        // DEVICE is the drifting clock under test).
        let elapsed = (out_frames as f64) / dev_rate;
        t += elapsed;
        inflow_accum += elapsed * RATE;
        let whole = (inflow_accum.floor() as usize).min(content.len());
        inflow_accum -= whole as f64;
        fifo.push(&content[..whole]);

        if t >= late_start {
            fill_sum += fifo.fill_fraction();
            fill_n += 1;
            late_drops += fifo.dropped_frames() - drops_before;
            max_applied = max_applied.max(resampler.ratio().ppm().abs());
        }
        drops_before = fifo.dropped_frames();
    }

    SimOutcome {
        avg_late_fill: fill_sum / (fill_n.max(1) as f64),
        late_drops,
        max_late_applied_ppm: max_applied,
    }
}

#[test]
fn device_clock_plus_30ppm_holds_centre_for_hours_without_drops() {
    // A +30 ppm device crystal (a perfectly ordinary HDA/vc4 part) against the
    // display clock, five simulated hours. A CLOSED skew loop settles the
    // applied ratio near +30 ppm and holds the fill at the setpoint forever;
    // an open-loop measurement (device-frame accounting) integrates into the
    // clamp at ~3–4 h, pegs the FIFO full, and late-drops every iteration.
    let out = run_sim(30.0, 5.0, 1.0, &FlipPlan::healthy());
    assert_eq!(
        out.late_drops, 0,
        "a closed skew loop must not drop after warm-up (drops in the final \
         simulated hour mean the servo saturated: open-loop skew)"
    );
    assert!(
        (out.avg_late_fill - 0.5).abs() < 0.15,
        "the fill must hold the setpoint in steady state under a +30 ppm \
         device crystal; final-hour average was {:.3}",
        out.avg_late_fill
    );
    assert!(
        out.max_late_applied_ppm < 1_000.0,
        "steady state needs only ~drift-magnitude correction; |applied| ppm \
         reached {:.1} (the clamp is 5000 — pegging means open loop)",
        out.max_late_applied_ppm
    );
}

#[test]
fn device_clock_minus_30ppm_holds_centre_without_drops() {
    // The mirror case: a SLOW device crystal. Open-loop accounting saturates
    // the other way (FIFO pegs empty — audible permanent underrun/silence
    // stretches); closed-loop holds the centre.
    let out = run_sim(-30.0, 4.0, 1.0, &FlipPlan::healthy());
    assert_eq!(out.late_drops, 0, "no drops in the final simulated hour");
    assert!(
        (out.avg_late_fill - 0.5).abs() < 0.15,
        "the fill must hold the setpoint under a -30 ppm device crystal; \
         final-hour average was {:.3}",
        out.avg_late_fill
    );
}

#[test]
fn frozen_flips_fall_back_to_fill_only_without_pegging_or_dropping() {
    // The display wedges (flips stop advancing — modeset hang, dead cable)
    // one minute in and never recovers, but the PCM keeps playing. The skew
    // term must HOLD/decay (fill-only control), not integrate the frozen
    // scanout span into the clamp: pre-fix the measured skew grows ~1 s/s,
    // pegs the servo at -5000 ppm instantly, and drops storm.
    let plan = FlipPlan {
        freeze_at: 60.0,
        resume_at: f64::INFINITY,
    };
    let out = run_sim(30.0, 0.4, 0.2, &plan);
    assert_eq!(
        out.late_drops, 0,
        "fill-only fallback must keep the loop centred — a wedged DISPLAY \
         must not storm audio drops"
    );
    assert!(
        (out.avg_late_fill - 0.5).abs() < 0.15,
        "fill-only control must hold the setpoint; late average was {:.3}",
        out.avg_late_fill
    );
    assert!(
        out.max_late_applied_ppm < 1_000.0,
        "the ratio must not peg while flips are frozen; |applied| ppm \
         reached {:.1}",
        out.max_late_applied_ppm
    );
}

#[test]
fn anchor_captures_the_pcm_delay_so_constant_ring_depth_cancels() {
    // MAJOR-1 fix (4): the anchor must capture the PCM delay AT ANCHOR TIME.
    // Pre-fix the measurement subtracts only the *current* delay, baking a
    // constant -D0 offset (-30 ms for a routine 1 440-frame ring) into every
    // skew sample — a permanent AV-sync bias the servo then chases.
    let mut tracker = SkewTracker::new();
    // 0.1 s of audio delivered before the first live-flip observation.
    tracker.on_written(4_800, RatioPpm::ZERO);
    let s0 = tracker.skew_input(1_000_000_000, Some(1_440), RatioPpm::ZERO, 48_000);
    assert!(
        s0.abs() < 1e-9,
        "the anchoring observation reports zero skew, got {s0}"
    );
    // Exactly one second of flips AND one second of audio later, ring depth
    // unchanged: a constant device-ring depth must cancel exactly.
    tracker.on_written(48_000, RatioPpm::ZERO);
    let s = tracker.skew_input(2_000_000_000, Some(1_440), RatioPpm::ZERO, 48_000);
    assert!(
        s.abs() < 1.0,
        "constant PCM ring depth must cancel via the anchor-time delay capture; \
         got {s} ms (-30 ms means the anchor baked in -D0)"
    );
}

#[test]
fn skew_fed_to_the_servo_is_clamped_to_the_input_band() {
    // MAJOR-1 fix (2): the skew INPUT is clamped to +-50 ms so a pathological
    // excursion (clock glitch, counter jump) biases the servo by at most
    // k_skew * 50 ppm — it can never peg the +-5000 ppm ratio clamp on its own.
    let mut tracker = SkewTracker::new();
    tracker.on_written(4_800, RatioPpm::ZERO);
    // Anchor, then ten seconds of audio against one second of scanout:
    // raw ~9 000 ms.
    let _ = tracker.skew_input(1_000_000_000, None, RatioPpm::ZERO, 48_000);
    tracker.on_written(480_000, RatioPpm::ZERO);
    let s = tracker.skew_input(2_000_000_000, None, RatioPpm::ZERO, 48_000);
    assert!(
        (s - 50.0).abs() < 1e-9,
        "a huge positive excursion must clamp to exactly +50 ms; got {s} ms"
    );
}

#[test]
fn flip_resume_re_anchors_and_recovers() {
    // Flips freeze five minutes in and resume five minutes later (the flip
    // timestamp jumps to the current edge, as the kernel's last-flip value
    // does). The tracker must re-anchor on resume — never difference the
    // pre-freeze anchor against the post-gap flip value — and the loop must
    // run centred and drop-free thereafter.
    let plan = FlipPlan {
        freeze_at: 300.0,
        resume_at: 600.0,
    };
    let out = run_sim(30.0, 0.4, 0.15, &plan);
    assert_eq!(
        out.late_drops, 0,
        "post-resume the loop must be drop-free (a stale anchor differenced \
         across the gap would slam the servo)"
    );
    assert!(
        (out.avg_late_fill - 0.5).abs() < 0.15,
        "post-resume the fill must re-centre; late average was {:.3}",
        out.avg_late_fill
    );
    assert!(
        out.max_late_applied_ppm < 1_000.0,
        "post-resume the applied ratio must settle, not peg; |applied| ppm \
         reached {:.1}",
        out.max_late_applied_ppm
    );
}
