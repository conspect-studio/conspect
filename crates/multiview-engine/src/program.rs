//! The **`Program`** abstraction (ADR-0030, MP-0).
//!
//! Today the engine runs exactly **one** program: one fixed-cadence
//! [`OutputClock`] drives one composited canvas per tick,
//! encoded once and fanned to N transports (invariant #7). ADR-0030 makes that
//! single program the first instance of a general `Program` so the engine can
//! eventually run N concurrent, independently start/stoppable output pipelines
//! (MP-1's `ProgramSet`), each one a multiview composite (today), a guarded
//! passthrough (MP-3), or a transcode (MP-4).
//!
//! ## What MP-0 lands
//!
//! [`MultiviewProgram`] is **today's per-program run core, relocated and owned by
//! one struct** — a *move*, not a rewrite. It owns the program's own
//! [`OutputClock`] + [`CompositorDrive`] (via an
//! [`EngineRuntime`]) and its own [`StopSignal`], and it
//! drives the protected per-tick loop. The CLI's run path
//! (`Pipeline::drive_streaming`) now constructs **one** [`MultiviewProgram`] from
//! a [`ProgramSpec`] derived from the existing single-program config and drives
//! the loop through it instead of building an [`EngineRuntime`] inline — with
//! **zero behavioural change** (proven by every existing invariant/pipeline test
//! passing unchanged). The off-hot-path bake → encode → fan-out egress stays in
//! the CLI (it is genuinely CLI machinery, not engine code); the `Program` owns
//! the clock + drive + stop, exactly as ADR-0030 §2.2 specifies.
//!
//! The kind enum [`ProgramKind`] is **`#[non_exhaustive]` with only `Multiview`
//! populated** (re-exported from `multiview-config`): the guarded-passthrough and
//! transcode kinds land with their own slices (MP-3/MP-4), so the enum never
//! names a kind the engine cannot run.
//!
//! ## Invariants (unchanged from the inline path)
//!
//! * **#1 (output-clock).** [`MultiviewProgram`] wraps one [`EngineRuntime`],
//!   which awaits **only** its own pacer deadline — never an input, never a
//!   consumer, never another program (the structural basis for "one program
//!   stalling never stalls another", MP-1).
//! * **#10 (isolation).** The run methods take the caller's wait-free
//!   [`EnginePublisher`] + the non-blocking per-tick
//!   `control` hook verbatim; the program adds no new engine→outside channel.

use std::sync::Arc;

use multiview_compositor::pipeline::Nv12Image;
use multiview_config::ProgramSpec;
pub use multiview_config::{ProgramId, ProgramKind};

use crate::clock::{OutputClock, TimeSource};
use crate::drive::{CompositedFrame, CompositorDrive};
use crate::error::{Error, Result};
use crate::isolation::EnginePublisher;
use crate::runtime::{EngineRuntime, Pacer, RunOutcome, StopSignal};

/// One multiview program: it owns its [`ProgramId`], its protected per-program
/// output core (an [`EngineRuntime`] = its own [`OutputClock`] +
/// [`CompositorDrive`] + time source + pacer), and its own [`StopSignal`].
///
/// This is the MP-0 home of the per-program run logic that previously lived
/// inline in `Pipeline::drive_streaming`: the clock build, the runtime, and the
/// per-tick drive loop. The CLI assembles the clock + drive (it knows the layout,
/// stores, and canvas color), hands them here, and drives the loop through
/// [`MultiviewProgram::run_with_control`] / [`MultiviewProgram::run_for_with_control`].
///
/// `P` is the [`Pacer`] (real-time in production; a cooperative test pacer in
/// deterministic tests), matching [`EngineRuntime`].
pub struct MultiviewProgram<P> {
    /// This program's identity (`"main"` for the legacy single-program run).
    id: ProgramId,
    /// The protected per-program output core: owns the clock, the compositor
    /// drive, the time source, and the pacer, and runs the per-tick loop.
    runtime: EngineRuntime<P>,
    /// This program's own stop handle. The run loop checks it once per tick;
    /// raising it asks the program to finish the current tick and return. MP-1's
    /// `ProgramSet::stop(id)` raises **only** this program's signal, leaving every
    /// sibling untouched. For the MP-0 single-program path the CLI builds the
    /// program with the stop it already wires to Ctrl-C / the control plane.
    stop: StopSignal,
}

