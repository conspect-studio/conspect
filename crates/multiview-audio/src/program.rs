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
///
/// The class is a property of the **path**, not of the tier (ADR-0059 §7): the
/// control-plane crosspoint classifier is authoritative for a given take, and
/// [`SwitchTier::class`] reports only the tier's worst-case cost floor.
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
///   ([`ProgramBus::repoint_crossfade`]).
///
/// ## The apply class is a property of the path, not of the tier
///
/// [`class`](Self::class) reports the tier's **worst-case cost floor** — the
/// class it carries on a **coded-passthrough** path, where choosing the decoded
/// cross-fade would *force* a transcode that was not otherwise happening (so
/// [`ClickFree`](Self::ClickFree) floors at [`Class2`](ApplyClass::Class2)). It
/// is **not** the live answer for a given take: on the program-bus path the bus
/// is *always* decode → mix → re-encode when program audio is on, so the
/// cross-fade adds no reset and no new transcode and the take is **Class-1**.
/// The authoritative classification is the control plane's crosspoint
/// classifier (invariant #11; ADR-0059 §7), never this tier. Gain, mute, AFV
/// cross-fades and master envelopes are all Class-1 on that path; only a bus
/// **layout** change (count / identity / per-track codec) stays Class-2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SwitchTier {
    /// Coded passthrough, AU-aligned step. Hot (Class-1) and video-clean, but
    /// not guaranteed pop-free (the coded-domain seam transient is
    /// uncancellable).
    SoftStep,
    /// Decoded-bus equal-power cross-fade. Genuinely pop-free; its worst-case
    /// cost floor is Class-2 (a coded-passthrough path would pay a transcode it
    /// was not otherwise paying), but on the always-re-encoded program-bus path
    /// the take is Class-1 (see the type docs).
    ClickFree,
}

impl SwitchTier {
    /// The tier's **worst-case cost floor**: the apply class it carries on a
    /// coded-passthrough path. [`SoftStep`](Self::SoftStep) is
    /// [`Class1`](ApplyClass::Class1); [`ClickFree`](Self::ClickFree) is
    /// [`Class2`](ApplyClass::Class2).
    ///
    /// This is a floor, not the live answer — the apply class is a property of
    /// the **path** (ADR-0059 §7), and the control-plane crosspoint classifier
    /// is authoritative for a given take (a program-bus re-point is Class-1).
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
    /// its ramp completes it is reclaimed from the mixer and dropped from
    /// `routes`. Each entry pairs the channel with its outgoing strip so mute
    /// applies to every contribution, and [`mix`](ProgramBus::mix) retires the
    /// strip on the tick its ramp finishes.
    fading_out: Vec<(RoutePoint, RoutePoint)>,
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
    /// * when the down-ramp completes the outgoing strip is **reclaimed**
    ///   ([`Mixer::remove_input`]) so the old source no longer contributes (no
    ///   lingering double-count) and its mixer slot is returned for reuse (the
    ///   strip storage stays bounded across cross-fades — invariant #9).
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
        self.fading_out.push((point, out_point));
        // The outgoing strip inherits the channel's mute envelope: re-pointing a
        // muted channel must stay silent across the cross-fade rather than leak
        // the old source's audio (mute ≠ unroute, ADR-0059 §4).
        self.mixer.copy_mute_state(point, out_point);

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

    /// Set a bus channel's steady program-route gain (linear) — the `SetGain`
    /// half of ADR-0059 §4. Leaves the route and the mute state untouched; a
    /// gain change is not a route change and not an unmute. A no-op for an
    /// unknown point.
    pub fn set_gain(&mut self, point: RoutePoint, gain: f64) {
        self.mixer.set_gain(point, gain);
    }

    /// Mute or unmute a bus channel's program contribution **without unrouting
    /// it** (ADR-0059 §4, "mute ≠ unroute"): the channel's steady gain is
    /// preserved, so an unmute restores it exactly, and the change rides the
    /// mixer's short equal-power anti-click envelope. Every outgoing cross-fade
    /// strip still feeding this channel receives the same mute transition; an
    /// unmute restores each strip's own preserved gain. A no-op for an unknown
    /// point.
    pub fn set_muted(&mut self, point: RoutePoint, muted: bool) {
        self.mixer.set_muted(point, muted);
        for &(channel, outgoing) in &self.fading_out {
            if channel == point {
                self.mixer.set_muted(outgoing, muted);
            }
        }
    }

    /// Whether a bus channel's program contribution is muted. `false` for an
    /// unknown point (nothing to mute).
    #[must_use]
    pub fn is_muted(&self, point: RoutePoint) -> bool {
        self.mixer.is_muted(point).unwrap_or(false)
    }

    /// The current steady program-route gain of a bus channel, if known.
    #[must_use]
    pub fn program_gain(&self, point: RoutePoint) -> Option<f64> {
        self.mixer.program_gain(point)
    }

    /// The number of physical mixer input-strip slots backing this bus
    /// (occupied strips + any reclaimed-free slots).
    ///
    /// The bounded-memory observable for the cross-fade path (invariant #9):
    /// repeated completed cross-fades must **reuse** their retired strip slots,
    /// so this stays bounded and never grows one leaked strip per fade. See
    /// [`Mixer::slot_count`](crate::mixer::Mixer::slot_count).
    #[must_use]
    pub fn mixer_slot_count(&self) -> usize {
        self.mixer.slot_count()
    }

    /// The number of currently-occupied mixer strips on this bus: the routed
    /// channel(s) plus any in-flight cross-fade's outgoing strip. A completed
    /// fade's outgoing strip is reclaimed and is **not** counted. See
    /// [`Mixer::live_input_count`](crate::mixer::Mixer::live_input_count).
    #[must_use]
    pub fn live_strip_count(&self) -> usize {
        self.mixer.live_input_count()
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
        // outgoing cross-fade strip whose `cos` taper has run out: reclaim its
        // mixer slot so the old source contributes nothing more (no lingering
        // double-count) and the slot is reused rather than leaked (invariant #9),
        // and drop it from the route table (its cursor freezes; never read again).
        self.mixer.advance_ramps(frames);
        self.retire_completed_fades();

        block
    }

    /// Reclaim any outgoing cross-fade strip whose ramp has completed. A retired
    /// ramp is gone from the mixer (`advance_ramps` cleared it), so a `fading_out`
    /// strip with no ramp left has finished its fade-out.
    ///
    /// A finished strip is **removed** from the mixer
    /// ([`Mixer::remove_input`](crate::mixer::Mixer::remove_input)) — not merely
    /// unrouted — so its slot is returned to the mixer's free-list for reuse and
    /// the strip storage stays bounded across arbitrarily many cross-fades
    /// (invariant #9), and it is dropped from the bus read loop (its cursor
    /// freezes; the old source is never read again).
    fn retire_completed_fades(&mut self) {
        let Self {
            mixer,
            routes,
            fading_out,
            ..
        } = self;
        fading_out.retain(|&(_, out_point)| {
            if mixer.gain_ramp(out_point).is_some() {
                return true; // still fading
            }
            // Fade done: reclaim the strip's mixer slot (returned to the free-list
            // for reuse) and take it out of the bus read loop.
            mixer.remove_input(out_point);
            routes.retain(|(p, _)| *p != out_point);
            false
        });
    }
}
