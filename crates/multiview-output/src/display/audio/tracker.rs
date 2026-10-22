//! The sample-vs-scanout skew tracker: the bookkeeping behind the servo's slow
//! alignment term (DEV-B4 / display-out §5, the three-clock servo).
//!
//! The drain loop ([`super::sink`]) owns one tracker per PCM session and feeds
//! it two event streams: frames accepted by the device ([`on_written`]) and the
//! per-iteration flip-clock/delay observation ([`skew_input`], whose return
//! value is the `skew_ms` input to [`BufferServo::correction`]). Extracted from
//! the drain loop so the whole skew path is drivable in closed-loop simulations
//! over the real [`AudioFifo`](super::AudioFifo) + [`BufferServo`] +
//! [`AdaptiveResampler`](multiview_audio::AdaptiveResampler) — no hardware, no
//! threads.
//!
//! ## Why content frames (the closed loop)
//!
//! The device consumes **post-resample** frames at its own crystal rate no
//! matter what ratio the resampler applies, so accumulating raw device frames
//! would measure device-vs-flip drift the servo *cannot affect* — an open loop
//! that integrates into the ratio clamp after a few hours of ordinary ppm
//! drift. The tracker therefore accumulates **content frames**: every accepted
//! device frame is scaled by the applied ratio's
//! [`input_step`](multiview_audio::RatioPpm::input_step) (input frames per
//! output frame), so speeding the drain up or down moves the measurement and
//! the servo can *cancel* the skew (pinned by the `display_audio_drift`
//! closed-loop sims).
//!
//! [`on_written`]: SkewTracker::on_written
//! [`skew_input`]: SkewTracker::skew_input
//! [`BufferServo`]: super::BufferServo
//! [`BufferServo::correction`]: super::BufferServo::correction

use multiview_audio::RatioPpm;

/// The skew value handed to the servo is clamped to this band (ms). A
/// pathological excursion (clock glitch, counter jump) then biases the servo
/// by at most `k_skew × 50` ppm — well inside the ±5000 ppm ratio clamp — so
/// the skew term alone can never peg the resampler.
const SKEW_CLAMP_MS: f64 = 50.0;

/// How much content (seconds' worth of frames) may pass without the flip clock
/// advancing before the tracker declares the display **stalled** and falls
/// back to fill-only control. Generous against real cadences: even a 24 Hz
/// head advances every ~42 ms, an order of magnitude under this.
const STALL_SECONDS: f64 = 0.25;

/// Per-observation decay applied to the held skew while the flip clock is
/// stalled: the skew term fades toward zero (fill-only control) with a time
/// constant of ~50 observations (~0.5 s at the ~10 ms drain cadence).
const STALL_DECAY: f64 = 0.98;

/// The skew baseline captured when the tracker (re-)anchors.
#[derive(Debug, Clone, Copy)]
struct Anchor {
    /// Flip-clock value at anchor time (ns).
    flip_ns: u64,
    /// Content frames delivered at anchor time.
    content: f64,
    /// Content-frame equivalent of the PCM delay (`snd_pcm_delay`) at anchor
    /// time — captured so a constant device-ring depth cancels exactly instead
    /// of baking a `-D₀` offset into every measurement.
    delay: f64,
}

/// Per-PCM-session skew bookkeeping: accumulates the **content** frames the
/// device accepted (device frames scaled by the applied resampler ratio — see
/// the module docs for why that closes the loop) and anchors them against the
/// display flip clock to measure how far the audio content position has run
/// ahead of (positive) or behind (negative) scanout.
///
/// The anchor captures the flip value, the content position **and the PCM
/// delay** at anchor time, so a constant device-ring depth cancels exactly.
/// The returned skew is clamped to ±50 ms. A flip clock that stops advancing
/// while content keeps flowing (wedged display) is detected as **stalled**:
/// the held skew decays toward zero (fill-only fallback) and the tracker
/// re-anchors when flips resume — never differencing a pre-freeze anchor
/// against a post-gap flip value. An xrun drops the anchor (the device
/// position jumps across a recover); the next observation with a live flip
/// clock re-anchors.
#[derive(Debug, Clone)]
pub struct SkewTracker {
    /// Total content frames accepted by the PCM this session (device frames
    /// scaled by the applied ratio at write time).
    content_delivered: f64,
    /// The measurement baseline; [`None`] until the first observation after a
    /// write while the flip clock is live, after any xrun, and after a flip
    /// stall resolves.
    anchor: Option<Anchor>,
    /// The flip value seen on the previous observation (stall detection).
    last_flip_ns: u64,
    /// Content position when the flip clock last advanced (stall detection).
    content_at_flip_advance: f64,
    /// Whether the flip clock is currently considered stalled.
    stalled: bool,
    /// The last skew handed to the servo (held + decayed through a stall).
    last_skew_ms: f64,
}

