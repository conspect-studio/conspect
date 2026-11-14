//! The multiview overlay layers on a routed layout's aux canvas (feature
//! `overlay`).
//!
//! Program's bake consumer draws per-cell label, meter, state flag, fault
//! badge, clock and watermark onto the Program canvas. A routed layout
//! (`PictureBus::Layout`, composed on the `routing-aux` thread by
//! [`crate::aux_layout::LayoutCanvas::compose`]) carries the same layers here,
//! selected per layout by its stored `overlays` attribute
//! ([`LayoutOverlays`]), plus a tally border.
//!
//! **Isolation (invariants #1/#10).** Nothing here runs on the output clock.
//! The bake consumer publishes the per-source signals it already has
//! (meters, content faults, the sources on Program) into one wait-free
//! [`SwapCell`] at most once per baked frame, and only while some routed
//! layout wants them ([`AuxSignalsPublisher::wanted`]: two relaxed loads,
//! zero work otherwise). The aux thread reads the latest snapshot once per
//! tick and never waits for the consumer. The per-cell state is the SAME
//! `TileRead` state the layout's compose saw this tick. A bake error never
//! holds the bus: the un-overlaid picture is published and the error counted.
//!
//! **Cost.** One bake per layout feed per tick: every output on `aux:<id>`
//! shares the one baked `Arc`. A layout with every layer off has no bake at
//! all.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use multiview_compositor::overlay::apply_overlays_to_nv12;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_core::time::MediaTime;
use multiview_core::traits::SourceState;
use multiview_framestore::SwapCell;

use crate::aux_layout::RoutedLayout;
use crate::licence::WatermarkSignal;
use crate::overlays::{
    tile_specs_with, LayoutOverlays, MeterChannels, OverlayBaker, TileActivity, TileDynamics,
    TileFault, TileTally,
};
use crate::wallclock::WallClockSource;

/// Freeze debounce, matching Program's fault detector (`pipeline::
/// FaultDetector`) so a source reads the same on both.
const FREEZE_WINDOW: usize = 12;
const FREEZE_MIN_PRESENT: usize = 9;

/// The most cells a routed layout may have and still bake overlays. A
/// larger layout still composes and publishes its picture; it gets no
/// overlay bake (warned once, counted in [`AuxOverlayCounters::skipped`]).
/// With at most [`crate::routing_aux::MAX_PARALLEL_BAKES`] layouts, this
/// bounds a tick's bake work and the wanted-source set by construction.
pub const MAX_BAKED_CELLS: usize = 64;

/// The most source ids the baking layouts can bind (the wanted-set and
/// published-snapshot bound): layouts × cells.
pub const MAX_WANTED_SOURCES: usize = crate::routing_aux::MAX_PARALLEL_BAKES * MAX_BAKED_CELLS;

/// Content-fault detection for one routed layout, on the aux thread, from the
/// frames its compose already read this tick (no second store read): black
/// and freeze through the same probe construction Program's detector uses
/// ([`crate::fault_probes`]): an operator-declared probe's threshold, zone,
/// dwell, severity and latch, else the shared defaults. Silence needs the
/// source's PCM, which only Program's consumer reads; it comes from the
/// consumer's snapshot.
///
/// Bounded: state per bound source of this layout only (retained across a
/// body rebuild), one probe pass per bound source per tick.
pub struct AuxFaultDetector {
    defaults: crate::fault_probes::DefaultProbes,
    declared: Arc<crate::fault_probes::SourceProbes>,
    per_source: HashMap<Arc<str>, AuxSourceFaults>,
}

struct AuxSourceFaults {
    black_probe: multiview_engine::BlackProbe,
    freeze_probe: multiview_engine::FreezeProbe,
    black: multiview_engine::AlarmStateMachine,
    freeze: multiview_engine::AlarmStateMachine,
    previous: Option<Arc<Nv12Image>>,
    window: std::collections::VecDeque<bool>,
    fault: TileFault,
}

impl std::fmt::Debug for AuxFaultDetector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuxFaultDetector")
            .field("sources", &self.per_source.len())
            .finish_non_exhaustive()
    }
}

