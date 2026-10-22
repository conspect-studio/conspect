//! The `multiview run` subcommand and its **FFmpeg-free software engine**.
//!
//! [`SoftwareEngine`] wires the protected output core from a validated
//! [`MultiviewConfig`]:
//!
//! * a fixed-cadence [`OutputClock`] at the canvas cadence (exact rational);
//! * one [`TileStore<Nv12Image>`] per declared source, holding the source's
//!   synthetic frame — real colour bars / solid for the `bars`/`solid` kinds, a
//!   per-tile placeholder for kinds that need a decoder this build lacks;
//! * a CPU reference [`CompositorDrive`] over the solved
//!   [`multiview_core::layout::Layout`]; and
//! * the engine's outbound [`EnginePublisher`] (invariant #10 isolation).
//!
//! It then drives exactly one composited frame per tick for a bounded number of
//! ticks (or until Ctrl-C in the binary), and reports the outcome. This is a
//! GPU-free, `FFmpeg`-free **software end-to-end smoke of invariant #1**: the
//! output emits one valid frame per tick, on cadence, forever, independent of
//! input health.
//!
//! The driver is the engine's own [`EngineRuntime`], parameterized by an
//! injected [`TimeSource`] + [`Pacer`] so the same code runs deterministically
//! in tests (manual time + cooperative pacer, no real sleeps) and in production
//! (monotonic time + realtime pacer).
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_config::{MultiviewConfig, Source, SourceKind};
use multiview_control::EngineStateSnapshot;
use multiview_core::layout::Layout;
use multiview_core::time::{MediaTime, Rational};
use multiview_engine::{
    CompositorDrive, EnginePublisher, EngineRuntime, ManualTimeSource, MonotonicTimeSource,
    OutputClock, Pacer, RealtimePacer, RunStop, StopSignal, TimeSource,
};
use multiview_events::Event;
use multiview_framestore::{NoSignalPolicy, TileStore, TileThresholds};

use crate::synth::{generator_loop, SyntheticKind};

/// The per-subscriber drop-oldest depth of the engine's outbound event stream.
/// The software smoke has no consumers, but the publisher still needs a positive
/// ring.
const EVENT_CAPACITY: usize = 64;

/// Capture the composited program frame into the live-preview slot every Nth
/// tick. At 30–60 fps this is ≈2–4 preview frames/sec — enough for a monitoring
/// still, cheap enough to clone on the hot loop without affecting the cadence.
const PREVIEW_CAPTURE_EVERY: u64 = 15;

/// The state snapshot the software engine publishes each tick (invariant #10):
/// the tick index and its presentation timestamp. Best-effort; no consumer can
/// back-pressure its publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct SoftwareState {
    /// The tick index this snapshot was produced for.
    pub tick: u64,
    /// The presentation timestamp of the tick (`out_pts = f(tick)`).
    pub pts: MediaTime,
}

/// A summary of a software run: how many ticks/frames were produced, the
/// cadence, the canvas geometry, the PTS span, and whether the output ever
/// faltered (it must not).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct RunReport {
    /// Ticks emitted by the output clock.
    pub ticks: u64,
    /// Frames composited and published (must equal [`RunReport::ticks`]).
    pub frames: u64,
    /// The fixed output cadence (exact rational, never a float fps).
    pub cadence: Rational,
    /// Output canvas width in pixels.
    pub canvas_width: u32,
    /// Output canvas height in pixels.
    pub canvas_height: u32,
    /// The PTS of the first frame, if any frame was produced.
    pub first_pts: Option<MediaTime>,
    /// The PTS of the last frame, if any frame was produced.
    pub last_pts: Option<MediaTime>,
    /// Whether the output ever faltered: `true` if frames != ticks, or a
    /// frame's PTS failed to advance monotonically. **Must be `false`.**
    pub faltered: bool,
}

impl RunReport {
    /// Render the report as the multi-line text the binary prints.
    #[must_use]
    pub fn render(&self) -> String {
        let cadence = self.cadence;
        let span = match (self.first_pts, self.last_pts) {
            (Some(first), Some(last)) => {
                format!("pts {}..={} ns", first.as_nanos(), last.as_nanos())
            }
            _ => "no frames".to_owned(),
        };
        let verdict = if self.faltered {
            "FALTERED"
        } else {
            "never faltered"
        };
        format!(
            "software run: {} frame(s) for {} tick(s) at {}/{} fps on {}x{}; {}; output {}",
            self.frames,
            self.ticks,
            cadence.num,
            cadence.den,
            self.canvas_width,
            self.canvas_height,
            span,
            verdict,
        )
    }
}

/// The exact operator/portal copy the Conspect startup gate (S1) refuses a NEW
/// engine instance with at the `block-new-instance` rung (ADR-0050 §5/§6.2, the
/// brief §6.2). **Verbatim** — the operator, the portal, and the API surface the
/// same words; do not reword. The trailing clause is the product promise: a
/// running program is untouched.
pub const BLOCK_NEW_INSTANCE_REASON: &str =
    "Lease expired — new engine instances won't start; running ones untouched";

