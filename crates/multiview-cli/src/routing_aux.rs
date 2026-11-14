//! The `routing-aux` thread (ADR-0108 slice 1, design §2.2-2.5, §4.4).
//!
//! One OS thread per run, at `nice +10`, never a tokio task. It blocks on the
//! clock's capacity-4 tick channel ([`multiview_engine::aux_tick_channel`]); the
//! clock sends a tick only while some routed feed is active, so an idle thread
//! costs nothing. Per tick it builds one canvas-size picture per active one-cell
//! feed (a source, or the M/E's PVW source) and publishes it into that picture
//! bus's wait-free slot. It reads only `TileStore::read_at` and `SwapCell`s; it
//! takes no lock the clock holds, and it never writes anything the clock reads.
//!
//! A tile that is already canvas-sized, opaque and tagged like the canvas is
//! passed through as the same `Arc` (the identity path, §2.4). Any other tile is
//! letterboxed or pillarboxed with the pure-integer wall scaler into one of four
//! reusable canvas buffers; colour samples are copied, never converted. A tile
//! whose colour tag differs from the canvas is NOT drawn (copying its samples
//! under the canvas tag would mislabel them, invariant #8): the bus holds and
//! the consumer's lag rule takes over.
//!
//! A routed stored layout (ADR-0108 slice 3) is an N-cell aux canvas composed
//! here by the integer path of [`crate::aux_layout`], from the same stores.
//!
//! A feed may carry an output format (#342, [`AuxFormat`]): the tile is then
//! composed directly at that size with that fit (contain, cover at an anchor,
//! fill) — one convert per (picture, format), shared by every output with the
//! same (bus, format). A tile already at the format is passed as the same
//! `Arc`. A feed without a format is the canvas, exactly as before.
//!
//! A stored layout registered as an M/E source (#307, [`MeLayoutCompose`]) is
//! composed here too, after the routed feeds, with its own [`LayoutCanvas`]
//! and published into its `layout:<id>` [`TileStore`] at the tick's pts; the
//! Program drive samples that store like any source, one tick later.
//!
//! A panic in a tick body, or any other exit, raises `failed`; the consumer
//! then moves every output on an aux bus back to Program (§4.4). An M/E layout
//! store then simply stops being fed and ages to STALE, then NO_SIGNAL (its
//! slate) on Program; the output clock never waits for it.
//!
//! [`LayoutCanvas`]: crate::aux_layout::LayoutCanvas

use std::borrow::Cow;
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::thread::JoinHandle;

use multiview_compositor::pipeline::Nv12Image;
use multiview_config::{OutputBus, OutputFit, VideoFeed};
use multiview_control::preview::WallCell;
use multiview_core::color::ColorInfo;
use multiview_engine::AuxTick;
use multiview_framestore::{SwapCell, TileStore};

use crate::nv12_blit::{blit_cell, blit_region};
use crate::output_fit::{fit_rects, Rect};

/// Reusable letterbox buffers per feed (§2.4). A buffer still held by an
/// encoder is not reused; with none free the bus holds for that tick.
pub const AUX_RING_BUFFERS: usize = 4;

/// A routed layout on a picture bus: its id and the body resolved by the
/// control plane at take time. Two refs are the same bus when their ids are
/// equal (the body is what that bus composes; a re-take may carry a newer one).
#[derive(Debug, Clone)]
pub struct LayoutRef {
    /// The stored layout id.
    pub id: String,
    /// The resolved body.
    pub body: Arc<crate::aux_layout::RoutedLayout>,
}

impl LayoutRef {
    /// A ref to `body`.
    #[must_use]
    pub fn new(body: Arc<crate::aux_layout::RoutedLayout>) -> Self {
        Self {
            id: body.id.clone(),
            body,
        }
    }
}

impl PartialEq for LayoutRef {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for LayoutRef {}

impl std::hash::Hash for LayoutRef {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

/// The picture an output's encoder reads.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PictureBus {
    /// The Program canvas (the base encoders; not composed here).
    Program,
    /// The M/E's PVW source as a one-cell canvas.
    Preview,
    /// The Program canvas before the overlay bake (published by the bake
    /// consumer itself; not composed here).
    Clean,
    /// One source as a one-cell canvas.
    Source(String),
    /// A stored layout as an N-cell aux canvas (slice 3).
    Layout(LayoutRef),
}

impl PictureBus {
    /// The picture-bus name, matching [`VideoFeed::picture_bus`]:
    /// `program`, `preview`, `clean`, `src:<id>` or `aux:<layout>`.
    #[must_use]
    pub fn as_str(&self) -> Cow<'_, str> {
        match self {
            Self::Program => Cow::Borrowed("program"),
            Self::Preview => Cow::Borrowed("preview"),
            Self::Clean => Cow::Borrowed("clean"),
            Self::Source(id) => Cow::Owned(format!("src:{id}")),
            Self::Layout(layout) => Cow::Owned(format!("aux:{}", layout.id)),
        }
    }
}

/// A feed that cannot be built from config alone (a layout needs its body,
/// resolved by the control plane).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnroutableFeed;

impl TryFrom<&VideoFeed> for PictureBus {
    type Error = UnroutableFeed;

    fn try_from(feed: &VideoFeed) -> Result<Self, Self::Error> {
        match feed {
            VideoFeed::Bus(OutputBus::Program) => Ok(Self::Program),
            VideoFeed::Bus(OutputBus::Preview) => Ok(Self::Preview),
            VideoFeed::Bus(OutputBus::Clean) => Ok(Self::Clean),
            VideoFeed::Source(id) => Ok(Self::Source(id.clone())),
            _ => Err(UnroutableFeed),
        }
    }
}

/// One composed aux picture for one tick.
#[derive(Debug)]
pub struct AuxFrame {
    /// The output tick it was composed for.
    pub tick: u64,
    /// Increments when the bus's content source changes (a PVW selection change).
    pub generation: u64,
    /// The canvas-size picture.
    pub frame: Arc<Nv12Image>,
}

/// A picture bus's wait-free slot: the newest composed frame.
#[derive(Debug, Default)]
pub struct PictureBusSlot {
    /// The newest frame (read by the bake consumer without waiting).
    pub latest: SwapCell<Option<Arc<AuxFrame>>>,
    /// Frames published into this slot.
    pub composed: AtomicU64,
}

/// An output picture format a routed feed is composed at (#342): size and fit.
/// Colour, rate, scan and pixel format always equal the canvas (config
/// refuses anything else in slice B).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AuxFormat {
    /// Width in pixels (even).
    pub width: u32,
    /// Height in pixels (even).
    pub height: u32,
    /// How a picture is brought to this size.
    pub fit: OutputFit,
}

impl AuxFormat {
    /// A format of `width × height` with `fit`.
    #[must_use]
    pub const fn new(width: u32, height: u32, fit: OutputFit) -> Self {
        Self { width, height, fit }
    }

    /// The format of an output's BASE (Program) key: `None` whenever its size
    /// is the canvas's. The Program canvas is identity there (the fit is
    /// irrelevant for a canvas-sized picture, §12.4 P2), so such an output
    /// shares the canvas key and its zero-copy path.
    #[must_use]
    pub fn base_of_output(output: &multiview_config::Output, canvas: AuxCanvas) -> Option<Self> {
        let f = output.format()?;
        (f.width != canvas.width || f.height != canvas.height)
            .then(|| Self::new(f.width, f.height, f.fit))
    }

