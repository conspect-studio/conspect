//! Per-output format convert (#342, output-routing §12.4 "one convert per
//! (picture, format)").
//!
//! A [`FormatConvert`] brings a picture (the baked Program canvas, or Clean) to
//! an output's [`AuxFormat`] with [`fit_rects`] + [`blit_region`] into a bounded
//! ring of [`CONVERT_RING_BUFFERS`] reusable buffers. It never blocks and never
//! fails once it has drawn a picture: with every buffer still held downstream
//! it repeats its last picture (or a black one at the format).
//!
//! **Base (Program) keys** (#342 review I1/I2): one [`ConvertStage`] per
//! distinct non-canvas base format owns a `format-convert` thread. The bake
//! consumer hands it each baked canvas with a non-blocking `try_send` on a
//! bounded channel of capacity [`CONVERT_QUEUE`] (full: dropped and counted);
//! the thread converts and publishes `(tick, picture)` into a wait-free
//! [`SwapCell`]. A base worker at that format ([`ConvertedWorker`]) never
//! converts and never fails on a convert: each tick it encodes the newest
//! published picture (at most one tick old in the steady state), or a black
//! picture at the format, allocated once on the worker. So a slow or wedged convert freezes (or blacks) only its own
//! outputs; the clock, the consumer, Program and every other output are
//! untouched. A converted base output lags by up to one tick (the aux path's
//! newest-frame rule). One convert per (Program, format) holds by
//! construction.
//!
//! **Live keys** (a routed Program/Clean at a non-canvas format) convert on
//! their own worker thread through [`ConvertLive`], which never errors either.
//! Colour samples are copied, never converted: the result carries the
//! source's tag (the canvas tag).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use multiview_compositor::pipeline::Nv12Image;
use multiview_core::color::ColorInfo;
use multiview_framestore::SwapCell;

use crate::nv12_blit::blit_region;
use crate::output_fit::{fit_rects, Rect};
use crate::routing_aux::AuxFormat;

#[cfg(feature = "ffmpeg")]
use crate::output_routing::LiveEncoder;
#[cfg(feature = "ffmpeg")]
use crate::pipeline::PipelineError;
#[cfg(feature = "ffmpeg")]
use crate::rendition_dispatch::{MoveGate, ParameterSets};
#[cfg(feature = "ffmpeg")]
use crate::rendition_workers::{BakedTick, ReorderBound, VideoEncodeWorker};
#[cfg(feature = "ffmpeg")]
use multiview_ffmpeg::EncodedPacket;

/// Reusable converted pictures per convert (bounded, like the aux ring).
pub const CONVERT_RING_BUFFERS: usize = 4;

/// The bake consumer -> convert thread queue (ticks). Full = drop + count.
pub const CONVERT_QUEUE: usize = 2;

/// A black picture at `format`, tagged `color` (the never-converted fallback).
#[must_use]
pub fn black_at(format: AuxFormat, color: ColorInfo) -> Option<Arc<Nv12Image>> {
    Nv12Image::solid(format.width, format.height, 16, 128, 128, color)
        .ok()
        .map(Arc::new)
}

/// A convert of pictures to one [`AuxFormat`], owned by one thread.
#[derive(Debug)]
pub struct FormatConvert {
    format: AuxFormat,
    ring: Vec<Arc<Nv12Image>>,
    last: Option<Arc<Nv12Image>>,
    converts: u64,
}

impl FormatConvert {
    /// A convert to `format`.
    #[must_use]
    pub const fn new(format: AuxFormat) -> Self {
        Self {
            format,
            ring: Vec::new(),
            last: None,
            converts: 0,
        }
    }

    /// The target format.
    #[must_use]
    pub const fn format(&self) -> AuxFormat {
        self.format
    }

    /// Buffers this convert has allocated (at most [`CONVERT_RING_BUFFERS`]).
    #[must_use]
    pub fn allocated(&self) -> usize {
        self.ring.len()
    }

    /// Pictures actually drawn.
    #[must_use]
    pub const fn converts(&self) -> u64 {
        self.converts
    }

