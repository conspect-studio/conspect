//! The mix/route model: a program bus plus discrete per-input tracks, with a
//! per-input gain/route matrix (ADR-R005).
//!
//! This is the pure-Rust *model* of audio routing — it operates on in-memory
//! [`AudioBlock`]s. The libav decode/resample that fills those blocks lives
//! behind the off-by-default `ffmpeg` feature and is not part of this layer.
//!
//! Routing rules (per the ADR):
//! - Each input fans out to (a) a clean **discrete track** carried unaltered
//!   and (b) the mixed **program bus** scaled by the input's program route
//!   gain.
//! - An input with no fresh block this tick contributes **silence** to the bus
//!   (gap-free; the mixer never stalls waiting for an input).
//! - The program bus is hard-limited to the `[-1.0, 1.0]` sample domain so it
//!   never overflows.
use crate::error::{AudioError, Result};
use crate::format::{AudioBlock, AudioFormat};

/// A per-sample gain envelope over a fixed span of frames — the pop-avoidance
/// primitive (RT-9, decoupled-routing §5 "AUDIO pop-avoidance").
///
/// A hard cut at a buffer edge is sample-accurate but waveform-discontinuous, so
/// it **clicks**. A [`GainRamp`] instead moves a strip's gain smoothly from
/// [`from`](Self::from) to [`to`](Self::to) over [`frames_total`](Self::frames_total)
/// frames using an **equal-power (sin/cos) taper**, applied *per sample* inside
/// [`Mixer::mix_program`]'s loop so the gain moves **within** a tick block, not
/// stepping at tick boundaries. Paired with an opposite ramp on the other strip
/// (one [`up`](Self::up), one [`down`](Self::down)), the summed power stays
/// constant across the fade (`sin² + cos² = 1`) — no audible dip and no click.
///
/// [`frames_done`](Self::frames_done) is the running position; the mixer advances
/// it by each tick's sample budget ([`Mixer::advance_ramps`]) so a fade longer
/// than one tick block carries seamlessly from one block into the next.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GainRamp {
    /// Linear gain the envelope starts from (at `frames_done == 0`).
    pub from: f64,
    /// Linear gain the envelope ends at (at `frames_done >= frames_total`).
    pub to: f64,
    /// Total length of the envelope in frames.
    pub frames_total: usize,
    /// Frames already elapsed in the envelope (advanced once per tick by the
    /// tick's sample budget).
    pub frames_done: usize,
}

impl GainRamp {
    /// An equal-power **fade-in**: gain rises `0.0 → 1.0` over `frames` frames
    /// along a `sin` taper (the rising half of the equal-power pair).
    #[must_use]
    pub const fn up(frames: usize) -> Self {
        Self {
            from: 0.0,
            to: 1.0,
            frames_total: frames,
            frames_done: 0,
        }
    }

    /// An equal-power **fade-out**: gain falls `1.0 → 0.0` over `frames` frames
    /// along a `cos` taper (the falling half of the equal-power pair).
    #[must_use]
    pub const fn down(frames: usize) -> Self {
        Self {
            from: 1.0,
            to: 0.0,
            frames_total: frames,
            frames_done: 0,
        }
    }

    /// Whether the envelope has run to completion (no further movement).
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.frames_done >= self.frames_total
    }

    /// The linear gain at envelope position `frame` (frames elapsed since the
    /// ramp began), interpolated from [`from`](Self::from) to [`to`](Self::to)
    /// along an **equal-power** curve.
    ///
    /// The progress `p = frame / frames_total ∈ [0, 1]` is mapped through a
    /// quarter-sine so that a `0 → 1` ramp follows `sin(p · π/2)` and a `1 → 0`
    /// ramp follows `cos(p · π/2)`; the two are complementary
    /// (`sin² + cos² = 1`), keeping a paired up/down cross-fade at constant
    /// power. A zero-length ramp returns [`to`](Self::to) immediately (a hard
    /// step). `frame` at/after `frames_total` clamps to [`to`](Self::to).
    #[must_use]
    pub fn envelope_at(&self, frame: usize) -> f32 {
        if self.frames_total == 0 || frame >= self.frames_total {
            return clamp_gain(self.to);
        }
        // p ∈ [0, 1): linear progress through the ramp.
        let p = ratio(frame, self.frames_total);
        // Equal-power shaping. A rising ramp (to >= from) follows `sin`; a
        // falling ramp follows `cos`. `sin² + cos² = 1`, so a paired up/down
        // cross-fade holds constant power (no -3 dB midpoint dip a linear fade
        // would cause).
        let theta = p * std::f64::consts::FRAC_PI_2;
        let rising = self.to >= self.from;
        let shaped = if rising { theta.sin() } else { theta.cos() };
        // Map the [0,1] equal-power shape onto the [lo, hi] gain span (handles
        // non-0/1 endpoints too): a rising ramp scales lo→hi, a falling ramp
        // hi→lo, both via the same `shaped` magnitude.
        let (lo, hi) = if rising {
            (self.from, self.to)
        } else {
            (self.to, self.from)
        };
        let gain = lo + (hi - lo) * shaped;
        clamp_gain(gain)
    }
}

