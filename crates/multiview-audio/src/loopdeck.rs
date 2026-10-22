//! The media-player audio **loop deck** (ADR-T019): a pure, deterministic,
//! libav-free buffer-and-replay looper — the audio analogue of the video
//! `MediaPlayer` transport core.
//!
//! # What it does
//!
//! A looping/vamping media player ([ADR-0097](../../../../docs/decisions/ADR-0097.md))
//! must loop its embedded audio on the **same wrap instant** as the video, with a
//! **click-free crossfade** at the loop seam. The video tile loops by an in-place
//! container seek + decoder flush; audio cannot do the same and stay glitch-free —
//! an audio container has no IDR, so a container audio seek lands on a packet
//! boundary (sample-imprecise = a click every lap). Instead, the player's
//! `[vamp_in, vamp_out)` audio segment is decoded **once** into a bounded buffer
//! and this deck **replays it as an overlap-add loop**:
//!
//! - the loop period is exactly the decoded 48 kHz sample count of the
//!   `[vamp_in, vamp_out)` *time range* — which is the audio duration of the
//!   `vamp_len` video frames, so the audio lap and the video lap are the **same
//!   media-time length at any cadence** (sample-lock; no `SampleClock::total_at`
//!   tick delta, which would conflate source-frame and output-tick indices —
//!   [ADR-T019](../../../../docs/decisions/ADR-T019.md) §3);
//! - the seam crossfades the previous lap's **tail** (real content past the loop
//!   point) into this lap's **head** with a **correlation-adaptive** law:
//!   equal-power (the BUILT [`GainRamp`](crate::mixer::GainRamp) curve) for a
//!   decorrelated seam (flat power), **linear** for a correlated/tonal one (flat
//!   amplitude — a literal equal-power fade swells `√2 ≈ +3 dB` on a sustained
//!   tone). Both are C0-continuous; the choice removes the *level* transient.
//!
//! [`LoopDeck::read_at`] is a **pure function of the absolute output frame**, so a
//! forced cursor realign (the program bus's read-cursor catch-up under load) still
//! emits the correctly faded seam for the landed position — no skipped /
//! un-crossfaded seam (rule 26).
//!
//! # Invariants
//!
//! The deck never blocks and never reads a wall clock: it returns samples, it does
//! not pace (inv #1). The loop length is an integer **sample** count; the only
//! floats are the fade gains and the one-time seam correlation (genuinely
//! continuous quantities), never time (inv #3). It is owned by the audio decode
//! thread (single-threaded, like the video `MediaPlayer`); it adds no channel into
//! the engine (inv #10).

use std::cell::Cell;
use std::f64::consts::FRAC_PI_2;

use multiview_core::time::Rational;

use crate::format::{AudioBlock, AudioFormat};

/// The default seam crossfade window, in frames at 48 kHz (~10 ms — the
/// established anti-click ramp length, matching the per-strip mute envelope of
/// [ADR-0059](../../../../docs/decisions/ADR-0059.md) §4). Clamped to `L/2` for a
/// short segment so the fade-in and fade-out windows never overlap each other.
pub const DEFAULT_CROSSFADE_FRAMES: usize = 480;

/// The hard cap on a player audio loop body, in **seconds** (ADR-T019 §5): the
/// segment is held in RAM for the channel's life, so it is bounded by an explicit
/// ceiling, not operator restraint (safety §5). At 48 kHz stereo `f32`
/// (`0.366 MiB/s`) this is ~220 MiB. A declared vamp window longer than this is
/// refused for audio — the player loops video normally and rides audio silence.
pub const MAX_LOOP_SECONDS: u64 = 600;

/// The canonical audio sample rate the loop deck and bus operate at (48 kHz). The
/// frame→sample window math ([`LoopDeck::from_clip_window`]) assumes the decoded
/// clip is at this rate (every per-source decode resamples to it before the bus
/// sees it, ADR-R005).
pub const SAMPLE_RATE: u64 = 48_000;

/// How many frames a decoded vamp window may fall short of its frame-derived
/// target before the deck **refuses** it (rather than looping a shifted-length
/// body — Defect 3). A few samples of resampler-edge shortfall (≈ 1 ms at 48 kHz)
/// is zero-padded and tolerated; anything more is a truncated asset → ride
/// silence. 64 frames ≈ 1.3 ms — inaudible at the loop point.
const SHORTFALL_TOLERANCE_FRAMES: usize = 64;

/// The correlation threshold above which the seam is treated as **correlated**
/// (a sustained tone/pad continuing across the loop) and a **linear**
/// constant-amplitude crossfade is used instead of equal-power. Below it the seam
/// is decorrelated and equal-power (constant-power) is used. `0.5` is the
/// midpoint of the normalized cross-correlation `[-1, 1]`: a value at or above it
/// means the two seam legs track each other closely enough that summing their
/// equal-power-weighted amplitudes would swell.
const CORRELATION_THRESHOLD: f64 = 0.5;

