//! True-peak (inter-sample peak) estimation by oversampling, per ITU-R
//! BS.1770-4 Annex 2.
//!
//! Sample-peak metering underestimates the real analog peak because the highest
//! point of the reconstructed waveform can fall *between* samples. BS.1770
//! prescribes oversampling (4× is sufficient up to 48 kHz) through a low-pass
//! interpolation filter, then taking the maximum absolute value of the
//! upsampled signal as the true-peak estimate (dBTP).
//!
//! This implements a 4× polyphase windowed-sinc interpolator: the prototype
//! low-pass FIR is split into four phase sub-filters, each producing one of the
//! four output samples per input sample.

const OVERSAMPLE: usize = 4;
/// Number of taps per polyphase phase (prototype length = `PHASE_TAPS *
/// OVERSAMPLE`). A 12-tap-per-phase (48-tap prototype) filter comfortably
/// suppresses imaging up to 48 kHz, matching the BS.1770 guidance.
const PHASE_TAPS: usize = 12;

/// A 4× oversampling true-peak detector for one channel.
///
/// Feed samples in order; the detector tracks the running maximum absolute
/// value of the reconstructed (upsampled) waveform.
#[derive(Debug, Clone)]
pub struct TruePeakDetector {
    /// `OVERSAMPLE` phase sub-filters, each `PHASE_TAPS` taps long.
    phases: [[f64; PHASE_TAPS]; OVERSAMPLE],
    /// The last `PHASE_TAPS` input samples in **newest-first** order: `window[0]`
    /// is the most-recent sample, `window[j]` the sample `j` steps in the past.
    /// Storing newest-first (a small `PHASE_TAPS`-element shift per push) lets the
    /// per-sample convolution be a straight `zip` of two equal-length fixed arrays
    /// — no per-tap modulo and no per-tap bounds check — which keeps `push` cheap
    /// even in a debug build, so a large catch-up block cannot stall the bake
    /// consumer (RT-8b).
    window: [f64; PHASE_TAPS],
    /// Number of samples pushed so far; the filter is primed once this reaches
    /// `PHASE_TAPS` (the window is full of real input rather than the initial
    /// zeros, so no artificial start-up step is counted as a peak).
    primed: usize,
    peak: f64,
}

impl Default for TruePeakDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl TruePeakDetector {
    /// Construct a detector with a precomputed polyphase interpolation filter.
    #[must_use]
    pub fn new() -> Self {
        Self {
            phases: build_polyphase(),
            window: [0.0; PHASE_TAPS],
            primed: 0,
            peak: 0.0,
        }
    }

    /// Push one input sample, updating the running true-peak.
    pub fn push(&mut self, x: f64) {
        // Shift the newest-first window right by one and insert `x` at the front.
        // `copy_within` is a single bounded memmove of `PHASE_TAPS-1` elements; the
        // first slot is then overwritten. Both index sites are compile-time within
        // the fixed `PHASE_TAPS` array, so this is panic-free without bounds-check
        // gymnastics. (A ring + modulo would cost a modulo per tap below; this pays
        // one small shift per push and makes the convolution a flat `zip`.)
        self.window.copy_within(0..PHASE_TAPS - 1, 1);
        if let Some(slot) = self.window.first_mut() {
            *slot = x;
        }
        if self.primed < PHASE_TAPS {
            self.primed += 1;
        }
        // Skip output until the window holds only real input — otherwise the
        // initial zeros form an artificial step whose filter overshoot would be
        // mis-counted as a true peak.
        if self.primed >= PHASE_TAPS {
            // Evaluate all OVERSAMPLE interpolated outputs for this input step.
            // `window[j]` is already the sample `j` steps in the past, so each phase
            // is a straight dot product over two equal-length `PHASE_TAPS` arrays —
            // `zip` elides the per-tap bounds check.
            for phase in &self.phases {
                let mut acc = 0.0;
                for (coeff, &sample) in phase.iter().zip(self.window.iter()) {
                    acc += coeff * sample;
                }
                let mag = acc.abs();
                if mag > self.peak {
                    self.peak = mag;
                }
            }
        }
    }

    /// The running true-peak magnitude (linear, where 1.0 == 0 dBTP).
    #[must_use]
    pub fn peak_linear(&self) -> f64 {
        self.peak
    }

    /// The filter's worst-case peak gain: `max_phase Σ|coeff|`, the `L1` norm of the
    /// most peaky phase. The true-peak of any block is bounded above by this times
    /// the block's **sample** peak (a band-limited reconstruction cannot exceed the
    /// sum of the absolute filter taps times the largest input magnitude). A
    /// feedforward limiter uses it to *prove* a block is safe — if `peak_gain_bound
    /// × sample_peak ≤ ceiling` no inter-sample peak can reach the ceiling, so the
    /// expensive per-sample oversampling FIR can be skipped for that block (only the
    /// seam window need be primed). Constant for the lifetime of the detector.
    #[must_use]
    pub fn peak_gain_bound(&self) -> f64 {
        let mut worst = 0.0f64;
        for phase in &self.phases {
            let l1: f64 = phase.iter().map(|c| c.abs()).sum();
            if l1 > worst {
                worst = l1;
            }
        }
        worst
    }

