//! The buffer-level servo (DEV-B4 / display-out §5: the three-clock servo).
//!
//! Three independent crystals meet at the display-audio sink: the engine tick,
//! the display **pixel** clock (observed through the flip timestamps the sink
//! exports — `display::sink` `last_flip_ns`), and the ALSA **sample** clock.
//! They drift at ppm levels, so a fixed resample ratio would slowly over- or
//! under-fill the audio FIFO and walk AV sync off the scanout clock. The servo
//! closes that loop: it watches the FIFO fill level (the fast, local error) plus
//! the long-run sample-vs-flip skew (the slow, absolute error) and emits a
//! resample-ratio correction in ppm. The display-audio sink feeds that ratio to
//! multiview-audio's [`AdaptiveResampler`](multiview_audio::AdaptiveResampler) —
//! the mpv/Kodi "display-resample" technique — so the resampler, not new
//! machinery, does the rate adjustment.
//!
//! It is a small **PI controller** on the fill error with a proportional skew
//! bias, all clamped to the resampler's audible band
//! ([`RatioPpm::MAX_PPM`](multiview_audio::RatioPpm::MAX_PPM)). Pure arithmetic —
//! fully unit-testable, no hardware.

use multiview_audio::RatioPpm;

/// PI servo mapping FIFO fill level (and sample-vs-flip skew) to a resample
/// ratio correction in ppm.
///
/// Holds the integral term across calls, so a *persistent* small error is
/// corrected even when the proportional term alone would leave a steady-state
/// offset. Construct with [`BufferServo::new`]; call
/// [`correction`](Self::correction) once per servo tick. [`Clone`] yields a
/// fresh integrator (used to evaluate a single-shot response in tests).
#[derive(Debug, Clone)]
pub struct BufferServo {
    /// Target FIFO fill as a fraction of capacity (mid-buffer gives equal
    /// headroom against both over- and under-run).
    setpoint: f64,
    /// Proportional gain on the fill error → ppm.
    kp: f64,
    /// Integral gain on the accumulated fill error → ppm.
    ki: f64,
    /// Proportional gain on the measured skew (ms) → ppm.
    k_skew: f64,
    /// Accumulated fill error (the integral state), pre-clamped to keep the
    /// controller from winding up beyond the output band.
    integral: f64,
}

impl Default for BufferServo {
    fn default() -> Self {
        Self::new()
    }
}

impl BufferServo {
    /// A servo with the default broadcast-safe tuning: mid-buffer setpoint and
    /// gains that pull a badly-off FIFO back within a few hundred ticks while
    /// staying well inside the resampler band in steady state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            setpoint: 0.5,
            // Full-scale fill error (±0.5) maps to a strong but sub-band push;
            // ki accumulates the residual; k_skew nudges absolute alignment.
            //
            // Gain coupling: kp is dimensioned against the drain loop's
            // quantum-per-iteration cadence — the per-iteration closed-loop
            // fill gain is kp × 1e-6 × DRAIN_FRAMES / fifo_capacity ≈ 2.3e-4
            // with the sink's 480-frame quantum and the pipeline's 8192-frame
            // FIFO (heavily damped, ~40 s settling). Retune kp if the quantum
            // or the FIFO capacity changes materially; pushing that product
            // toward 1 makes the loop oscillate. The skew term is bounded by
            // construction: its input is clamped to ±50 ms upstream
            // (`SkewTracker`), so k_skew × 50 = 1000 ppm is the most the slow
            // term can ever demand — it cannot peg the ±5000 ppm band alone.
            kp: 4_000.0,
            ki: 50.0,
            k_skew: 20.0,
            integral: 0.0,
        }
    }

    /// The target fill fraction.
    #[must_use]
    pub const fn setpoint(&self) -> f64 {
        self.setpoint
    }

    /// Compute the **drain-rate** correction for the current `fill_fraction`
    /// (0..=1) and `skew_ms` (the [`skew_ms`](crate::display::audio::skew_ms)
    /// measurement: audio minus scanout elapsed, positive = audio **ahead** of
    /// the flip clock).
    ///
    /// Sign convention — the output is *drain* ppm, **not** the resampler's
    /// output-per-input ppm (the two are reciprocal; see [`drain_ratio`]): a
    /// fill **above** setpoint must be drained faster ⇒ *positive* ppm (more
    /// FIFO content consumed per device second). A persistent positive skew
    /// (audio ahead of scanout) also means content is being consumed too fast ⇒
    /// the `-k_skew` term pulls the drain ppm down so scanout catches back up.
    /// The result is clamped to the resampler's band.
    pub fn correction(&mut self, fill_fraction: f64, skew_ms: f64) -> RatioPpm {
        let fill = if fill_fraction.is_finite() {
            fill_fraction.clamp(0.0, 1.0)
        } else {
            self.setpoint
        };
        let error = fill - self.setpoint;

        // Anti-windup: accumulate, then clamp the integral so its ppm
        // contribution alone can never exceed the band.
        self.integral += error;
        let int_clamp = RatioPpm::MAX_PPM / self.ki.max(f64::MIN_POSITIVE);
        self.integral = self.integral.clamp(-int_clamp, int_clamp);

        let skew = if skew_ms.is_finite() { skew_ms } else { 0.0 };
        let ppm = self.kp * error + self.ki * self.integral - self.k_skew * skew;
        RatioPpm::from_ppm(ppm)
    }
}