/// The transport state of a loop deck. Mirrors the publishing subset of the video
/// [`MediaPlayerState`](crate::format) — a deck only ever **vamps** (loops) or is
/// held/ended; it does not carry the video machine's `Cued`/`Loading` states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeckState {
    /// Parked before the first `vamp`/`play`: contributes silence.
    Idle,
    /// Looping the segment (the steady vamp state). `exit_armed` commits a
    /// fade-out-to-silence at the next seam.
    Vamping { exit_armed: bool },
    /// Paused: contributes silence (not a frozen DC sample, which would click on
    /// resume); the video tile freezes the picture separately.
    Paused,
    /// The armed exit has been committed at absolute frame `exit_seam` (the next
    /// loop wrap at-or-after the arm): for frames `< exit_seam` the loop plays;
    /// `[exit_seam, exit_seam + xfade)` is a cosine fade-out tail; beyond it,
    /// silence.
    Exiting { exit_seam: u64 },
}

/// A pure buffer-and-replay audio looper for a media-player channel.
///
/// Construct empty ([`LoopDeck::empty`] — a silent/no-audio asset) or over a
/// decoded segment ([`LoopDeck::with_segment`]); drive transport with the verb
/// methods; pull looped audio with [`LoopDeck::read_at`] (pure, by absolute frame)
/// or [`LoopDeck::read`] (advancing an internal cursor, the steady driver path).
#[derive(Debug)]
pub struct LoopDeck {
    format: AudioFormat,
    /// The loop body: exactly `loop_frames * channels` interleaved samples (the
    /// `[vamp_in, vamp_out)` content). Empty for a silent deck.
    body: Vec<f32>,
    /// The precomputed seam region: exactly `xfade * channels` interleaved
    /// samples = the previous lap's tail (`lapover`) faded out, overlap-added with
    /// this lap's head faded in, under the chosen (equal-power / linear) law.
    /// Indexed by the in-lap frame `m ∈ [0, xfade)`.
    seam: Vec<f32>,
    /// The loop length in frames (`L`). Zero for a silent/empty deck.
    loop_frames: usize,
    /// The seam crossfade window in frames (`W = min(W_target, L/2)`).
    xfade: usize,
    /// The transport state.
    state: DeckState,
    /// The internal absolute-frame cursor for the advancing [`LoopDeck::read`]
    /// path, and the high-water mark [`LoopDeck::arm_exit`] anchors the next-seam
    /// computation to. Interior-mutable so [`LoopDeck::read_at`] can advance the
    /// high-water without a `&mut` borrow (its sample OUTPUT stays a pure function
    /// of the absolute frame; only this bookkeeping mutates).
    cursor: Cell<u64>,
    /// The **phase anchor**: the absolute frame that maps to lap position 0. The
    /// in-lap position of absolute frame `f` is `(f − phase) mod L`. Defaults to 0
    /// (the loop is phase-locked to the absolute timeline at boot). [`LoopDeck::stop`]
    /// re-cues by latching a fresh phase so the next `vamp` restarts the segment from
    /// `body[0]` at the then-current frame — distinct from [`LoopDeck::pause`], which
    /// holds the phase (resume continues mid-loop). Interior-mutable so the
    /// pure-function-of-`abs_frame` read path can apply a pending re-cue.
    phase: Cell<u64>,
    /// Whether a re-cue is pending: set by [`LoopDeck::stop`], consumed by the first
    /// vamping read (or [`LoopDeck::arm_exit_at`]) which then latches `phase` to the
    /// current absolute frame so the loop restarts from its head there.
    needs_rephase: Cell<bool>,
}

impl LoopDeck {
    /// An empty deck for a player whose asset has no audio (or a failed / over-cap
    /// prime): it contributes silence forever — every read returns exactly the
    /// requested frame count of silence, never a short block, never a panic
    /// (hold-last-good / never off-air).
    #[must_use]
    pub fn empty(format: AudioFormat) -> Self {
        Self {
            format,
            body: Vec::new(),
            seam: Vec::new(),
            loop_frames: 0,
            xfade: 0,
            state: DeckState::Idle,
            cursor: Cell::new(0),
            phase: Cell::new(0),
            needs_rephase: Cell::new(false),
        }
    }