/// The fraction `frame / total` as an `f64`, without an `as` cast (bounded
/// frame counts widen losslessly through `u32`; an out-of-`u32`-range value —
/// not reachable for real ~10 ms ramps — saturates rather than wrapping).
fn ratio(frame: usize, total: usize) -> f64 {
    let f = u32::try_from(frame).map_or(f64::from(u32::MAX), f64::from);
    let t = u32::try_from(total.max(1)).map_or(f64::from(u32::MAX), f64::from);
    f / t
}

/// Clamp an envelope gain to the `[-1.0, 1.0]` `f32` linear-gain domain (the
/// equal-power taper stays well within this; the clamp is belt-and-braces).
#[allow(clippy::as_conversions, clippy::cast_possible_truncation)] // reason: value is clamped to [-1,1]; f64->f32 narrowing is exact-enough and bounded.
fn clamp_gain(v: f64) -> f32 {
    v.clamp(-1.0, 1.0) as f32
}

/// A handle to a routing endpoint. Currently an input slot; the type leaves
/// room for future endpoints (e.g. named submixes) without changing call sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RoutePoint {
    input: usize,
}

impl RoutePoint {
    /// A route point referring to mixer input `index`.
    #[must_use]
    pub const fn input(index: usize) -> Self {
        Self { input: index }
    }

    /// The input index this route point refers to.
    #[must_use]
    pub const fn index(self) -> usize {
        self.input
    }
}

/// One input strip: its identifier, program-route gain, the latest submitted
/// block (if any this tick), and an optional per-sample [`GainRamp`] for a
/// pop-free cross-fade.
#[derive(Debug)]
struct InputStrip {
    id: String,
    program_gain: f64,
    routed_to_program: bool,
    latest: Option<AudioBlock>,
    /// An in-flight per-sample gain envelope (RT-9). When present, the strip's
    /// effective gain at frame `n` of a mix is `program_gain · ramp.envelope_at(
    /// ramp.frames_done + n)`, so the envelope moves *within* the tick block.
    gain_ramp: Option<GainRamp>,
}

/// A program-bus + discrete-track mixer over a fixed working [`AudioFormat`].
#[derive(Debug)]
pub struct Mixer {
    format: AudioFormat,
    inputs: Vec<InputStrip>,
}

impl Mixer {
    /// Create a mixer whose program bus runs at `format`.
    #[must_use]
    pub fn new(format: AudioFormat) -> Self {
        Self {
            format,
            inputs: Vec::new(),
        }
    }

    /// The mixer's working format (also the program bus format).
    #[must_use]
    pub const fn format(&self) -> AudioFormat {
        self.format
    }

    /// Register a new input strip, returning its [`RoutePoint`]. The input is
    /// not routed to the program bus until [`Mixer::route_to_program`] is
    /// called.
    pub fn add_input(&mut self, id: impl Into<String>) -> RoutePoint {
        let index = self.inputs.len();
        self.inputs.push(InputStrip {
            id: id.into(),
            program_gain: 1.0,
            routed_to_program: false,
            latest: None,
            gain_ramp: None,
        });
        RoutePoint::input(index)
    }