impl Default for SkewTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl SkewTracker {
    /// A fresh, un-anchored tracker (one per PCM session).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            content_delivered: 0.0,
            anchor: None,
            last_flip_ns: 0,
            content_at_flip_advance: 0.0,
            stalled: false,
            last_skew_ms: 0.0,
        }
    }

    /// Record `frames` accepted by the device at the currently-applied
    /// resampler ratio.
    ///
    /// The device frames are scaled by `applied`'s input step (input frames per
    /// output frame) so the tracker accumulates **content** position — the
    /// quantity the resampler ratio actually moves, which is what closes the
    /// skew loop.
    pub fn on_written(&mut self, frames: u64, applied: RatioPpm) {
        self.content_delivered += f64_from_frames(frames) * applied.input_step();
    }

    /// The device position jumps across an xrun recover: drop the anchor (and
    /// the held skew) so the next live observation re-anchors.
    pub fn on_xrun(&mut self) {
        self.anchor = None;
        self.last_skew_ms = 0.0;
    }

    /// Whether the tracker currently holds a skew anchor (telemetry/tests).
    #[must_use]
    pub const fn is_anchored(&self) -> bool {
        self.anchor.is_some()
    }

    /// One per-iteration observation: the latest flip-clock value (`0` while
    /// no flip has landed / no flip clock exists), the PCM's current delay in
    /// device frames (`snd_pcm_delay`), the resampler ratio currently applied,
    /// and the negotiated device rate. Returns the skew (ms; positive = audio
    /// ahead of scanout, clamped to ±50 ms) to feed the servo.
    ///
    /// Un-anchored: returns `0.0` and anchors `(flip_ns, content, delay)` once
    /// at least one write has landed while the flip clock is live. Anchored:
    /// the skew is the content played since the anchor — content delivered
    /// minus the current delay, baselined against **both** captured at anchor
    /// time — against the flip-clock span since the anchor. A stalled flip
    /// clock (no advance across [`STALL_SECONDS`] of content) returns the held
    /// skew decaying toward zero and re-anchors when flips resume.
    pub fn skew_input(
        &mut self,
        flip_ns: u64,
        delay_frames: Option<i64>,
        applied: RatioPpm,
        rate: u32,
    ) -> f64 {
        if flip_ns == 0 || rate == 0 {
            return 0.0;
        }

        // Stall detection: the flip clock advancing clears a stall (re-anchor —
        // the kernel's last-flip value jumps straight to the current edge, so
        // differencing the pre-freeze anchor across the gap would slam the
        // servo); content flowing for `STALL_SECONDS` without an advance sets
        // one.
        if flip_ns != self.last_flip_ns {
            if self.stalled {
                self.stalled = false;
                self.anchor = None;
                self.last_skew_ms = 0.0;
            }
            self.last_flip_ns = flip_ns;
            self.content_at_flip_advance = self.content_delivered;
        } else if self.content_delivered - self.content_at_flip_advance
            > f64::from(rate) * STALL_SECONDS
        {
            self.stalled = true;
        }
        if self.stalled {
            // Fill-only fallback: hold the last measurement, fading it out so
            // the servo degrades to pure FIFO-fill control while the display
            // is wedged.
            self.last_skew_ms *= STALL_DECAY;
            return self.last_skew_ms;
        }

        let delay = delay_frames.map_or(0.0, |d| f64_from_delay(d.max(0)) * applied.input_step());
        match self.anchor {
            None => {
                if self.content_delivered > 0.0 {
                    self.anchor = Some(Anchor {
                        flip_ns,
                        content: self.content_delivered,
                        delay,
                    });
                }
                0.0
            }
            Some(anchor) => {
                if flip_ns > anchor.flip_ns {
                    let played = (self.content_delivered - delay) - (anchor.content - anchor.delay);
                    let audio_ms = played * 1_000.0 / f64::from(rate);
                    let scanout_ms = ns_ms(flip_ns - anchor.flip_ns);
                    let skew = (audio_ms - scanout_ms).clamp(-SKEW_CLAMP_MS, SKEW_CLAMP_MS);
                    self.last_skew_ms = skew;
                    skew
                } else {
                    0.0
                }
            }
        }
    }
}

/// `u64 → f64` for device frame counts.
#[allow(clippy::as_conversions, clippy::cast_precision_loss)]
// reason: frame counts in any real run are far below 2^53 and the result is a
// servo-grade f64; no fallible `From<u64> for f64` exists.
fn f64_from_frames(frames: u64) -> f64 {
    frames as f64
}

/// Non-negative `i64 → f64` for the PCM delay (`snd_pcm_delay`).
#[allow(clippy::as_conversions, clippy::cast_precision_loss)]
// reason: the delay is bounded by the device ring (a few thousand frames),
// far below 2^53; no fallible `From<i64> for f64` exists.
fn f64_from_delay(delay: i64) -> f64 {
    delay as f64
}

/// Nanoseconds expressed as milliseconds.
#[allow(clippy::as_conversions, clippy::cast_precision_loss)]
// reason: monotonic-clock spans are far below 2^53 ns over any run we care
// about at ms servo precision; no fallible `From<u64> for f64` exists.
fn ns_ms(ns: u64) -> f64 {
    (ns as f64) / 1_000_000.0
}