    /// `src` at this format. A picture already at the format is returned as
    /// the same `Arc` (nothing converted or allocated). With every buffer held
    /// downstream it repeats its last picture; `None` only when nothing could
    /// ever be drawn (a degenerate size before any success).
    pub fn convert(&mut self, src: &Arc<Nv12Image>) -> Option<Arc<Nv12Image>> {
        if src.width() == self.format.width && src.height() == self.format.height {
            return Some(Arc::clone(src));
        }
        let Some(frame) = self.draw(src) else {
            return self.last.clone();
        };
        self.converts = self.converts.saturating_add(1);
        self.last = Some(Arc::clone(&frame));
        Some(frame)
    }

    fn draw(&mut self, src: &Nv12Image) -> Option<Arc<Nv12Image>> {
        let f = self.format;
        let (crop, dst) = fit_rects(
            src.width(),
            src.height(),
            f.width,
            f.height,
            f.fit.mode,
            f.fit.anchor,
        )?;
        let color = src.color();
        let index = match self.ring.iter_mut().position(|b| {
            Arc::get_mut(b).is_some_and(|img| img.color() == color && img.alpha_plane().is_none())
        }) {
            Some(index) => index,
            None if self.ring.len() < CONVERT_RING_BUFFERS => {
                let image = Nv12Image::solid(f.width, f.height, 16, 128, 128, color).ok()?;
                self.ring.push(Arc::new(image));
                self.ring.len() - 1
            }
            // Every buffer is still held downstream: repeat (never block,
            // never grow).
            None => return None,
        };
        let buffer = self.ring.get_mut(index)?;
        let image = Arc::get_mut(buffer)?;
        let (y_plane, uv_plane) = image.planes_mut();
        if dst != Rect::full(f.width, f.height) {
            y_plane.fill(16);
            uv_plane.fill(128);
        }
        blit_region(src, crop, f.width, y_plane, uv_plane, dst);
        Some(Arc::clone(buffer))
    }
}

/// One published converted picture.
#[derive(Debug)]
pub struct Converted {
    /// The Program tick it was converted from.
    pub tick: u64,
    /// The picture at the format.
    pub frame: Arc<Nv12Image>,
}

/// The wait-free output of one convert stage, read by every base worker at
/// its format.
#[derive(Debug, Default)]
pub struct ConvertedSlot {
    latest: SwapCell<Option<Arc<Converted>>>,
    /// Pictures the convert thread published.
    pub published: AtomicU64,
    /// Ticks the bake consumer could not hand over (queue full).
    pub dropped: AtomicU64,
    /// Raised when the convert thread panicked or ended.
    pub failed: AtomicBool,
}

impl ConvertedSlot {
    /// The newest published picture, if any.
    #[must_use]
    pub fn latest(&self) -> Option<Arc<Converted>> {
        self.latest.load()
    }

    /// Publish `frame` for `tick` (the convert thread; tests).
    pub fn publish(&self, tick: u64, frame: Arc<Nv12Image>) {
        self.latest.store(Some(Arc::new(Converted { tick, frame })));
        self.published.fetch_add(1, Ordering::Relaxed);
    }
}

/// A test hook run on the convert thread before each convert (chaos tests
/// wedge it). `None` in production.
pub type ConvertHook = Arc<dyn Fn(u64) + Send + Sync>;

/// One base format's convert thread, owned by the bake consumer.
pub struct ConvertStage {
    format: AuxFormat,
    tx: Option<std::sync::mpsc::SyncSender<(u64, Arc<Nv12Image>)>>,
    slot: Arc<ConvertedSlot>,
}

impl std::fmt::Debug for ConvertStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConvertStage")
            .field("format", &self.format)
            .finish_non_exhaustive()
    }
}

/// Drops an armed flag's owner into `failed` however the thread ends.
struct RaiseOnExit(Arc<ConvertedSlot>);

impl Drop for RaiseOnExit {
    fn drop(&mut self) {
        self.0.failed.store(true, Ordering::Release);
    }
}