    /// Build a deck over a contiguously-decoded buffer.
    ///
    /// `decoded` is `loop_frames` body frames (the `[vamp_in, vamp_out)` content)
    /// followed by however many **lap-over** frames were decoded past the loop
    /// point (`[vamp_out, …)`, used as the fade-out leg of the seam). The deck
    /// loops the first `loop_frames` frames; the crossfade window is
    /// `W = min(xfade_frames, loop_frames / 2)`. If fewer than `W` lap-over frames
    /// are present (the loop point is the clip end), the lap-over wraps to the
    /// body head — a loop-to-self seam, which the correlation test then resolves
    /// to a linear (transparent) fade.
    ///
    /// # Errors
    ///
    /// [`LoopDeckError::Ragged`] if `decoded.len()` is not a whole multiple of the
    /// channel count, or shorter than `loop_frames` frames.
    pub fn with_segment(
        format: AudioFormat,
        decoded: &[f32],
        loop_frames: usize,
        xfade_frames: usize,
    ) -> Result<Self, LoopDeckError> {
        let channels = format.channel_count();
        if channels == 0 || decoded.len() % channels != 0 {
            return Err(LoopDeckError::Ragged {
                samples: decoded.len(),
                channels,
            });
        }
        let total_frames = decoded.len() / channels;
        if total_frames < loop_frames {
            return Err(LoopDeckError::Ragged {
                samples: decoded.len(),
                channels,
            });
        }
        // A degenerate zero-length loop is an empty (silent) deck.
        if loop_frames == 0 {
            return Ok(Self::empty(format));
        }
        let xfade = xfade_frames.min(loop_frames / 2);
        let body_len = loop_frames.saturating_mul(channels);
        let body: Vec<f32> = decoded.get(..body_len).unwrap_or(decoded).to_vec();

        let seam = Self::build_seam(decoded, &body, loop_frames, xfade, channels);

        Ok(Self {
            format,
            body,
            seam,
            loop_frames,
            xfade,
            // A deck built over a segment is ready to loop: it begins vamping, the
            // steady playout for a looping channel (the executor re-issues `vamp`/
            // `play`/`pause`/`stop` as transport verbs arrive, but absent any verb
            // the channel loops, matching the video player's `loop_on_start`).
            state: DeckState::Vamping { exit_armed: false },
            cursor: Cell::new(0),
            // Phase-locked to the absolute timeline at boot (lap position == abs mod
            // L); a `stop` re-cue latches a fresh phase later.
            phase: Cell::new(0),
            needs_rephase: Cell::new(false),
        })
    }

    /// Build a loop deck by **slicing the vamp segment from a whole decoded clip**
    /// at the asset's cadence — the CLI driver's path (ADR-T019 §1/§3).
    ///
    /// `clip` is the whole `[in_point, out_point)` (or longer) clip decoded to
    /// 48 kHz / `format` / interleaved `f32`. The vamp window `[vamp_in_frames,
    /// vamp_out_frames)` is in **asset source frames** at `cadence`; this maps each
    /// frame to its exact 48 kHz **sample** position
    /// (`sample(f) = round(f × 48000 × den / num)`, exact integer rationals — never
    /// a `SampleClock::total_at` *tick* delta, which would conflate source-frame
    /// and output-tick indices) and slices `[sample(vamp_in), sample(vamp_out) + W)`
    /// as the loop body + lap-over. The resulting loop length is the audio duration
    /// of `vamp_len` asset frames — the **same sample count at any output cadence**,
    /// so the audio lap and the video lap stay the same media-time length
    /// (sample-lock). `xfade_frames == 0` selects [`DEFAULT_CROSSFADE_FRAMES`].
    ///
    /// A window that exceeds [`MAX_LOOP_SECONDS`], or resolves to an empty/degenerate
    /// range, or lies outside the decoded `clip`, yields an **empty** (silent) deck
    /// (the player loops video normally and rides audio silence — never an OOM,
    /// never a panic).
    ///
    /// # Errors
    ///
    /// [`LoopDeckError::Ragged`] only if `clip.len()` is not a whole multiple of the
    /// channel count (a programming error in the decode path). A geometry that
    /// cannot be satisfied returns `Ok` with an empty deck, not an error.
    pub fn from_clip_window(
        format: AudioFormat,
        clip: &[f32],
        vamp_in_frames: u64,
        vamp_out_frames: u64,
        cadence: Rational,
        xfade_frames: usize,
    ) -> Result<Self, LoopDeckError> {
        let channels = format.channel_count();
        if channels == 0 || clip.len() % channels != 0 {
            return Err(LoopDeckError::Ragged {
                samples: clip.len(),
                channels,
            });
        }
        let xfade = if xfade_frames == 0 {
            DEFAULT_CROSSFADE_FRAMES
        } else {
            xfade_frames
        };

        // Map asset frames → exact 48 kHz sample indices. Bail to a silent deck on
        // any degenerate / unsatisfiable geometry (never an error, never an OOM).
        let Some(in_sample) = frame_to_sample(vamp_in_frames, cadence) else {
            return Ok(Self::empty(format));
        };
        let Some(out_sample) = frame_to_sample(vamp_out_frames, cadence) else {
            return Ok(Self::empty(format));
        };
        if out_sample <= in_sample {
            return Ok(Self::empty(format));
        }
        let loop_samples = out_sample - in_sample;
        // Bounded-memory cap (ADR-T019 §5): refuse an over-long window for audio.
        if loop_samples > MAX_LOOP_SECONDS.saturating_mul(SAMPLE_RATE) {
            return Ok(Self::empty(format));
        }
        let loop_frames = usize::try_from(loop_samples).unwrap_or(usize::MAX);
        let in_idx = usize::try_from(in_sample).unwrap_or(usize::MAX);

        // The window we need is [in_sample, out_sample + W). The loop length is the
        // **declared** body (`loop_frames`) so the audio stays sample-locked to the
        // video's `vamp_len` — it is NEVER silently clamped to a shorter decoded
        // span (that would shift the loop point off the video, Defect 3). A decoded
        // time range can fall a *few* samples short of the frame-derived target
        // (resampler edge), so a small tolerance is allowed; a **materially** short
        // clip (a truncated asset) is **refused** → the channel rides silence.
        let total_frames = clip.len() / channels;
        if in_idx >= total_frames {
            return Ok(Self::empty(format));
        }
        let available = total_frames - in_idx;
        if available + SHORTFALL_TOLERANCE_FRAMES < loop_frames {
            // Materially shorter than the declared window — refuse rather than loop
            // a shifted-length body (a tiny resampler-edge shortfall is tolerated).
            return Ok(Self::empty(format));
        }
        // The body is the full declared length, zero-padded if a tiny resampler
        // shortfall left the last few samples missing (inaudible, keeps the loop
        // sample-locked). The lap-over is whatever real content follows, up to W.
        let body_end = in_idx.saturating_add(loop_frames);
        let want_end = body_end.saturating_add(xfade).min(total_frames);
        let start_off = in_idx.saturating_mul(channels);
        let end_off = want_end.saturating_mul(channels);
        let mut window: Vec<f32> = clip.get(start_off..end_off).unwrap_or(&[]).to_vec();
        // Pad to at least the body length (covers a sub-tolerance shortfall) so the
        // body is exactly `loop_frames` frames — sample-locked, never shifted.
        let body_span = loop_frames.saturating_mul(channels);
        if window.len() < body_span {
            window.resize(body_span, 0.0);
        }
        Self::with_segment(format, &window, loop_frames, xfade)
    }