/// The Conspect **startup gate** (S1, ADR-0050 §5): decide whether a NEW engine
/// instance may be created from the published entitlement [`EnforcementLevel`].
///
/// Returns `Ok(())` for every rung except `block-new-instance`, and `None`
/// (no entitlement plane / no installed lease) always allows a start — the gate
/// fails toward leniency, hardening only on positive evidence of the hardest
/// lapse (ADR-0050 §6.3). At `block-new-instance` it returns
/// [`RunError::LeaseExpired`] carrying [`BLOCK_NEW_INSTANCE_REASON`].
///
/// # Never off air (invariant #1)
///
/// This gate is consulted **before** an engine is built (mirroring the NDI
/// pre-flight gate); a **running** engine never re-enters it, so the hardest rung
/// can only refuse a *new* start — it can never stop a running program. It reads
/// one pre-derived enum and returns; no engine handle, no I/O, no lock.
///
/// # Errors
///
/// [`RunError::LeaseExpired`] iff `level` is
/// [`EnforcementLevel::BlockNewInstance`](multiview_licence::EnforcementLevel::BlockNewInstance).
pub fn start_gate(level: Option<multiview_licence::EnforcementLevel>) -> Result<(), RunError> {
    match level {
        Some(level) if level.blocks_new_instances() => {
            Err(RunError::LeaseExpired(BLOCK_NEW_INSTANCE_REASON.to_owned()))
        }
        _ => Ok(()),
    }
}

/// Errors that can occur building or running the software engine.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RunError {
    /// The configuration failed validation before any engine was built.
    #[error("invalid configuration: {0}")]
    Config(#[from] multiview_config::ConfigError),
    /// The Conspect startup gate (S1) refused creating a **new** engine instance
    /// because the entitlement lease is at the `block-new-instance` rung. The
    /// string is [`BLOCK_NEW_INSTANCE_REASON`] — a **running** instance is never
    /// affected (this gate runs only at build time, ADR-0050 §5/§6.3).
    #[error("{0}")]
    LeaseExpired(String),
    /// The output clock rejected the canvas cadence.
    #[error("output clock: {0}")]
    Clock(String),
    /// The compositor drive or canvas was rejected by the engine.
    #[error("engine: {0}")]
    Engine(String),
    /// A synthetic test-pattern frame could not be built.
    #[error("test pattern: {0}")]
    Pattern(String),
}

/// A built, ready-to-run software engine.
///
/// Construct one with [`SoftwareEngine::build`] from a validated config, then
/// drive it with [`SoftwareEngine::run_for`] (deterministic, injected time) or
/// [`SoftwareEngine::run_for_realtime`] / [`SoftwareEngine::run_until_stopped`]
/// (production wall-clock pacing).
///
/// The engine is consumed-shaped: each `run_*` method takes `&mut self` and is
/// intended to be driven once; it rebuilds the compositor drive per run so the
/// stores (and their synthetic frames) are reused intact.
pub struct SoftwareEngine {
    /// The solved layout (canvas + normalized cells), shared into the drive.
    layout: Arc<Layout>,
    /// The fixed output cadence (exact rational).
    cadence: Rational,
    /// Per-source last-good-frame stores, keyed by source id.
    stores: HashMap<String, Arc<TileStore<Nv12Image>>>,
    /// The synthetic frame each source contributes (real bars/solid, or a
    /// placeholder card), kept so a run can (re)publish it into the stores.
    patterns: HashMap<String, Arc<Nv12Image>>,
    /// Sources whose picture **changes over time** (the `clock` kind), driven by
    /// a [`synth::generator_loop`](crate::synth::generator_loop) thread rather
    /// than primed once. Empty unless an animated source is built **and** this
    /// build renders it (the `overlay` feature is on); without `overlay` an
    /// animated source falls back to the primed placeholder card instead.
    animated: Vec<AnimatedSource>,
    /// The fixed canvas color (ADR-C001 SDR BT.709 limited by default).
    canvas_color: CanvasColor,
    /// The "no signal" slate composited for tiles with no usable frame.
    nosignal_card: Nv12Image,
    /// The canvas background shown where no tile covers.
    background: LinearRgba,
    /// Whether to publish synthetic test-pattern frames into the stores at the
    /// start of a run (default `true`). Set `false` to prove the output is
    /// independent of input health (every tile shows the slate).
    publish_test_frames: bool,
    /// Wait-free slot the continuous run loop publishes a throttled clone of the
    /// composited program frame into, for the control plane's live preview. Read
    /// by the preview provider off the hot loop (invariant #10).
    program_preview: crate::preview::ProgramSlot,
    /// The per-source producer stop flags (ADR-W018): every startup generator
    /// registers its flag here, and the live-source hub shares the same
    /// registry, so a live `RemoveSource` can tear down exactly one producer.
    stop_registry: crate::live_sources::StopRegistry,
    /// The run's epoch **anchor** slot (DEV-C1 / ADR-M010): the forever run
    /// publishes its tick-0 seed + monotonic source here when the runtime
    /// seeds, so the timing-status task can derive the outbound presentation
    /// epoch off the hot path (lock-free store/load — inv #1/#10).
    epoch_anchor: crate::timing_status::EpochAnchorSlot,
}

/// An animated synthetic source recorded for a generator thread: its store, the
/// resolved synthetic kind, and the canvas geometry it renders at.
struct AnimatedSource {
    /// The per-source last-good-frame store the generator publishes into.
    store: Arc<TileStore<Nv12Image>>,
    /// The resolved synthetic kind (a `clock` variant — [`SyntheticKind::animated`]).
    kind: SyntheticKind,
    /// The width the generator renders at (the full canvas width; the compositor
    /// scales to the tile).
    width: u32,
    /// The height the generator renders at (the full canvas height).
    height: u32,
}

