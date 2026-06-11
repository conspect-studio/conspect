//! The program-bus mix on the output clock (AUD-3, [`ProgramBus`]).
//!
//! [`ProgramBus`] composes the three audio primitives into the one operation the
//! output clock needs each tick:
//!
//! * [`SampleClock`](crate::cadence::SampleClock) — how many samples this tick
//!   owes (exact, drift-free; the audio side of invariant #1/#3);
//! * one [`AudioStore`](crate::store::AudioStore) per routed source — the
//!   last-good, silence-filling, lock-free read (a stalled source is silence,
//!   never a gap);
//! * the [`Mixer`](crate::mixer::Mixer) — sum the routed inputs at their program
//!   gains into the program bus.
//!
//! Each [`ProgramBus::tick`] pulls exactly `samples_per_tick` frames from every
//! source and returns the mixed program [`AudioBlock`] of exactly that length.
//! It does **no I/O and cannot block** — the stores are wait-free reads — so it
//! is safe to call on the output-clock hot loop (invariant #1), and it never
//! back-pressures any input (invariant #10). The encode + dual-stream mux of the
//! returned block is AUD-4.

use std::sync::Arc;

use multiview_core::time::Rational;

use crate::cadence::SampleClock;
use crate::format::{AudioBlock, AudioFormat};
use crate::mixer::{GainRamp, Mixer, RoutePoint};
use crate::store::AudioStore;

/// The live-apply class of an audio switch (invariant #11): whether it is a
/// hot/seamless frame-boundary change (Class-1) or a controlled-reset change
/// that pays a transcode/migration (Class-2).
///
/// This mirrors the engine-wide Class-1/Class-2 split; it is restated here as a
/// small, dependency-free enum so the audio crate (which sits below the engine
/// and control crates) can surface the class of a [`SwitchTier`] without a
/// crate-dependency cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ApplyClass {
    /// Hot / seamless at a frame boundary — no encoder/session reset.
    Class1,
    /// Controlled reset: a decode → mix → re-encode (or make-before-break)
    /// transition the operator opts into.
    Class2,
}

/// Which pop-avoidance tier an audio switch uses (RT-9, decoupled-routing §5
/// "Two tiers, surfaced as a badge").
///
/// The two tiers reflect a real DSP floor:
///
/// * [`SoftStep`](Self::SoftStep) — a **coded passthrough** switch: an
///   AU-aligned step in the coded domain. It is video-clean and hot (Class-1),
///   but the AAC IMDCT/TDAC overlap-add seam transient is *uncancellable* in the
///   coded domain, so it is **not guaranteed pop-free**. This crate models the
///   tier (so a caller/UI can badge it) but does not implement the coded path.
/// * [`ClickFree`](Self::ClickFree) — a **decode → program-bus cross-fade →
///   re-encode** switch: the equal-power [`GainRamp`] cross-fade implemented here
///   ([`ProgramBus::repoint_crossfade`]). It is genuinely pop-free but pays the
///   transcode floor, so it is **Class-2** (the operator opts in).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SwitchTier {
    /// Coded passthrough, AU-aligned step. Class-1, video-clean, not guaranteed
    /// pop-free.
    SoftStep,
    /// Decoded-bus equal-power cross-fade. Class-2, genuinely pop-free.
    ClickFree,
}

impl SwitchTier {
    /// The invariant-#11 apply class of this tier: [`SoftStep`](Self::SoftStep)
    /// is [`Class1`](ApplyClass::Class1); [`ClickFree`](Self::ClickFree) is
    /// [`Class2`](ApplyClass::Class2).
    #[must_use]
    pub const fn class(self) -> ApplyClass {
        match self {
            Self::SoftStep => ApplyClass::Class1,
            Self::ClickFree => ApplyClass::Class2,
        }
    }

    /// Whether this tier is *guaranteed* pop-free. Only the decoded-bus
    /// [`ClickFree`](Self::ClickFree) cross-fade is; the coded
    /// [`SoftStep`](Self::SoftStep) leaves an uncancellable seam transient.
    #[must_use]
    pub const fn is_pop_free(self) -> bool {
        matches!(self, Self::ClickFree)
    }
}

