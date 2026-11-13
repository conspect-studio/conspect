//! Decode size follows the **active** layout (inv #6: sources are decoded near
//! display size).
//!
//! The control drain publishes the live layout into a [`DisplayLayoutSlot`] at
//! the frame boundary: one wait-free store, only when the layout changed. Every
//! decode thread polls that slot between packets and runs the rest of the
//! machinery here on its own thread, so nothing new reaches the output clock
//! (invariants #1/#10):
//!
//! * [`display_size`] is the size a source wants under a layout;
//! * [`SettleGate`] debounces a burst of layout edits;
//! * [`plan_display_change`] picks retarget-the-scaler or reopen-the-decoder;
//! * [`DisplayDriver`] is the per-decoder state machine over all of the above;
//! * [`Handover`] is the make-before-break reopen: `*_cuvid` fixes its resize at
//!   open, so a replacement decoder opens on a helper thread, warms up on the
//!   same packets from the next keyframe, and takes over at a frame boundary
//!   while the old decoder keeps publishing. The tile is never blanked, and any
//!   failure leaves the old decoder running.
//!
//! This module is feature-independent so the decisions are tested with fake
//! decoders in every build; the libav wiring lives in `pipeline`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use multiview_compositor::pipeline::Nv12Image;
use multiview_core::layout::{Cell, Layout};
use multiview_engine::{RequestedSize, SourceKey, SourceRegistry};
use multiview_ffmpeg::{nvdec_resize_target, TileSize};
use multiview_framestore::SwapCell;

/// The decode size of a source no cell binds. Such a source only feeds the
/// source previews, which are monitoring-sized, so it must not cost a
/// full-canvas decode, download and scale.
pub const UNBOUND_DISPLAY_SIZE: (u32, u32) = (640, 360);

/// How long a new size must hold before a decoder acts on it, so a burst of
/// layout edits costs one reopen rather than one per edit.
pub const DISPLAY_SETTLE: Duration = Duration::from_millis(1500);

/// The minimum spacing between two size changes on one decoder.
pub const DISPLAY_MIN_INTERVAL: Duration = Duration::from_secs(5);

/// How long a handover may take end to end (open, wait for a keyframe, first
/// frame) before it is abandoned. Comfortably longer than a live GOP.
pub const HANDOVER_DEADLINE: Duration = Duration::from_secs(20);

/// The most warm-up frames a handover holds. These are tile-sized and only
/// accumulate while the old decoder catches up to the new one's first frame.
pub const HANDOVER_MAX_FRAMES: usize = 32;

/// How many handovers may run at once process-wide. Each one briefly holds a
/// second NVDEC session, so a layout change across many sources is staggered
/// rather than doubling sessions and VRAM all at once.
pub const MAX_CONCURRENT_HANDOVERS: usize = 2;

/// The live layout, written at the frame boundary, read by the decode threads.
pub type DisplayLayoutSlot = Arc<SwapCell<Arc<Layout>>>;

/// Aux sizing (ADR-0108 §2.4): the cells of every routed layout composing
/// now, on the running canvas. Written by the output router when its set of
/// layout canvases changes, read by the decode threads. A source a routed
/// cell binds decodes at the per-axis supremum over Program's cells AND
/// these, so a routed layout's cells are drawn at their own size.
pub type AuxSizingSlot = Arc<SwapCell<Arc<Vec<Cell>>>>;

/// An empty [`AuxSizingSlot`].
#[must_use]
pub fn aux_sizing_slot() -> AuxSizingSlot {
    Arc::new(SwapCell::from_pointee(Vec::new()))
}

/// `layout` plus the routed `extra` cells (the sizing layout of §2.4).
/// `layout` itself when there are none (no allocation).
#[must_use]
pub fn sizing_layout(layout: &Arc<Layout>, extra: &[Cell]) -> Arc<Layout> {
    if extra.is_empty() {
        return Arc::clone(layout);
    }
    let mut sizing = Layout::clone(layout);
    sizing.cells.extend_from_slice(extra);
    Arc::new(sizing)
}

/// The decode size for `source_id`: the **per-axis supremum** of the pixel sizes
/// of every cell that binds it, or [`None`] if no cell binds it.
///
/// A source can tile into several cells of different sizes (e.g. a big PGM cell
/// plus a small PIP). Decoding once at the per-axis max satisfies the largest
/// consumer on each axis (ADR-0030 §3); each cell then scales down at composite.
/// Taking the FIRST binding cell would under-decode every larger tile bound to
/// the same source.
#[must_use]
pub fn cell_pixel_size(layout: &Layout, source_id: &str) -> Option<(u32, u32)> {
    layout
        .cells
        .iter()
        .filter(|c| c.source.as_deref() == Some(source_id))
        .map(|c| cell_dims(c, layout.canvas.width, layout.canvas.height))
        .reduce(|(aw, ah), (bw, bh)| (aw.max(bw), ah.max(bh)))
}

/// The size a decode following the layout should produce for `source_id`: its
/// [`cell_pixel_size`], or [`UNBOUND_DISPLAY_SIZE`] (never larger than the
/// canvas, and even for NV12) when no cell binds it.
#[must_use]
pub fn display_size(layout: &Layout, source_id: &str) -> (u32, u32) {
    cell_pixel_size(layout, source_id).unwrap_or_else(|| {
        let (w, h) = UNBOUND_DISPLAY_SIZE;
        (
            (w.min(layout.canvas.width) & !1).max(2),
            (h.min(layout.canvas.height) & !1).max(2),
        )
    })
}

/// The size of a producer that cannot resize after it starts: its
/// [`cell_pixel_size`], or the full canvas so it stays sharp wherever it is
/// routed later.
#[must_use]
pub fn fixed_size(layout: &Layout, source_id: &str) -> (u32, u32) {
    cell_pixel_size(layout, source_id).unwrap_or((layout.canvas.width, layout.canvas.height))
}

/// Convert a cell's normalized `w`/`h` into even pixel dimensions (NV12 needs
/// even extents), clamped to at least 2x2.
#[must_use]
pub fn cell_dims(cell: &Cell, canvas_w: u32, canvas_h: u32) -> (u32, u32) {
    let to_even_px = |frac: f32, extent: u32| -> u32 {
        let raw = frac_to_px(frac, extent);
        let even = raw & !1; // round down to even
        even.max(2)
    };
    (to_even_px(cell.w, canvas_w), to_even_px(cell.h, canvas_h))
}