impl AuxFaultDetector {
    /// A detector honouring `declared` per-source probes (the run's, shared
    /// read-only; empty: every source on the defaults).
    #[must_use]
    pub fn new(declared: Arc<crate::fault_probes::SourceProbes>) -> Self {
        Self {
            defaults: crate::fault_probes::DefaultProbes::default(),
            declared,
            per_source: HashMap::new(),
        }
    }

    /// Forget sources not in `bound`.
    fn retain(&mut self, bound: &HashSet<&str>) {
        self.per_source.retain(|id, _| bound.contains(&**id));
    }

    fn state_for(&self, id: &str) -> AuxSourceFaults {
        use crate::fault_probes::{declared_black, declared_freeze, fault_machine};
        use multiview_core::alarm::{AlarmKind, PerceivedSeverity};
        let ov = self.declared.get(id);
        let d = self.defaults;
        AuxSourceFaults {
            black_probe: ov.and_then(declared_black).unwrap_or(d.black),
            freeze_probe: ov.and_then(declared_freeze).unwrap_or(d.freeze),
            black: fault_machine(
                id,
                ov.and_then(|o| o.black.as_ref()),
                AlarmKind::Black,
                PerceivedSeverity::Major,
                d.black_dwell,
            ),
            freeze: fault_machine(
                id,
                ov.and_then(|o| o.freeze.as_ref()),
                AlarmKind::Freeze,
                PerceivedSeverity::Major,
                d.freeze_dwell,
            ),
            previous: None,
            window: std::collections::VecDeque::with_capacity(FREEZE_WINDOW + 1),
            fault: TileFault::None,
        }
    }

    /// Observe this tick's frames (parallel to `states`) at `pts`, for the
    /// cells in `first` only (each bound source's first cell, precomputed per
    /// body, so a source bound twice is observed once without a per-tick
    /// scan); afterwards [`AuxFaultDetector::fault`] answers per source.
    pub fn observe(
        &mut self,
        states: &[(Arc<str>, SourceState)],
        frames: &[Option<Arc<Nv12Image>>],
        first: &[usize],
        pts: MediaTime,
    ) {
        use multiview_engine::LumaView;
        for &index in first {
            let (Some((id, _)), Some(frame)) = (states.get(index), frames.get(index)) else {
                continue;
            };
            if !self.per_source.contains_key(id) {
                let state = self.state_for(id);
                self.per_source.insert(Arc::clone(id), state);
            }
            let Some(entry) = self.per_source.get_mut(id) else {
                continue;
            };
            let (black_now, freeze_now) = match frame.as_deref() {
                None => (false, false),
                Some(img) => match LumaView::packed(img.y_plane(), img.width(), img.height()) {
                    Err(_) => (false, false),
                    Ok(current) => {
                        let black = entry.black_probe.detect(&current).condition_present;
                        let frozen = entry.previous.as_deref().is_some_and(|prev| {
                            LumaView::packed(prev.y_plane(), prev.width(), prev.height()).is_ok_and(
                                |p| entry.freeze_probe.detect(&current, &p).condition_present,
                            )
                        });
                        (black, frozen)
                    }
                },
            };
            if frame.is_some() {
                entry.previous.clone_from(frame);
            } else {
                entry.previous = None;
                entry.window.clear();
            }
            entry.window.push_back(freeze_now);
            while entry.window.len() > FREEZE_WINDOW {
                entry.window.pop_front();
            }
            let present = entry.window.iter().filter(|f| **f).count();
            let frozen = if entry.window.len() >= FREEZE_WINDOW {
                present >= FREEZE_MIN_PRESENT
            } else {
                present.saturating_mul(2) > entry.window.len()
            };
            entry.black.observe(black_now, pts);
            entry.freeze.observe(frozen, pts);
            entry.fault = if entry.black.is_active() {
                TileFault::Black
            } else if entry.freeze.is_active() {
                TileFault::Frozen
            } else {
                TileFault::None
            };
        }
    }

    /// The picture fault this detector holds for `id` (`None`: unknown here).
    #[must_use]
    pub fn fault(&self, id: &str) -> Option<TileFault> {
        self.per_source.get(id).map(|s| s.fault)
    }