/// The program-bus mixer driven by the output clock.
///
/// Build one per run at the canonical program format with [`ProgramBus::new`],
/// register each source with [`ProgramBus::add_source`], then call
/// [`ProgramBus::tick`] once per output tick.
#[derive(Debug)]
pub struct ProgramBus {
    mixer: Mixer,
    clock: SampleClock,
    /// Per routed source: its mixer route point + the store the output clock
    /// pulls from. Shared (`Arc`) with the source's decode thread, which
    /// publishes into the same store.
    routes: Vec<(RoutePoint, Arc<AudioStore>)>,
    /// Outgoing strips that are fading out during a cross-fade (RT-9). Each is a
    /// temporary route reading the *old* store at a declining (`cos`) ramp; once
    /// its ramp completes it is unrouted and removed from `routes`. Tracked here
    /// so [`mix`](ProgramBus::mix) can retire it on the tick the ramp finishes.
    fading_out: Vec<RoutePoint>,
    format: AudioFormat,
}

impl ProgramBus {
    /// Build an empty program bus at `format`, paced by output cadence `fps`.
    #[must_use]
    pub fn new(format: AudioFormat, fps: Rational) -> Self {
        Self {
            mixer: Mixer::new(format),
            clock: SampleClock::new(format.sample_rate(), fps),
            routes: Vec::new(),
            fading_out: Vec::new(),
            format,
        }
    }

    /// Route a source's audio store onto the program bus at linear `program_gain`,
    /// returning the [`RoutePoint`] that now reads it.
    ///
    /// The store must already be at the bus [`format`](Self::format) (every
    /// source decode resamples to the canonical program format upstream).
    ///
    /// This **appends** a new route; to swap which store an *existing* route
    /// point reads (an audio breakaway / re-point), use
    /// [`repoint`](Self::repoint).
    pub fn add_source(
        &mut self,
        id: impl Into<String>,
        store: Arc<AudioStore>,
        program_gain: f64,
    ) -> RoutePoint {
        let point = self.mixer.add_input(id);
        self.mixer.route_to_program(point, program_gain);
        self.routes.push((point, store));
        point
    }

    /// Re-point an **existing** route point onto a different `Arc<AudioStore>`
    /// (replace semantics) — the audio-breakaway primitive (RT-8a).
    ///
    /// Distinct from [`add_source`](Self::add_source)'s append: this swaps the
    /// store bound to a route point already on the bus, leaving its mixer strip
    /// (gain/route/id) untouched, so the program bus channel now reads the new
    /// source. To keep the switch **sample-aligned at the seam** (no silence gap,
    /// no climb from frame 0 through a warm store's evicted history), the new
    /// store's read cursor is seeked to its live edge
    /// ([`AudioStore::seek_to_live_edge`]) as part of the re-point.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::UnknownInput`](crate::error::AudioError::UnknownInput)
    /// if `point` is not a route on this bus — an honest error, never a panic and
    /// never a silent wrong-source bind.
    pub fn repoint(
        &mut self,
        point: RoutePoint,
        store: Arc<AudioStore>,
    ) -> crate::error::Result<()> {
        let slot = self
            .routes
            .iter_mut()
            .find(|(p, _)| *p == point)
            .map(|(_, s)| s)
            .ok_or(crate::error::AudioError::UnknownInput(point.index()))?;
        // Align the warm store to its live edge so the seam reads fresh audio,
        // not silence climbing from frame 0 through evicted history.
        store.seek_to_live_edge();
        *slot = store;
        Ok(())
    }