    /// Prime the FIR seam window directly from the tail of one channel of an
    /// interleaved (frame-major) input span **without** running the per-sample
    /// convolution, marking the filter primed.
    ///
    /// Pushing a whole block sample-by-sample only to read the final window state is
    /// wasteful when the block needs no peak measurement (a limiter proved it safe
    /// via [`peak_gain_bound`](Self::peak_gain_bound)). This sets the newest-first
    /// window to the last `PHASE_TAPS` samples of channel `channel` (stride
    /// `channels`), zero-padded at the front if the span is shorter — exactly the
    /// window state a full push sequence would leave, so the next block's
    /// interpolation is seam-continuous. The running peak is left untouched (the
    /// caller resets it as needed). A `channels == 0` span is a no-op.
    pub fn prime_tail_interleaved(&mut self, interleaved: &[f64], channel: usize, channels: usize) {
        if channels == 0 || channel >= channels {
            return;
        }
        let frames = interleaved.len() / channels;
        // Newest-first: window[0] = last frame's sample for this channel, window[1]
        // = the previous frame's, … `j` frames back.
        for (j, slot) in self.window.iter_mut().enumerate() {
            *slot = frames
                .checked_sub(1 + j)
                .map(|frame| frame * channels + channel)
                .and_then(|idx| interleaved.get(idx).copied())
                .unwrap_or(0.0);
        }
        if frames > 0 {
            self.primed = PHASE_TAPS;
        }
    }

    /// Reset only the running peak to zero, **keeping** the FIR history and primed
    /// state intact.
    ///
    /// A normal [`peak_linear`](Self::peak_linear) is a running maximum over the
    /// whole signal — it never decays. A feedforward limiter that needs the peak of
    /// *just the most recent span* (e.g. the loudnorm per-block true-peak limiter,
    /// AUD-6) zeroes the peak between spans while preserving the polyphase ring so
    /// the inter-sample interpolation stays continuous across the span boundary
    /// (no artificial seam step).
    pub fn reset_peak(&mut self) {
        self.peak = 0.0;
    }

    /// Scale every retained input sample in the FIR history ring by `factor`.
    ///
    /// The interpolation filter is linear, so scaling the stored input history by a
    /// constant is exactly equivalent to having pushed those inputs already scaled.
    /// A feedforward limiter that decides a **block-wide** attenuation only after
    /// probing the un-attenuated block (the loudnorm true-peak limiter, AUD-6) uses
    /// this to fold that attenuation into the persistent seam history in `O(taps)`
    /// — instead of re-running the FIR over the whole block a second time with the
    /// attenuated samples — so the next block's inter-sample interpolation continues
    /// from the correct post-attenuation tail with no seam step. Does not touch the
    /// running peak (reset that separately with [`reset_peak`](Self::reset_peak)).
    pub fn scale_history(&mut self, factor: f64) {
        for s in &mut self.window {
            *s *= factor;
        }
    }

    /// The running true-peak in dBTP, or `None` if no non-zero sample has been
    /// seen (silence has no defined peak level).
    #[must_use]
    pub fn peak_dbtp(&self) -> Option<f64> {
        if self.peak <= 0.0 {
            None
        } else {
            Some(20.0 * self.peak.log10())
        }
    }
}

/// Exact small-index `usize -> f64` for filter design (values are < 48).
#[allow(clippy::as_conversions, clippy::cast_precision_loss)] // reason: filter-design indices are tiny (< 2^53); lossless, no fallible From.
fn idx_f64(n: usize) -> f64 {
    n as f64
}