    /// The fault to show for `id`, given the consumer's verdict `remote`.
    ///
    /// A source on Program (`on_program`) shows exactly the consumer's
    /// verdict: that is Program's own detector, with every declared probe,
    /// latch and silence analysis. A source NOT on Program shows this
    /// layout's picture verdict, or the consumer's silence (the only fault it
    /// can still know for a source off the program bus) when there is none.
    #[must_use]
    pub fn resolve(&self, id: &str, remote: TileFault, on_program: bool) -> TileFault {
        if on_program {
            return remote;
        }
        match self.per_source.get(id) {
            Some(local) if local.fault.is_present() => local.fault,
            _ if remote == TileFault::Silent => remote,
            Some(local) => local.fault,
            None => remote,
        }
    }
}

/// The per-source signals Program's bake consumer has and a routed layout's
/// overlays need: each source's live meter peaks, its content fault, and the
/// set of sources on Program (the tally).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AuxOverlaySignals {
    /// Per source id: the live pre-mix meter peaks.
    pub meters: HashMap<String, MeterChannels>,
    /// Per source id: the detected content fault.
    pub faults: HashMap<String, TileFault>,
    /// The source ids on Program (tally red).
    pub program: HashSet<String>,
}

/// The wait-free latest-signals slot (written by the bake consumer, read by
/// the aux thread once per tick).
pub type AuxSignalsSlot = Arc<SwapCell<Arc<AuxOverlaySignals>>>;

/// The source ids the baking routed layouts bind (written by the aux thread
/// only when its bakes change; read by the consumer per published frame).
/// Bounds the signals snapshot to layouts × cells.
pub type WantedSources = Arc<SwapCell<Arc<HashSet<String>>>>;

/// A source id → display label lookup (config names first, then the live
/// catalogue). Must never take a lock the clock holds.
pub type LabelLookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// What the aux thread needs to bake layout overlays.
#[derive(Clone)]
pub struct AuxOverlayInputs {
    /// The latest signals the bake consumer published.
    pub signals: AuxSignalsSlot,
    /// Raised by the aux thread while some active layout feed has a bake.
    pub wanted: Arc<AtomicBool>,
    /// The source ids the baking layouts bind (aux thread → consumer).
    pub wanted_sources: WantedSources,
    /// Display labels for the cells' sources (fallback: the id).
    pub labels: LabelLookup,
    /// The Conspect watermark signal (the same corner mark Program carries).
    pub watermark: Option<WatermarkSignal>,
    /// The displayed time-of-day source.
    pub wall_clock: WallClockSource,
    /// The canvas colour (for the overlay blend).
    pub canvas_color: CanvasColor,
    /// The run's operator-declared per-source probes (resolved once at
    /// build time, shared read-only): a source only on a routed layout is
    /// judged by its declared black/freeze probe exactly as Program would.
    pub source_probes: Arc<crate::fault_probes::SourceProbes>,
}

impl std::fmt::Debug for AuxOverlayInputs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuxOverlayInputs")
            .field("wanted", &self.wanted.load(Ordering::Relaxed))
            .field("watermark", &self.watermark.is_some())
            .finish_non_exhaustive()
    }
}

/// The bake consumer's side of the signals slot.
#[derive(Clone)]
pub struct AuxSignalsPublisher {
    slot: AuxSignalsSlot,
    wanted: Arc<AtomicBool>,
    sources: WantedSources,
    /// The clock's aux-tick active flag: false while nothing is routed (the
    /// aux thread then never ticks, so `wanted` may be stale).
    active: Arc<AtomicBool>,
}

impl std::fmt::Debug for AuxSignalsPublisher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuxSignalsPublisher")
            .field("wanted", &self.wanted())
            .finish_non_exhaustive()
    }
}

impl AuxSignalsPublisher {
    /// A publisher over `slot`, gated on the aux thread's `wanted` flag and
    /// the clock's aux `active` flag.
    #[must_use]
    pub fn new(
        slot: AuxSignalsSlot,
        wanted: Arc<AtomicBool>,
        sources: WantedSources,
        active: Arc<AtomicBool>,
    ) -> Self {
        Self {
            slot,
            wanted,
            sources,
            active,
        }
    }