    /// Route an input to the program bus at linear `gain`. Calling again
    /// updates the gain. A no-op (but not an error) for an unknown input — use
    /// only handles returned by [`Mixer::add_input`].
    pub fn route_to_program(&mut self, point: RoutePoint, gain: f64) {
        if let Some(strip) = self.inputs.get_mut(point.index()) {
            strip.program_gain = gain;
            strip.routed_to_program = true;
        }
    }

    /// Remove an input from the program bus (its discrete track remains).
    pub fn unroute_from_program(&mut self, point: RoutePoint) {
        if let Some(strip) = self.inputs.get_mut(point.index()) {
            strip.routed_to_program = false;
        }
    }

    /// Attach a per-sample [`GainRamp`] to an input's program contribution
    /// (RT-9). While present the strip's gain follows the ramp envelope
    /// per-sample inside [`mix_program`](Self::mix_program), advanced once per
    /// tick by [`advance_ramps`](Self::advance_ramps). A no-op for an unknown
    /// input. Pass a fresh ramp (`frames_done == 0`); calling again replaces any
    /// in-flight ramp.
    pub fn set_gain_ramp(&mut self, point: RoutePoint, ramp: GainRamp) {
        if let Some(strip) = self.inputs.get_mut(point.index()) {
            strip.gain_ramp = Some(ramp);
        }
    }

    /// Drop any in-flight [`GainRamp`] on an input (the strip returns to its
    /// steady `program_gain`). A no-op for an unknown input.
    pub fn clear_gain_ramp(&mut self, point: RoutePoint) {
        if let Some(strip) = self.inputs.get_mut(point.index()) {
            strip.gain_ramp = None;
        }
    }

    /// The in-flight [`GainRamp`] on an input, if any (primarily for the program
    /// bus to detect completion and unroute the faded-out strip).
    #[must_use]
    pub fn gain_ramp(&self, point: RoutePoint) -> Option<GainRamp> {
        self.inputs.get(point.index())?.gain_ramp
    }

    /// Advance every in-flight ramp by `frames` (one tick's sample budget),
    /// clamping each at completion and dropping ramps that have run out so the
    /// strip settles at its final steady gain. Call once per tick **after**
    /// [`mix_program`](Self::mix_program) has consumed the pre-advance envelope
    /// position for this tick.
    pub fn advance_ramps(&mut self, frames: usize) {
        for strip in &mut self.inputs {
            if let Some(ramp) = strip.gain_ramp.as_mut() {
                ramp.frames_done = ramp.frames_done.saturating_add(frames);
                if ramp.is_complete() {
                    // Settle the steady gain at the ramp's endpoint and retire it
                    // so future ticks are a plain scalar (no per-sample cost).
                    strip.program_gain *= f64::from(ramp.envelope_at(ramp.frames_total));
                    strip.gain_ramp = None;
                }
            }
        }
    }

    /// Submit the latest decoded block for an input this tick.
    ///
    /// # Errors
    ///
    /// - [`AudioError::UnknownInput`] if `point` is not a known input.
    /// - [`AudioError::FormatMismatch`] if the block's format differs from the
    ///   mixer's working format.
    pub fn submit(&mut self, point: RoutePoint, block: AudioBlock) -> Result<()> {
        if block.format() != self.format {
            return Err(AudioError::FormatMismatch {
                expected_rate: self.format.sample_rate(),
                expected_channels: self.format.channel_count(),
                actual_rate: block.format().sample_rate(),
                actual_channels: block.format().channel_count(),
            });
        }
        let strip = self
            .inputs
            .get_mut(point.index())
            .ok_or(AudioError::UnknownInput(point.index()))?;
        strip.latest = Some(block);
        Ok(())
    }

    /// The discrete (clean, unaltered) track for an input this tick, or `None`
    /// if the input is unknown or has not submitted a block.
    #[must_use]
    pub fn discrete_track(&self, point: RoutePoint) -> Option<&AudioBlock> {
        self.inputs.get(point.index())?.latest.as_ref()
    }

    /// The identifier of an input, if known.
    #[must_use]
    pub fn input_id(&self, point: RoutePoint) -> Option<&str> {
        self.inputs.get(point.index()).map(|s| s.id.as_str())
    }