/// Build the `OVERSAMPLE` polyphase sub-filters from a Blackman-windowed sinc
/// interpolation prototype.
///
/// The sinc is centred exactly on a phase-0 sample (an integer multiple of
/// `OVERSAMPLE`), so phase 0 is a pure passthrough (it reproduces the original
/// samples) and phases `1..OVERSAMPLE` interpolate between them. Each phase is
/// then normalised to unity DC gain, so a constant input reconstructs to the
/// same constant and the true-peak of a passband tone is not inflated by filter
/// gain.
fn build_polyphase() -> [[f64; PHASE_TAPS]; OVERSAMPLE] {
    let proto_len = PHASE_TAPS * OVERSAMPLE;
    // Centre on a phase-0 sample => phase 0 is an exact passthrough.
    let center = idx_f64((PHASE_TAPS / 2) * OVERSAMPLE);
    let cutoff = 1.0 / idx_f64(OVERSAMPLE); // passband edge = original Nyquist
    let two_pi = 2.0 * core::f64::consts::PI;
    let last = idx_f64(proto_len - 1);
    let mut proto = vec![0.0f64; proto_len];
    for (n, tap) in proto.iter_mut().enumerate() {
        let t = idx_f64(n) - center;
        let x = two_pi * cutoff * t;
        let sinc = if x.abs() < 1e-9 { 1.0 } else { x.sin() / x };
        // Blackman window over the prototype span.
        let r = idx_f64(n) / last;
        let window = 0.42 - 0.5 * (two_pi * r).cos() + 0.08 * (2.0 * two_pi * r).cos();
        *tap = sinc * window;
    }

    // Polyphase decomposition: phase p uses prototype taps p, p+OVERSAMPLE, …
    // Normalise each phase to unity DC gain.
    let mut phases = [[0.0f64; PHASE_TAPS]; OVERSAMPLE];
    for (p, phase) in phases.iter_mut().enumerate() {
        let mut sum = 0.0;
        for (j, tap) in phase.iter_mut().enumerate() {
            let proto_idx = j * OVERSAMPLE + p;
            let coeff = proto.get(proto_idx).copied().unwrap_or(0.0);
            *tap = coeff;
            sum += coeff;
        }
        if sum.abs() > 1e-12 {
            for tap in phase.iter_mut() {
                *tap /= sum;
            }
        }
    }
    phases
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    // reason: loop-index to float is exact for the small ranges used here.
    #![allow(clippy::as_conversions, clippy::cast_precision_loss)]
    use super::*;

    #[test]
    fn dc_passes_at_unity() {
        let mut d = TruePeakDetector::new();
        for _ in 0..200 {
            d.push(0.5);
        }
        approx::assert_abs_diff_eq!(d.peak_linear(), 0.5, epsilon = 0.02);
    }

    #[test]
    fn silence_has_no_dbtp() {
        let mut d = TruePeakDetector::new();
        for _ in 0..100 {
            d.push(0.0);
        }
        assert_eq!(d.peak_dbtp(), None);
    }

    /// `peak_gain_bound` must be a true upper bound: for ANY bounded input the FIR
    /// output magnitude can never exceed `peak_gain_bound × max|input|`. This is the
    /// soundness guarantee the loudnorm limiter's sample-peak pre-screen relies on
    /// (it skips the FIR only when `sample_peak × peak_gain_bound ≤ ceiling`).
    #[test]
    fn peak_gain_bound_is_a_true_upper_bound() {
        let d = TruePeakDetector::new();
        let bound = d.peak_gain_bound();
        assert!(bound >= 1.0, "DC-unity filter has L1 >= 1, got {bound}");
        // Drive a worst-case alternating ±1 signal (maximises a band-limited
        // reconstruction's inter-sample overshoot) and confirm the measured peak
        // never exceeds the analytic bound.
        let mut probe = TruePeakDetector::new();
        for n in 0..500 {
            probe.push(if n % 2 == 0 { 1.0 } else { -1.0 });
        }
        assert!(
            probe.peak_linear() <= bound + 1e-9,
            "true-peak {} exceeded the peak_gain_bound {bound}",
            probe.peak_linear()
        );
    }

    /// `prime_tail_interleaved` must leave the filter in exactly the window state a
    /// full per-sample `push` sequence over the same channel would — so a limiter
    /// that skips the FIR for a safe block keeps the next block seam-continuous.
    #[test]
    fn prime_tail_matches_full_push() {
        let channels = 2;
        // Interleaved stereo ramp; check channel 1 (the strided one).
        let frames = 50;
        let mut interleaved = Vec::with_capacity(frames * channels);
        for f in 0..frames {
            interleaved.push(f as f64 * 0.01); // ch 0
            interleaved.push(1.0 - f as f64 * 0.01); // ch 1
        }

        // Full push of channel 1 sample-by-sample.
        let mut pushed = TruePeakDetector::new();
        for f in 0..frames {
            pushed.push(interleaved[f * channels + 1]);
        }

        // Prime-from-tail of channel 1.
        let mut primed = TruePeakDetector::new();
        primed.prime_tail_interleaved(&interleaved, 1, channels);

        // The two must now produce identical FIR output on the SAME next sample
        // (i.e. their windows are equal). Push one more sample through each and
        // compare the running peak after a reset.
        pushed.reset_peak();
        primed.reset_peak();
        let next = 0.5;
        pushed.push(next);
        primed.push(next);
        approx::assert_abs_diff_eq!(pushed.peak_linear(), primed.peak_linear(), epsilon = 1e-12);
    }
}