    /// Publish the signals of exactly the wanted sources: meters and faults
    /// for ids the baking layouts bind, and which of those are on Program.
    /// The snapshot is bounded by the wanted set, however many sources the
    /// consumer tracks.
    pub fn publish_for<V>(
        &self,
        meters: &HashMap<String, MeterChannels>,
        faults: &HashMap<String, TileFault>,
        program: &HashMap<String, V>,
    ) {
        let wanted = self.sources.load();
        let mut signals = AuxOverlaySignals::default();
        // Every lookup is driven by the wanted set (bounded by construction,
        // MAX_WANTED_SOURCES; the take is a second, local guarantee), so the
        // work per frame never grows with Program's layout or the tracked maps.
        for id in wanted.iter().take(MAX_WANTED_SOURCES) {
            if let Some(m) = meters.get(id) {
                signals.meters.insert(id.clone(), *m);
            }
            if let Some(f) = faults.get(id) {
                signals.faults.insert(id.clone(), *f);
            }
            if program.contains_key(id) {
                signals.program.insert(id.clone());
            }
        }
        self.publish(signals);
    }

    /// Whether some active routed layout bakes overlays (two relaxed loads).
    #[must_use]
    pub fn wanted(&self) -> bool {
        self.active.load(Ordering::Relaxed) && self.wanted.load(Ordering::Relaxed)
    }

    /// Publish the newest signals (wait-free store).
    pub fn publish(&self, signals: AuxOverlaySignals) {
        self.slot.store(Arc::new(signals));
    }
}

/// A fresh, empty wanted-sources cell.
#[must_use]
pub fn wanted_sources() -> WantedSources {
    Arc::new(SwapCell::from_pointee(HashSet::new()))
}

/// A fresh, empty signals slot.
#[must_use]
pub fn signals_slot() -> AuxSignalsSlot {
    Arc::new(SwapCell::from_pointee(AuxOverlaySignals::default()))
}

/// Counters of one layout's overlay bake.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AuxOverlayCounters {
    /// Frames baked with overlays.
    pub baked: u64,
    /// Bakes that failed (the un-overlaid picture was published).
    pub errors: u64,
}

/// Counters of layouts refused an overlay bake (process-wide, diagnostics):
/// layouts with more than [`MAX_BAKED_CELLS`] cells, which compose clean.
#[derive(Debug, Default)]
pub struct SkippedBakes {
    /// Layout bodies refused a bake for exceeding [`MAX_BAKED_CELLS`].
    pub too_many_cells: std::sync::atomic::AtomicU64,
}

/// The process-wide [`SkippedBakes`] counters.
#[must_use]
pub fn skipped_bakes() -> &'static SkippedBakes {
    static SKIPPED: SkippedBakes = SkippedBakes {
        too_many_cells: std::sync::atomic::AtomicU64::new(0),
    };
    &SKIPPED
}

/// Whether `body` is within the overlay bake's cell budget; warns once per
/// process and counts when it is not.
fn within_cell_budget(body: &RoutedLayout) -> bool {
    static WARNED: AtomicBool = AtomicBool::new(false);
    let cells = body.layout.cells.len();
    if cells <= MAX_BAKED_CELLS {
        return true;
    }
    skipped_bakes()
        .too_many_cells
        .fetch_add(1, Ordering::Relaxed);
    if !WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            layout = %body.id,
            cells,
            max = MAX_BAKED_CELLS,
            "routing-aux: a routed layout has more cells than the overlay budget; it is \
             composed and published without overlays"
        );
    }
    false
}

/// Per-body indices into the compose's per-cell lists (computed once per
/// body, never per tick).
#[derive(Debug, Default)]
struct BodyIndex {
    /// The first per-cell index of each bound source: the cells fault
    /// detection observes (a source bound twice is observed once).
    first: Vec<usize>,
    /// Per source id: its first per-cell index (the state its chip shows).
    state_of: HashMap<String, usize>,
}