impl<P: Pacer> MultiviewProgram<P> {
    /// Build a multiview program from a [`ProgramSpec`] and the already-assembled
    /// per-program clock + compositor drive + time source + pacer + stop handle.
    ///
    /// The `spec` supplies the program identity (and, in later slices, the
    /// per-program canvas/layout/outputs the caller will assemble the `drive` and
    /// egress from); the caller supplies the `clock`/`drive` it built from that
    /// spec's geometry. The cadence the clock runs at **must** match the spec's
    /// canvas fps — that is the program's contract — so this rejects a mismatch
    /// rather than silently running at the wrong rate.
    ///
    /// [`EngineRuntime::new`] reads tick 0's seed from `time` here, so construct
    /// the program at the instant tick 0 should be due (the CLI does its
    /// prime-wait *before* calling this, exactly as before).
    ///
    /// # Errors
    ///
    /// - [`Error::WrongProgramKind`] if `spec.kind` is not
    ///   [`ProgramKind::Multiview`] (the passthrough/transcode kinds run through
    ///   their own program types, MP-3/MP-4).
    /// - [`Error::InvalidCadence`] if the `clock`'s cadence does not match the
    ///   spec's multiview canvas fps (a programming error in the caller's
    ///   assembly, surfaced rather than run at the wrong rate).
    pub fn new(
        spec: &ProgramSpec,
        clock: OutputClock,
        drive: CompositorDrive<Nv12Image>,
        time: Arc<dyn TimeSource>,
        pacer: P,
        stop: StopSignal,
    ) -> Result<Self> {
        // `ProgramKind` is `#[non_exhaustive]`: handle the multiview kind this
        // program serves and reject any other (future) kind with a typed error —
        // never a panic, never a silent skip.
        let spec_cadence = match &spec.kind {
            ProgramKind::Multiview { canvas, .. } => canvas.fps,
            other => return Err(Error::WrongProgramKind(other.tag())),
        };
        if clock.cadence() != spec_cadence.rational() {
            return Err(Error::invalid_cadence(clock.cadence()));
        }
        let runtime = EngineRuntime::new(clock, drive, time, pacer);
        Ok(Self {
            id: spec.id.clone(),
            runtime,
            stop,
        })
    }

    /// This program's identity.
    #[must_use]
    pub const fn id(&self) -> &ProgramId {
        &self.id
    }

    /// The fixed output cadence of this program's clock.
    #[must_use]
    pub fn cadence(&self) -> multiview_core::time::Rational {
        self.runtime.cadence()
    }

    /// The seed instant (time-source nanoseconds) this program's tick 0 is
    /// anchored to (see [`EngineRuntime::seed_nanos`](crate::EngineRuntime::seed_nanos)).
    ///
    /// DEV-C1 (ADR-M010): the outbound presentation epoch binds this anchor to
    /// disciplined wall time — reading it is a pure value access and can never
    /// influence the clock.
    #[must_use]
    pub const fn seed_nanos(&self) -> i64 {
        self.runtime.seed_nanos()
    }

    /// A clone of this program's [`StopSignal`] — the "stop handle" the program
    /// owns (ADR-0030 §2.2). MP-1's `ProgramSet::stop(id)` raises this to stop
    /// exactly this program; siblings are untouched.
    #[must_use]
    pub fn stop_signal(&self) -> StopSignal {
        self.stop.clone()
    }

    /// Total ticks this program has emitted so far (cumulative across run calls).
    ///
    /// MP-1's chaos gate samples this to prove a sibling's clock keeps advancing
    /// while another program's egress is wedged (invariants #1 + #10, per
    /// program).
    #[must_use]
    pub fn ticks_emitted(&self) -> u64 {
        self.runtime.ticks_emitted()
    }

    /// A clone of the wait-free cumulative-ticks counter (see
    /// [`EngineRuntime::ticks_counter`](crate::EngineRuntime::ticks_counter)).
    ///
    /// MP-1's [`ProgramSet`](crate::ProgramSet) keeps this handle on the
    /// supervisor side so it can read a running program's `ticks_emitted` from
    /// another task — proving a sibling clock keeps advancing while this program
    /// runs (or while another program's egress is wedged) — without ever touching
    /// the program's hot loop (invariants #1 + #10).
    #[must_use]
    pub fn ticks_counter(&self) -> std::sync::Arc<std::sync::atomic::AtomicU64> {
        self.runtime.ticks_counter()
    }

