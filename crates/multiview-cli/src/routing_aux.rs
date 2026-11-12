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
//! A panic in a tick body, or any other exit, raises `failed`; the consumer
//! then moves every output on an aux bus back to Program (§4.4).

use std::borrow::Cow;
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::thread::JoinHandle;

use multiview_compositor::pipeline::Nv12Image;
use multiview_config::{OutputBus, VideoFeed};
use multiview_control::preview::WallCell;
use multiview_core::color::ColorInfo;
use multiview_engine::AuxTick;
use multiview_framestore::{SwapCell, TileStore};

use crate::nv12_blit::blit_cell;

/// Reusable letterbox buffers per feed (§2.4). A buffer still held by an
/// encoder is not reused; with none free the bus holds for that tick.
pub const AUX_RING_BUFFERS: usize = 4;

/// The picture an output's encoder reads (slice 1 one-cell buses).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PictureBus {
    /// The Program canvas (the base encoders; not composed here).
    Program,
    /// The M/E's PVW source as a one-cell canvas.
    Preview,
    /// One source as a one-cell canvas.
    Source(String),
}

impl PictureBus {
    /// The picture-bus name, matching [`VideoFeed::picture_bus`]:
    /// `program`, `preview` or `src:<id>`.
    #[must_use]
    pub fn as_str(&self) -> Cow<'_, str> {
        match self {
            Self::Program => Cow::Borrowed("program"),
            Self::Preview => Cow::Borrowed("preview"),
            Self::Source(id) => Cow::Owned(format!("src:{id}")),
        }
    }
}

/// A feed slice 1 cannot route (Clean or a layout).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnroutableFeed;

impl TryFrom<&VideoFeed> for PictureBus {
    type Error = UnroutableFeed;

    fn try_from(feed: &VideoFeed) -> Result<Self, Self::Error> {
        match feed {
            VideoFeed::Bus(OutputBus::Program) => Ok(Self::Program),
            VideoFeed::Bus(OutputBus::Preview) => Ok(Self::Preview),
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

/// One active routed feed the aux thread composes.
#[derive(Debug, Clone)]
pub struct AuxFeed {
    /// The bus (Program entries are ignored: Program is not composed here).
    pub bus: PictureBus,
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
        let key: Arc<str> = Arc::from(bus.as_str().as_ref());
        Self {
            bus,
            key,
            slot: Arc::new(PictureBusSlot::default()),
        }
    }
}

/// The active feed set, replaced whole by the consumer when it changes.
pub type AuxFeeds = Arc<SwapCell<Arc<Vec<AuxFeed>>>>;

/// Resolve a source id to its store. Must see sources added at runtime and must
/// never take a lock the clock holds (the pipeline passes a closure over the
/// catalogue `SwapCell`).
pub type StoreLookup = Arc<dyn Fn(&str) -> Option<Arc<TileStore<Nv12Image>>> + Send + Sync>;

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
}

/// Spawn the `routing-aux` thread.
///
/// # Errors
/// Returns the OS error if the thread cannot be spawned.
pub fn spawn_routing_aux(inputs: AuxInputs) -> std::io::Result<RoutingAuxHandle> {
    let failed = Arc::new(AtomicBool::new(false));
    let thread_failed = Arc::clone(&failed);
    let join = std::thread::Builder::new()
        .name("routing-aux".to_owned())
        .spawn(move || {
            let _guard = FailGuard(Arc::clone(&thread_failed));
            lower_priority();
            run(&inputs, &thread_failed);
        })?;
    Ok(RoutingAuxHandle {
        failed,
        join: Some(join),
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

fn run(inputs: &AuxInputs, failed: &AtomicBool) {
    let mut state = AuxState::default();
    while let Ok(tick) = inputs.rx.recv() {
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
}

#[derive(Default)]
struct FeedState {
    source: Option<Arc<str>>,
    generation: u64,
    ring: BlitRing,
    warned_color: bool,
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
                PictureBus::Program => continue,
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
            let Some(picture) = entry.compose(tile, inputs.canvas) else {
                continue;
            };
            feed.slot.latest.store(Some(Arc::new(AuxFrame {
                tick: tick.index,
                generation: entry.generation,
                frame: picture,
            })));
            feed.slot.composed.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl FeedState {
    fn compose(&mut self, tile: &Arc<Nv12Image>, canvas: AuxCanvas) -> Option<Arc<Nv12Image>> {
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
        if is_identity(tile, canvas) {
            return Some(Arc::clone(tile));
        }
        self.ring.letterbox(tile, canvas)
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
        for feed in [
            VideoFeed::PROGRAM,
            VideoFeed::Bus(OutputBus::Preview),
            VideoFeed::Source("cam-1".to_owned()),
        ] {
            let bus = PictureBus::try_from(&feed).unwrap();
            assert_eq!(bus.as_str(), feed.picture_bus());
        }
        assert!(PictureBus::try_from(&VideoFeed::Bus(OutputBus::Clean)).is_err());
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
        })
        .unwrap();
        (tx, handle)
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
}