impl BodyIndex {
    /// The compose's per-cell order is the source-bound cells by ascending
    /// `z`, stable in cell order (`LayoutCanvas::new`), which this mirrors.
    fn of(body: &RoutedLayout) -> Self {
        let cells = &body.layout.cells;
        let mut order: Vec<usize> = (0..cells.len()).collect();
        order.sort_by_key(|&i| cells.get(i).map_or(i32::MIN, |c| c.z));
        let mut index = Self::default();
        let mut slot = 0usize;
        for i in order {
            let Some(id) = cells.get(i).and_then(|c| c.source.as_deref()) else {
                continue;
            };
            if !index.state_of.contains_key(id) {
                index.state_of.insert(id.to_owned(), slot);
                index.first.push(slot);
            }
            slot += 1;
        }
        index
    }
}

/// One routed layout's overlay bake on the aux thread.
pub struct LayoutOverlayBake {
    baker: OverlayBaker,
    layers: LayoutOverlays,
    body: Arc<RoutedLayout>,
    canvas_color: CanvasColor,
    counters: AuxOverlayCounters,
    warned: bool,
    /// Black/freeze detection over this layout's own bound sources (M1): a
    /// source only on a routed layout still gets its content-fault badge.
    faults: AuxFaultDetector,
    /// Per-body indices into the compose's per-cell lists.
    index: BodyIndex,
    /// Reused per tick.
    dynamics: HashMap<String, TileDynamics>,
    captions: HashMap<String, Vec<String>>,
    bitmaps: HashMap<String, multiview_ffmpeg::caption::CueBitmap>,
}

impl std::fmt::Debug for LayoutOverlayBake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LayoutOverlayBake")
            .field("layout", &self.body.id)
            .field("layers", &self.layers)
            .field("counters", &self.counters)
            .finish_non_exhaustive()
    }
}

impl LayoutOverlayBake {
    /// Build the bake for `body` (rare: once per new layout body). `None` when
    /// every layer is off (a clean layout: no bake, zero cost) or the bundled
    /// fonts fail to load (warned; the layout is then published clean).
    #[must_use]
    /// `None` too when the body exceeds [`MAX_BAKED_CELLS`] (it composes
    /// clean; warned once, counted in [`skipped_bakes`]).
    pub fn new(body: &Arc<RoutedLayout>, inputs: &AuxOverlayInputs) -> Option<Self> {
        if !body.overlays.any() || !within_cell_budget(body) {
            return None;
        }
        let tiles = tile_specs_with(&body.layout, |id| (inputs.labels)(id));
        let mut baker = match OverlayBaker::new(tiles, inputs.wall_clock.clone()) {
            Ok(baker) => baker.with_layers(body.overlays),
            Err(error) => {
                tracing::warn!(layout = %body.id, %error, "routing-aux: layout overlays unavailable; the layout is published without them");
                return None;
            }
        };
        if let Some(signal) = inputs.watermark.clone() {
            baker =
                baker.with_watermark(signal, body.layout.canvas.width, body.layout.canvas.height);
        }
        Some(Self {
            baker,
            layers: body.overlays,
            body: Arc::clone(body),
            canvas_color: inputs.canvas_color,
            counters: AuxOverlayCounters::default(),
            warned: false,
            faults: AuxFaultDetector::new(Arc::clone(&inputs.source_probes)),
            index: BodyIndex::of(body),
            dynamics: HashMap::new(),
            captions: HashMap::new(),
            bitmaps: HashMap::new(),
        })
    }

    /// Adopt an edited body of the same layout without reloading fonts:
    /// re-resolve the tiles (meter ballistics survive for unchanged sources)
    /// and the layers. Returns `false` when the new body turns every layer
    /// off or exceeds [`MAX_BAKED_CELLS`] (the caller drops the bake).
    pub fn rebuild(&mut self, body: &Arc<RoutedLayout>, labels: &LabelLookup) -> bool {
        if !body.overlays.any() || !within_cell_budget(body) {
            return false;
        }
        self.baker
            .set_tiles(tile_specs_with(&body.layout, |id| labels(id)));
        self.baker.set_layers(body.overlays);
        self.layers = body.overlays;
        self.index = BodyIndex::of(body);
        let bound: HashSet<&str> = self.index.state_of.keys().map(String::as_str).collect();
        self.faults.retain(&bound);
        self.body = Arc::clone(body);
        true
    }

