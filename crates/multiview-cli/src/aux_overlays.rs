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

/// Freeze probe tolerance and debounce, matching Program's fault detector
/// (`pipeline::FaultDetector`) so a source reads the same on both.
const FREEZE_TOLERANCE: u8 = 6;
const FREEZE_WINDOW: usize = 12;
const FREEZE_MIN_PRESENT: usize = 9;
/// The longest raise dwell (freeze): a layout's "clear" verdict is trusted
/// over the consumer's only after this much observation.
const WARM_AFTER: MediaTime = MediaTime::from_nanos(2_000_000_000);

/// Content-fault detection for one routed layout, on the aux thread, from the
/// frames its compose already read this tick (no second store read): black
/// and freeze through the same engine probes and dwell machines Program's
/// detector uses. Silence needs the source's PCM, which only Program's
/// consumer reads; it comes from the consumer's snapshot.
///
/// Bounded: state per bound source of this layout only (rebuilt with the
/// layout), one probe pass per bound cell per tick.
pub struct AuxFaultDetector {
    black: multiview_engine::BlackProbe,
    freeze: multiview_engine::FreezeProbe,
    black_dwell: multiview_engine::AlarmHysteresis,
    freeze_dwell: multiview_engine::AlarmHysteresis,
    per_source: HashMap<Arc<str>, AuxSourceFaults>,
}

struct AuxSourceFaults {
    black: multiview_engine::AlarmStateMachine,
    freeze: multiview_engine::AlarmStateMachine,
    previous: Option<Arc<Nv12Image>>,
    window: std::collections::VecDeque<bool>,
    fault: TileFault,
    /// When this layout first observed the source: its "clear" verdict is
    /// authoritative only after the longest dwell (freeze, 2 s) has run.
    since: MediaTime,
    warm: bool,
}

impl std::fmt::Debug for AuxFaultDetector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuxFaultDetector")
            .field("sources", &self.per_source.len())
            .finish_non_exhaustive()
    }
}

impl Default for AuxFaultDetector {
    fn default() -> Self {
        let dwell = |nanos: i64| MediaTime::from_nanos(nanos);
        let down = dwell(300_000_000);
        Self {
            black: multiview_engine::BlackProbe::new(multiview_engine::BlackConfig::default()),
            freeze: multiview_engine::FreezeProbe::new(
                multiview_engine::FreezeConfig::default().with_tolerance(FREEZE_TOLERANCE),
            ),
            black_dwell: multiview_engine::AlarmHysteresis::new(dwell(500_000_000), down),
            freeze_dwell: multiview_engine::AlarmHysteresis::new(dwell(2_000_000_000), down),
            per_source: HashMap::new(),
        }
    }
}

impl AuxFaultDetector {
    /// Forget sources no cell of the (new) layout binds.
    fn retain(&mut self, bound: &[(Arc<str>, SourceState)]) {
        self.per_source
            .retain(|id, _| bound.iter().any(|(b, _)| b == id));
    }