/// Map the servo's **drain** ppm onto the [`AdaptiveResampler`]'s
/// **output-frames-per-input** ppm: the exact reciprocal ratio.
///
/// The two speak opposite dialects. The device consumes output frames at a
/// fixed sample rate `R`, so input (FIFO content) is consumed at
/// `R / (1 + p_resampler)` — draining the FIFO *faster* (positive servo ppm)
/// requires emitting **fewer** output frames per input frame (negative
/// resampler ppm), and vice-versa. Applying the servo ppm directly would be
/// positive feedback: a too-full FIFO would stretch its content over *more*
/// device time and fill further until it saturates at the clamp. Pinned by the
/// `display_audio_servo_physics` closed-loop test.
///
/// [`AdaptiveResampler`]: multiview_audio::AdaptiveResampler
#[must_use]
pub fn drain_ratio(servo: RatioPpm) -> RatioPpm {
    // ratio_resampler = 1 / ratio_drain, expressed back in ppm.
    RatioPpm::from_ppm((1.0 / servo.frame_ratio() - 1.0) * 1_000_000.0)
}

/// The measured audio-vs-scanout skew in milliseconds: how far the audio
/// sample clock has run **ahead of** the display flip clock since the two were
/// anchored (negative = audio behind).
///
/// `audio_frames` is the count of device frames played since the anchor (the
/// frames delivered to the PCM minus its current delay, when the backend can
/// report one), `sample_rate` the negotiated PCM rate, and
/// `scanout_elapsed_ns` the span between the anchor flip timestamp and the
/// latest one (`display::sink` `last_flip_ns`). Degenerate inputs (zero rate)
/// yield `0.0` — the servo then holds sync on the FIFO term alone.
#[must_use]
pub fn skew_ms(audio_frames: u64, sample_rate: u32, scanout_elapsed_ns: u64) -> f64 {
    if sample_rate == 0 {
        return 0.0;
    }
    let audio_ms = frames_ms(audio_frames, sample_rate);
    let scanout_ms = ns_ms(scanout_elapsed_ns);
    audio_ms - scanout_ms
}

/// Frames at `rate` expressed as milliseconds.
#[allow(clippy::as_conversions, clippy::cast_precision_loss)]
// reason: frame counts in any real run are far below 2^53 and the result is a
// telemetry-grade f64; no fallible `From<u64> for f64` exists.
fn frames_ms(frames: u64, rate: u32) -> f64 {
    (frames as f64) * 1_000.0 / f64::from(rate)
}

/// Nanoseconds expressed as milliseconds.
#[allow(clippy::as_conversions, clippy::cast_precision_loss)]
// reason: monotonic-clock spans are far below 2^53 ns over any run we care
// about at ms telemetry precision; no fallible `From<u64> for f64` exists.
fn ns_ms(ns: u64) -> f64 {
    (ns as f64) / 1_000_000.0
}