/// Convert a normalized fraction in `[0,1]` to a pixel count in `[0, extent]`,
/// `as`-cast-free (the guardrails deny `as_conversions`).
#[must_use]
pub fn frac_to_px(frac: f32, extent: u32) -> u32 {
    if !frac.is_finite() || frac <= 0.0 {
        return 0;
    }
    let target = f64::from(frac) * f64::from(extent);
    if target >= f64::from(extent) {
        return extent;
    }
    // Largest u32 candidate whose lossless f64 widening is <= target.
    let mut lo: u32 = 0;
    let mut hi: u32 = extent;
    while lo < hi {
        let mid = lo.saturating_add((hi - lo).saturating_add(1) / 2);
        if f64::from(mid) <= target {
            lo = mid;
        } else {
            hi = mid.saturating_sub(1);
        }
    }
    lo
}

/// A source's link to the live layout: where a decode reads its wanted size and
/// where the adopted size is recorded.
#[derive(Clone)]
pub struct DisplayFollow {
    layout: DisplayLayoutSlot,
    aux: Option<AuxSizingSlot>,
    registry: Option<Arc<SourceRegistry<Nv12Image>>>,
    #[cfg(test)]
    rehearsal: Option<Arc<Rehearsal>>,
}

/// Test seam: run the reopen path on a GPU-free box with software decoders.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct Rehearsal {
    /// Replacement decoders opened.
    pub opens: AtomicUsize,
    /// Cutovers the decode loop performed.
    pub cutovers: AtomicUsize,
}

impl DisplayFollow {
    /// Follow `layout`, recording adopted sizes in `registry` when given.
    #[must_use]
    pub fn new(
        layout: DisplayLayoutSlot,
        registry: Option<Arc<SourceRegistry<Nv12Image>>>,
    ) -> Self {
        Self {
            layout,
            aux: None,
            registry,
            #[cfg(test)]
            rehearsal: None,
        }
    }

    /// Also size for the routed layouts in `aux` (§2.4 aux sizing).
    #[must_use]
    pub fn with_aux_sizing(mut self, aux: AuxSizingSlot) -> Self {
        self.aux = Some(aux);
        self
    }

    /// The routed cells now (empty without aux sizing).
    fn aux_cells(&self) -> Arc<Vec<Cell>> {
        static NONE: std::sync::OnceLock<Arc<Vec<Cell>>> = std::sync::OnceLock::new();
        self.aux.as_ref().map_or_else(
            || Arc::clone(NONE.get_or_init(|| Arc::new(Vec::new()))),
            |slot| slot.load(),
        )
    }

    /// The layout every size decision uses: the live layout plus the routed
    /// cells.
    fn sizing(&self) -> Arc<Layout> {
        sizing_layout(&self.layout.load(), &self.aux_cells())
    }

    /// Treat this decode as NVDEC, with software replacements, counting into
    /// `rehearsal`.
    #[cfg(test)]
    #[must_use]
    pub fn rehearsing(mut self, rehearsal: Arc<Rehearsal>) -> Self {
        self.rehearsal = Some(rehearsal);
        self
    }