    /// Re-point an existing route point onto a new store with an **equal-power
    /// cross-fade** instead of a hard swap — the pop-avoidance breakaway (RT-9,
    /// the CLICK-FREE tier of decoupled-routing §5).
    ///
    /// A plain [`repoint`](Self::repoint) is sample-accurate but
    /// waveform-discontinuous at the seam, so it **clicks**. This instead fades
    /// over `ramp_frames` (~10 ms worth, e.g. 480 frames @ 48 kHz):
    ///
    /// * the OLD store stays routed on a temporary "outgoing" strip at a
    ///   declining `cos` taper ([`GainRamp::down`]);
    /// * the channel `point` is re-pointed to the NEW store (seeked to its live
    ///   edge, exactly like [`repoint`]) and given a rising `sin` taper
    ///   ([`GainRamp::up`]);
    /// * because `sin² + cos² = 1`, the summed power is **constant** across the
    ///   fade (no audible dip), and the per-sample envelope (applied inside
    ///   [`Mixer::mix_program`]) means **no sample discontinuity** at the seam (no
    ///   click) even when the fade spans a tick block;
    /// * when the down-ramp completes the outgoing strip is **unrouted**
    ///   ([`Mixer::unroute_from_program`]) so the old source no longer
    ///   contributes (no lingering double-count).
    ///
    /// Returns the [`SwitchTier`] applied: [`SwitchTier::ClickFree`] for a real
    /// cross-fade (`ramp_frames > 0`), or [`SwitchTier::SoftStep`] when
    /// `ramp_frames == 0` (a degenerate request degrades to the hard
    /// [`repoint`](Self::repoint) swap rather than a zero-length fade).
    ///
    /// Does no I/O and cannot block (it only mutates the route table and seeks a
    /// cursor), so it is safe on the frame-boundary control hook (invariants
    /// #1/#10).
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::UnknownInput`](crate::error::AudioError::UnknownInput)
    /// if `point` is not a route on this bus — never a panic, never a silent
    /// wrong-source bind.
    pub fn repoint_crossfade(
        &mut self,
        point: RoutePoint,
        store: Arc<AudioStore>,
        ramp_frames: usize,
    ) -> crate::error::Result<SwitchTier> {
        // A zero-length ramp is a hard step (the SOFT-STEP-equivalent fast cut).
        if ramp_frames == 0 {
            self.repoint(point, store)?;
            return Ok(SwitchTier::SoftStep);
        }

        // Locate the channel and capture the OLD store before we overwrite it.
        let old_store = self
            .routes
            .iter()
            .find(|(p, _)| *p == point)
            .map(|(_, s)| Arc::clone(s))
            .ok_or(crate::error::AudioError::UnknownInput(point.index()))?;
        // The channel's steady gain is the base both tapers ride on.
        let base_gain = self.mixer.program_gain(point).unwrap_or(1.0);

        // OUTGOING strip: a fresh route reading the OLD store, faded out on a
        // `cos` taper from the channel's current gain to silence. Its cursor is
        // continued from where the channel left off (it is the same store the
        // channel was just reading), so the fade-out audio is continuous.
        let out_id = self
            .mixer
            .input_id(point)
            .map_or_else(|| "xfade-out".to_owned(), str::to_owned);
        let out_point = self.mixer.add_input(out_id);
        self.mixer.route_to_program(out_point, base_gain);
        self.mixer
            .set_gain_ramp(out_point, GainRamp::down(ramp_frames));
        self.routes.push((out_point, old_store));
        self.fading_out.push(out_point);

        // INCOMING: re-point the channel onto the NEW store (live-edge seek, as
        // in `repoint`) and fade it in on a `sin` taper. The channel keeps its
        // identity (`point`), so subsequent re-points still address it.
        store.seek_to_live_edge();
        if let Some(slot) = self
            .routes
            .iter_mut()
            .find(|(p, _)| *p == point)
            .map(|(_, s)| s)
        {
            *slot = store;
        }
        self.mixer.set_gain_ramp(point, GainRamp::up(ramp_frames));

        Ok(SwitchTier::ClickFree)
    }

    /// The program-bus format.
    #[must_use]
    pub const fn format(&self) -> AudioFormat {
        self.format
    }

    /// Mix one output tick of program audio.
    ///
    /// Advances the [`SampleClock`] by one tick to
    /// get this tick's exact sample budget, then mixes (see [`tick_to`](Self::tick_to)
    /// for the per-source pull). Use this when the consumer is guaranteed to call
    /// exactly once per output tick; under `DropOnOverload` (where some ticks are
    /// skipped) drive the bus by the absolute tick index with
    /// [`tick_to`](Self::tick_to) instead, so audio cannot drift from video
    /// (invariant #3).
    pub fn tick(&mut self) -> AudioBlock {
        let frames = self.clock.next_tick();
        self.mix(frames)
    }

    /// Mix program audio up to the absolute output **tick index** `target_tick`.
    ///
    /// Advances the [`SampleClock`] to `target_tick`,
    /// **catching up the samples for any ticks that were skipped** since the last
    /// call (e.g. a `DropOnOverload` gap), and returns an [`AudioBlock`] of
    /// exactly that many frames. Because the sample budget is a pure function of
    /// the tick index — not paced by surviving frames — the cumulative emitted
    /// samples stay locked to `SampleClock::total_at(target_tick)`, so audio
    /// never drifts away from the video tick timeline (invariant #3, RT-8a). A
    /// `target_tick` at or behind the clock is a no-op (returns a zero-length
    /// block; the clock never rewinds).
    ///
    /// This is the API RT-8b drives from the output tick index carried on each
    /// `StreamItem`; combined with [`repoint`](Self::repoint)'s live-edge seek it
    /// keeps an audio breakaway lip-synced across overload gaps.
    pub fn tick_to(&mut self, target_tick: u64) -> AudioBlock {
        let frames = self.clock.advance_to(target_tick);
        self.mix(frames)
    }

    /// Pull `frames` from each routed source's store and mix the program bus,
    /// returning an [`AudioBlock`] of exactly `frames` frames — never shorter,
    /// never absent (the audio continuity guarantee). Does no I/O and cannot
    /// block (the stores are wait-free reads), so it is safe on the output-clock
    /// hot loop (invariant #1) and never back-pressures any input (invariant #10).
    fn mix(&mut self, frames: usize) -> AudioBlock {
        // Pull-and-submit + mix with the PRE-advance envelope position (so this
        // tick's block uses the ramp values for its own frames), then advance the
        // ramps and retire any completed cross-fade. Disjoint field borrows:
        // `routes` (read) + `mixer` (mutated) are different fields.
        let block = {
            let Self {
                mixer,
                routes,
                format,
                ..
            } = self;
            for (point, store) in routes.iter() {
                // `read` always returns exactly `frames` frames at the store
                // format (= the bus format), so `submit` cannot fail on a format
                // mismatch; a (impossible) error degrades to dropping that input
                // this tick rather than panicking on the hot loop.
                let block = store.read(frames);
                let _ = mixer.submit(*point, block);
            }
            // The mix is `frames` long whenever any source submitted (each
            // submits exactly `frames`); with no sources it is empty — fall back
            // to a full budget of silence so the program bus is never short or
            // absent.
            match mixer.mix_program() {
                Some(block) if block.frame_count() == frames => block,
                _ => AudioBlock::silence(*format, frames),
            }
        };

        // Advance every in-flight ramp by this tick's budget, then retire any
        // outgoing cross-fade strip whose `cos` taper has run out: unroute it so
        // the old source contributes nothing more (no lingering double-count) and
        // drop it from the route table (its cursor freezes; never read again).
        self.mixer.advance_ramps(frames);
        self.retire_completed_fades();

        block
    }

    /// Unroute and forget any outgoing cross-fade strip whose ramp has completed.
    /// A retired ramp is gone from the mixer (`advance_ramps` cleared it), so a
    /// `fading_out` strip with no ramp left has finished its fade-out.
    fn retire_completed_fades(&mut self) {
        let Self {
            mixer,
            routes,
            fading_out,
            ..
        } = self;
        fading_out.retain(|&out_point| {
            if mixer.gain_ramp(out_point).is_some() {
                return true; // still fading
            }
            // Fade done: take it off the bus and out of the read loop.
            mixer.unroute_from_program(out_point);
            routes.retain(|(p, _)| *p != out_point);
            false
        });
    }
}