/// A running set of synthetic-source generator threads (the `clock` sources).
///
/// Each thread runs [`synth::generator_loop`](crate::synth::generator_loop),
/// publishing a freshly-baked frame into its lock-free [`TileStore`] every tick
/// (re-baking only when the displayed second changes). The engine only *samples*
/// those stores, so a generator can neither pace nor stall the output clock
/// (invariant #1) nor back-pressure the engine (invariant #10).
///
/// Each thread carries its **own** stop flag (ADR-W018), registered in the
/// engine's shared [`StopRegistry`](crate::live_sources::StopRegistry) so a
/// live `RemoveSource` can tear down exactly one startup generator;
/// [`GeneratorSupervisor::shutdown`] raises every flag and joins every thread.
/// The chunked `sleep_until` inside `generator_loop` makes teardown prompt (a
/// thread observes its flag within ≤25 ms).
#[must_use = "the generators run until shutdown; drop without shutdown leaks threads"]
pub struct GeneratorSupervisor {
    /// One (stop flag, join handle) per spawned generator thread.
    producers: Vec<(Arc<AtomicBool>, JoinHandle<()>)>,
}

impl GeneratorSupervisor {
    /// Raise every generator's stop flag and join every generator thread.
    ///
    /// Idempotent-shaped: consumes the supervisor. A thread that failed to spawn
    /// was never recorded, so this only joins live threads. A generator already
    /// torn down by a live remove (its flag raised via the registry) has exited;
    /// its join returns immediately.
    pub fn shutdown(self) {
        for (stop, _) in &self.producers {
            stop.store(true, Ordering::Release);
        }
        for (_, handle) in self.producers {
            // A generator thread only ever *writes* a lock-free store it shares by
            // `Arc` and owns no external resource, so a join error (a panicked
            // thread) cannot corrupt the produced output; log and continue so one
            // wedged generator never blocks teardown of the rest.
            if handle.join().is_err() {
                tracing::error!("a synthetic-source generator thread panicked");
            }
        }
    }
}

impl SoftwareEngine {
    /// Build a software engine from an already-validated configuration.
    ///
    /// Solves the layout, creates one [`TileStore`] per source (with
    /// [`NoSignalPolicy::HoldForever`] so a once-published synthetic frame stays
    /// available across the whole bounded run), and builds each source's
    /// synthetic frame via [`software_source_frame`] (real bars/solid, else a
    /// per-tile placeholder).
    ///
    /// # Errors
    ///
    /// Returns [`RunError::Config`] if the layout cannot be solved (the document
    /// should be validated first), or [`RunError::Pattern`] if a synthetic frame
    /// cannot be constructed for the canvas geometry.
    pub fn build(config: &MultiviewConfig) -> Result<Self, RunError> {
        let layout = config.solve_layout()?;
        let cadence = config.canvas.fps.rational();
        let canvas_color = CanvasColor::default();
        let tag = canvas_color.output_tag();

        // One store per declared source. HoldForever keeps the synthetic frame
        // available for the whole bounded run regardless of how far the manual
        // clock advances; the *state* still rides the LIVE/STALE ladder.
        let mut stores: HashMap<String, Arc<TileStore<Nv12Image>>> =
            HashMap::with_capacity(config.sources.len());
        let mut patterns: HashMap<String, Arc<Nv12Image>> =
            HashMap::with_capacity(config.sources.len());
        let mut animated: Vec<AnimatedSource> = Vec::new();

        for (index, source) in config.sources.iter().enumerate() {
            let store = Arc::new(TileStore::new(
                source.id.clone(),
                TileThresholds::default(),
                NoSignalPolicy::HoldForever,
            ));
            stores.insert(source.id.clone(), Arc::clone(&store));

            // An animated synthetic kind (the `clock`) is driven by a generator
            // thread (one bake/sec) — but only when this build can actually render
            // it. The `clock` render needs the `overlay` feature; without it the
            // generator returns `OverlayRequired` and would publish nothing, so an
            // overlay-off build falls through to the primed placeholder card
            // instead (an honest static fallback rather than a blank tile).
            let synthetic = SyntheticKind::from_source_kind(&source.kind);
            let drive_with_generator = cfg!(feature = "overlay")
                && synthetic.as_ref().is_some_and(SyntheticKind::animated);
            if let Some(kind) = synthetic.filter(|_| drive_with_generator) {
                // Recorded for a generator; NOT primed (no double-publish): the
                // generator owns this tile's stream of frames.
                animated.push(AnimatedSource {
                    store: Arc::clone(&store),
                    kind,
                    width: config.canvas.width,
                    height: config.canvas.height,
                });
                continue;
            }

            let pattern = software_source_frame(
                source,
                config.canvas.width,
                config.canvas.height,
                index,
                canvas_color,
            )
            .map_err(|e| RunError::Pattern(e.to_string()))?;
            patterns.insert(source.id.clone(), Arc::new(pattern));
        }

        // The slate card spans the whole canvas; a tile with no usable frame
        // contributes it (mid-gray, tagged like the canvas).
        let nosignal_card =
            Nv12Image::solid(config.canvas.width, config.canvas.height, 16, 128, 128, tag)
                .map_err(|e| RunError::Pattern(e.to_string()))?;

        Ok(Self {
            layout: Arc::new(layout),
            cadence,
            stores,
            patterns,
            animated,
            canvas_color,
            nosignal_card,
            background: LinearRgba::opaque(0.02, 0.02, 0.05),
            publish_test_frames: true,
            program_preview: crate::preview::program_slot(),
            stop_registry: crate::live_sources::stop_registry(),
            epoch_anchor: crate::timing_status::anchor_slot(),
        })
    }