    /// Build a loop deck over a **pre-sliced vamp window** that already starts at
    /// `vamp_in` (the CLI driver's path after a windowed, prefix-discarding decode —
    /// ADR-T019 §5 / CRITICAL-3). `window` is `loop_frames` body frames followed by
    /// however many lap-over frames were decoded past the loop point; the loop is the
    /// first `loop_frames`, crossfaded with the lap-over at the seam.
    ///
    /// Applies the same **refuse-don't-clamp** rule as [`LoopDeck::from_clip_window`]:
    /// a window the decode under-delivers (a truncated asset / in-point past clip
    /// end) by **more than** a few-sample resampler-edge shortfall
    /// ([`SHORTFALL_TOLERANCE_FRAMES`]) yields an **empty (silent)** deck rather than
    /// a shifted-length loop (which would break the sample-lock to the video wrap);
    /// only a sub-tolerance shortfall is zero-padded and tolerated. A `loop_frames`
    /// of 0 (or an over-cap body the caller already rejected) also yields an empty
    /// deck. Never errors on a short window — it rides silence.
    #[must_use]
    pub fn from_window(
        format: AudioFormat,
        window: &[f32],
        loop_frames: usize,
        xfade_frames: usize,
    ) -> Self {
        let channels = format.channel_count();
        if channels == 0 || loop_frames == 0 {
            return Self::empty(format);
        }
        let have_frames = window.len() / channels;
        // Materially short → refuse (ride silence), never a shifted-length loop.
        if have_frames.saturating_add(SHORTFALL_TOLERANCE_FRAMES) < loop_frames {
            return Self::empty(format);
        }
        // Pad a sub-tolerance shortfall so the body is exactly `loop_frames`
        // (sample-locked); otherwise use the window as-is.
        let body_span = loop_frames.saturating_mul(channels);
        if window.len() < body_span {
            let mut padded = window.to_vec();
            padded.resize(body_span, 0.0);
            return Self::with_segment(format, &padded, loop_frames, xfade_frames)
                .unwrap_or_else(|_| Self::empty(format));
        }
        Self::with_segment(format, window, loop_frames, xfade_frames)
            .unwrap_or_else(|_| Self::empty(format))
    }