    /// The format a routed source/PVW FEED is composed at for an output:
    /// `None` only when it is the canvas size with the default fit (contain,
    /// centre) — exactly today's letterbox. A canvas-sized cover/fill or an
    /// anchored contain is honoured (#342 review M3).
    #[must_use]
    pub fn feed_of_output(output: &multiview_config::Output, canvas: AuxCanvas) -> Option<Self> {
        let f = output.format()?;
        let format = Self::new(f.width, f.height, f.fit);
        (format != Self::new(canvas.width, canvas.height, OutputFit::default())).then_some(format)
    }
}

/// One active routed feed the aux thread composes.
#[derive(Debug, Clone)]
pub struct AuxFeed {
    /// The bus (Program entries are ignored: Program is not composed here).
    pub bus: PictureBus,
    /// The output format composed (`None` = the canvas, as before #342).
    pub format: Option<AuxFormat>,
    /// Stable per-feed key (the bus name), so the thread's per-feed state is
    /// found without allocating each tick.
    pub key: Arc<str>,
    /// Where its frames go.
    pub slot: Arc<PictureBusSlot>,
}

impl AuxFeed {
    /// Build a feed for `bus` with a fresh slot.
    #[must_use]
    pub fn new(bus: PictureBus) -> Self {
        Self::with_format(bus, None)
    }

    /// Build a feed for `bus` composed at `format` (`None` = the canvas).
    #[must_use]
    pub fn with_format(bus: PictureBus, format: Option<AuxFormat>) -> Self {
        let key: Arc<str> = Arc::from(feed_name(&bus, format));
        Self {
            bus,
            format,
            key,
            slot: Arc::new(PictureBusSlot::default()),
        }
    }
}

/// The stable name of a (bus, format) feed: the bus name for the canvas
/// format (exactly as before #342), else `bus@WxH/mode/anchor`.
#[must_use]
pub fn feed_name(bus: &PictureBus, format: Option<AuxFormat>) -> String {
    match format {
        None => bus.as_str().into_owned(),
        Some(f) => format!(
            "{}@{}x{}/{:?}/{:?}",
            bus.as_str(),
            f.width,
            f.height,
            f.fit.mode,
            f.fit.anchor
        ),
    }
}

/// The active feed set, replaced whole by the consumer when it changes.
pub type AuxFeeds = Arc<SwapCell<Arc<Vec<AuxFeed>>>>;

/// One stored layout registered as an M/E source (#307): its engine key
/// (`layout:<id>`), its resolved body and the store the aux thread feeds.
#[derive(Debug, Clone)]
pub struct MeLayoutCompose {
    /// The engine source key, `layout:<id>`.
    pub key: Arc<str>,
    /// The resolved body (swapped whole on a live edit).
    pub body: Arc<crate::aux_layout::RoutedLayout>,
    /// The composed-picture store the Program drive samples.
    pub store: Arc<TileStore<Nv12Image>>,
}

/// The registered M/E layout sources, replaced whole by the registry.
pub type MeLayoutComposeSet = Arc<SwapCell<Arc<Vec<MeLayoutCompose>>>>;

/// Resolve a source id to its store. Must see sources added at runtime and must
/// never take a lock the clock holds (the pipeline passes a closure over the
/// catalogue `SwapCell`).
pub type StoreLookup = Arc<dyn Fn(&str) -> Option<Arc<TileStore<Nv12Image>>> + Send + Sync>;

/// `base`, falling back to the stores of the stored layouts registered as
/// M/E sources (#307 review M2), so `PictureBus::Preview` (and a routed
/// `src:` lookup) resolves `layout:<id>`. Wait-free: one `SwapCell` load
/// and a linear find over at most four entries, and only for an id `base`
/// does not know.
#[must_use]
pub fn with_me_layouts(base: StoreLookup, me_layouts: Option<MeLayoutComposeSet>) -> StoreLookup {
    let Some(set) = me_layouts else {
        return base;
    };
    Arc::new(move |id: &str| {
        base(id).or_else(|| {
            id.starts_with("layout:")
                .then(|| {
                    set.load()
                        .iter()
                        .find(|entry| entry.key.as_ref() == id)
                        .map(|entry| Arc::clone(&entry.store))
                })
                .flatten()
        })
    })
}

/// The output canvas the aux pictures match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuxCanvas {
    /// Width in pixels (even).
    pub width: u32,
    /// Height in pixels (even).
    pub height: u32,
    /// The canvas output colour tag.
    pub color: ColorInfo,
}

/// Owner handle of the aux thread. Dropping it never joins.
#[derive(Debug)]
pub struct RoutingAuxHandle {
    /// Raised on a panic or any exit of the thread.
    pub failed: Arc<AtomicBool>,
    /// The thread, for an optional bounded join at teardown (never on the consumer).
    pub join: Option<JoinHandle<()>>,
    /// Raised while the aux cadence is halved (§7 back-off).
    pub halved: Arc<AtomicBool>,
}

/// Everything the aux thread reads.
pub struct AuxInputs {
    /// Ticks from the clock.
    pub rx: Receiver<AuxTick>,
    /// The active feeds.
    pub feeds: AuxFeeds,
    /// Store lookup over the live catalogue.
    pub stores: StoreLookup,
    /// The M/E's PVW source id, if a switcher exists.
    pub preview_selection: Option<Arc<SwapCell<Option<Arc<str>>>>>,
    /// The canvas.
    pub canvas: AuxCanvas,
    /// The canvas a routed layout composes onto (size, colour, background).
    pub layout_canvas: crate::aux_layout::LayoutCanvasSpec,
    /// Program's lateness, for the §7 back-off (filled once Program runs;
    /// `None` in tests that do not exercise it).
    pub program_lateness: Option<ProgramLateness>,
    /// The output tick period in nanoseconds (the back-off threshold is half).
    pub tick_ns: u64,
    /// Stored layouts registered as M/E sources (#307); `None` without a
    /// switcher.
    pub me_layouts: Option<MeLayoutComposeSet>,
}

/// Program's lateness histogram, published once Program runs (the slot the
/// pipeline fills; `None` inside before that).
pub type ProgramLateness = Arc<SwapCell<Option<Arc<multiview_engine::lateness::TickLateness>>>>;

/// Program-lateness window of the §7 back-off.
pub const BACKOFF_WINDOW: std::time::Duration = std::time::Duration::from_secs(10);

/// ADR-0108 §7 "never degrade Program": when Program's tick lateness p99
/// over a [`BACKOFF_WINDOW`] exceeds half the tick, the aux thread halves its
/// cadence (composes even ticks only; picture buses hold) until a window
/// recovers. Reads Program's wait-free histogram only; never touches Program.
#[derive(Debug)]
pub struct BackOff {
    tick_ns: u64,
    window: Option<(
        std::time::Instant,
        multiview_engine::lateness::LatenessSnapshot,
    )>,
    halved: bool,
}

impl BackOff {
    /// A back-off for a tick of `tick_ns` nanoseconds (0 disables it).
    #[must_use]
    pub const fn new(tick_ns: u64) -> Self {
        Self {
            tick_ns,
            window: None,
            halved: false,
        }
    }

    /// Whether the cadence is halved now.
    #[must_use]
    pub const fn halved(&self) -> bool {
        self.halved
    }