    /// Build a software engine **through the Conspect startup gate** (S1,
    /// ADR-0050 §5): consult the published entitlement [`EnforcementLevel`]
    /// (sampled off the hot loop) **before** constructing the engine, and refuse
    /// to create a NEW instance at the `block-new-instance` rung with
    /// [`BLOCK_NEW_INSTANCE_REASON`]. Every softer rung (and `None` — no plane /
    /// no lease) builds exactly as [`SoftwareEngine::build`].
    ///
    /// # Never off air (invariant #1)
    ///
    /// The gate runs at build time only; a **running** engine never re-enters
    /// `build`/`build_gated`, so the hardest rung can never stop a running program
    /// (ADR-0050 §6.3). The binary calls this with the level sampled from the
    /// entitlement store.
    ///
    /// # Errors
    ///
    /// [`RunError::LeaseExpired`] when the gate refuses (block-new-instance), else
    /// the same errors as [`SoftwareEngine::build`].
    pub fn build_gated(
        config: &MultiviewConfig,
        level: Option<multiview_licence::EnforcementLevel>,
    ) -> Result<Self, RunError> {
        start_gate(level)?;
        Self::build(config)
    }

    /// The shared per-source producer stop registry (ADR-W018): hand this to
    /// the [`LiveSourceHub`](crate::live_sources::LiveSourceHub) so a live
    /// remove can tear down a startup generator.
    #[must_use]
    pub fn stop_registry(&self) -> crate::live_sources::StopRegistry {
        Arc::clone(&self.stop_registry)
    }

    /// The wait-free program-preview slot (shared with the control plane's
    /// preview provider; the continuous run loop publishes into it).
    #[must_use]
    pub fn program_preview(&self) -> crate::preview::ProgramSlot {
        Arc::clone(&self.program_preview)
    }

    /// The per-source frame stores (shared with the preview provider for the
    /// per-input thumbnails).
    #[must_use]
    pub fn preview_stores(&self) -> HashMap<String, Arc<TileStore<Nv12Image>>> {
        self.stores.clone()
    }

    /// DEV-C1 (ADR-M010): the run's epoch **anchor** slot, filled by the
    /// forever run when its runtime seeds tick 0; the timing-status task binds
    /// to it lazily with a lock-free load (inv #1/#10).
    #[must_use]
    pub fn epoch_anchor_slot(&self) -> crate::timing_status::EpochAnchorSlot {
        Arc::clone(&self.epoch_anchor)
    }

    /// The fixed output cadence (exact rational).
    #[must_use]
    pub const fn cadence(&self) -> Rational {
        self.cadence
    }

    /// The number of per-source frame stores wired into this engine.
    #[must_use]
    pub fn source_count(&self) -> usize {
        self.stores.len()
    }

    /// Control whether synthetic test-pattern frames are published into the
    /// stores at the start of a run.
    ///
    /// Default `true`: each source's tile shows its synthetic frame — real bars/
    /// solid, else a placeholder card (LIVE). Set `false` to leave every store
    /// empty, proving the output produces a valid slate frame per tick even with
    /// no inputs (invariant #1 + #2).
    pub fn set_publish_test_frames(&mut self, publish: bool) {
        self.publish_test_frames = publish;
    }

    /// The number of animated synthetic sources this build drives with a
    /// generator thread (the `clock` kind, when the `overlay` feature renders it).
    #[must_use]
    pub fn animated_source_count(&self) -> usize {
        self.animated.len()
    }

    /// Spawn one [`synth::generator_loop`](crate::synth::generator_loop) thread
    /// per **animated** synthetic source (the `clock` kind), publishing a freshly
    /// baked frame into its lock-free [`TileStore`] at the canvas cadence (re-
    /// baking only when the displayed second changes — one bake/sec).
    ///
    /// Returns a [`GeneratorSupervisor`]; call [`GeneratorSupervisor::shutdown`]
    /// to stop and join the threads. When test-pattern publishing is disabled
    /// (see [`SoftwareEngine::set_publish_test_frames`]) or there are no animated
    /// sources, the returned supervisor owns no threads (every tile then rides
    /// the slate — proving the output is independent of input health, inv #1/#2).
    ///
    /// The generators only ever *write* the stores the engine samples, so they
    /// can neither pace nor stall the output clock (invariant #1) nor back-
    /// pressure the engine (invariant #10).
    //
    // The returned `GeneratorSupervisor` is itself `#[must_use]` (dropping it
    // without `shutdown` would leak threads), so a redundant `#[must_use]` here
    // would trip `clippy::double_must_use`.
    pub fn spawn_generators(&self) -> GeneratorSupervisor {
        let mut producers = Vec::with_capacity(self.animated.len());
        if !self.publish_test_frames {
            return GeneratorSupervisor { producers };
        }
        for source in &self.animated {
            // Per-source stop flag (ADR-W018): registered in the shared stop
            // registry so a live RemoveSource tears down exactly this producer;
            // shutdown still raises every flag.
            let stop = Arc::new(AtomicBool::new(false));
            let store = Arc::clone(&source.store);
            let kind = source.kind.clone();
            let (width, height) = (source.width, source.height);
            let canvas = self.canvas_color;
            let cadence = self.cadence;
            let id = store.id().to_owned();
            let exited = crate::live_sources::register_stop(&self.stop_registry, &id, &stop);
            // ExitGuard built BEFORE spawn: its Drop flips `exited` on thread exit OR if
            // Builder::spawn fails (the dropped closure drops the guard) — so a failed
            // spawn never orphans an exited=false entry teardown would busy-wait on
            // (ADR-W018 §5).
            let exit_guard = crate::live_sources::ExitGuard::new(&exited);
            let thread_stop = Arc::clone(&stop);
            let builder = std::thread::Builder::new().name(format!("multiview-synth-{id}"));
            match builder.spawn(move || {
                let _exit = exit_guard;
                generator_loop(kind, &store, width, height, canvas, cadence, &thread_stop);
            }) {
                Ok(handle) => producers.push((stop, handle)),
                Err(e) => {
                    // A generator that cannot spawn is logged and skipped: its tile
                    // simply rides the slate rather than failing the run (invariant
                    // #1 — the output clock is independent of inputs).
                    tracing::error!(error = %e, source = %id, "could not spawn synthetic generator thread");
                }
            }
        }
        GeneratorSupervisor { producers }
    }