    /// Precompute the `xfade`-frame seam region: for in-lap frame `m ∈ [0, xfade)`
    /// overlap-add the previous lap's tail `lapover[m]` (fading out) with this
    /// lap's head `body[m]` (fading in), under the correlation-adaptive law. The
    /// lap-over is the real decoded content past the loop point when present, else
    /// the wrapped body head.
    fn build_seam(
        decoded: &[f32],
        body: &[f32],
        loop_frames: usize,
        xfade: usize,
        channels: usize,
    ) -> Vec<f32> {
        if xfade == 0 {
            return Vec::new();
        }
        // Gather the two seam legs as `xfade * channels` interleaved samples.
        // head leg = body[0..xfade); tail leg = decoded[loop_frames .. loop_frames+xfade)
        // (real post-loop content) or, when absent, the wrapped head (body[0..xfade)).
        let span = xfade.saturating_mul(channels);
        let head: &[f32] = body.get(..span).unwrap_or(body);
        let tail_start = loop_frames.saturating_mul(channels);
        let tail: Vec<f32> = match decoded.get(tail_start..tail_start.saturating_add(span)) {
            Some(slice) => slice.to_vec(),
            None => head.to_vec(),
        };

        let linear = !Self::is_decorrelated(head, &tail);
        let mut seam = vec![0.0f32; span];
        for m in 0..xfade {
            // Centred phase p = (m + 0.5) / xfade ∈ (0, 1): at m=0 the tail
            // dominates (continuous with the previous sample), at m=xfade-1 the
            // head dominates (continuous into the clean middle).
            // `f64::midpoint(2·ratio, 1)` == `(2·ratio + 1)/2` bit-for-bit here (both
            // operands are far below `f64::MAX/2`, so `midpoint` takes its plain
            // `(a+b)/2` branch); the Rust-1.85 `midpoint` form silences
            // `clippy::manual_midpoint` (MSRV-gated on) without changing the value.
            let p = f64::midpoint(frame_ratio(m, xfade) * 2.0, 1.0);
            let (g_tail, g_head) = if linear {
                // Linear constant-amplitude: g_tail + g_head == 1.
                (1.0 - p, p)
            } else {
                // Equal-power constant-power: g_tail² + g_head² == 1 (cos/sin).
                let theta = p * FRAC_PI_2;
                (theta.cos(), theta.sin())
            };
            for c in 0..channels {
                let idx = m.saturating_mul(channels).saturating_add(c);
                let t = f64::from(*tail.get(idx).unwrap_or(&0.0));
                let h = f64::from(*head.get(idx).unwrap_or(&0.0));
                if let Some(slot) = seam.get_mut(idx) {
                    *slot = clamp_sample(g_tail * t + g_head * h);
                }
            }
        }
        seam
    }

    /// Whether the two seam legs are **decorrelated** (so equal-power is the
    /// click-free, level-flat choice) vs correlated (linear). The decision is the
    /// normalized cross-correlation of the interleaved legs vs
    /// [`CORRELATION_THRESHOLD`]; a near-zero-energy leg (silence) is treated as
    /// decorrelated (equal-power degrades gracefully to a fade of/to silence).
    fn is_decorrelated(head: &[f32], tail: &[f32]) -> bool {
        let len = head.len().min(tail.len());
        let mut dot = 0.0f64;
        let mut energy_head = 0.0f64;
        let mut energy_tail = 0.0f64;
        for i in 0..len {
            let hv = f64::from(*head.get(i).unwrap_or(&0.0));
            let tv = f64::from(*tail.get(i).unwrap_or(&0.0));
            dot += hv * tv;
            energy_head += hv * hv;
            energy_tail += tv * tv;
        }
        if energy_head <= f64::EPSILON || energy_tail <= f64::EPSILON {
            return true;
        }
        let rho = dot / (energy_head.sqrt() * energy_tail.sqrt());
        rho < CORRELATION_THRESHOLD
    }

    /// The deck's audio format.
    #[must_use]
    pub const fn format(&self) -> AudioFormat {
        self.format
    }

    /// The loop length in frames (`L`) — the decoded sample count of the vamp
    /// segment. Zero for an empty/silent deck.
    #[must_use]
    pub const fn loop_frames(&self) -> usize {
        self.loop_frames
    }

    /// The seam crossfade window in frames (`W`).
    #[must_use]
    pub const fn crossfade_frames(&self) -> usize {
        self.xfade
    }

    // ---- transport verbs -------------------------------------------------

    /// Begin (or resume) vamping — loop the segment. The default playout for a
    /// looping channel. Re-cues to the head: the next `read`/`read_at` from the
    /// current cursor plays the loop from the appropriate lap position.
    pub fn vamp(&mut self) {
        self.state = DeckState::Vamping { exit_armed: false };
    }

    /// Pause: contribute silence (the bus mixes nothing for this source) until a
    /// fresh `vamp`. Click-free on resume (silence, not a frozen DC sample).
    pub fn pause(&mut self) {
        self.state = DeckState::Paused;
    }

    /// Stop: **re-cue to the head** and contribute silence until a fresh `vamp`.
    /// Latches a pending re-phase so the next `vamp` restarts the loop from `body[0]`
    /// at the then-current absolute frame (distinct from [`LoopDeck::pause`], which
    /// holds the loop phase so a resume continues mid-loop). The cursor is reset so a
    /// fresh `vamp` re-anchors cleanly.
    pub fn stop(&mut self) {
        self.state = DeckState::Idle;
        self.cursor.set(0);
        self.needs_rephase.set(true);
    }