    /// Feed the cumulative lateness `now_counts` at `now`; returns whether
    /// the cadence is halved. A window decides only once it is
    /// [`BACKOFF_WINDOW`] long and holds at least one tick.
    pub fn observe(
        &mut self,
        now: std::time::Instant,
        now_counts: multiview_engine::lateness::LatenessSnapshot,
    ) -> bool {
        let Some((start, earlier)) = self.window else {
            self.window = Some((now, now_counts));
            return self.halved;
        };
        if now.saturating_duration_since(start) < BACKOFF_WINDOW {
            return self.halved;
        }
        let window = now_counts.since(&earlier);
        if let Some(p99) = window.fine_quantile_upper_ns(990) {
            let late = self.tick_ns > 0 && p99 > self.tick_ns / 2;
            if late != self.halved {
                if late {
                    tracing::warn!(
                        p99_ns = p99,
                        "routing-aux: Program is late; routed pictures compose every second tick"
                    );
                } else {
                    tracing::info!(
                        "routing-aux: Program recovered; routed pictures compose every tick"
                    );
                }
            }
            self.halved = late;
        }
        self.window = Some((now, now_counts));
        self.halved
    }

    /// Whether tick `index` is skipped (halved cadence: odd ticks).
    #[must_use]
    pub const fn skips(&self, index: u64) -> bool {
        self.halved && index % 2 == 1
    }
}

/// Spawn the `routing-aux` thread.
///
/// # Errors
/// Returns the OS error if the thread cannot be spawned.
pub fn spawn_routing_aux(inputs: AuxInputs) -> std::io::Result<RoutingAuxHandle> {
    let failed = Arc::new(AtomicBool::new(false));
    let thread_failed = Arc::clone(&failed);
    let halved = Arc::new(AtomicBool::new(false));
    let thread_halved = Arc::clone(&halved);
    let join = std::thread::Builder::new()
        .name("routing-aux".to_owned())
        .spawn(move || {
            let _guard = FailGuard(Arc::clone(&thread_failed));
            lower_priority();
            run(&inputs, &thread_failed, &thread_halved);
        })?;
    Ok(RoutingAuxHandle {
        failed,
        join: Some(join),
        halved,
    })
}

/// Raises `failed` however the thread ends (§4.4 drop guard).
struct FailGuard(Arc<AtomicBool>);

impl Drop for FailGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[cfg(all(
    target_os = "linux",
    any(feature = "moq-output", feature = "heartbeat")
))]
fn lower_priority() {
    // On Linux the nice value is per thread; `who = 0` names the calling thread.
    if let Err(error) = rustix::process::setpriority_process(None, 10) {
        tracing::warn!(%error, "routing-aux: could not lower thread priority");
    }
}

#[cfg(not(all(
    target_os = "linux",
    any(feature = "moq-output", feature = "heartbeat")
)))]
fn lower_priority() {
    tracing::info!("routing-aux: runs at normal priority (no priority control in this build)");
}

fn run(inputs: &AuxInputs, failed: &AtomicBool, halved: &AtomicBool) {
    let mut state = AuxState::default();
    let mut backoff = BackOff::new(inputs.tick_ns);
    while let Ok(tick) = inputs.rx.recv() {
        if let Some(metrics) = inputs
            .program_lateness
            .as_ref()
            .and_then(|slot| slot.load())
        {
            let now = backoff.observe(std::time::Instant::now(), metrics.snapshot());
            halved.store(now, Ordering::Relaxed);
        }
        if backoff.skips(tick.index) {
            continue; // halved cadence: every routed picture holds this tick
        }
        let body = std::panic::catch_unwind(AssertUnwindSafe(|| state.tick(inputs, tick)));
        if body.is_err() {
            failed.store(true, Ordering::Release);
            tracing::error!("routing-aux: a compose panicked; routed feeds fall back to Program");
            return;
        }
    }
}

#[derive(Default)]
struct AuxState {
    feeds_seen: Option<Arc<Vec<AuxFeed>>>,
    per_feed: HashMap<Arc<str>, FeedState>,
    me_seen: Option<Arc<Vec<MeLayoutCompose>>>,
    /// Per M/E layout key: its compose state (rebuilt when the body changes).
    per_me_layout: HashMap<Arc<str>, Option<crate::aux_layout::LayoutCanvas>>,
}

#[derive(Default)]
struct FeedState {
    source: Option<Arc<str>>,
    generation: u64,
    ring: BlitRing,
    warned_color: bool,
    /// A routed layout's compose state (rebuilt when the body changes).
    layout: Option<crate::aux_layout::LayoutCanvas>,
}

impl AuxState {
    fn tick(&mut self, inputs: &AuxInputs, tick: AuxTick) {
        let feeds = inputs.feeds.load();
        let changed = self
            .feeds_seen
            .as_ref()
            .is_none_or(|seen| !Arc::ptr_eq(seen, &feeds));
        if changed {
            // Rare (a take or a close): forget state of feeds no longer active.
            self.per_feed
                .retain(|key, _| feeds.iter().any(|f| f.key == *key));
            self.feeds_seen = Some(Arc::clone(&feeds));
        }
        for feed in feeds.iter() {
            let source: Arc<str> = match &feed.bus {
                // Program and Clean come from the bake consumer, never here.
                PictureBus::Program | PictureBus::Clean => continue,
                PictureBus::Layout(layout) => {
                    self.compose_layout(inputs, feed, layout, tick);
                    continue;
                }
                PictureBus::Source(id) => match self.per_feed.get(&feed.key) {
                    Some(FeedState {
                        source: Some(s), ..
                    }) => Arc::clone(s),
                    _ => Arc::from(id.as_str()),
                },
                PictureBus::Preview => {
                    match inputs.preview_selection.as_ref().and_then(|s| s.load()) {
                        Some(id) => id,
                        None => continue,
                    }
                }
            };
            let entry = self.per_feed.entry(Arc::clone(&feed.key)).or_default();
            if entry.source.as_deref() != Some(&*source) {
                entry.generation = entry.generation.wrapping_add(1);
                entry.source = Some(Arc::clone(&source));
            }
            let Some(store) = (inputs.stores)(&source) else {
                continue;
            };
            let read = store.read_at(tick.pts);
            let Some(tile) = read.frame() else {
                continue; // never live: publish nothing (hold)
            };
            let Some(picture) = entry.compose_at(tile, inputs.canvas, feed.format) else {
                continue;
            };
            feed.slot.latest.store(Some(Arc::new(AuxFrame {
                tick: tick.index,
                generation: entry.generation,
                frame: picture,
            })));
            feed.slot.composed.fetch_add(1, Ordering::Relaxed);
        }
        self.compose_me_layouts(inputs, tick);
    }
}