    /// Drive the engine for exactly `max_ticks` ticks under an injected,
    /// jumpable [`TimeSource`] + [`Pacer`], publishing synthetic frames first.
    ///
    /// Deterministic and **sleep-free** when wired with a [`ManualTimeSource`] +
    /// [`multiview_engine::CooperativePacer`]: before driving, the manual clock is jumped past the
    /// last tick's deadline so the pacer never gates, and the loop emits exactly
    /// one frame per tick as fast as the executor cooperatively yields. Produces
    /// and publishes one frame per tick via the engine's own [`EngineRuntime`];
    /// the per-tick state/event publish goes out through the wait-free isolation
    /// channels and cannot be back-pressured.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::Clock`] if the cadence is rejected, or
    /// [`RunError::Engine`] if the compositor drive/canvas is rejected.
    pub async fn run_for<TS, P>(
        &mut self,
        time_source: Arc<TS>,
        pacer: P,
        max_ticks: u64,
    ) -> Result<RunReport, RunError>
    where
        TS: Advanceable + 'static,
        P: Pacer,
    {
        self.run_for_with_control(time_source, pacer, max_ticks, |_d| {})
            .await
    }

    /// Like [`SoftwareEngine::run_for`], but applies control-plane reconfiguration
    /// at each frame boundary via `control` (e.g. the command-bus drain from
    /// [`crate::control::command_drain`]). `control` runs on the deterministic
    /// output-clock loop and must be non-blocking (invariants #1 + #10): even a
    /// flooded command bus must not stall the clock or skip a frame.
    ///
    /// # Errors
    ///
    /// See [`SoftwareEngine::run_for`].
    pub async fn run_for_with_control<TS, P, FC>(
        &mut self,
        time_source: Arc<TS>,
        pacer: P,
        max_ticks: u64,
        control: FC,
    ) -> Result<RunReport, RunError>
    where
        TS: Advanceable + 'static,
        P: Pacer,
        FC: FnMut(&mut CompositorDrive<Nv12Image>),
    {
        self.prime_stores(time_source.as_ref());
        let ts: Arc<dyn TimeSource> = time_source.clone();
        let mut runtime = self.build_runtime(ts, pacer)?;
        // Pace the (jumpable) clock forward ONE TICK PERIOD PER FRESH COMPOSE rather
        // than jumping it once to the final tick's deadline. A single up-front jump to
        // `seed + pts_at(max_ticks)` leaves the clock a full `max_ticks` periods ahead
        // of media-time, which ADR-T018's drop/repeat-to-cadence loop correctly reads
        // as massive overload: for `max_ticks > MAX_REPEATS_PER_TICK` it emits a cap's
        // worth of last-good repeats then `skip_to`s the counter to wall-clock — which
        // advances the tick counter past `emitted`, so the bounded run can never reach
        // `max_ticks` frames and parks forever on a post-skip deadline the frozen clock
        // never covers (the live-overlay-churn / live-source-apply CI hang). Advancing
        // exactly one period per fresh compose keeps the clock a single tick ahead of
        // the composing tick, so the cadence-hold path stays dormant (a healthy 1.0×
        // pace, no spurious repeats) and the loop composes exactly one fresh frame per
        // tick — still sleep-free and deterministic under the `CooperativePacer`. The
        // user's `control` hook still runs once per fresh compose, after the advance.
        let seed = runtime.seed_nanos();
        let cadence = self.cadence;
        let advance_ts = Arc::clone(&time_source);
        let mut fresh_index: u64 = 0;
        let mut control = control;
        let paced_control = move |drive: &mut CompositorDrive<Nv12Image>| {
            // The hook for the tick that is about to compose fresh: release the NEXT
            // tick's deadline so the pacer never gates and the loop never falls behind
            // wall-clock (which would wake the cadence-hold repeat path).
            let next = fresh_index.saturating_add(1);
            advance_ts.advance_to(seed.saturating_add(pts_at(cadence, next).as_nanos()));
            fresh_index = next;
            control(drive);
        };
        let publisher: EnginePublisher<SoftwareState, SoftwareState> =
            EnginePublisher::new(EVENT_CAPACITY);
        let stop = StopSignal::new();
        // Animated synthetic sources (the `clock`) are driven by wall-clock
        // generator threads; the engine only samples their stores. This bounded,
        // paced-clock path runs at executor speed (no real time elapses), so a
        // clock does not visibly animate here — but the generators are still
        // spawned and torn down cleanly after the tick budget so the wiring is
        // identical across paths and no thread leaks.
        let generators = self.spawn_generators();
        let report = self
            .drive(
                &mut runtime,
                &publisher,
                &stop,
                Some(max_ticks),
                |f: &multiview_engine::CompositedFrame| SoftwareState {
                    tick: f.tick.index,
                    pts: f.pts(),
                },
                |f: &multiview_engine::CompositedFrame| {
                    Some(SoftwareState {
                        tick: f.tick.index,
                        pts: f.pts(),
                    })
                },
                paced_control,
            )
            .await;
        generators.shutdown();
        report
    }