    /// Whether this decode is a test rehearsal of the reopen path. Always false
    /// outside tests.
    #[must_use]
    pub fn rehearses_reopen(&self) -> bool {
        #[cfg(test)]
        {
            self.rehearsal.is_some()
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    /// Count a replacement open in a rehearsal.
    pub fn note_open(&self) {
        #[cfg(test)]
        if let Some(rehearsal) = &self.rehearsal {
            rehearsal.opens.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// Count a cutover in a rehearsal.
    pub fn note_cutover(&self) {
        #[cfg(test)]
        if let Some(rehearsal) = &self.rehearsal {
            rehearsal.cutovers.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// The size `source_id` wants under the live layout.
    #[must_use]
    pub fn size_for(&self, source_id: &str) -> (u32, u32) {
        display_size(&self.sizing(), source_id)
    }

    /// [`fixed_size`] under the live layout.
    #[must_use]
    pub fn fixed_size_for(&self, source_id: &str) -> (u32, u32) {
        fixed_size(&self.sizing(), source_id)
    }

    /// The height to pick an HLS variant for. An unbound source keeps the canvas
    /// height: the variant is fixed for the whole connection, so pinning a low
    /// rung for the small unbound decode would leave the source soft when it is
    /// later routed to a large tile. NVDEC shrinks the high rung for free.
    #[must_use]
    pub fn variant_height(&self, source_id: &str) -> u32 {
        let layout = self.sizing();
        cell_pixel_size(&layout, source_id).map_or(layout.canvas.height, |(_, h)| h)
    }

    /// Record the size `source_id` now decodes at. Decode thread only.
    pub fn record(&self, source_id: &str, (width, height): (u32, u32)) {
        if let Some(registry) = &self.registry {
            let _known = registry.set_requested(
                &SourceKey::from_canonical(source_id),
                RequestedSize { width, height },
            );
        }
    }
}

/// Debounce for one decoder: reports a wanted size only once it has held for
/// [`DISPLAY_SETTLE`] and [`DISPLAY_MIN_INTERVAL`] has passed since the last
/// adoption. A due size keeps being reported until [`adopted`](Self::adopted)
/// is called, so a caller that could not act yet simply tries again.
#[derive(Debug)]
pub struct SettleGate {
    applied: (u32, u32),
    candidate: Option<((u32, u32), Instant)>,
    earliest: Option<Instant>,
}

impl SettleGate {
    /// A gate for a decoder currently producing `applied`.
    #[must_use]
    pub fn new(applied: (u32, u32)) -> Self {
        Self {
            applied,
            candidate: None,
            earliest: None,
        }
    }

    /// Feed the currently wanted size; `Some` when it is time to act on it.
    pub fn observe(&mut self, desired: (u32, u32), now: Instant) -> Option<(u32, u32)> {
        if desired == self.applied {
            self.candidate = None;
            return None;
        }
        let since = match self.candidate {
            Some((size, since)) if size == desired => since,
            _ => {
                self.candidate = Some((desired, now));
                now
            }
        };
        let settled = now.saturating_duration_since(since) >= DISPLAY_SETTLE;
        let spaced = self.earliest.is_none_or(|earliest| now >= earliest);
        (settled && spaced).then_some(desired)
    }

    /// The decoder now produces `size`.
    pub fn adopted(&mut self, size: (u32, u32), now: Instant) {
        self.applied = size;
        self.candidate = None;
        self.earliest = Some(now + DISPLAY_MIN_INTERVAL);
    }
}

/// What a running decoder is producing, for [`plan_display_change`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecoderState {
    /// The stream's coded size.
    pub source: TileSize,
    /// Whether the decoder is NVDEC. Software decode has no resize to reopen.
    pub hardware: bool,
    /// The fused resize the decoder was opened with.
    pub resize: Option<TileSize>,
}

/// How to reach a new display size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayChange {
    /// Already there.
    None,
    /// The decoder's output is unaffected: only the host scaler changes.
    Retarget,
    /// The fused resize must change, which needs a new decoder.
    Reopen {
        /// The resize the replacement decoder must open with.
        resize: Option<TileSize>,
    },
}

/// Decide how a decoder in `state`, scaling to `tile`, reaches `desired`.
/// `resize_env` is the value of the resize opt-out variable.
#[must_use]
pub fn plan_display_change(
    state: DecoderState,
    tile: (u32, u32),
    desired: (u32, u32),
    resize_env: Option<&str>,
) -> DisplayChange {
    if state.hardware {
        let wanted = nvdec_resize_target(
            state.source,
            TileSize::new(desired.0, desired.1),
            resize_env,
        );
        if wanted != state.resize {
            return DisplayChange::Reopen { resize: wanted };
        }
    }
    if tile == desired {
        DisplayChange::None
    } else {
        DisplayChange::Retarget
    }
}

/// One decode thread's view of the live layout: polls the slot and debounces.
pub struct DisplayFollower {
    follow: DisplayFollow,
    id: String,
    seen: Arc<Layout>,
    seen_aux: Arc<Vec<Cell>>,
    desired: (u32, u32),
    gate: SettleGate,
}

impl DisplayFollower {
    /// Follow for source `id`, whose decode was opened producing `applied`.
    #[must_use]
    pub fn new(follow: DisplayFollow, id: &str, applied: (u32, u32)) -> Self {
        let seen = follow.layout.load();
        let seen_aux = follow.aux_cells();
        let desired = display_size(&sizing_layout(&seen, &seen_aux), id);
        Self {
            follow,
            id: id.to_owned(),
            seen,
            seen_aux,
            desired,
            gate: SettleGate::new(applied),
        }
    }

    /// The settled size to move to, if any. A lock-free load and a pointer
    /// compare unless the layout changed, so this is cheap per packet.
    pub fn due(&mut self, now: Instant) -> Option<(u32, u32)> {
        let live = self.follow.layout.load();
        let live_aux = self.follow.aux_cells();
        if !Arc::ptr_eq(&live, &self.seen) || !Arc::ptr_eq(&live_aux, &self.seen_aux) {
            self.desired = display_size(&sizing_layout(&live, &live_aux), &self.id);
            self.seen = live;
            self.seen_aux = live_aux;
        }
        self.gate.observe(self.desired, now)
    }

    /// The decode now produces `size`; records it in the registry.
    pub fn adopted(&mut self, size: (u32, u32), now: Instant) {
        self.gate.adopted(size, now);
        self.follow.record(&self.id, size);
    }
}

static HANDOVERS_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// One of the [`MAX_CONCURRENT_HANDOVERS`] slots, returned on drop.
#[derive(Debug)]
pub struct HandoverPermit(());

impl HandoverPermit {
    /// Take a slot, or `None` when all are in use (try again on a later poll).
    #[must_use]
    pub fn try_acquire() -> Option<Self> {
        let mut held = HANDOVERS_IN_FLIGHT.load(Ordering::Acquire);
        while held < MAX_CONCURRENT_HANDOVERS {
            match HANDOVERS_IN_FLIGHT.compare_exchange_weak(
                held,
                held + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(Self(())),
                Err(now) => held = now,
            }
        }
        None
    }
}

impl Drop for HandoverPermit {
    fn drop(&mut self) {
        HANDOVERS_IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The decoder surface a [`Handover`] drives.
pub trait HandoverDecoder {
    /// A demuxed packet.
    type Packet;
    /// A decoded frame.
    type Frame;
    /// Send one packet.
    ///
    /// # Errors
    /// A decode failure; it abandons the handover.
    fn send(&mut self, packet: &Self::Packet) -> Result<(), String>;
    /// Pull one decoded frame, or `None` when the decoder needs more input.
    ///
    /// # Errors
    /// A decode failure; it abandons the handover.
    fn receive(&mut self) -> Result<Option<Self::Frame>, String>;
    /// Whether a decoder can start cleanly at `packet`.
    fn is_key(packet: &Self::Packet) -> bool;
    /// `packet`'s presentation time in stream ticks.
    fn packet_pts(packet: &Self::Packet) -> Option<i64>;
    /// `frame`'s presentation time in stream ticks.
    fn frame_pts(frame: &Self::Frame) -> Option<i64>;
}

/// The result of polling a [`Handover`].
pub enum HandoverPoll<D: HandoverDecoder> {
    /// Keep decoding on the old decoder.
    Pending,
    /// Switch to `decoder` now.
    Ready {
        /// The warmed-up replacement.
        decoder: D,
        /// Warm-up frames to publish first, all later than the old decoder's
        /// last frame.
        frames: VecDeque<D::Frame>,
        /// Set when the new decoder is behind the old one: drop its frames up
        /// to and including this time so the picture never steps backwards.
        skip_through: Option<i64>,
    },
    /// The handover failed. The old decoder stays.
    Abandoned(String),
}

/// A make-before-break decoder replacement. See the module docs.
///
/// Bounded: one helper thread, at most [`HANDOVER_MAX_FRAMES`] frames, and
/// [`HANDOVER_DEADLINE`]. Dropping it mid-flight is safe: a still-running open
/// finds its channel closed and drops the decoder it made, and only then gives
/// back its permit, so a wedged open cannot push the session count up.
pub struct Handover<D: HandoverDecoder> {
    opening: Option<Receiver<Result<D, String>>>,
    decoder: Option<D>,
    anchor: Option<i64>,
    emitted: bool,
    frames: VecDeque<D::Frame>,
    deadline: Instant,
    failure: Option<String>,
    _permit: Arc<HandoverPermit>,
}

impl<D> Handover<D>
where
    D: HandoverDecoder + Send + 'static,
{
    /// Start opening the replacement on a helper thread.
    ///
    /// # Errors
    /// The helper thread could not be spawned.
    pub fn spawn<F>(
        thread_name: String,
        permit: HandoverPermit,
        now: Instant,
        open: F,
    ) -> Result<Self, String>
    where
        F: FnOnce() -> Result<D, String> + Send + 'static,
    {
        let permit = Arc::new(permit);
        let held = Arc::clone(&permit);
        let (tx, rx) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                let _permit = held;
                let _closed = tx.send(open());
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            opening: Some(rx),
            decoder: None,
            anchor: None,
            emitted: false,
            frames: VecDeque::new(),
            deadline: now + HANDOVER_DEADLINE,
            failure: None,
            _permit: permit,
        })
    }
}

impl<D: HandoverDecoder> Handover<D> {
    /// Whether the replacement has finished opening.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.decoder.is_some()
    }

    /// Offer the packet just sent to the old decoder. The replacement starts at
    /// the first keyframe after it opened and then sees every packet.
    pub fn on_packet(&mut self, packet: &D::Packet) {
        if self.failure.is_some() {
            return;
        }
        let Some(decoder) = self.decoder.as_mut() else {
            return;
        };
        let anchor = if let Some(anchor) = self.anchor {
            anchor
        } else {
            let Some(pts) = D::is_key(packet).then(|| D::packet_pts(packet)).flatten() else {
                return;
            };
            self.anchor = Some(pts);
            pts
        };
        if let Err(e) = decoder.send(packet) {
            self.failure = Some(e);
            return;
        }
        loop {
            match decoder.receive() {
                Ok(Some(frame)) => {
                    let Some(pts) = D::frame_pts(&frame) else {
                        self.failure = Some("replacement frame has no timestamp".to_owned());
                        return;
                    };
                    // Open-GOP leading pictures sit before the keyframe and
                    // reference frames this decoder never saw.
                    if pts < anchor {
                        continue;
                    }
                    self.emitted = true;
                    if self.frames.len() >= HANDOVER_MAX_FRAMES {
                        self.failure = Some("old decoder never reached the keyframe".to_owned());
                        return;
                    }
                    self.frames.push_back(frame);
                }
                Ok(None) => return,
                Err(e) => {
                    self.failure = Some(e);
                    return;
                }
            }
        }
    }

    /// Advance the handover. `last_old_pts` is the newest frame the old decoder
    /// has handed over for publishing.
    pub fn poll(&mut self, last_old_pts: Option<i64>, now: Instant) -> HandoverPoll<D> {
        if let Some(opening) = &self.opening {
            match opening.try_recv() {
                Ok(Ok(decoder)) => {
                    self.decoder = Some(decoder);
                    self.opening = None;
                }
                Ok(Err(e)) => return HandoverPoll::Abandoned(e),
                Err(TryRecvError::Disconnected) => {
                    return HandoverPoll::Abandoned("decoder open thread died".to_owned());
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        if let Some(failure) = self.failure.take() {
            return HandoverPoll::Abandoned(failure);
        }
        if now >= self.deadline {
            return HandoverPoll::Abandoned("timed out before the first frame".to_owned());
        }
        let (Some(anchor), Some(last)) = (self.anchor, last_old_pts) else {
            return HandoverPoll::Pending;
        };
        if !self.emitted || last < anchor {
            return HandoverPoll::Pending;
        }
        while self
            .frames
            .front()
            .and_then(D::frame_pts)
            .is_some_and(|pts| pts <= last)
        {
            self.frames.pop_front();
        }
        let Some(decoder) = self.decoder.take() else {
            return HandoverPoll::Pending;
        };
        let frames = std::mem::take(&mut self.frames);
        let skip_through = frames.is_empty().then_some(last);
        HandoverPoll::Ready {
            decoder,
            frames,
            skip_through,
        }
    }
}

/// Drops the frames a replacement decoder repeats after a cutover.
///
/// Bounded by [`HANDOVER_MAX_FRAMES`]: timestamps are not trustworthy across a
/// discontinuity (an HLS ad break, a 33-bit wrap), and without the bound a
/// backward jump would match every later frame and freeze the tile.
#[derive(Debug, Default)]
pub struct SkipThrough {
    limit: Option<i64>,
    dropped: usize,
}

impl SkipThrough {
    /// Start dropping frames at or before `limit`, if any.
    pub fn arm(&mut self, limit: Option<i64>) {
        self.limit = limit;
        self.dropped = 0;
    }

    /// Whether the frame at `pts` should be dropped. The first frame that is
    /// kept ends the skip.
    pub fn drops(&mut self, pts: Option<i64>) -> bool {
        let (Some(limit), Some(pts)) = (self.limit, pts) else {
            self.limit = None;
            return false;
        };
        if pts > limit || self.dropped >= HANDOVER_MAX_FRAMES {
            self.limit = None;
            return false;
        }
        self.dropped += 1;
        true
    }
}

/// What the decode loop must do after [`DisplayDriver::poll`].
pub enum DisplayStep<D: HandoverDecoder> {
    /// Nothing changes.
    Idle,
    /// Scale to this size from the next frame. The decoder stays.
    Retarget((u32, u32)),
    /// Replace the decoder and scale to `tile`.
    Cutover {
        /// The warmed-up replacement.
        decoder: D,
        /// Frames to publish before pulling from `decoder`.
        frames: VecDeque<D::Frame>,
        /// Drop decoded frames up to and including this time.
        skip_through: Option<i64>,
        /// The new scale target.
        tile: (u32, u32),
    },
}

/// A reopen in flight, with the state the decoder will have once it lands.
struct Reopening<D: HandoverDecoder> {
    handover: Handover<D>,
    tile: (u32, u32),
    resize: Option<TileSize>,
}

/// The follow-the-layout state machine for one decoder, driven from the decode
/// thread between packets.
pub struct DisplayDriver<D: HandoverDecoder> {
    follower: DisplayFollower,
    id: String,
    tile: (u32, u32),
    state: DecoderState,
    resize_env: Option<String>,
    reopening: Option<Reopening<D>>,
}

impl<D> DisplayDriver<D>
where
    D: HandoverDecoder + Send + 'static,
{
    /// Drive source `id`, whose decoder is in `state` and scales to `tile`.
    /// `resize_env` is the value of the resize opt-out variable.
    #[must_use]
    pub fn new(
        follow: DisplayFollow,
        id: &str,
        tile: (u32, u32),
        state: DecoderState,
        resize_env: Option<String>,
    ) -> Self {
        follow.record(id, tile);
        Self {
            follower: DisplayFollower::new(follow, id, tile),
            id: id.to_owned(),
            tile,
            state,
            resize_env,
            reopening: None,
        }
    }

    /// Pass the packet just sent to the running decoder on to a replacement.
    pub fn on_packet(&mut self, packet: &D::Packet) {
        if let Some(reopening) = self.reopening.as_mut() {
            reopening.handover.on_packet(packet);
        }
    }

    /// Advance. `last_pts` is the newest frame taken from the running decoder;
    /// `make_open` builds the blocking open for a resize, run on a helper thread.
    pub fn poll<F>(
        &mut self,
        last_pts: Option<i64>,
        now: Instant,
        make_open: impl FnOnce(Option<TileSize>) -> F,
    ) -> DisplayStep<D>
    where
        F: FnOnce() -> Result<D, String> + Send + 'static,
    {
        if self.reopening.is_some() {
            return self.poll_reopening(last_pts, now);
        }
        let Some(desired) = self.follower.due(now) else {
            return DisplayStep::Idle;
        };
        match plan_display_change(self.state, self.tile, desired, self.resize_env.as_deref()) {
            DisplayChange::None => {
                self.follower.adopted(desired, now);
                DisplayStep::Idle
            }
            DisplayChange::Retarget => self.retarget(desired, now),
            DisplayChange::Reopen { resize } => {
                // No slot free: the gate offers this size again on a later poll.
                let Some(permit) = HandoverPermit::try_acquire() else {
                    return DisplayStep::Idle;
                };
                let name = format!("multiview-reopen-{}", self.id);
                match Handover::spawn(name, permit, now, make_open(resize)) {
                    Ok(handover) => {
                        self.reopening = Some(Reopening {
                            handover,
                            tile: desired,
                            resize,
                        });
                        DisplayStep::Idle
                    }
                    Err(reason) => self.abandon(desired, &reason, now),
                }
            }
        }
    }

    fn poll_reopening(&mut self, last_pts: Option<i64>, now: Instant) -> DisplayStep<D> {
        let Some(reopening) = self.reopening.as_mut() else {
            return DisplayStep::Idle;
        };
        let tile = reopening.tile;
        let resize = reopening.resize;
        match reopening.handover.poll(last_pts, now) {
            HandoverPoll::Pending => DisplayStep::Idle,
            HandoverPoll::Abandoned(reason) => {
                self.reopening = None;
                self.abandon(tile, &reason, now)
            }
            HandoverPoll::Ready {
                decoder,
                frames,
                skip_through,
            } => {
                self.reopening = None;
                let ((from_w, from_h), (to_w, to_h)) = (self.tile, tile);
                let id = &self.id;
                tracing::info!(
                    "nvdec display size changed source={id} from={from_w}x{from_h} to={to_w}x{to_h}"
                );
                self.tile = tile;
                self.state.resize = resize;
                self.follower.adopted(tile, now);
                DisplayStep::Cutover {
                    decoder,
                    frames,
                    skip_through,
                    tile,
                }
            }
        }
    }

    /// The reopen failed: the old decoder stays, and the host scaler still
    /// brings the picture to the tile size. A later change or reconnect plans
    /// from the real decoder state.
    fn abandon(&mut self, desired: (u32, u32), reason: &str, now: Instant) -> DisplayStep<D> {
        tracing::warn!(
            source = %self.id,
            %reason,
            "decoder reopen abandoned; keeping the running decoder and scaling on the host"
        );
        self.retarget(desired, now)
    }

    fn retarget(&mut self, desired: (u32, u32), now: Instant) -> DisplayStep<D> {
        let ((from_w, from_h), (to_w, to_h)) = (self.tile, desired);
        let id = &self.id;
        tracing::info!(
            "host scale size changed source={id} from={from_w}x{from_h} to={to_w}x{to_h}"
        );
        self.tile = desired;
        self.follower.adopted(desired, now);
        DisplayStep::Retarget(desired)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use multiview_core::layout::Canvas;

    fn cell(source: Option<&str>, w: f32, h: f32) -> Cell {
        Cell {
            w,
            h,
            source: source.map(str::to_owned),
            ..Cell::default()
        }
    }

    fn layout(width: u32, height: u32, cells: Vec<Cell>) -> Layout {
        Layout {
            name: "t".to_owned(),
            canvas: Canvas {
                width,
                height,
                fps_num: 25,
                fps_den: 1,
            },
            cells,
        }
    }

    #[test]
    fn a_bound_source_takes_its_cell_size() {
        let l = layout(1920, 1080, vec![cell(Some("a"), 1.0 / 3.0, 0.5)]);
        assert_eq!(display_size(&l, "a"), (640, 540));
    }

    #[test]
    fn an_unbound_source_never_decodes_at_full_canvas() {
        let l = layout(1920, 1080, vec![cell(Some("a"), 1.0, 1.0)]);
        assert_eq!(cell_pixel_size(&l, "b"), None);
        assert_eq!(display_size(&l, "b"), UNBOUND_DISPLAY_SIZE);
    }

    #[test]
    fn the_unbound_size_is_capped_by_a_small_canvas_and_stays_even() {
        let l = layout(321, 201, vec![]);
        assert_eq!(display_size(&l, "b"), (320, 200));
    }

    #[test]
    fn several_cells_give_the_per_axis_supremum() {
        let l = layout(
            1200,
            720,
            vec![cell(Some("a"), 0.25, 0.5), cell(Some("a"), 0.5, 0.25)],
        );
        assert_eq!(display_size(&l, "a"), (600, 360));
    }

    #[test]
    fn odd_cell_sizes_round_down_to_even() {
        let l = layout(1000, 1000, vec![cell(Some("a"), 0.333, 0.111)]);
        assert_eq!(display_size(&l, "a"), (332, 110));
    }

    #[test]
    fn a_vanishing_cell_is_clamped_to_the_nv12_minimum() {
        let l = layout(1000, 1000, vec![cell(Some("a"), 0.0, 0.0)]);
        assert_eq!(display_size(&l, "a"), (2, 2));
    }

    #[test]
    fn an_unbound_source_pins_its_variant_for_the_canvas_not_the_small_decode() {
        let l = layout(1920, 1080, vec![cell(Some("a"), 0.5, 0.5)]);
        let follow = DisplayFollow::new(Arc::new(SwapCell::new(Arc::new(l))), None);
        assert_eq!(follow.variant_height("a"), 540);
        assert_eq!(follow.variant_height("unbound"), 1080);
    }

    #[test]
    fn the_gate_waits_for_a_burst_to_settle_then_acts_once() {
        let t0 = Instant::now();
        let mut gate = SettleGate::new((1920, 1080));
        assert_eq!(gate.observe((640, 360), t0), None);
        // A second edit inside the window restarts the wait.
        let t1 = t0 + DISPLAY_SETTLE / 2;
        assert_eq!(gate.observe((480, 270), t1), None);
        assert_eq!(gate.observe((480, 270), t0 + DISPLAY_SETTLE), None);
        let t2 = t1 + DISPLAY_SETTLE;
        assert_eq!(gate.observe((480, 270), t2), Some((480, 270)));
        gate.adopted((480, 270), t2);
        assert_eq!(gate.observe((480, 270), t2 + DISPLAY_SETTLE), None);
    }

    #[test]
    fn an_edit_undone_inside_the_window_costs_nothing() {
        let t0 = Instant::now();
        let mut gate = SettleGate::new((640, 360));
        assert_eq!(gate.observe((1280, 720), t0), None);
        assert_eq!(gate.observe((640, 360), t0 + DISPLAY_SETTLE / 2), None);
        assert_eq!(gate.observe((640, 360), t0 + DISPLAY_SETTLE * 4), None);
    }

    #[test]
    fn the_gate_spaces_changes_and_keeps_offering_until_adopted() {
        let t0 = Instant::now();
        let mut gate = SettleGate::new((640, 360));
        gate.adopted((640, 360), t0);
        assert_eq!(gate.observe((320, 180), t0), None);
        assert_eq!(gate.observe((320, 180), t0 + DISPLAY_SETTLE), None);
        let due = t0 + DISPLAY_MIN_INTERVAL;
        assert_eq!(gate.observe((320, 180), due), Some((320, 180)));
        assert_eq!(
            gate.observe((320, 180), due + Duration::from_millis(1)),
            Some((320, 180)),
            "a caller refused a permit must be offered the size again"
        );
    }

    fn hw(resize: Option<TileSize>) -> DecoderState {
        DecoderState {
            source: TileSize::new(1920, 1080),
            hardware: true,
            resize,
        }
    }

    #[test]
    fn nvdec_reopens_when_the_resize_target_moves() {
        assert_eq!(
            plan_display_change(hw(None), (1920, 1080), (640, 360), None),
            DisplayChange::Reopen {
                resize: Some(TileSize::new(640, 360))
            }
        );
    }

    #[test]
    fn growing_past_the_source_reopens_without_a_resize() {
        assert_eq!(
            plan_display_change(
                hw(Some(TileSize::new(640, 360))),
                (640, 360),
                (3840, 2160),
                None
            ),
            DisplayChange::Reopen { resize: None }
        );
    }

    #[test]
    fn software_decode_only_retargets_the_scaler() {
        let state = DecoderState {
            hardware: false,
            ..hw(None)
        };
        assert_eq!(
            plan_display_change(state, (1920, 1080), (640, 360), None),
            DisplayChange::Retarget
        );
    }

    #[test]
    fn the_resize_opt_out_only_retargets_the_scaler() {
        assert_eq!(
            plan_display_change(hw(None), (1920, 1080), (640, 360), Some("1")),
            DisplayChange::Retarget
        );
    }

    #[test]
    fn an_unchanged_size_needs_nothing() {
        let state = hw(Some(TileSize::new(640, 360)));
        assert_eq!(
            plan_display_change(state, (640, 360), (640, 360), None),
            DisplayChange::None
        );
    }

    #[test]
    fn a_failed_reopen_is_retried_by_the_next_change_not_lost() {
        // After an abandoned handover the scaler is at 640x360 while the decoder
        // still emits source size; a later change re-plans from the real state.
        assert_eq!(
            plan_display_change(hw(None), (640, 360), (480, 270), None),
            DisplayChange::Reopen {
                resize: Some(TileSize::new(480, 270))
            }
        );
    }

    /// A decoder with a fixed output delay, in packets.
    #[derive(Default)]
    struct Fake {
        delay: usize,
        queue: VecDeque<i64>,
        fail_send: bool,
        no_pts: bool,
    }

    struct Pkt {
        pts: i64,
        key: bool,
    }

    impl HandoverDecoder for Fake {
        type Packet = Pkt;
        type Frame = Option<i64>;
        fn send(&mut self, packet: &Pkt) -> Result<(), String> {
            if self.fail_send {
                return Err("send failed".to_owned());
            }
            self.queue.push_back(packet.pts);
            Ok(())
        }
        fn receive(&mut self) -> Result<Option<Option<i64>>, String> {
            if self.queue.len() <= self.delay {
                return Ok(None);
            }
            let pts = self.queue.pop_front();
            Ok(Some(if self.no_pts { None } else { pts }))
        }
        fn is_key(packet: &Pkt) -> bool {
            packet.key
        }
        fn packet_pts(packet: &Pkt) -> Option<i64> {
            Some(packet.pts)
        }
        fn frame_pts(frame: &Option<i64>) -> Option<i64> {
            *frame
        }
    }

    fn ready(fake: Fake, now: Instant) -> Handover<Fake> {
        let permit = loop {
            if let Some(permit) = HandoverPermit::try_acquire() {
                break permit;
            }
            std::thread::yield_now();
        };
        let mut handover =
            Handover::spawn("test-open".to_owned(), permit, now, move || Ok(fake)).unwrap();
        while handover.decoder.is_none() {
            assert!(matches!(handover.poll(None, now), HandoverPoll::Pending));
            std::thread::yield_now();
        }
        handover
    }

    fn pkt(pts: i64, key: bool) -> Pkt {
        Pkt { pts, key }
    }

    #[test]
    fn the_replacement_starts_at_a_keyframe_and_cuts_over_with_no_gap_or_repeat() {
        let now = Instant::now();
        let mut handover = ready(
            Fake {
                delay: 2,
                ..Fake::default()
            },
            now,
        );
        // Not a keyframe: the replacement does not start here.
        handover.on_packet(&pkt(9, false));
        assert_eq!(handover.anchor, None);
        for pts in 10..=12 {
            handover.on_packet(&pkt(pts, pts == 10));
        }
        // The old decoder is still behind the anchor: it keeps publishing.
        assert!(matches!(handover.poll(Some(9), now), HandoverPoll::Pending));
        handover.on_packet(&pkt(13, false));
        match handover.poll(Some(10), now) {
            HandoverPoll::Ready {
                frames,
                skip_through,
                ..
            } => {
                assert_eq!(frames, VecDeque::from([Some(11)]));
                assert_eq!(skip_through, None);
            }
            _ => panic!("expected a cutover"),
        }
    }

    #[test]
    fn a_replacement_behind_the_old_decoder_never_steps_backwards() {
        let now = Instant::now();
        let mut handover = ready(Fake::default(), now);
        handover.on_packet(&pkt(10, true));
        match handover.poll(Some(14), now) {
            HandoverPoll::Ready {
                frames,
                skip_through,
                ..
            } => {
                assert!(frames.is_empty());
                assert_eq!(skip_through, Some(14));
            }
            _ => panic!("expected a cutover"),
        }
    }

    #[test]
    fn leading_pictures_before_the_keyframe_are_discarded() {
        let now = Instant::now();
        let mut handover = ready(Fake::default(), now);
        handover.on_packet(&pkt(10, true));
        handover.on_packet(&pkt(8, false));
        handover.on_packet(&pkt(11, false));
        assert_eq!(handover.frames, VecDeque::from([Some(10), Some(11)]));
    }

    #[test]
    fn a_decode_failure_abandons_and_keeps_the_old_decoder() {
        let now = Instant::now();
        let mut handover = ready(
            Fake {
                fail_send: true,
                ..Fake::default()
            },
            now,
        );
        handover.on_packet(&pkt(10, true));
        assert!(matches!(
            handover.poll(Some(10), now),
            HandoverPoll::Abandoned(_)
        ));
    }

    #[test]
    fn a_frame_without_a_timestamp_abandons() {
        let now = Instant::now();
        let mut handover = ready(
            Fake {
                no_pts: true,
                ..Fake::default()
            },
            now,
        );
        handover.on_packet(&pkt(10, true));
        assert!(matches!(
            handover.poll(Some(10), now),
            HandoverPoll::Abandoned(_)
        ));
    }

    #[test]
    fn a_failed_open_abandons() {
        let now = Instant::now();
        let permit = loop {
            if let Some(permit) = HandoverPermit::try_acquire() {
                break permit;
            }
            std::thread::yield_now();
        };
        let mut handover =
            Handover::<Fake>::spawn("test-open".to_owned(), permit, now, || Err("no".to_owned()))
                .unwrap();
        loop {
            match handover.poll(None, now) {
                HandoverPoll::Pending => std::thread::yield_now(),
                HandoverPoll::Abandoned(reason) => {
                    assert_eq!(reason, "no");
                    break;
                }
                HandoverPoll::Ready { .. } => panic!("a failed open cannot be ready"),
            }
        }
    }

    #[test]
    fn a_keyframe_that_never_comes_times_out() {
        let now = Instant::now();
        let mut handover = ready(Fake::default(), now);
        handover.on_packet(&pkt(1, false));
        assert!(matches!(
            handover.poll(None, now + HANDOVER_DEADLINE),
            HandoverPoll::Abandoned(_)
        ));
    }

    #[test]
    fn a_wedged_open_neither_blocks_the_poll_nor_outlives_its_deadline() {
        let now = Instant::now();
        let permit = loop {
            if let Some(permit) = HandoverPermit::try_acquire() {
                break permit;
            }
            std::thread::yield_now();
        };
        let (release, wedged) = mpsc::channel::<()>();
        let mut handover =
            Handover::<Fake>::spawn("test-open".to_owned(), permit, now, move || {
                let _released = wedged.recv();
                Err("released".to_owned())
            })
            .unwrap();
        handover.on_packet(&pkt(1, true));
        assert!(matches!(handover.poll(Some(1), now), HandoverPoll::Pending));
        assert!(matches!(
            handover.poll(Some(1), now + HANDOVER_DEADLINE),
            HandoverPoll::Abandoned(_)
        ));
        drop(handover);
        drop(release);
    }

    #[test]
    fn the_skip_ends_at_the_first_later_frame() {
        let mut skip = SkipThrough::default();
        skip.arm(Some(10));
        assert!(skip.drops(Some(9)));
        assert!(skip.drops(Some(10)));
        assert!(!skip.drops(Some(11)));
        assert!(!skip.drops(Some(5)), "the skip is over");
    }

    #[test]
    fn a_backward_timestamp_jump_cannot_freeze_the_tile() {
        let mut skip = SkipThrough::default();
        skip.arm(Some(1_000_000));
        let dropped = (0..1000).take_while(|pts| skip.drops(Some(*pts))).count();
        assert_eq!(dropped, HANDOVER_MAX_FRAMES);
    }

    #[test]
    fn an_unarmed_skip_or_a_frame_without_a_timestamp_is_kept() {
        let mut skip = SkipThrough::default();
        assert!(!skip.drops(Some(1)));
        skip.arm(Some(10));
        assert!(!skip.drops(None));
        assert!(!skip.drops(Some(1)));
    }

    #[test]
    fn warm_up_frames_are_bounded() {
        let now = Instant::now();
        let mut handover = ready(Fake::default(), now);
        let extra = i64::try_from(HANDOVER_MAX_FRAMES).unwrap() + 1;
        for pts in 0..extra {
            handover.on_packet(&pkt(pts, pts == 0));
        }
        assert_eq!(handover.frames.len(), HANDOVER_MAX_FRAMES);
        assert!(matches!(
            handover.poll(None, now),
            HandoverPoll::Abandoned(_)
        ));
    }

    fn driver(slot: &DisplayLayoutSlot, state: DecoderState) -> DisplayDriver<Fake> {
        DisplayDriver::new(
            DisplayFollow::new(Arc::clone(slot), None),
            "a",
            (1920, 1080),
            state,
            None,
        )
    }

    fn fullscreen_slot() -> DisplayLayoutSlot {
        Arc::new(SwapCell::new(Arc::new(layout(
            1920,
            1080,
            vec![cell(Some("a"), 1.0, 1.0)],
        ))))
    }

    fn sixth_layout() -> Arc<Layout> {
        Arc::new(layout(
            1920,
            1080,
            vec![cell(Some("a"), 1.0 / 3.0, 1.0 / 3.0)],
        ))
    }

    fn open_fake(resize: Option<TileSize>) -> impl FnOnce() -> Result<Fake, String> + Send {
        assert_eq!(resize, Some(TileSize::new(640, 360)));
        || Ok(Fake::default())
    }

    #[test]
    fn a_smaller_layout_reopens_at_the_new_resize_and_keeps_the_old_size_until_cutover() {
        let slot = fullscreen_slot();
        let mut driver = driver(&slot, hw(None));
        let t0 = Instant::now();
        slot.store(sixth_layout());
        assert!(matches!(
            driver.poll(None, t0, open_fake),
            DisplayStep::Idle
        ));
        let due = t0 + DISPLAY_SETTLE;
        // Other tests may hold the process-wide slots: the driver retries on a
        // later poll, as it does in production.
        while driver.reopening.is_none() {
            assert!(matches!(
                driver.poll(None, due, open_fake),
                DisplayStep::Idle
            ));
            std::thread::yield_now();
        }
        assert_eq!(driver.tile, (1920, 1080), "the old size holds meanwhile");

        loop {
            driver.on_packet(&pkt(10, true));
            match driver.poll(Some(10), due, open_fake) {
                DisplayStep::Idle => std::thread::yield_now(),
                DisplayStep::Cutover { tile, .. } => {
                    assert_eq!(tile, (640, 360));
                    break;
                }
                DisplayStep::Retarget(_) => panic!("NVDEC must reopen, not host-scale"),
            }
        }
        assert_eq!(driver.state.resize, Some(TileSize::new(640, 360)));
        // Settled: no second reopen for the same layout.
        let later = due + DISPLAY_MIN_INTERVAL;
        assert!(matches!(
            driver.poll(None, later, open_fake),
            DisplayStep::Idle
        ));
        assert!(driver.reopening.is_none());
    }

    #[test]
    fn a_failed_open_falls_back_to_the_host_scaler_and_does_not_retry_in_a_loop() {
        let slot = fullscreen_slot();
        let mut driver = driver(&slot, hw(None));
        let t0 = Instant::now();
        let fail = |_resize| || -> Result<Fake, String> { Err("no session".to_owned()) };
        slot.store(sixth_layout());
        let _idle = driver.poll(None, t0, fail);
        let due = t0 + DISPLAY_SETTLE;
        while driver.reopening.is_none() {
            let _idle = driver.poll(None, due, fail);
            std::thread::yield_now();
        }
        let retargeted = loop {
            match driver.poll(None, due, fail) {
                DisplayStep::Idle => std::thread::yield_now(),
                DisplayStep::Retarget(size) => break size,
                DisplayStep::Cutover { .. } => panic!("a failed open cannot cut over"),
            }
        };
        assert_eq!(retargeted, (640, 360));
        assert_eq!(
            driver.state.resize, None,
            "the old decoder is still running"
        );
        let later = due + DISPLAY_MIN_INTERVAL;
        assert!(matches!(driver.poll(None, later, fail), DisplayStep::Idle));
        assert!(driver.reopening.is_none());
    }

    #[test]
    fn software_decode_follows_without_a_reopen() {
        let slot = fullscreen_slot();
        let state = DecoderState {
            hardware: false,
            ..hw(None)
        };
        let mut driver = driver(&slot, state);
        let t0 = Instant::now();
        let never = |_resize| || -> Result<Fake, String> { panic!("software never reopens") };
        slot.store(sixth_layout());
        let _idle = driver.poll(None, t0, never);
        assert!(matches!(
            driver.poll(None, t0 + DISPLAY_SETTLE, never),
            DisplayStep::Retarget((640, 360))
        ));
    }

    #[test]
    fn the_follower_sees_a_published_layout_and_the_gate_debounces_it() {
        let boot = layout(1920, 1080, vec![cell(Some("other"), 1.0, 1.0)]);
        let slot: DisplayLayoutSlot = Arc::new(SwapCell::new(Arc::new(boot)));
        let mut follower = DisplayFollower::new(
            DisplayFollow::new(Arc::clone(&slot), None),
            "a",
            (1920, 1080),
        );
        let t0 = Instant::now();
        assert_eq!(follower.due(t0), None);
        slot.store(Arc::new(layout(
            1920,
            1080,
            vec![cell(Some("a"), 1.0 / 3.0, 1.0 / 3.0)],
        )));
        assert_eq!(follower.due(t0), None);
        assert_eq!(follower.due(t0 + DISPLAY_SETTLE), Some((640, 360)));
    }

    /// PR 2 (#306, §2.4 aux sizing): a source a routed layout binds decodes
    /// at the per-axis supremum over Program's cells and the routed cells;
    /// with no routed cells the follower sees Program's layout unchanged.
    #[test]
    fn aux_sizing_takes_the_supremum_over_program_and_routed_cells() {
        let program = layout(1920, 1080, vec![cell(Some("cam"), 0.25, 0.25)]);
        let slot: DisplayLayoutSlot = Arc::new(SwapCell::new(Arc::new(program)));
        let aux = aux_sizing_slot();
        let follow = DisplayFollow::new(Arc::clone(&slot), None).with_aux_sizing(Arc::clone(&aux));
        let small = follow.size_for("cam");
        let unbound = follow.size_for("other");
        aux.store(Arc::new(vec![
            Cell {
                w: 1.0,
                h: 1.0,
                source: Some("cam".to_owned()),
                ..Cell::default()
            },
            Cell {
                w: 0.5,
                h: 0.5,
                source: Some("other".to_owned()),
                ..Cell::default()
            },
        ]));
        let canvas = (slot.load().canvas.width, slot.load().canvas.height);
        assert_eq!(
            follow.size_for("cam"),
            canvas,
            "the routed full-canvas cell wins"
        );
        assert!(
            follow.size_for("other") != unbound,
            "a routed-only source is sized"
        );
        assert_eq!(
            follow.size_for("other"),
            ((canvas.0 / 2) & !1, (canvas.1 / 2) & !1)
        );
        aux.store(Arc::new(Vec::new()));
        assert_eq!(follow.size_for("cam"), small);
        let base = slot.load();
        assert!(
            Arc::ptr_eq(&sizing_layout(&base, &[]), &base),
            "no copy without routed cells"
        );
    }
}
