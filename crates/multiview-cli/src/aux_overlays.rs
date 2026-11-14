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
    pub fn new(slot: AuxSignalsSlot, wanted: Arc<AtomicBool>, active: Arc<AtomicBool>) -> Self {
        Self {
            slot,
            wanted,
            active,
        }
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
        self.body = Arc::clone(body);
        true
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

    /// The per-tile dynamics of the last bake (keyed by source id).
    #[must_use]
    pub fn last_dynamics(&self) -> &HashMap<String, TileDynamics> {
        &self.dynamics
    }

    /// Bake this layout's overlays onto `picture` at `pts`. `states` is the
    /// per-cell state the compose saw this tick (by source id); `signals` the
    /// consumer's latest snapshot; `preview` the M/E's PVW source. On any
    /// error the un-overlaid `picture` is returned (counted, warned once):
    /// never holds the bus, never panics.
    pub fn bake(
        &mut self,
        picture: &Arc<Nv12Image>,
        pts: MediaTime,
        states: &[(Arc<str>, SourceState)],
        signals: &AuxOverlaySignals,
        preview: Option<&str>,
    ) -> Arc<Nv12Image> {
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
                    fault: signals.faults.get(id).copied().unwrap_or(TileFault::None),
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
mod tests;