    /// Drive the engine for `max_ticks` ticks under the production realtime
    /// pacer (monotonic time, real `sleep`s). Used by the binary's `--ticks`
    /// path and by realtime soak tests; paces to the wall clock.
    ///
    /// # Errors
    ///
    /// See [`SoftwareEngine::run_for`].
    pub async fn run_for_realtime(&mut self, max_ticks: u64) -> Result<RunReport, RunError> {
        let time = Arc::new(MonotonicTimeSource::new());
        let ts: Arc<dyn TimeSource> = time;
        self.prime_stores(ts.as_ref());
        let mut runtime = self.build_runtime(ts, RealtimePacer)?;
        let publisher: EnginePublisher<SoftwareState, SoftwareState> =
            EnginePublisher::new(EVENT_CAPACITY);
        let stop = StopSignal::new();
        // Real time elapses on this path, so an animated `clock` source actually
        // animates: its generator thread re-bakes once a second into the lock-free
        // store the engine samples. Tear the generators down after the tick budget.
        let generators = self.spawn_generators();
        let report = self
            .drive(
                &mut runtime,
                &publisher,
                &stop,
                Some(max_ticks),
                |f: &multiview_engine::CompositedFrame| SoftwareState {
                    tick: f.tick.index,
                    pts: f.pts(),
                },
                |f: &multiview_engine::CompositedFrame| {
                    Some(SoftwareState {
                        tick: f.tick.index,
                        pts: f.pts(),
                    })
                },
                |_d: &mut CompositorDrive<Nv12Image>| {},
            )
            .await;
        generators.shutdown();
        report
    }

    /// Drive the engine **forever** under the production realtime pacer until
    /// `stop` is raised (the binary wires this to Ctrl-C).
    ///
    /// # Errors
    ///
    /// See [`SoftwareEngine::run_for`].
    pub async fn run_until_stopped(
        &mut self,
        stop: &StopSignal,
        publisher: &EnginePublisher<EngineStateSnapshot, Event>,
    ) -> Result<RunReport, RunError> {
        self.run_until_stopped_with_control(stop, publisher, |_d| {})
            .await
    }

    /// Like [`SoftwareEngine::run_until_stopped`], but additionally applies
    /// control-plane reconfiguration at each frame boundary via `control` (e.g.
    /// the command-bus drain from [`crate::control::command_drain`]). `control`
    /// runs on the output-clock loop and must be non-blocking (invariants #1+#10).
    ///
    /// # Errors
    ///
    /// See [`SoftwareEngine::run_for`].
    pub async fn run_until_stopped_with_control<FC>(
        &mut self,
        stop: &StopSignal,
        publisher: &EnginePublisher<EngineStateSnapshot, Event>,
        control: FC,
    ) -> Result<RunReport, RunError>
    where
        FC: FnMut(&mut CompositorDrive<Nv12Image>),
    {
        let time = Arc::new(MonotonicTimeSource::new());
        let ts: Arc<dyn TimeSource> = time;
        self.prime_stores(ts.as_ref());
        let ts_for_anchor = Arc::clone(&ts);
        let mut runtime = self.build_runtime(ts, RealtimePacer)?;
        // DEV-C1 (ADR-M010): publish the run's epoch anchor (tick-0 seed + the
        // run's monotonic source) for the off-hot-path timing-status task -- a
        // single lock-free store that can never pace the clock (inv #1/#10).
        self.epoch_anchor.store(Some(std::sync::Arc::new(
            multiview_engine::epoch::EpochAnchor::new(ts_for_anchor, runtime.seed_nanos()),
        )));
        // The caller owns the publisher so the control plane can share it
        // (read-only). State is the compact per-tick JSON snapshot; events are
        // left sparse for now (none emitted here — they arrive via change-driven
        // mirrors in a follow-up), so the broadcast carries no per-tick flood.
        //
        // The same per-tick projection also publishes a THROTTLED clone of the
        // composited canvas into the wait-free program-preview slot (every
        // `PREVIEW_CAPTURE_EVERY`th tick ≈ a couple of frames per second), so the
        // control plane can serve a live still without cloning every frame on the
        // hot loop. The store is a single atomic swap — it never blocks the clock.
        let preview = Arc::clone(&self.program_preview);
        // Sparse tile-state events: the projection tracks each source's last
        // *emitted* lifecycle state and emits at most one `tile.state` change per
        // tick (seeding every tile once, then on transitions). The control plane
        // sets the envelope id to the source id, so the monitoring UI keys each
        // tile by it. This is change-driven — not a per-tick flood (inv #10).
        let mut last_states: HashMap<String, multiview_core::traits::SourceState> = HashMap::new();
        // Real time elapses on the forever path, so an animated `clock` source
        // animates: its generator re-bakes once a second into the lock-free store
        // the engine samples. The generators share their own stop flag; raising
        // the run's `StopSignal` (Ctrl-C) returns from `drive`, after which they
        // are torn down and joined here.
        let generators = self.spawn_generators();
        let report = self
            .drive(
                &mut runtime,
                publisher,
                stop,
                None,
                move |f: &multiview_engine::CompositedFrame| {
                    if f.tick.index % PREVIEW_CAPTURE_EVERY == 0 {
                        preview.store(Some(Arc::new(f.canvas.clone())));
                    }
                    let mut snapshot = crate::control::state_snapshot(
                        f.tick.index,
                        f.pts().as_nanos(),
                        f.canvas.width(),
                        f.canvas.height(),
                    );
                    // Thread the per-tile lifecycle states into the conflated
                    // blob so a connecting client is seeded with the CURRENT
                    // tile states (the `tiles` `$snapshot`) instead of waiting
                    // for the next sparse `tile.state` delta.
                    crate::control::fold_tile_states(&mut snapshot, &f.source_states);
                    snapshot
                },
                move |f: &multiview_engine::CompositedFrame| -> Option<Event> {
                    for (source, &state) in &f.source_states {
                        if last_states.get(source) != Some(&state) {
                            let from = last_states.get(source).copied().unwrap_or(state);
                            last_states.insert(source.clone(), state);
                            return Some(Event::TileState(multiview_events::TileState {
                                from: from.into(),
                                to: state.into(),
                                input: Some(source.clone()),
                                trigger: "state_change".to_owned(),
                            }));
                        }
                    }
                    None
                },
                control,
            )
            .await;
        generators.shutdown();
        report
    }