impl AuxState {
    /// Compose every stored layout registered as an M/E source into its
    /// store at `tick.pts` (#307). Per-key state is forgotten for keys that
    /// left the set; a body swap (another `Arc`) rebuilds that key's canvas.
    fn compose_me_layouts(&mut self, inputs: &AuxInputs, tick: AuxTick) {
        let Some(set) = inputs.me_layouts.as_ref() else {
            return;
        };
        let set = set.load();
        let changed = self
            .me_seen
            .as_ref()
            .is_none_or(|seen| !Arc::ptr_eq(seen, &set));
        if changed {
            self.per_me_layout
                .retain(|key, _| set.iter().any(|entry| entry.key == *key));
            self.me_seen = Some(Arc::clone(&set));
        }
        for entry in set.iter() {
            let canvas = self
                .per_me_layout
                .entry(Arc::clone(&entry.key))
                .or_default();
            let stale = canvas
                .as_ref()
                .is_none_or(|c| !Arc::ptr_eq(c.layout(), &entry.body));
            if stale {
                *canvas = crate::aux_layout::LayoutCanvas::new(
                    Arc::clone(&entry.body),
                    inputs.layout_canvas,
                );
            }
            let Some(canvas) = canvas.as_mut() else {
                continue; // another canvas size: never composed (the store ages)
            };
            let Some(picture) = canvas.compose(&inputs.stores, tick.pts) else {
                continue; // every buffer in use downstream: the store holds
            };
            entry.store.publish_arc(picture, tick.pts);
            // Latch our own frame: a registered layout that no bus reads yet
            // (patched to an input only) still lets the store prune its ring,
            // so the canvas's four buffers keep recycling. Program reads at a
            // later tick and never needs an older frame.
            let _ = entry.store.state_at(tick.pts);
        }
    }

    /// Compose one routed layout for `tick` into its slot (hold on `None`).
    fn compose_layout(
        &mut self,
        inputs: &AuxInputs,
        feed: &AuxFeed,
        layout: &LayoutRef,
        tick: AuxTick,
    ) {
        let entry = self.per_feed.entry(Arc::clone(&feed.key)).or_default();
        let stale = entry
            .layout
            .as_ref()
            .is_none_or(|c| !Arc::ptr_eq(c.layout(), &layout.body));
        if stale {
            entry.layout = crate::aux_layout::LayoutCanvas::new(
                Arc::clone(&layout.body),
                inputs.layout_canvas,
            );
            entry.generation = entry.generation.wrapping_add(1);
        }
        let Some(canvas) = entry.layout.as_mut() else {
            return; // a layout for another canvas size: never composed (hold)
        };
        let Some(picture) = canvas.compose(&inputs.stores, tick.pts) else {
            return;
        };
        feed.slot.latest.store(Some(Arc::new(AuxFrame {
            tick: tick.index,
            generation: entry.generation,
            frame: picture,
        })));
        feed.slot.composed.fetch_add(1, Ordering::Relaxed);
    }
}

impl FeedState {
    #[cfg(test)]
    fn compose(&mut self, tile: &Arc<Nv12Image>, canvas: AuxCanvas) -> Option<Arc<Nv12Image>> {
        self.compose_at(tile, canvas, None)
    }

    /// Compose `tile` at `format` (`None` = the canvas, as before #342).
    fn compose_at(
        &mut self,
        tile: &Arc<Nv12Image>,
        canvas: AuxCanvas,
        format: Option<AuxFormat>,
    ) -> Option<Arc<Nv12Image>> {
        if tile.color() != canvas.color {
            if !self.warned_color {
                tracing::warn!(
                    "routing-aux: a routed source's colour tag differs from the output canvas; \
                     it is held instead of being shown mislabelled"
                );
                self.warned_color = true;
            }
            return None;
        }
        if passes_through(tile, canvas, format) {
            return Some(Arc::clone(tile));
        }
        match format {
            None => self.ring.letterbox(tile, canvas),
            Some(format) => self.ring.fit(tile, canvas, format),
        }
    }
}

/// The identity rule of §2.4: canvas-sized, opaque, tagged like the canvas.
/// (`Nv12Image` is always NV12 and carries no interlace flag: ingest
/// deinterlaces or delivers progressive frames, so every tile is progressive.)
#[must_use]
pub fn is_identity(tile: &Nv12Image, canvas: AuxCanvas) -> bool {
    tile.width() == canvas.width
        && tile.height() == canvas.height
        && tile.alpha_plane().is_none()
        && tile.color() == canvas.color
}

/// Whether the aux thread passes `tile` to a feed at `format` as the SAME
/// `Arc` (no copy, no convert): §12.4 P2. `None` = the canvas, the §2.4
/// identity rule; `Some(format)`: the tile is exactly the format's size,
/// opaque and tagged like the canvas — any fit of an equal-size picture is
/// the identity. The one predicate the aux compose and the plan share
/// (#342 review M1/M2), so they cannot drift.
#[must_use]
pub fn passes_through(tile: &Nv12Image, canvas: AuxCanvas, format: Option<AuxFormat>) -> bool {
    match format {
        None => is_identity(tile, canvas),
        Some(f) => {
            tile.width() == f.width
                && tile.height() == f.height
                && tile.alpha_plane().is_none()
                && tile.color() == canvas.color
        }
    }
}

/// The centred, aspect-preserving rectangle for `(sw, sh)` inside the canvas,
/// with even origin and size (4:2:0 chroma).
#[must_use]
pub fn contain_rect(sw: u32, sh: u32, cw: u32, ch: u32) -> Option<(u32, u32, u32, u32)> {
    if sw == 0 || sh == 0 || cw < 2 || ch < 2 {
        return None;
    }
    let (src_w, src_h) = (u64::from(sw), u64::from(sh));
    let (dst_w, dst_h) = (u64::from(cw), u64::from(ch));
    let (w, h) = if src_w * dst_h >= src_h * dst_w {
        (dst_w, src_h * dst_w / src_w)
    } else {
        (src_w * dst_h / src_h, dst_h)
    };
    let even = |v: u64| u32::try_from(v & !1).ok();
    let (w, h) = (even(w)?.max(2), even(h)?.max(2));
    let x = ((cw - w) / 2) & !1;
    let y = ((ch - h) / 2) & !1;
    Some((x, y, w, h))
}

#[derive(Default)]
struct BlitRing {
    buffers: Vec<Arc<Nv12Image>>,
}

impl BlitRing {
    /// A free buffer of `w × h` (allocating up to [`AUX_RING_BUFFERS`]);
    /// `None` when every buffer is still held downstream.
    fn free(&mut self, w: u32, h: u32, color: ColorInfo) -> Option<usize> {
        match self
            .buffers
            .iter_mut()
            .position(|b| Arc::get_mut(b).is_some())
        {
            Some(index) => Some(index),
            None if self.buffers.len() < AUX_RING_BUFFERS => {
                let image = Nv12Image::solid(w, h, 16, 128, 128, color).ok()?;
                self.buffers.push(Arc::new(image));
                Some(self.buffers.len() - 1)
            }
            None => None,
        }
    }

    /// `tile` at `format` with its fit (#342): one crop/scale into a reusable
    /// buffer of the format's size, tagged like the canvas.
    fn fit(
        &mut self,
        tile: &Nv12Image,
        canvas: AuxCanvas,
        format: AuxFormat,
    ) -> Option<Arc<Nv12Image>> {
        let (crop, dst) = fit_rects(
            tile.width(),
            tile.height(),
            format.width,
            format.height,
            format.fit.mode,
            format.fit.anchor,
        )?;
        let index = self.free(format.width, format.height, canvas.color)?;
        let buffer = self.buffers.get_mut(index)?;
        let image = Arc::get_mut(buffer)?;
        let (y_plane, uv_plane) = image.planes_mut();
        if dst != Rect::full(format.width, format.height) {
            y_plane.fill(16);
            uv_plane.fill(128);
        }
        blit_region(tile, crop, format.width, y_plane, uv_plane, dst);
        Some(Arc::clone(buffer))
    }