    /// Arm the vamp exit: a cosine fade-out-to-silence fires at the **next loop
    /// seam** (the next wrap strictly after the deck's current high-water frame),
    /// after which the deck contributes silence. No-op if not vamping or already
    /// exiting. Reversible by [`LoopDeck::cancel_exit`] until the seam fires.
    pub fn arm_exit(&mut self) {
        let here = self.cursor.get();
        self.arm_exit_at(here);
    }

    /// Arm the vamp exit anchored at an explicit absolute frame `anchor` (the
    /// video rail's arm position, in this deck's 48 kHz frame space) rather than
    /// the deck's own cursor — so the audio exit fires at the **same** next-vamp
    /// boundary the video rail computes (ADR-T019: the two rails share the
    /// `[vamp_in, vamp_out)` geometry anchored at output media-time ZERO, so the
    /// next wrap at-or-after `anchor` is one instant for both). The exit fires at
    /// the next wrap **strictly after** `anchor` so the current lap completes.
    /// No-op if not vamping or already exiting.
    pub fn arm_exit_at(&mut self, anchor: u64) {
        if let DeckState::Vamping { .. } = self.state {
            self.state = DeckState::Vamping { exit_armed: true };
            if self.loop_frames > 0 {
                // Consume any pending re-cue at the arm anchor so the seam math and
                // the playout agree on the loop phase (a `stop`→`vamp`→`arm` sequence
                // re-phases here rather than waiting for the first read).
                if self.needs_rephase.get() {
                    self.phase.set(anchor);
                    self.needs_rephase.set(false);
                }
                // The next wrap strictly after `anchor`, measured from the phase
                // origin: `phase + (⌊(anchor − phase)/L⌋ + 1)·L`.
                let l = to_u64(self.loop_frames);
                let phase = self.phase.get();
                let rel = anchor.saturating_sub(phase);
                let next_wrap =
                    phase.saturating_add(rel.saturating_div(l).saturating_add(1).saturating_mul(l));
                self.state = DeckState::Exiting {
                    exit_seam: next_wrap,
                };
            }
        }
    }

    /// Take the vamp exit: arm it for the soonest seam. Functionally
    /// [`LoopDeck::arm_exit`]; never forces a mid-lap cut.
    pub fn take_exit(&mut self) {
        self.arm_exit();
    }

    /// Cancel a pending vamp exit: keep looping. No-op if the exit seam has already
    /// fired (the boundary won) or no exit is armed.
    pub fn cancel_exit(&mut self) {
        if let DeckState::Exiting { exit_seam } = self.state {
            // Only cancellable while the boundary has not yet fired.
            if self.cursor.get() < exit_seam {
                self.state = DeckState::Vamping { exit_armed: false };
            }
        } else if let DeckState::Vamping { exit_armed: true } = self.state {
            self.state = DeckState::Vamping { exit_armed: false };
        }
    }

    /// Whether the armed exit has fully fired — the deck has settled to silence
    /// (the high-water frame is past the end of the exit fade tail).
    #[must_use]
    pub fn has_ended(&self) -> bool {
        match self.state {
            DeckState::Exiting { exit_seam } => {
                self.cursor.get() >= exit_seam.saturating_add(to_u64(self.xfade))
            }
            _ => false,
        }
    }

    /// The absolute frame at-and-after which an armed-and-fired exit contributes
    /// **silence** — `exit_seam + xfade` (the end of the fade-out tail) when the deck
    /// is exiting, else `None` (no settle: the deck loops forever). The CLI fill loop
    /// uses this to clamp its publish horizon so it never publishes a long silent
    /// tail past the boundary (ADR-T019 §2.3 — an efficiency clamp; correctness comes
    /// from re-deriving the window each block).
    #[must_use]
    pub fn settle_frame(&self) -> Option<u64> {
        match self.state {
            DeckState::Exiting { exit_seam } => Some(exit_seam.saturating_add(to_u64(self.xfade))),
            _ => None,
        }
    }

    // ---- reads -----------------------------------------------------------

    /// Pull `frames` frames of the looped stream starting at the deck's internal
    /// cursor, advancing the cursor by `frames`. The steady driver path.
    #[must_use]
    pub fn read(&mut self, frames: usize) -> AudioBlock {
        let at = self.cursor.get();
        let block = self.read_at(at, frames);
        self.cursor.set(at.saturating_add(to_u64(frames)));
        block
    }

    /// Realign the internal cursor to absolute frame `abs_frame` (the bus's
    /// read-cursor catch-up under load): the next [`LoopDeck::read`] resumes from
    /// there. Because [`LoopDeck::read_at`] is a pure function of the absolute
    /// frame, the realign still lands inside a correctly faded seam.
    pub fn seek_to(&mut self, abs_frame: u64) {
        self.cursor.set(abs_frame);
    }