    /// The source ids this bake's cells bind.
    pub fn bound_sources(&self) -> impl Iterator<Item = &str> {
        self.baker.tiles().iter().map(|t| t.source_id.as_str())
    }

    /// The layers this bake draws.
    #[must_use]
    pub const fn layers(&self) -> LayoutOverlays {
        self.layers
    }

    /// The counters so far.
    #[must_use]
    pub const fn counters(&self) -> AuxOverlayCounters {
        self.counters
    }

    /// Whether this bake reads per-cell frames (its flags layer shows
    /// content faults). The compose keeps them only then.
    #[must_use]
    pub const fn wants_frames(&self) -> bool {
        self.layers.flags
    }

    /// The per-tile dynamics of the last bake (keyed by source id).
    #[must_use]
    pub fn last_dynamics(&self) -> &HashMap<String, TileDynamics> {
        &self.dynamics
    }

    /// Bake this layout's overlays onto `picture` at `pts`. `states` is the
    /// per-cell state the compose saw this tick (by source id) and `frames`
    /// the per-cell frames it read (parallel to `states`; empty when the
    /// flags layer is off); `signals` the consumer's latest snapshot;
    /// `preview` the M/E's PVW source. On any error the un-overlaid `picture`
    /// is returned (counted, warned once): never holds the bus, never panics.
    ///
    /// Content faults: a source on Program shows the consumer's verdict
    /// (Program's own detector, with declared probes and latches). A source
    /// not on Program shows this layout's black/freeze verdict over `frames`
    /// (its declared probe, else the defaults), or the consumer's silence.
    pub fn bake(
        &mut self,
        picture: &Arc<Nv12Image>,
        pts: MediaTime,
        states: &[(Arc<str>, SourceState)],
        frames: &[Option<Arc<Nv12Image>>],
        signals: &AuxOverlaySignals,
        preview: Option<&str>,
    ) -> Arc<Nv12Image> {
        if self.layers.flags && !frames.is_empty() {
            self.faults.observe(states, frames, &self.index.first, pts);
        }
        self.dynamics.clear();
        for tile in self.baker.tiles() {
            let id = tile.source_id.as_str();
            if self.dynamics.contains_key(id) {
                continue;
            }
            let state = self
                .index
                .state_of
                .get(id)
                .and_then(|&i| states.get(i))
                .filter(|(source, _)| &**source == id)
                .map_or(SourceState::NoSignal, |(_, state)| *state);
            let on_program = signals.program.contains(id);
            let tally = if on_program {
                TileTally::Program
            } else if preview == Some(id) {
                TileTally::Preview
            } else {
                TileTally::Off
            };
            self.dynamics.insert(
                id.to_owned(),
                TileDynamics {
                    meter_db: multiview_audio::Ballistics::FLOOR_DB,
                    channels: signals
                        .meters
                        .get(id)
                        .copied()
                        .unwrap_or(MeterChannels::EMPTY),
                    state,
                    fault: self.faults.resolve(
                        id,
                        signals.faults.get(id).copied().unwrap_or(TileFault::None),
                        on_program,
                    ),
                    activity: TileActivity::Ready,
                    tally,
                },
            );
        }
        let baked = self
            .baker
            .draw_list(pts, &self.dynamics, &self.captions, &self.bitmaps)
            .map_err(|e| e.to_string())
            .and_then(|list| {
                apply_overlays_to_nv12(picture, &list, self.canvas_color).map_err(|e| e.to_string())
            });
        match baked {
            Ok(image) => {
                self.counters.baked += 1;
                Arc::new(image)
            }
            Err(error) => {
                self.counters.errors += 1;
                if !self.warned {
                    self.warned = true;
                    tracing::warn!(
                        layout = %self.body.id,
                        %error,
                        "routing-aux: a layout overlay bake failed; the layout is published \
                         without overlays"
                    );
                }
                Arc::clone(picture)
            }
        }
    }
}

#[cfg(test)]
#[path = "aux_overlays_tests.rs"]
pub(crate) mod tests;