    /// The current steady program-route gain of an input, if known (the base the
    /// per-sample [`GainRamp`] envelope multiplies). Returns `None` for an
    /// unknown input.
    #[must_use]
    pub fn program_gain(&self, point: RoutePoint) -> Option<f64> {
        self.inputs.get(point.index()).map(|s| s.program_gain)
    }

    /// Mix all program-routed inputs into the program bus for this tick.
    ///
    /// The bus length is the longest submitted block; shorter inputs (and
    /// dropped inputs) contribute silence for the missing frames. The result is
    /// hard-limited to `[-1.0, 1.0]`. Returns a silent block of length 0 when no
    /// input has submitted anything.
    ///
    /// ## Per-sample gain envelope (RT-9)
    /// A strip carrying a [`GainRamp`] contributes a **per-sample** gain: at
    /// frame `n` of the block its effective gain is
    /// `program_gain · ramp.envelope_at(ramp.frames_done + n)`, so the gain moves
    /// *within* the block (not a single per-tick scalar that would step at the
    /// tick boundary). Paired up/down equal-power ramps on the old and new strips
    /// give a click-free, constant-power cross-fade. This is read-only over the
    /// ramp state; advance it with [`advance_ramps`](Self::advance_ramps) once the
    /// mix is taken.
    #[must_use]
    pub fn mix_program(&self) -> Option<AudioBlock> {
        let channels = self.format.channel_count();
        if channels == 0 {
            return None;
        }
        // Longest routed-and-submitted block sets the bus length.
        let frames = self
            .inputs
            .iter()
            .filter(|s| s.routed_to_program)
            .filter_map(|s| s.latest.as_ref())
            .map(AudioBlock::frame_count)
            .max()
            .unwrap_or(0);

        let mut acc = vec![0.0f64; frames.saturating_mul(channels)];
        for strip in self.inputs.iter().filter(|s| s.routed_to_program) {
            let Some(block) = strip.latest.as_ref() else {
                continue; // dropout => contributes silence
            };
            let src = block.interleaved();
            match strip.gain_ramp {
                // Steady gain: one scalar across the whole block (the common,
                // no-cross-fade path — unchanged behaviour).
                None => {
                    let gain = strip.program_gain;
                    for (dst, &s) in acc.iter_mut().zip(src.iter()) {
                        *dst += gain * f64::from(s);
                    }
                }
                // In-flight ramp: a PER-SAMPLE envelope. The gain is recomputed
                // each frame from the ramp's running position, so it moves
                // smoothly across the block (and, via `frames_done`, across the
                // tick boundary) instead of stepping once per tick.
                Some(ramp) => {
                    let base = strip.program_gain;
                    // Iterate whole frames; each frame applies one envelope value
                    // to all of its channels (a gain envelope is per-frame, not
                    // per-channel). Bounds come from the shorter of acc/src so no
                    // index can go out of range.
                    let block_frames = src.len() / channels;
                    let dst_frames = acc.len() / channels;
                    let n = block_frames.min(dst_frames);
                    for frame in 0..n {
                        let env =
                            f64::from(ramp.envelope_at(ramp.frames_done.saturating_add(frame)));
                        let gain = base * env;
                        let off = frame * channels;
                        if let (Some(dst), Some(s)) = (
                            acc.get_mut(off..off.saturating_add(channels)),
                            src.get(off..off.saturating_add(channels)),
                        ) {
                            for (d, &sv) in dst.iter_mut().zip(s.iter()) {
                                *d += gain * f64::from(sv);
                            }
                        }
                    }
                }
            }
        }

        let samples: Vec<f32> = acc.iter().map(|&v| clamp_sample(v)).collect();
        // Length is `frames * channels` by construction, so this never errors.
        AudioBlock::from_interleaved(self.format, samples).ok()
    }
}

/// Hard-limit a mixed `f64` sample to the `[-1.0, 1.0]` `f32` sample domain.
#[allow(clippy::as_conversions, clippy::cast_possible_truncation)] // reason: value is clamped to [-1,1]; f64->f32 narrowing is exact-enough and bounded.
fn clamp_sample(v: f64) -> f32 {
    v.clamp(-1.0, 1.0) as f32
}