    /// Observe this tick's frames (parallel to `states`) at `pts`; afterwards
    /// [`AuxFaultDetector::fault`] answers per source.
    pub fn observe(
        &mut self,
        states: &[(Arc<str>, SourceState)],
        frames: &[Option<Arc<Nv12Image>>],
        pts: MediaTime,
    ) {
        use multiview_core::alarm::{AlarmId, AlarmKind, AlarmScope, PerceivedSeverity};
        use multiview_engine::{AlarmStateMachine, LumaView};
        for (index, ((id, _), frame)) in states.iter().zip(frames).enumerate() {
            // A source bound by two cells is observed once per tick.
            if states.iter().take(index).any(|(seen, _)| seen == id) {
                continue;
            }
            let (bd, fd) = (self.black_dwell, self.freeze_dwell);
            if !self.per_source.contains_key(id) {
                let mk = |kind: AlarmKind, hyst| {
                    AlarmStateMachine::new(
                        AlarmId::new(format!("{id}:{kind:?}")),
                        kind,
                        AlarmScope::Probe { id: id.to_string() },
                        PerceivedSeverity::Major,
                        hyst,
                    )
                };
                self.per_source.insert(
                    Arc::clone(id),
                    AuxSourceFaults {
                        black: mk(AlarmKind::Black, bd),
                        freeze: mk(AlarmKind::Freeze, fd),
                        previous: None,
                        window: std::collections::VecDeque::with_capacity(FREEZE_WINDOW + 1),
                        fault: TileFault::None,
                        since: pts,
                        warm: false,
                    },
                );
            }
            let Some(entry) = self.per_source.get_mut(id) else {
                continue;
            };
            let (black_now, freeze_now) = match frame.as_deref() {
                None => (false, false),
                Some(img) => match LumaView::packed(img.y_plane(), img.width(), img.height()) {
                    Err(_) => (false, false),
                    Ok(current) => {
                        let black = self.black.detect(&current).condition_present;
                        let frozen = entry.previous.as_deref().is_some_and(|prev| {
                            LumaView::packed(prev.y_plane(), prev.width(), prev.height())
                                .is_ok_and(|p| self.freeze.detect(&current, &p).condition_present)
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
            entry.warm = entry.warm || pts.saturating_sub(entry.since) >= WARM_AFTER;
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
    /// A layout-local fault always shows; once warm, the local verdict wins
    /// outright (also over a source on Program). Silence only the consumer
    /// measures, so a remote `Silent` stands unless a picture fault outranks
    /// it. Before the local dwell has run, the remote verdict fills in.
    #[must_use]
    pub fn resolve(&self, id: &str, remote: TileFault) -> TileFault {
        match self.per_source.get(id) {
            Some(local) if local.fault.is_present() => local.fault,
            Some(local) if local.warm && remote != TileFault::Silent => local.fault,
            // Unobserved, still warming, or silent per the consumer.
            _ => remote,
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
    pub fn publish_for<'a>(
        &self,
        meters: &HashMap<String, MeterChannels>,
        faults: &HashMap<String, TileFault>,
        program: impl Iterator<Item = &'a String>,
    ) {
        let wanted = self.sources.load();
        let mut signals = AuxOverlaySignals::default();
        for id in wanted.iter() {
            if let Some(m) = meters.get(id) {
                signals.meters.insert(id.clone(), *m);
            }
            if let Some(f) = faults.get(id) {
                signals.faults.insert(id.clone(), *f);
            }
        }
        for id in program {
            if wanted.contains(id) {
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
    pub fn new(body: &Arc<RoutedLayout>, inputs: &AuxOverlayInputs) -> Option<Self> {
        if !body.overlays.any() {
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
            faults: AuxFaultDetector::default(),
            dynamics: HashMap::new(),
            captions: HashMap::new(),
            bitmaps: HashMap::new(),
        })
    }

    /// Adopt an edited body of the same layout without reloading fonts:
    /// re-resolve the tiles (meter ballistics survive for unchanged sources)
    /// and the layers. Returns `false` when the new body turns every layer
    /// off (the caller drops the bake).
    pub fn rebuild(&mut self, body: &Arc<RoutedLayout>, labels: &LabelLookup) -> bool {
        if !body.overlays.any() {
            return false;
        }
        self.baker
            .set_tiles(tile_specs_with(&body.layout, |id| labels(id)));
        self.baker.set_layers(body.overlays);
        self.layers = body.overlays;
        let bound: Vec<(Arc<str>, SourceState)> = body
            .layout
            .cells
            .iter()
            .filter_map(|c| c.source.as_deref())
            .map(|id| (Arc::from(id), SourceState::NoSignal))
            .collect();
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
    /// Content faults: black/freeze come from this layout's own detector
    /// over `frames` (preferred, also for a source that is on Program); the
    /// consumer's snapshot supplies silence (it alone reads PCM) and, for a
    /// source this layout has not observed, its picture fault.
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
            self.faults.observe(states, frames, pts);
        }
        self.dynamics.clear();
        for tile in self.baker.tiles() {
            let id = tile.source_id.as_str();
            if self.dynamics.contains_key(id) {
                continue;
            }
            let state = states
                .iter()
                .find(|(source, _)| &**source == id)
                .map_or(SourceState::NoSignal, |(_, state)| *state);
            let tally = if signals.program.contains(id) {
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