    /// Drive the protected per-tick loop **forever**, until this program's
    /// [`StopSignal`] is raised, applying the per-tick `control` reconfiguration
    /// hook at each frame boundary.
    ///
    /// This is the live-daemon entry the CLI's `run_until`/`run_until_serving`
    /// path uses. It is a thin, behaviour-preserving delegation to
    /// [`EngineRuntime::run_with_control`] using this program's **own** stop
    /// signal — the same call the inline path made, only the stop handle is now
    /// owned by the program.
    ///
    /// `state_of`/`event_of` project each tick's [`CompositedFrame`] into the
    /// caller's wire state/event types (published through the wait-free
    /// `publisher`); `control` runs on the output-clock loop and **must not
    /// block, await, or hold a client-fillable lock** (invariants #1 + #10).
    ///
    /// # Errors
    ///
    /// Propagates [`Error::Canvas`] if the compositor rejects the (structurally
    /// fixed) canvas geometry — input health is never an error.
    pub async fn run_with_control<S, E, FS, FE, FC>(
        &mut self,
        publisher: &EnginePublisher<S, E>,
        state_of: FS,
        event_of: FE,
        control: FC,
    ) -> Result<RunOutcome>
    where
        FS: FnMut(&CompositedFrame) -> S,
        FE: FnMut(&CompositedFrame) -> Option<E>,
        FC: FnMut(&mut CompositorDrive<Nv12Image>),
    {
        self.runtime
            .run_with_control(publisher, &self.stop, state_of, event_of, control)
            .await
    }

    /// Drive the protected per-tick loop for at most `max_ticks` ticks (or until
    /// this program's [`StopSignal`] is raised), applying the per-tick `control`
    /// hook at each frame boundary.
    ///
    /// This is the bounded/offline entry the CLI's `run_for` path uses.
    /// Behaviour-preserving delegation to
    /// [`EngineRuntime::run_for_with_control`] over this program's own stop
    /// signal.
    ///
    /// # Errors
    ///
    /// See [`MultiviewProgram::run_with_control`].
    pub async fn run_for_with_control<S, E, FS, FE, FC>(
        &mut self,
        publisher: &EnginePublisher<S, E>,
        max_ticks: u64,
        state_of: FS,
        event_of: FE,
        control: FC,
    ) -> Result<RunOutcome>
    where
        FS: FnMut(&CompositedFrame) -> S,
        FE: FnMut(&CompositedFrame) -> Option<E>,
        FC: FnMut(&mut CompositorDrive<Nv12Image>),
    {
        self.runtime
            .run_for_with_control(
                publisher, &self.stop, max_ticks, state_of, event_of, control,
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::collections::HashMap;

    use multiview_compositor::blend::LinearRgba;
    use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
    use multiview_config::ProgramSpec;
    use multiview_core::color::ColorInfo;
    use multiview_core::layout::{Canvas as CoreCanvas, Layout as CoreLayout};
    use multiview_core::time::Rational;

    use crate::clock::{ManualTimeSource, OutputClock};
    use crate::isolation::EnginePublisher;
    use crate::runtime::{CooperativePacer, RunStop, StopSignal};

    /// Build a `"main"` multiview [`ProgramSpec`] at `num/den` fps by
    /// deserialization (the config schema structs are `#[non_exhaustive]`, so
    /// they cannot be struct-literal-built across the crate boundary; the wire
    /// form is the canonical constructor).
    fn spec_at(num: i64, den: i64) -> ProgramSpec {
        let json = format!(
            r##"{{
                "id": "main",
                "kind": "multiview",
                "canvas": {{
                    "width": 16,
                    "height": 16,
                    "fps": "{num}/{den}",
                    "pixel_format": "nv12",
                    "background": "#000000",
                    "color": {{ "profile": "sdr-bt709-limited" }}
                }},
                "layout": {{ "kind": "preset", "preset": "1x1" }}
            }}"##
        );
        serde_json::from_str(&json).expect("main multiview spec deserializes")
    }