    fn letterbox(&mut self, tile: &Nv12Image, canvas: AuxCanvas) -> Option<Arc<Nv12Image>> {
        if tile.width() < 2 || tile.height() < 2 {
            return None;
        }
        let (x, y, w, h) = contain_rect(tile.width(), tile.height(), canvas.width, canvas.height)?;
        let cell = WallCell {
            source: String::new(),
            x,
            y,
            w,
            h,
            live: true,
        };
        let index = match self
            .buffers
            .iter_mut()
            .position(|b| Arc::get_mut(b).is_some())
        {
            Some(index) => index,
            None if self.buffers.len() < AUX_RING_BUFFERS => {
                let image =
                    Nv12Image::solid(canvas.width, canvas.height, 16, 128, 128, canvas.color)
                        .ok()?;
                self.buffers.push(Arc::new(image));
                self.buffers.len() - 1
            }
            None => return None, // every buffer still held downstream: hold
        };
        let buffer = self.buffers.get_mut(index)?;
        let image = Arc::get_mut(buffer)?;
        let (y_plane, uv_plane) = image.planes_mut();
        y_plane.fill(16);
        uv_plane.fill(128);
        blit_cell(tile, canvas.width, y_plane, uv_plane, &cell);
        Some(Arc::clone(buffer))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use multiview_compositor::pipeline::CanvasColor;
    use multiview_core::time::MediaTime;
    use multiview_engine::aux_tick_channel;
    use std::time::{Duration, Instant};

    fn canvas() -> AuxCanvas {
        AuxCanvas {
            width: 1920,
            height: 1080,
            color: CanvasColor::default().output_tag(),
        }
    }

    fn gradient(w: u32, h: u32, color: ColorInfo) -> Nv12Image {
        let (wu, hu) = (usize::try_from(w).unwrap(), usize::try_from(h).unwrap());
        let y = (0..wu * hu)
            .map(|i| u8::try_from(16 + (i * 7) % 219).unwrap())
            .collect();
        let uv = (0..wu * hu / 2)
            .map(|i| u8::try_from(16 + (i * 3) % 224).unwrap())
            .collect();
        Nv12Image::new(w, h, y, uv, color).unwrap()
    }

    #[test]
    fn picture_bus_names_match_the_config_feed_names() {
        // Clean is now a routable picture bus (behaviour intentionally
        // changed by #305): it joins the accepted feeds below instead of
        // being refused. Only an actual layout feed is still unroutable.
        for feed in [
            VideoFeed::PROGRAM,
            VideoFeed::Bus(OutputBus::Preview),
            VideoFeed::Bus(OutputBus::Clean),
            VideoFeed::Source("cam-1".to_owned()),
        ] {
            let bus = PictureBus::try_from(&feed).unwrap();
            assert_eq!(bus.as_str(), feed.picture_bus());
        }
        assert!(PictureBus::try_from(&VideoFeed::Layout("l".to_owned())).is_err());
    }

    #[test]
    fn identity_returns_the_same_arc() {
        let tile = Arc::new(gradient(1920, 1080, canvas().color));
        let mut state = FeedState::default();
        let out = state.compose(&tile, canvas()).unwrap();
        assert!(Arc::ptr_eq(&out, &tile));
    }

    #[test]
    #[allow(clippy::many_single_char_names)]
    fn letterbox_keeps_aspect_and_matches_blit_cell_byte_for_byte() {
        let c = canvas();
        // 640x360 is 16:9 like the canvas: full-canvas rect, scaled.
        assert_eq!(contain_rect(640, 360, 1920, 1080), Some((0, 0, 1920, 1080)));
        // 4:3 pillarbox and 21:9-ish letterbox keep aspect, centred.
        assert_eq!(
            contain_rect(640, 480, 1920, 1080),
            Some((240, 0, 1440, 1080))
        );
        assert_eq!(
            contain_rect(1920, 800, 1920, 1080),
            Some((0, 140, 1920, 800))
        );

        for (sw, sh) in [(640, 360), (640, 480)] {
            let tile = Arc::new(gradient(sw, sh, c.color));
            let mut state = FeedState::default();
            let out = state.compose(&tile, c).unwrap();
            assert!(!Arc::ptr_eq(&out, &tile));
            assert_eq!((out.width(), out.height()), (1920, 1080));
            assert_eq!(out.color(), c.color);
            let (x, y, w, h) = contain_rect(sw, sh, 1920, 1080).unwrap();
            let mut ey = vec![16u8; 1920 * 1080];
            let mut euv = vec![128u8; 1920 * 1080 / 2];
            let cell = WallCell {
                source: String::new(),
                x,
                y,
                w,
                h,
                live: true,
            };
            blit_cell(&tile, 1920, &mut ey, &mut euv, &cell);
            assert!(out.y_plane() == ey.as_slice(), "luma parity {sw}x{sh}");
            assert!(out.uv_plane() == euv.as_slice(), "chroma parity {sw}x{sh}");
        }
    }

    /// H (lens 1 #5): odd source sizes. An NV12 tile itself can never be odd
    /// (`Nv12Image::new` rejects it), but `contain_rect` takes any size: the
    /// rect for 641x481 -> 1920x1080 and for a sweep of odd sizes is
    /// even-aligned and inside the canvas (no out-of-bounds blit), and the
    /// largest even tile below it composes to a canvas-size image.
    #[test]
    fn odd_sizes_give_an_even_in_bounds_rect() {
        let (x, y, w, h) = contain_rect(641, 481, 1920, 1080).unwrap();
        assert_eq!((x % 2, y % 2, w % 2, h % 2), (0, 0, 0, 0));
        assert!(x + w <= 1920 && y + h <= 1080);
        assert_eq!(h, 1080, "4:3-ish pillarbox fills the height");
        for sw in (1..2000).step_by(37) {
            for sh in (1..1200).step_by(41) {
                for (cw, ch) in [(1920, 1080), (1280, 720), (2, 2), (642, 482)] {
                    let (x, y, w, h) = contain_rect(sw, sh, cw, ch).unwrap();
                    assert_eq!((x | y | w | h) & 1, 0, "{sw}x{sh} in {cw}x{ch}");
                    assert!(x + w <= cw && y + h <= ch, "{sw}x{sh} in {cw}x{ch}");
                }
            }
        }
        assert!(Nv12Image::new(
            641,
            481,
            vec![16; 641 * 481],
            vec![128; 641 * 481 / 2],
            canvas().color
        )
        .is_err());
        let tile = Arc::new(gradient(640, 480, canvas().color));
        let out = FeedState::default().compose(&tile, canvas()).unwrap();
        assert_eq!((out.width(), out.height()), (1920, 1080));
    }

    #[test]
    fn the_ring_reuses_free_buffers_and_holds_when_all_are_in_use() {
        let c = canvas();
        let tile = Arc::new(gradient(640, 360, c.color));
        let mut state = FeedState::default();
        let mut held = Vec::new();
        for _ in 0..AUX_RING_BUFFERS {
            held.push(state.compose(&tile, c).unwrap());
        }
        assert!(state.compose(&tile, c).is_none(), "no free buffer: hold");
        let first = Arc::as_ptr(&held[0]);
        held.remove(0);
        let reused = state.compose(&tile, c).unwrap();
        assert_eq!(
            Arc::as_ptr(&reused),
            first,
            "freed buffer reused, no allocation"
        );
    }

    #[test]
    fn a_mislabelled_tile_is_held_not_drawn() {
        let c = canvas();
        let other = ColorInfo::default().resolve_defaults(640, 480);
        assert_ne!(other, c.color);
        let tile = Arc::new(gradient(640, 480, other));
        assert!(FeedState::default().compose(&tile, c).is_none());
    }

    fn spawn_with(
        stores: StoreLookup,
        feeds: Vec<AuxFeed>,
    ) -> (multiview_engine::AuxTickSender, RoutingAuxHandle) {
        let (tx, rx) = aux_tick_channel();
        tx.active_flag().store(true, Ordering::Release);
        let handle = spawn_routing_aux(AuxInputs {
            rx,
            feeds: Arc::new(SwapCell::from_pointee(feeds)),
            stores,
            preview_selection: None,
            canvas: canvas(),
            layout_canvas: crate::aux_layout::LayoutCanvasSpec {
                width: 1920,
                height: 1080,
                color: CanvasColor::default(),
                background: [16, 128, 128],
            },
            program_lateness: None,
            tick_ns: 0,
            me_layouts: None,
        })
        .unwrap();
        (tx, handle)
    }

    /// A stored layout registered as an M/E source (#307) is composed into
    /// its `layout:<id>` store at the tick's pts with no routed feed; a body
    /// swap rebuilds its canvas; a key that leaves the set is no longer fed.
    #[test]
    fn me_layouts_compose_into_their_store_at_the_tick_pts() {
        let cam = Arc::new(TileStore::<Nv12Image>::with_defaults("cam"));
        let lookup_store = Arc::clone(&cam);
        let stores: StoreLookup =
            Arc::new(move |id: &str| (id == "cam").then(|| Arc::clone(&lookup_store)));
        let body = |revision: u64| {
            Arc::new(crate::aux_layout::RoutedLayout {
                id: "wall".to_owned(),
                layout: multiview_core::layout::Layout {
                    name: "wall".into(),
                    canvas: multiview_core::layout::Canvas {
                        width: 1920,
                        height: 1080,
                        fps_num: 30,
                        fps_den: 1,
                    },
                    cells: vec![multiview_core::layout::Cell {
                        x: 0.,
                        y: 0.,
                        w: 0.5,
                        h: 0.5,
                        source: Some("cam".into()),
                        ..multiview_core::layout::Cell::default()
                    }],
                },
                slates: vec![multiview_config::FailoverSlate::Black],
                audio: multiview_config::LayoutAudio::default(),
                revision,
            })
        };
        let store = Arc::new(TileStore::<Nv12Image>::with_defaults("layout:wall"));
        let set: MeLayoutComposeSet = Arc::new(SwapCell::from_pointee(vec![MeLayoutCompose {
            key: Arc::from("layout:wall"),
            body: body(1),
            store: Arc::clone(&store),
        }]));
        let (tx, rx) = aux_tick_channel();
        // Only the M/E flag: no routed feed is active.
        tx.me_layout_flag().store(true, Ordering::Release);
        let handle = spawn_routing_aux(AuxInputs {
            rx,
            feeds: Arc::new(SwapCell::from_pointee(Vec::new())),
            stores,
            preview_selection: None,
            canvas: canvas(),
            layout_canvas: crate::aux_layout::LayoutCanvasSpec {
                width: 1920,
                height: 1080,
                color: CanvasColor::default(),
                background: [16, 128, 128],
            },
            program_lateness: None,
            tick_ns: 0,
            me_layouts: Some(Arc::clone(&set)),
        })
        .unwrap();
        cam.publish(gradient(1920, 1080, canvas().color), tick(0).pts);
        tx.offer(tick(0));
        assert!(wait_for(|| store.sequence() == 1));
        let read = store.read_at(tick(0).pts);
        let frame = read.frame().expect("composed at the tick pts").clone();
        assert_eq!((frame.width(), frame.height()), (1920, 1080));
        assert!(store.state_at(tick(0).pts).is_live());
        // A body swap (a live edit) keeps composing into the same store.
        set.store(Arc::new(vec![MeLayoutCompose {
            key: Arc::from("layout:wall"),
            body: body(2),
            store: Arc::clone(&store),
        }]));
        tx.offer(tick(1));
        assert!(wait_for(|| store.sequence() == 2));
        assert!(store.state_at(tick(1).pts).is_live());
        // Unregistered: the store is no longer fed (it ages on Program).
        set.store(Arc::new(Vec::new()));
        tx.offer(tick(2));
        tx.offer(tick(3));
        drop(tx);
        assert!(wait_for(|| handle.failed.load(Ordering::Acquire)));
        assert_eq!(store.sequence(), 2, "a removed key is not fed");
    }

    fn wall_body(
        revision: u64,
        cells: Vec<multiview_core::layout::Cell>,
    ) -> Arc<crate::aux_layout::RoutedLayout> {
        Arc::new(crate::aux_layout::RoutedLayout {
            id: "wall".to_owned(),
            layout: multiview_core::layout::Layout {
                name: "wall".into(),
                canvas: multiview_core::layout::Canvas {
                    width: 1920,
                    height: 1080,
                    fps_num: 30,
                    fps_den: 1,
                },
                cells,
            },
            slates: vec![multiview_config::FailoverSlate::Black],
            audio: multiview_config::LayoutAudio::default(),
            revision,
        })
    }

    fn cell(x: f32, y: f32, w: f32, h: f32, source: &str) -> multiview_core::layout::Cell {
        multiview_core::layout::Cell {
            x,
            y,
            w,
            h,
            source: Some(source.into()),
            ..multiview_core::layout::Cell::default()
        }
    }

    fn me_inputs(
        rx: std::sync::mpsc::Receiver<multiview_engine::routing_aux::AuxTick>,
        feeds: Vec<AuxFeed>,
        stores: StoreLookup,
        preview: Option<Arc<SwapCell<Option<Arc<str>>>>>,
        set: &MeLayoutComposeSet,
    ) -> AuxInputs {
        AuxInputs {
            rx,
            feeds: Arc::new(SwapCell::from_pointee(feeds)),
            stores: with_me_layouts(stores, Some(Arc::clone(set))),
            preview_selection: preview,
            canvas: canvas(),
            layout_canvas: crate::aux_layout::LayoutCanvasSpec {
                width: 1920,
                height: 1080,
                color: CanvasColor::default(),
                background: [16, 128, 128],
            },
            program_lateness: None,
            tick_ns: 0,
            me_layouts: Some(Arc::clone(set)),
        }
    }

    /// #307 review M2: a routed Preview feed whose M/E selection goes cam ->
    /// `layout:wall` -> cam publishes cam pixels, then the composed layout's
    /// pixels, then cam again (the lookup falls back to the M/E layout set).
    #[test]
    fn a_routed_preview_of_a_layout_publishes_the_composed_layout() {
        let cam = Arc::new(TileStore::<Nv12Image>::with_defaults("cam"));
        let lookup_store = Arc::clone(&cam);
        let stores: StoreLookup =
            Arc::new(move |id: &str| (id == "cam").then(|| Arc::clone(&lookup_store)));
        let wall = Arc::new(TileStore::<Nv12Image>::with_defaults("layout:wall"));
        let set: MeLayoutComposeSet = Arc::new(SwapCell::from_pointee(vec![MeLayoutCompose {
            key: Arc::from("layout:wall"),
            body: wall_body(1, vec![cell(0., 0., 0.5, 0.5, "cam")]),
            store: Arc::clone(&wall),
        }]));
        let preview: Arc<SwapCell<Option<Arc<str>>>> =
            Arc::new(SwapCell::new(Some(Arc::from("cam"))));
        let feed = AuxFeed::new(PictureBus::Preview);
        let slot = Arc::clone(&feed.slot);
        let (tx, rx) = aux_tick_channel();
        tx.active_flag().store(true, Ordering::Release);
        tx.me_layout_flag().store(true, Ordering::Release);
        let _handle = spawn_routing_aux(me_inputs(
            rx,
            vec![feed],
            stores,
            Some(Arc::clone(&preview)),
            &set,
        ))
        .unwrap();
        let cam_frame = gradient(1920, 1080, canvas().color);
        let cam_luma = cam_frame.y_plane().to_vec();
        let published = |i: u64| {
            cam.publish(cam_frame.clone(), tick(i).pts);
            tx.offer(tick(i));
            assert!(wait_for(
                || slot.latest.load().is_some_and(|f| f.tick == i) && wall.sequence() > i
            ));
            slot.latest.load().unwrap()
        };
        let first = published(0);
        assert_eq!(first.frame.y_plane(), cam_luma.as_slice(), "cam pixels");
        preview.store(Some(Arc::from("layout:wall")));
        let layout = published(1);
        assert_ne!(layout.frame.y_plane(), cam_luma.as_slice());
        assert!(layout.generation > first.generation);
        // The composed layout: the background outside the cell, the cam
        // inside it (top-left quarter).
        let y = layout.frame.y_plane();
        let w = 1920_usize;
        assert_eq!(y[1079 * w + 1919], 16, "background luma bottom-right");
        let (cell_y, cell_x) = (100_usize, 100_usize);
        assert_ne!(y[cell_y * w + cell_x], 16, "the cam is drawn in its cell");
        let composed = wall.read_at(tick(0).pts);
        assert_eq!(
            composed.frame().unwrap().y_plane(),
            y,
            "exactly the layout store's composed picture"
        );
        preview.store(Some(Arc::from("cam")));
        let back = published(2);
        assert_eq!(back.frame.y_plane(), cam_luma.as_slice(), "cam again");
        assert!(back.generation > layout.generation);
    }

    /// #307 review M4: an edited body that changes geometry produces
    /// different composed pixels in the same store.
    #[test]
    fn an_edited_body_changes_the_composed_pixels() {
        let cam = Arc::new(TileStore::<Nv12Image>::with_defaults("cam"));
        let lookup_store = Arc::clone(&cam);
        let stores: StoreLookup =
            Arc::new(move |id: &str| (id == "cam").then(|| Arc::clone(&lookup_store)));
        let wall = Arc::new(TileStore::<Nv12Image>::with_defaults("layout:wall"));
        let set: MeLayoutComposeSet = Arc::new(SwapCell::from_pointee(vec![MeLayoutCompose {
            key: Arc::from("layout:wall"),
            body: wall_body(1, vec![cell(0., 0., 0.5, 0.5, "cam")]),
            store: Arc::clone(&wall),
        }]));
        let (tx, rx) = aux_tick_channel();
        tx.me_layout_flag().store(true, Ordering::Release);
        let _handle = spawn_routing_aux(me_inputs(rx, Vec::new(), stores, None, &set)).unwrap();
        let cam_frame = gradient(1920, 1080, canvas().color);
        cam.publish(cam_frame.clone(), tick(0).pts);
        tx.offer(tick(0));
        assert!(wait_for(|| wall.sequence() == 1));
        let before = wall
            .read_at(tick(0).pts)
            .frame()
            .unwrap()
            .y_plane()
            .to_vec();
        let w = 1920_usize;
        // Top-left drawn, bottom-right background.
        assert_ne!(before[100 * w + 100], 16);
        assert_eq!(before[1000 * w + 1800], 16);
        // The edit moves the cell to the bottom-right quarter.
        set.store(Arc::new(vec![MeLayoutCompose {
            key: Arc::from("layout:wall"),
            body: wall_body(2, vec![cell(0.5, 0.5, 0.5, 0.5, "cam")]),
            store: Arc::clone(&wall),
        }]));
        cam.publish(cam_frame, tick(1).pts);
        tx.offer(tick(1));
        assert!(wait_for(|| wall.sequence() == 2));
        let after = wall
            .read_at(tick(1).pts)
            .frame()
            .unwrap()
            .y_plane()
            .to_vec();
        assert_ne!(before, after, "the edit changed the composed pixels");
        assert_eq!(after[100 * w + 100], 16, "top-left is background now");
        assert_ne!(after[1000 * w + 1800], 16, "the cam is drawn bottom-right");
    }

    fn wait_for(cond: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        false
    }

    fn tick(i: u64) -> AuxTick {
        AuxTick {
            index: i,
            pts: MediaTime::from_nanos(i64::try_from(i).unwrap() * 33_366_666),
        }
    }

    #[test]
    fn a_panicking_compose_sets_failed_and_ends_the_thread() {
        let stores: StoreLookup = Arc::new(|_id: &str| panic!("injected compose panic"));
        let feed = AuxFeed::new(PictureBus::Source("cam".to_owned()));
        let (tx, mut handle) = spawn_with(stores, vec![feed]);
        tx.offer(tick(0));
        assert!(wait_for(|| handle.failed.load(Ordering::Acquire)));
        handle.join.take().unwrap().join().unwrap();
    }

    #[test]
    fn a_never_live_store_publishes_nothing() {
        let store = Arc::new(TileStore::<Nv12Image>::with_defaults("cam"));
        let lookup_store = Arc::clone(&store);
        let stores: StoreLookup =
            Arc::new(move |id: &str| (id == "cam").then(|| Arc::clone(&lookup_store)));
        let feed = AuxFeed::new(PictureBus::Source("cam".to_owned()));
        let slot = Arc::clone(&feed.slot);
        let missing = AuxFeed::new(PictureBus::Source("absent".to_owned()));
        let missing_slot = Arc::clone(&missing.slot);
        let (tx, handle) = spawn_with(stores, vec![feed, missing]);
        for i in 0..3 {
            tx.offer(tick(i));
        }
        // Then a live frame lands: the thread really is running and publishing.
        store.publish(gradient(1920, 1080, canvas().color), tick(3).pts);
        tx.offer(tick(3));
        assert!(wait_for(|| slot.composed.load(Ordering::Relaxed) == 1));
        assert_eq!(slot.latest.load().unwrap().tick, 3);
        assert_eq!(missing_slot.composed.load(Ordering::Relaxed), 0);
        assert!(missing_slot.latest.load().is_none());
        assert!(!handle.failed.load(Ordering::Acquire));
        drop(tx);
        assert!(
            wait_for(|| handle.failed.load(Ordering::Acquire)),
            "exit raises failed"
        );
    }

    fn vertical() -> AuxFormat {
        AuxFormat::new(
            1080,
            1920,
            OutputFit::new(
                multiview_config::OutputFitMode::Cover,
                multiview_config::Anchor::Center,
            ),
        )
    }

    #[test]
    fn a_vertical_cover_feed_composes_at_its_format_with_the_cover_crop() {
        let c = canvas();
        let tile = Arc::new(gradient(1920, 1080, c.color));
        let mut state = FeedState::default();
        let out = state.compose_at(&tile, c, Some(vertical())).unwrap();
        assert_eq!((out.width(), out.height()), (1080, 1920));
        assert_eq!(out.color(), c.color);
        let (crop, dst) = fit_rects(
            1920,
            1080,
            1080,
            1920,
            multiview_config::OutputFitMode::Cover,
            multiview_config::Anchor::Center,
        )
        .unwrap();
        assert_eq!(crop, Rect::new(656, 0, 608, 1080));
        let mut y = vec![0u8; 1080 * 1920];
        let mut uv = vec![0u8; 1080 * 1920 / 2];
        blit_region(&tile, crop, 1080, &mut y, &mut uv, dst);
        assert!(out.y_plane().eq(y.as_slice()), "luma parity");
        assert!(out.uv_plane().eq(uv.as_slice()), "chroma parity");
    }

    #[test]
    fn a_tile_already_at_the_format_passes_the_same_arc() {
        let c = canvas();
        let tile = Arc::new(gradient(1080, 1920, c.color));
        let mut state = FeedState::default();
        let out = state.compose_at(&tile, c, Some(vertical())).unwrap();
        assert!(
            Arc::ptr_eq(&out, &tile),
            "identity at the format allocates nothing"
        );
        // A tile with another colour tag is still held, at any format.
        let other = ColorInfo::default().resolve_defaults(640, 480);
        let odd = Arc::new(gradient(640, 480, other));
        assert!(state.compose_at(&odd, c, Some(vertical())).is_none());
    }

    #[test]
    fn the_aux_thread_publishes_a_formatted_feed_at_its_size() {
        let store = Arc::new(TileStore::<Nv12Image>::with_defaults("cam"));
        let lookup_store = Arc::clone(&store);
        let stores: StoreLookup =
            Arc::new(move |id: &str| (id == "cam").then(|| Arc::clone(&lookup_store)));
        let plain = AuxFeed::new(PictureBus::Source("cam".to_owned()));
        let tall = AuxFeed::with_format(PictureBus::Source("cam".to_owned()), Some(vertical()));
        assert_ne!(plain.key, tall.key, "per-feed state is per (bus, format)");
        let (plain_slot, tall_slot) = (Arc::clone(&plain.slot), Arc::clone(&tall.slot));
        let (tx, _handle) = spawn_with(stores, vec![plain, tall]);
        store.publish(gradient(1920, 1080, canvas().color), tick(0).pts);
        tx.offer(tick(0));
        assert!(wait_for(|| tall_slot.composed.load(Ordering::Relaxed) == 1
            && plain_slot.composed.load(Ordering::Relaxed) == 1));
        let frame = tall_slot.latest.load().unwrap();
        assert_eq!((frame.frame.width(), frame.frame.height()), (1080, 1920));
        let canvas_frame = plain_slot.latest.load().unwrap();
        assert_eq!(
            (canvas_frame.frame.width(), canvas_frame.frame.height()),
            (1920, 1080)
        );
    }

    #[test]
    fn base_formats_collapse_at_canvas_size_and_feed_formats_keep_the_fit() {
        let cfg = multiview_config::MultiviewConfig::load_from_toml(
            r##"
schema_version = 1
[canvas]
width = 1920
height = 1080
fps = "30000/1001"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "preset"
preset = "2x2"
[[sources]]
id = "a"
kind = "test"
[[outputs]]
kind = "hls"
id = "same"
path = "/x"
codec = "h264"
[outputs.format]
width = 1920
height = 1080
[[outputs]]
kind = "hls"
id = "tall"
path = "/y"
codec = "h264"
[outputs.format]
width = 1080
height = 1920
fit = { mode = "cover" }
[[outputs]]
kind = "hls"
id = "same-cover"
path = "/w"
codec = "h264"
[outputs.format]
width = 1920
height = 1080
fit = { mode = "cover", anchor = "top" }
[[outputs]]
kind = "hls"
id = "none"
path = "/z"
codec = "h264"
"##,
        )
        .unwrap();
        let base: Vec<_> = cfg
            .outputs
            .iter()
            .map(|o| AuxFormat::base_of_output(o, canvas()))
            .collect();
        assert_eq!(base, vec![None, Some(vertical()), None, None]);
        // A routed feed honours a canvas-sized non-default fit (#342 M3).
        let feeds: Vec<_> = cfg
            .outputs
            .iter()
            .map(|o| AuxFormat::feed_of_output(o, canvas()))
            .collect();
        let canvas_cover_top = AuxFormat::new(
            1920,
            1080,
            OutputFit::new(
                multiview_config::OutputFitMode::Cover,
                multiview_config::Anchor::Top,
            ),
        );
        assert_eq!(
            feeds,
            vec![None, Some(vertical()), Some(canvas_cover_top), None]
        );
    }

    #[test]
    fn a_canvas_sized_cover_feed_fills_the_canvas_instead_of_letterboxing() {
        let c = canvas();
        let tile = Arc::new(gradient(640, 480, c.color));
        let cover = AuxFormat::new(
            1920,
            1080,
            OutputFit::new(
                multiview_config::OutputFitMode::Cover,
                multiview_config::Anchor::Center,
            ),
        );
        let out = FeedState::default()
            .compose_at(&tile, c, Some(cover))
            .unwrap();
        let (crop, dst) = fit_rects(
            640,
            480,
            1920,
            1080,
            multiview_config::OutputFitMode::Cover,
            multiview_config::Anchor::Center,
        )
        .unwrap();
        assert_eq!(dst, Rect::full(1920, 1080), "no letterbox bars");
        let mut y = vec![0u8; 1920 * 1080];
        let mut uv = vec![0u8; 1920 * 1080 / 2];
        blit_region(&tile, crop, 1920, &mut y, &mut uv, dst);
        assert!(out.y_plane().eq(y.as_slice()), "luma parity");
        assert!(out.uv_plane().eq(uv.as_slice()), "chroma parity");
    }

    /// PR 2 (#306, §7): Program's p99 lateness over a 10 s window above half
    /// the tick halves the aux cadence (odd ticks skipped); a window under
    /// half restores it. A window shorter than 10 s never decides.
    #[test]
    fn the_back_off_halves_on_late_program_windows_and_recovers() {
        let tick_ns = 33_366_667; // 29.97
        let lateness = multiview_engine::lateness::TickLateness::shared();
        let mut backoff = BackOff::new(tick_ns);
        let t0 = Instant::now();
        assert!(!backoff.observe(t0, lateness.snapshot()));
        // 100 ticks at 20 ms late (> 16.7 ms = half the tick), 5 s in: too
        // early to decide.
        for _ in 0..100 {
            lateness.record(20_000_000, 0);
        }
        assert!(!backoff.observe(t0 + Duration::from_secs(5), lateness.snapshot()));
        assert!(backoff.observe(t0 + Duration::from_secs(10), lateness.snapshot()));
        assert!(backoff.skips(1) && !backoff.skips(2));
        // A window on time recovers.
        for _ in 0..300 {
            lateness.record(1_000_000, 0);
        }
        assert!(!backoff.observe(t0 + Duration::from_secs(20), lateness.snapshot()));
        assert!(!backoff.skips(1));
        // A disabled back-off (no tick) never halves.
        let mut off = BackOff::new(0);
        off.observe(t0, multiview_engine::lateness::LatenessSnapshot::default());
        for _ in 0..100 {
            lateness.record(60_000_000, 0);
        }
        assert!(!off.observe(t0 + Duration::from_secs(11), lateness.snapshot()));
    }
}