    /// Publish each source's synthetic frame into its store at the current
    /// time-source instant (so the tile reads LIVE), unless publishing is
    /// disabled.
    fn prime_stores(&self, time_source: &dyn TimeSource) {
        if !self.publish_test_frames {
            return;
        }
        let at = MediaTime::from_nanos(time_source.now_nanos());
        for (id, pattern) in &self.patterns {
            if let Some(store) = self.stores.get(id) {
                store.publish_arc(Arc::clone(pattern), at);
            }
        }
    }

    /// Build a fresh [`EngineRuntime`] over a fresh [`CompositorDrive`] sharing
    /// this engine's stores.
    fn build_runtime<P: Pacer>(
        &self,
        time_source: Arc<dyn TimeSource>,
        pacer: P,
    ) -> Result<EngineRuntime<P>, RunError> {
        let clock = OutputClock::new(self.cadence).map_err(|e| RunError::Clock(e.to_string()))?;
        let drive = CompositorDrive::new(
            Arc::clone(&self.layout),
            self.stores.clone(),
            self.nosignal_card.clone(),
            self.canvas_color,
            self.background,
        )
        .map_err(|e| RunError::Engine(e.to_string()))?;
        Ok(EngineRuntime::new(clock, drive, time_source, pacer))
    }

    /// Run the engine's tick loop and fold the outcome into a [`RunReport`],
    /// verifying the output never faltered (frames == ticks, monotone PTS).
    ///
    /// The projection closures are cheap, panic-free, and run on the hot loop;
    /// they publish a per-tick state snapshot and event through the (non-blocking,
    /// drop-oldest) isolation channels — best-effort, never back-pressuring.
    #[allow(clippy::too_many_arguments)]
    // reason: this is the single private dispatcher that folds the engine's run
    // outcome into a RunReport; its parameters (runtime, publisher, stop,
    // max_ticks, and the three hot-loop closures state_of/event_of/control) are
    // each distinct and dictated by `EngineRuntime::run*_with_control`'s
    // signature. Bundling them into a struct would only move the arity without
    // improving clarity for the four thin callers.
    async fn drive<P, S, E, FS, FE, FC>(
        &self,
        runtime: &mut EngineRuntime<P>,
        publisher: &EnginePublisher<S, E>,
        stop: &StopSignal,
        max_ticks: Option<u64>,
        state_of: FS,
        event_of: FE,
        control: FC,
    ) -> Result<RunReport, RunError>
    where
        P: Pacer,
        FS: FnMut(&multiview_engine::CompositedFrame) -> S,
        FE: FnMut(&multiview_engine::CompositedFrame) -> Option<E>,
        FC: FnMut(&mut CompositorDrive<Nv12Image>),
    {
        let outcome = match max_ticks {
            Some(max) => {
                runtime
                    .run_for_with_control(publisher, stop, max, state_of, event_of, control)
                    .await
            }
            None => {
                runtime
                    .run_with_control(publisher, stop, state_of, event_of, control)
                    .await
            }
        }
        .map_err(|e| RunError::Engine(e.to_string()))?;

        let frames = outcome.ticks;
        let first_pts = (frames > 0).then(|| pts_at(self.cadence, 0));
        let last_pts = frames
            .checked_sub(1)
            .map(|last_index| pts_at(self.cadence, last_index));

        // Falter check: the runtime emits exactly one frame per tick by
        // construction, and PTS is `f(tick)` (strictly increasing for a positive
        // cadence). We re-assert that contract in the report rather than assume
        // it: any deviation (a short loop, a non-advancing PTS) flips `faltered`.
        let monotone = match (first_pts, last_pts) {
            (Some(first), Some(last)) => frames <= 1 || last.as_nanos() > first.as_nanos(),
            _ => true,
        };
        let stopped_cleanly = matches!(outcome.stop, RunStop::Completed | RunStop::Stopped);
        let count_matches = match max_ticks {
            Some(max) => frames == max,
            None => true,
        };
        let faltered = !(monotone && stopped_cleanly && count_matches);

        Ok(RunReport {
            ticks: outcome.ticks,
            frames,
            cadence: self.cadence,
            canvas_width: self.layout.canvas.width,
            canvas_height: self.layout.canvas.height,
            first_pts,
            last_pts,
            faltered,
        })
    }
}

/// A [`TimeSource`] whose position can be set forward, for the deterministic
/// (sleep-free) bounded software run.
///
/// Implemented for [`ManualTimeSource`] ([`ManualTimeSource::set`] jumps the
/// clock). A real monotonic source cannot be jumped, so the realtime path uses
/// [`SoftwareEngine::run_for_realtime`] (which paces against true elapsed time)
/// instead of [`SoftwareEngine::run_for`].
pub trait Advanceable: TimeSource {
    /// Move the source forward to at least `nanos` (never backwards).
    fn advance_to(&self, nanos: i64);
}