impl ConvertStage {
    /// Spawn the `format-convert` thread for `format` (normal priority).
    ///
    /// # Errors
    /// The OS error if the thread cannot be spawned.
    pub fn spawn(format: AuxFormat, hook: Option<ConvertHook>) -> std::io::Result<Self> {
        let (tx, rx) = std::sync::mpsc::sync_channel::<(u64, Arc<Nv12Image>)>(CONVERT_QUEUE);
        let slot = Arc::new(ConvertedSlot::default());
        let thread_slot = Arc::clone(&slot);
        std::thread::Builder::new()
            .name("format-convert".to_owned())
            .spawn(move || {
                let _raise = RaiseOnExit(Arc::clone(&thread_slot));
                let mut convert = FormatConvert::new(format);
                while let Ok((tick, canvas)) = rx.recv() {
                    let body = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        if let Some(hook) = hook.as_ref() {
                            hook(tick);
                        }
                        convert.convert(&canvas)
                    }));
                    match body {
                        Ok(Some(frame)) => thread_slot.publish(tick, frame),
                        Ok(None) => {}
                        Err(_) => {
                            tracing::error!(
                                width = format.width,
                                height = format.height,
                                "format-convert: a convert panicked; its outputs hold their \
                                 last picture"
                            );
                            return;
                        }
                    }
                }
            })?;
        Ok(Self {
            format,
            tx: Some(tx),
            slot,
        })
    }

    /// The format converted.
    #[must_use]
    pub const fn format(&self) -> AuxFormat {
        self.format
    }

    /// The wait-free output the base workers read.
    #[must_use]
    pub fn slot(&self) -> Arc<ConvertedSlot> {
        Arc::clone(&self.slot)
    }

    /// Hand the baked canvas of `tick` to the convert thread. Never blocks: a
    /// full queue (or an ended thread) drops the tick and counts it.
    pub fn offer(&self, tick: u64, canvas: &Arc<Nv12Image>) {
        let Some(tx) = self.tx.as_ref() else {
            return;
        };
        if tx.try_send((tick, Arc::clone(canvas))).is_err() {
            self.slot.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The convert stages of a run's base keys: one per distinct base format.
#[derive(Debug, Default)]
pub struct ConvertStages {
    stages: Vec<ConvertStage>,
}

impl ConvertStages {
    /// The slot of `format`, spawning its stage on first use.
    ///
    /// # Errors
    /// The OS error if the thread cannot be spawned.
    pub fn slot_for(
        &mut self,
        format: AuxFormat,
        hook: Option<&ConvertHook>,
    ) -> std::io::Result<Arc<ConvertedSlot>> {
        if let Some(stage) = self.stages.iter().find(|s| s.format == format) {
            return Ok(stage.slot());
        }
        let stage = ConvertStage::spawn(format, hook.cloned())?;
        let slot = stage.slot();
        self.stages.push(stage);
        Ok(slot)
    }

    /// Hand `canvas` of `tick` to every stage (never blocks).
    pub fn offer(&self, tick: u64, canvas: &Arc<Nv12Image>) {
        for stage in &self.stages {
            stage.offer(tick, canvas);
        }
    }

    /// Whether any stage exists (a run with only canvas-format keys has none).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.stages.is_empty()
    }
}

#[cfg(feature = "ffmpeg")]
/// A base (Program) rendition worker at a non-canvas format. It never
/// converts and never fails on a convert: each tick encodes the newest
/// picture the format's convert thread published, else a black picture at
/// the format (allocated once, on this worker, from the canvas colour tag).
pub struct ConvertedWorker {
    slot: Arc<ConvertedSlot>,
    format: AuxFormat,
    black: Option<Arc<Nv12Image>>,
    inner: Box<dyn VideoEncodeWorker>,
}

#[cfg(feature = "ffmpeg")]
impl ConvertedWorker {
    /// Encode `slot`'s pictures (at `format`) with `inner`.
    #[must_use]
    pub fn new(
        slot: Arc<ConvertedSlot>,
        format: AuxFormat,
        inner: Box<dyn VideoEncodeWorker>,
    ) -> Self {
        Self {
            slot,
            format,
            black: None,
            inner,
        }
    }

    /// The picture for this tick: the newest converted one, or black.
    fn picture(&mut self, canvas: &Nv12Image) -> Result<Arc<Nv12Image>, PipelineError> {
        if let Some(converted) = self.slot.latest() {
            return Ok(Arc::clone(&converted.frame));
        }
        if let Some(black) = self.black.as_ref() {
            return Ok(Arc::clone(black));
        }
        let black = black_at(self.format, canvas.color()).ok_or_else(|| PipelineError::Output {
            kind: "format-convert",
            reason: format!(
                "cannot allocate a {}x{} picture",
                self.format.width, self.format.height
            ),
        })?;
        self.black = Some(Arc::clone(&black));
        Ok(black)
    }
}

#[cfg(feature = "ffmpeg")]
impl VideoEncodeWorker for ConvertedWorker {
    fn encode(&mut self, item: BakedTick) -> Result<Vec<EncodedPacket>, PipelineError> {
        let canvas = self.picture(&item.canvas)?;
        self.inner.encode(BakedTick {
            tick: item.tick,
            canvas,
        })
    }
    fn reorder_bound(&self) -> ReorderBound {
        self.inner.reorder_bound()
    }
    fn finish(&mut self) -> Result<Vec<EncodedPacket>, PipelineError> {
        self.inner.finish()
    }
    fn force_keyframe(&mut self) {
        self.inner.force_keyframe();
    }
}

#[cfg(feature = "ffmpeg")]
/// A live-opened (routed Program/Clean) encoder at a non-canvas format: the
/// convert runs on the key's own worker thread, and never errors (it repeats
/// its last picture, or a black one at the format).
pub struct ConvertLive {
    convert: FormatConvert,
    black: Option<Arc<Nv12Image>>,
    inner: Box<dyn LiveEncoder>,
}

#[cfg(feature = "ffmpeg")]
impl ConvertLive {
    /// Wrap `inner` behind a convert to `format`.
    #[must_use]
    pub fn new(format: AuxFormat, inner: Box<dyn LiveEncoder>) -> Self {
        Self {
            convert: FormatConvert::new(format),
            black: None,
            inner,
        }
    }

    fn picture(&mut self, src: &Arc<Nv12Image>) -> Arc<Nv12Image> {
        if let Some(frame) = self.convert.convert(src) {
            return frame;
        }
        let format = self.convert.format();
        match self.black.as_ref() {
            Some(black) => Arc::clone(black),
            None => {
                // Allocated once, on this worker, only if nothing ever drew.
                let black = black_at(format, src.color()).unwrap_or_else(|| Arc::clone(src));
                self.black = Some(Arc::clone(&black));
                black
            }
        }
    }
}

#[cfg(feature = "ffmpeg")]
impl VideoEncodeWorker for ConvertLive {
    fn encode(&mut self, item: BakedTick) -> Result<Vec<EncodedPacket>, PipelineError> {
        let canvas = self.picture(&item.canvas);
        self.inner.encode(BakedTick {
            tick: item.tick,
            canvas,
        })
    }
    fn reorder_bound(&self) -> ReorderBound {
        self.inner.reorder_bound()
    }
    fn finish(&mut self) -> Result<Vec<EncodedPacket>, PipelineError> {
        self.inner.finish()
    }
    fn force_keyframe(&mut self) {
        self.inner.force_keyframe();
    }
}

#[cfg(feature = "ffmpeg")]
impl LiveEncoder for ConvertLive {
    fn start_at(&mut self, t0: u64) {
        self.inner.start_at(t0);
    }
    fn time_base(&self) -> multiview_core::time::Rational {
        self.inner.time_base()
    }
    fn move_gate(&self) -> MoveGate {
        self.inner.move_gate()
    }
    fn parameter_sets(&self) -> Option<ParameterSets> {
        self.inner.parameter_sets()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use multiview_compositor::pipeline::CanvasColor;
    use multiview_config::{Anchor, OutputFit, OutputFitMode};
    use std::time::{Duration, Instant};

    fn picture(w: u32, h: u32) -> Arc<Nv12Image> {
        Arc::new(Nv12Image::color_bars(w, h, CanvasColor::default()).unwrap())
    }

    fn vertical() -> AuxFormat {
        AuxFormat::new(
            108,
            192,
            OutputFit::new(OutputFitMode::Cover, Anchor::Center),
        )
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

    #[test]
    fn identity_passes_the_same_arc_and_allocates_nothing() {
        let mut c = FormatConvert::new(AuxFormat::new(192, 108, OutputFit::default()));
        let src = picture(192, 108);
        let out = c.convert(&src).unwrap();
        assert!(Arc::ptr_eq(&out, &src));
        assert_eq!(c.allocated(), 0);
        assert_eq!(c.converts(), 0);
    }

    /// #342 review I1 regression: every ring buffer held downstream never
    /// errors — the convert repeats its last picture, never grows, never waits.
    #[test]
    fn every_ring_buffer_held_repeats_and_never_fails() {
        let mut c = FormatConvert::new(vertical());
        let mut held = Vec::new();
        for _ in 0..20 {
            held.push(
                c.convert(&picture(192, 108))
                    .expect("never None after a draw"),
            );
        }
        assert_eq!(c.allocated(), CONVERT_RING_BUFFERS, "never grows");
        let newest = Arc::clone(&held[CONVERT_RING_BUFFERS - 1]);
        for repeated in &held[CONVERT_RING_BUFFERS..] {
            assert!(Arc::ptr_eq(repeated, &newest), "repeat, never wait or grow");
        }
        drop(held);
        let fresh = c.convert(&picture(192, 108)).unwrap();
        assert_eq!((fresh.width(), fresh.height()), (108, 192));
    }

    #[test]
    fn cover_output_equals_blit_region_of_the_expected_crop() {
        let src = picture(192, 108);
        let out = FormatConvert::new(vertical()).convert(&src).unwrap();
        let (crop, dst) =
            fit_rects(192, 108, 108, 192, OutputFitMode::Cover, Anchor::Center).unwrap();
        let mut y = vec![0u8; 108 * 192];
        let mut uv = vec![0u8; 108 * 192 / 2];
        blit_region(&src, crop, 108, &mut y, &mut uv, dst);
        assert!(out.y_plane().eq(y.as_slice()), "luma parity");
        assert!(out.uv_plane().eq(uv.as_slice()), "chroma parity");
    }

    /// One convert thread per base format: two base keys at one format read
    /// one slot, so each Program picture is converted once.
    #[test]
    fn one_stage_per_format_converts_each_tick_once() {
        let mut stages = ConvertStages::default();
        let a = stages.slot_for(vertical(), None).unwrap();
        let b = stages.slot_for(vertical(), None).unwrap();
        assert!(
            Arc::ptr_eq(&a, &b),
            "two renditions at one format share a stage"
        );
        for tick in 0..10 {
            stages.offer(tick, &picture(192, 108));
            assert!(wait_for(|| a.latest().is_some_and(|c| c.tick == tick)));
        }
        assert_eq!(
            a.published.load(Ordering::Relaxed),
            10,
            "one convert per tick"
        );
        let frame = a.latest().unwrap();
        assert_eq!((frame.frame.width(), frame.frame.height()), (108, 192));
    }

    /// A wedged convert thread never blocks the offering consumer: the
    /// bounded queue fills and later ticks are dropped and counted.
    #[test]
    fn a_wedged_stage_never_blocks_the_offer() {
        let (_hold, wait) = std::sync::mpsc::channel::<()>();
        let wait = std::sync::Mutex::new(wait);
        let hook: ConvertHook = Arc::new(move |_| {
            let _ = wait.lock().unwrap().recv();
        });
        let stage = ConvertStage::spawn(vertical(), Some(hook)).unwrap();
        let started = Instant::now();
        for tick in 0..100 {
            stage.offer(tick, &picture(192, 108));
        }
        assert!(started.elapsed() < Duration::from_secs(1), "never blocks");
        let dropped = stage.slot().dropped.load(Ordering::Relaxed);
        assert!(
            dropped >= 100 - 1 - CONVERT_QUEUE as u64,
            "dropped {dropped}"
        );
        assert!(stage.slot().latest().is_none());
    }

    #[test]
    fn a_panicking_convert_raises_failed_and_publishes_nothing_more() {
        let hook: ConvertHook = Arc::new(|tick| assert!(tick < 3, "injected convert panic"));
        let stage = ConvertStage::spawn(vertical(), Some(hook)).unwrap();
        let slot = stage.slot();
        for tick in 0..3 {
            stage.offer(tick, &picture(192, 108));
            assert!(wait_for(|| slot.latest().is_some_and(|c| c.tick == tick)));
        }
        stage.offer(3, &picture(192, 108));
        assert!(wait_for(|| slot.failed.load(Ordering::Acquire)));
        assert_eq!(slot.published.load(Ordering::Relaxed), 3);
    }
}