    fn nosignal_card(w: u32, h: u32) -> Nv12Image {
        let color = ColorInfo::default().resolve_defaults(1920, 1080);
        Nv12Image::solid(w, h, 16, 128, 128, color).expect("nosignal card builds")
    }

    /// A bare, cell-free 16x16 drive at `cadence` — enough to drive the clock
    /// (the per-program run logic), with no sources to feed.
    fn empty_drive(cadence: Rational) -> CompositorDrive<Nv12Image> {
        let layout = CoreLayout {
            name: "mp0-test".to_owned(),
            canvas: CoreCanvas {
                width: 16,
                height: 16,
                fps_num: cadence.num,
                fps_den: cadence.den,
            },
            cells: Vec::new(),
        };
        CompositorDrive::new(
            Arc::new(layout),
            HashMap::new(),
            nosignal_card(16, 16),
            CanvasColor::default(),
            LinearRgba::TRANSPARENT,
        )
        .expect("16x16 cell-free drive builds")
    }

    #[tokio::test]
    async fn run_for_emits_exactly_max_ticks_through_the_program() {
        let cadence = Rational::new(25, 1);
        let clock = OutputClock::new(cadence).unwrap();
        let drive = empty_drive(cadence);
        let manual = Arc::new(ManualTimeSource::new());
        let time: Arc<dyn TimeSource> = manual.clone();
        let stop = StopSignal::new();
        let spec = spec_at(25, 1);
        let mut program =
            MultiviewProgram::new(&spec, clock, drive, time, CooperativePacer, stop).unwrap();

        assert_eq!(program.id().as_str(), "main");
        assert_eq!(program.cadence(), cadence);

        // The cooperative pacer returns once `now >= deadline`; the runtime read
        // its seed at construction, so advancing the manual source one full
        // second PAST the seed puts all 5 (25 fps) tick deadlines in the past,
        // letting the bounded run drive deterministically with no real sleep.
        manual.advance(std::time::Duration::from_secs(1));

        let publisher = EnginePublisher::<u64, ()>::new(8);
        let outcome = program
            .run_for_with_control(
                &publisher,
                5,
                |f: &CompositedFrame| f.tick.index,
                |_f: &CompositedFrame| None,
                |_d: &mut CompositorDrive<Nv12Image>| {},
            )
            .await
            .unwrap();
        assert_eq!(outcome.ticks, 5);
        assert_eq!(outcome.stop, RunStop::Completed);
        assert_eq!(program.ticks_emitted(), 5);
    }

    #[tokio::test]
    async fn stop_signal_handle_stops_this_program() {
        let cadence = Rational::new(30, 1);
        let clock = OutputClock::new(cadence).unwrap();
        let drive = empty_drive(cadence);
        let time: Arc<dyn TimeSource> = Arc::new(ManualTimeSource::new());
        let stop = StopSignal::new();
        let spec = spec_at(30, 1);
        let mut program =
            MultiviewProgram::new(&spec, clock, drive, time, CooperativePacer, stop).unwrap();

        // Raise the program's OWN stop handle before running: the forever loop
        // returns immediately with zero ticks (the handle the program exposes is
        // the one the loop checks).
        program.stop_signal().stop();
        let publisher = EnginePublisher::<u64, ()>::new(8);
        let outcome = program
            .run_with_control(
                &publisher,
                |f: &CompositedFrame| f.tick.index,
                |_f: &CompositedFrame| None,
                |_d: &mut CompositorDrive<Nv12Image>| {},
            )
            .await
            .unwrap();
        assert_eq!(outcome.ticks, 0);
        assert_eq!(outcome.stop, RunStop::Stopped);
    }

    #[test]
    fn cadence_mismatch_between_clock_and_spec_is_rejected() {
        // The clock runs at 25 fps but the spec declares 30 fps: the contract is
        // violated, so construction errors rather than running at the wrong rate.
        let clock = OutputClock::new(Rational::new(25, 1)).unwrap();
        let drive = empty_drive(Rational::new(25, 1));
        let time: Arc<dyn TimeSource> = Arc::new(ManualTimeSource::new());
        let spec = spec_at(30, 1);
        let err = MultiviewProgram::new(
            &spec,
            clock,
            drive,
            time,
            CooperativePacer,
            StopSignal::new(),
        );
        assert!(err.is_err());
    }
}