    /// Pull `frames` frames of the looped stream as a **pure function of the
    /// absolute output frame** `abs_frame` — the realign-safe read. Always returns
    /// exactly `frames` frames (silence for an empty/paused/ended deck, the looped
    /// segment otherwise). Advances the deck's high-water mark (so a later
    /// [`LoopDeck::arm_exit`] anchors to the right seam) without affecting the
    /// returned samples.
    ///
    /// Allocates a fresh `AudioBlock` — convenient for tests and one-off reads. The
    /// **hot path** (the fill loop) uses [`LoopDeck::read_into`] with a reused
    /// buffer to avoid per-block allocation (rule 22).
    #[must_use]
    pub fn read_at(&self, abs_frame: u64, frames: usize) -> AudioBlock {
        let channels = self.format.channel_count();
        let mut out = vec![0.0f32; frames.saturating_mul(channels)];
        self.fill(abs_frame, frames, &mut out);
        AudioBlock::from_interleaved(self.format, out)
            .unwrap_or_else(|_| AudioBlock::silence(self.format, frames))
    }

    /// Pull `frames` frames of the looped stream into a **caller-provided reusable
    /// buffer** `out`, filled IN PLACE — the **zero-per-block-allocation** hot path
    /// (rule 22 / ADR-T019 §1). `out` is resized to `frames × channels` (reusing
    /// its existing capacity when sufficient — the caller sizes it once at prime),
    /// then overwritten; the deck holds no scratch of its own. Same realign-safe
    /// pure-function-of-`abs_frame` semantics as [`LoopDeck::read_at`].
    pub fn read_into(&self, abs_frame: u64, frames: usize, out: &mut Vec<f32>) {
        let channels = self.format.channel_count();
        let needed = frames.saturating_mul(channels);
        // `resize` reuses the existing allocation when `capacity >= needed`, so no
        // realloc on the steady path (the caller pre-sizes to the max refill).
        out.clear();
        out.resize(needed, 0.0);
        self.fill(abs_frame, frames, out);
    }

    /// Fill `out` (already sized to `frames × channels`, zeroed) with the looped
    /// stream starting at `abs_frame`. The shared core of [`LoopDeck::read_at`] and
    /// [`LoopDeck::read_into`]. Advances the deck's high-water mark (exit-seam
    /// anchoring) without affecting the samples.
    fn fill(&self, abs_frame: u64, frames: usize, out: &mut [f32]) {
        let channels = self.format.channel_count();
        // Track how far we have been read (for exit-seam anchoring); the output is
        // unaffected by this bookkeeping.
        let end = abs_frame.saturating_add(to_u64(frames));
        if end > self.cursor.get() {
            self.cursor.set(end);
        }

        // States that contribute silence: empty deck, idle, or paused. `out` is
        // already zeroed (silence) by the caller — nothing to write.
        let silent =
            self.loop_frames == 0 || matches!(self.state, DeckState::Idle | DeckState::Paused);
        if silent {
            return;
        }

        // Consume a pending re-cue (latched by `stop`): a fresh `vamp` restarts the
        // loop from its head at THIS absolute frame, so the loop phase is re-anchored
        // to `abs_frame`. Vamping/Exiting only (an idle/paused deck is silent above).
        if self.needs_rephase.get() && matches!(self.state, DeckState::Vamping { .. }) {
            self.phase.set(abs_frame);
            self.needs_rephase.set(false);
        }

        let l = to_u64(self.loop_frames);
        let phase = self.phase.get();
        let exit_seam = match self.state {
            DeckState::Exiting { exit_seam } => Some(exit_seam),
            _ => None,
        };

        for f in 0..frames {
            let abs = abs_frame.saturating_add(to_u64(f));

            // Past the exit: silence (`out` is already zeroed).
            if let Some(seam) = exit_seam {
                if abs >= seam.saturating_add(to_u64(self.xfade)) {
                    continue;
                }
                if abs >= seam {
                    // The exit fade-out tail: the CONTINUING loop content at this
                    // absolute frame (the same in-lap position the loop would play),
                    // cosine-faded to silence over `xfade`. Advancing the position —
                    // rather than holding the seam frame — fades the real ongoing
                    // waveform, so a tonal/ramped clip exits click-free (ADR-T019).
                    let j = to_usize(abs - seam);
                    let g = exit_gain(j, self.xfade);
                    let m = to_usize(abs.saturating_sub(phase) % l);
                    self.write_loop_frame(out, f, m, channels, Some(g));
                    continue;
                }
            }

            // Phase-relative in-lap position: `(abs − phase) mod L`.
            let m = to_usize(abs.saturating_sub(phase) % l);
            self.write_loop_frame(out, f, m, channels, None);
        }
    }