impl Advanceable for ManualTimeSource {
    fn advance_to(&self, nanos: i64) {
        self.set(nanos);
    }
}

/// The PTS of tick `index` at `cadence` (`out_pts = f(tick)`, exact, never
/// float-accumulated).
fn pts_at(cadence: Rational, index: u64) -> MediaTime {
    let tick = i64::try_from(index).unwrap_or(i64::MAX);
    MediaTime::from_tick(tick, cadence)
}

/// Build a distinctly-colored NV12 test pattern for source `index`, tagged like
/// the canvas. Cycles through a small palette of luma/chroma triples so adjacent
/// tiles are visually distinct in the composite.
///
/// # Errors
///
/// Returns the compositor [`multiview_compositor::Error`] if the geometry is
/// rejected (odd/zero dimensions).
/// The **static** synthetic frame a source contributes in the FFmpeg-free
/// software engine (primed once into its store).
///
/// `bars` and `solid` render their real picture (the static synthetic kinds that
/// need no decoder — ADR-0027). Every other kind contributes a distinct per-tile
/// placeholder card so the smoke still composites a frame: a decoded feed the
/// software build cannot open, **and** `clock` in this exact non-animated path —
/// the animated `clock` is instead driven by a generator thread (see
/// [`SoftwareEngine::spawn_generators`]) when this build can render it (the
/// `overlay` feature), so it reaches here only as the honest static fallback for
/// an overlay-off build.
fn software_source_frame(
    source: &Source,
    width: u32,
    height: u32,
    index: usize,
    canvas: CanvasColor,
) -> Result<Nv12Image, multiview_compositor::Error> {
    match &source.kind {
        SourceKind::Bars => Nv12Image::color_bars(width, height, canvas),
        SourceKind::Solid { color } => {
            // The colour was validated at config time; fall back to a slate if a
            // caller somehow bypassed validation (never panic on the build path).
            let (r, g, b) = multiview_config::parse_hex_color(color).unwrap_or((16, 16, 24));
            Nv12Image::solid_rgb(width, height, r, g, b, canvas)
        }
        _ => test_pattern(width, height, index, canvas.output_tag()),
    }
}

/// A distinct per-tile placeholder card (a flat hue per source index), used in
/// the software smoke for kinds it does not render natively.
fn test_pattern(
    width: u32,
    height: u32,
    index: usize,
    tag: multiview_core::color::ColorInfo,
) -> Result<Nv12Image, multiview_compositor::Error> {
    // A small palette of (Y, Cb, Cr) limited-range code values: gray bars in
    // distinct hues. Index modulo the palette length keeps it total.
    const PALETTE: [(u8, u8, u8); 8] = [
        (180, 90, 240),  // reddish
        (170, 240, 110), // bluish
        (150, 44, 142),  // greenish
        (200, 128, 128), // light gray
        (120, 200, 60),  // teal-ish
        (90, 160, 200),  // amber-ish
        (210, 128, 128), // near white
        (40, 128, 128),  // near black
    ];
    let (y, cb, cr) = PALETTE
        .get(index % PALETTE.len())
        .copied()
        .unwrap_or((128, 128, 128));
    Nv12Image::solid(width, height, y, cb, cr, tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pull a real [`Source`] out of the parser (it is `#[non_exhaustive]`, so it
    /// cannot be struct-literal-constructed from this crate) by wrapping its
    /// `kind` fields in a minimal 1x1 document.
    fn source_with(kind_fields: &str) -> Source {
        let doc = format!(
            r##"schema_version = 1
[canvas]
width = 320
height = 240
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]
[[sources]]
id = "in_a"
{kind_fields}
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"
[[outputs]]
kind = "hls"
path = "/tmp/x.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##
        );
        let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse minimal config");
        cfg.sources.into_iter().next().expect("one source")
    }

    #[test]
    fn bars_source_routes_to_real_colour_bars() {
        let canvas = CanvasColor::default();
        let src = source_with("kind = \"bars\"");
        let got = software_source_frame(&src, 560, 240, 0, canvas).expect("frame");
        let bars = Nv12Image::color_bars(560, 240, canvas).expect("bars");
        assert_eq!(
            got.y_plane(),
            bars.y_plane(),
            "a bars source must render real colour bars, not the placeholder"
        );
    }

    #[test]
    fn solid_source_routes_to_its_configured_colour() {
        let canvas = CanvasColor::default();
        let src = source_with("kind = \"solid\"\ncolor = \"#22aa44\"");
        let got = software_source_frame(&src, 64, 64, 0, canvas).expect("frame");
        let want = Nv12Image::solid_rgb(64, 64, 0x22, 0xaa, 0x44, canvas).expect("solid");
        assert_eq!(
            got.y_plane(),
            want.y_plane(),
            "a solid source must render its configured colour"
        );
    }

    #[test]
    fn a_decoded_kind_does_not_masquerade_as_bars() {
        // A kind the software smoke cannot decode (rtsp) gets the per-index
        // placeholder card — never silently rendered as bars.
        let canvas = CanvasColor::default();
        let src = source_with("kind = \"rtsp\"\nurl = \"rtsp://example/stream\"");
        let got = software_source_frame(&src, 560, 240, 0, canvas).expect("frame");
        let bars = Nv12Image::color_bars(560, 240, canvas).expect("bars");
        assert_ne!(
            got.y_plane(),
            bars.y_plane(),
            "a decoded kind must not look like bars"
        );
    }
}