    /// Write one looped frame `m ∈ [0, L)` into output frame slot `f`, optionally
    /// scaled by `gain` (the exit fade). Uses the precomputed seam for the seam
    /// region `[0, xfade)` and the body for the clean middle `[xfade, L)`.
    fn write_loop_frame(
        &self,
        out: &mut [f32],
        f: usize,
        m: usize,
        channels: usize,
        gain: Option<f64>,
    ) {
        // The seam region `[0, xfade)` reads the precomputed crossfade; the clean
        // middle `[xfade, L)` reads the body. Both index by the in-lap frame `m`.
        let src: &[f32] = if m < self.xfade {
            &self.seam
        } else {
            &self.body
        };
        let src_off = m.saturating_mul(channels);
        let dst_off = f.saturating_mul(channels);
        for c in 0..channels {
            let s = *src.get(src_off.saturating_add(c)).unwrap_or(&0.0);
            let v = match gain {
                Some(g) => clamp_sample(g * f64::from(s)),
                None => s,
            };
            if let Some(slot) = out.get_mut(dst_off.saturating_add(c)) {
                *slot = v;
            }
        }
    }
}

/// The cosine exit-tail gain at fade frame `j ∈ [0, xfade)`: `1 → 0` so the bus
/// contribution ends click-free. `cos((j+0.5)/xfade · π/2)`.
fn exit_gain(j: usize, xfade: usize) -> f64 {
    if xfade == 0 {
        return 0.0;
    }
    let p = (frame_to_f64(j) * 2.0 + 1.0) / (2.0 * frame_to_f64(xfade).max(1.0));
    (p * FRAC_PI_2).cos()
}

/// `frame / total` as an `f64` ratio in `[0, 1)` (matches the mixer's `ratio`):
/// the lint policy forbids `as` in non-test code, so the conversion goes through
/// `u32::try_from` (frame counts are far inside `u32` for any realistic segment).
fn frame_ratio(frame: usize, total: usize) -> f64 {
    frame_to_f64(frame) / frame_to_f64(total.max(1))
}

/// `usize → f64` for a frame index without `as`, saturating at `u32::MAX` (a
/// segment with more than ~4 billion frames — ~24 h at 48 kHz — is rejected by the
/// `MAX_LOOP_SECONDS` cap long before this clamps). Mirrors `mixer::ratio`.
fn frame_to_f64(v: usize) -> f64 {
    u32::try_from(v).map_or(f64::from(u32::MAX), f64::from)
}

/// Hard-limit a mixed sample to `[-1.0, 1.0]` and narrow to `f32`, matching
/// `mixer::clamp_sample` — the established crate pattern for the one sanctioned
/// `f64 → f32` narrowing (value pre-clamped to `[-1, 1]`, bounded, saturating).
#[allow(clippy::as_conversions, clippy::cast_possible_truncation)] // reason: value is clamped to [-1,1]; f64->f32 narrowing is exact-enough and bounded.
fn clamp_sample(v: f64) -> f32 {
    v.clamp(-1.0, 1.0) as f32
}

/// The exact 48 kHz **sample** index of asset frame `f` at `cadence` = `num/den`
/// fps: `round(f × 48000 × den / num)`, computed in `u128` with no float and no
/// `as` (inv #3 — the audio loop point lands on an exact sample, never a drifting
/// average). Returns `None` for a non-positive cadence (caller rides silence).
fn frame_to_sample(frame: u64, cadence: Rational) -> Option<u64> {
    let num = u64::try_from(cadence.num).ok().filter(|n| *n > 0)?;
    let den = u64::try_from(cadence.den).ok().filter(|d| *d > 0)?;
    // sample = frame * SAMPLE_RATE * den / num, rounded to nearest.
    let numer = u128::from(frame)
        .saturating_mul(u128::from(SAMPLE_RATE))
        .saturating_mul(u128::from(den));
    let denom = u128::from(num);
    let rounded = numer.saturating_add(denom / 2) / denom;
    u64::try_from(rounded).ok()
}

/// `usize → u64` for frame counts/indices, saturating (no `as`). Widening on
/// 64-bit; `try_from` keeps it portable and lint-clean.
fn to_u64(v: usize) -> u64 {
    u64::try_from(v).unwrap_or(u64::MAX)
}

/// `u64 → usize` for an absolute-frame remainder that is known `< loop_frames`
/// (so it always fits `usize`), saturating (no `as`).
fn to_usize(v: u64) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

/// Why a [`LoopDeck`] could not be built from a decoded buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LoopDeckError {
    /// The decoded buffer length is not a whole multiple of the channel count, or
    /// is shorter than the declared loop length.
    #[error("decoded audio buffer is ragged: {samples} samples is not {channels} whole frames (or shorter than the loop length)")]
    Ragged {
        /// The interleaved sample count supplied.
        samples: usize,
        /// The channel count.
        channels: usize,
    },
}
