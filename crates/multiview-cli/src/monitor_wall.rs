//! The shared monitor wall (ADR-0103): every source's newest frame scaled into
//! one grid picture, composed once and encoded once, published as one optional
//! rendition (`output_id = "wall"`) that any number of `MoQ` viewers subscribe to.
//!
//! Isolation (invariants #1/#10): the worker runs on its own `moq-wall` thread,
//! never on the output clock or the bake consumer. It reads source frames with
//! the wait-free [`LatestSlot::load`](multiview_framestore::LatestSlot::load)
//! (it never advances a tile's latch watermark), loads the run's subscription
//! factory from its `SwapCell`, and hands encoded windows to the registry with a
//! bounded `try_send` that drops on a full queue. Nothing on the clock path
//! waits for it; a stalled or dead worker only makes the wall unavailable.
use crate::live_sources::SharedStores;
use crate::output_window_registry::{
    Dependency, DependencyKind, OptionalWindowProducer, SubscriptionFactory, VideoDescriptor,
    WindowSubscriptionSlot,
};
use crate::preview::WallSlot;
use multiview_compositor::pipeline::Nv12Image;
use multiview_control::preview::{WallCadence, WallCell, WallGeometry};
use multiview_core::color::ColorInfo;
use multiview_core::time::Rational;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The registry identity of the wall rendition. Never a configured output id.
pub const WALL_OUTPUT_ID: &str = "wall";
/// The bus identity the wall registers under, distinct from every program bus.
pub const WALL_BUS_ID: &str = "wall";

/// Operator configuration. Off unless `MULTIVIEW_MOQ_WALL=1`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WallConfig {
    /// Grid columns.
    pub columns: u32,
    /// Grid rows. Capacity is `columns * rows` cells; extra sources get none.
    pub rows: u32,
    /// Cell width in pixels (even).
    pub cell_width: u32,
    /// Cell height in pixels (even).
    pub cell_height: u32,
    /// Integer divisor of the production cadence (2 → 15 fps at 30000/1001).
    pub divisor: u32,
    /// Target bitrate, bits per second.
    pub bitrate_bps: u32,
    /// libav encoder name. `h264_nvenc` by default; a software H.264 encoder
    /// (`libx264`) opens only in a `gpl-codecs` build (LGPL rule).
    pub encoder: String,
}

impl Default for WallConfig {
    fn default() -> Self {
        Self {
            columns: 4,
            rows: 4,
            cell_width: 320,
            cell_height: 180,
            divisor: 2,
            bitrate_bps: 3_000_000,
            encoder: "h264_nvenc".to_owned(),
        }
    }
}

impl WallConfig {
    /// Read `MULTIVIEW_MOQ_WALL` (`1` enables) plus optional
    /// `MULTIVIEW_MOQ_WALL_{COLUMNS,ROWS,CELL_WIDTH,CELL_HEIGHT,DIVISOR,BITRATE,ENCODER}`.
    /// A malformed value disables the wall (logged), never a guessed default.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Option<Self> {
        if get("MULTIVIEW_MOQ_WALL").as_deref() != Some("1") {
            return None;
        }
        let mut config = Self::default();
        let fields: [(&str, &mut u32); 6] = [
            ("MULTIVIEW_MOQ_WALL_COLUMNS", &mut config.columns),
            ("MULTIVIEW_MOQ_WALL_ROWS", &mut config.rows),
            ("MULTIVIEW_MOQ_WALL_CELL_WIDTH", &mut config.cell_width),
            ("MULTIVIEW_MOQ_WALL_CELL_HEIGHT", &mut config.cell_height),
            ("MULTIVIEW_MOQ_WALL_DIVISOR", &mut config.divisor),
            ("MULTIVIEW_MOQ_WALL_BITRATE", &mut config.bitrate_bps),
        ];
        for (key, slot) in fields {
            if let Some(value) = get(key) {
                let Ok(parsed) = value.trim().parse::<u32>() else {
                    tracing::warn!(key, "monitor wall disabled: value is not an integer");
                    return None;
                };
                *slot = parsed;
            }
        }
        if let Some(encoder) = get("MULTIVIEW_MOQ_WALL_ENCODER") {
            encoder.trim().clone_into(&mut config.encoder);
        }
        config.validated()
    }

    fn validated(self) -> Option<Self> {
        let width = self.columns.checked_mul(self.cell_width)?;
        let height = self.rows.checked_mul(self.cell_height)?;
        let ok = (1..=8).contains(&self.columns)
            && (1..=8).contains(&self.rows)
            && self.cell_width >= 16
            && self.cell_height >= 16
            && self.cell_width % 2 == 0
            && self.cell_height % 2 == 0
            && width <= 1920
            && height <= 1088
            && (1..=8).contains(&self.divisor)
            && (100_000..=20_000_000).contains(&self.bitrate_bps)
            && !self.encoder.is_empty()
            && (!matches!(self.encoder.as_str(), "libx264" | "libx265")
                || cfg!(feature = "gpl-codecs"));
        if ok {
            Some(self)
        } else {
            tracing::warn!(config = ?self, "monitor wall disabled: configuration out of range");
            None
        }
    }

    /// Coded picture width.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.columns.saturating_mul(self.cell_width)
    }

    /// Coded picture height.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.rows.saturating_mul(self.cell_height)
    }

    fn capacity(&self) -> usize {
        usize::try_from(self.columns.saturating_mul(self.rows)).unwrap_or(0)
    }

    /// The wall cadence: the production cadence divided by an integer, exact.
    #[must_use]
    pub fn cadence(&self, production: Rational) -> Rational {
        Rational::new(
            production.num,
            production.den.saturating_mul(i64::from(self.divisor)),
        )
    }
}

/// Cell assignment: the first `capacity` source ids in sorted order, row-major.
#[must_use]
pub fn assign_cells(config: &WallConfig, ids: &[String]) -> Vec<WallCell> {
    let mut ids: Vec<&String> = ids.iter().collect();
    ids.sort();
    ids.into_iter()
        .take(config.capacity())
        .zip(0u32..)
        .map(|(source, index)| WallCell {
            source: source.clone(),
            x: (index % config.columns) * config.cell_width,
            y: (index / config.columns) * config.cell_height,
            w: config.cell_width,
            h: config.cell_height,
        })
        .collect()
}

/// Scale `src` into the rectangle `cell` of the wall planes by 2x2 box sampling
/// at nearest source positions. Pure integer arithmetic, no allocation.
pub fn blit_cell(
    src: &Nv12Image,
    wall_width: u32,
    y_plane: &mut [u8],
    uv_plane: &mut [u8],
    cell: &WallCell,
) {
    let (sw, sh) = (src.width(), src.height());
    let (sy, suv) = (src.y_plane(), src.uv_plane());
    let to = |v: u32| usize::try_from(v).unwrap_or(0);
    let (sw_u, ww) = (to(sw), to(wall_width));
    for row in 0..cell.h {
        let src_row = u64::from(row) * u64::from(sh) / u64::from(cell.h);
        let r0 = to(u32::try_from(src_row).unwrap_or(0));
        let r1 = (r0 + 1).min(to(sh).saturating_sub(1));
        let dst_row = to(cell.y + row) * ww;
        for col in 0..cell.w {
            let src_col =
                to(u32::try_from(u64::from(col) * u64::from(sw) / u64::from(cell.w)).unwrap_or(0));
            let c1 = (src_col + 1).min(sw_u.saturating_sub(1));
            let px = |r: usize, c: usize| u16::from(sy.get(r * sw_u + c).copied().unwrap_or(16));
            let sum = px(r0, src_col) + px(r0, c1) + px(r1, src_col) + px(r1, c1);
            if let Some(dst) = y_plane.get_mut(dst_row + to(cell.x + col)) {
                *dst = u8::try_from(sum / 4).unwrap_or(u8::MAX);
            }
        }
    }
    // Chroma: one interleaved Cb/Cr pair per 2x2 luma block.
    let (cw, ch) = (cell.w / 2, cell.h / 2);
    let (scw, sch) = (sw / 2, sh / 2);
    for row in 0..ch {
        let r =
            to(u32::try_from(u64::from(row) * u64::from(sch) / u64::from(ch.max(1))).unwrap_or(0));
        let dst_row = to(cell.y / 2 + row) * ww;
        for col in 0..cw {
            let c = to(
                u32::try_from(u64::from(col) * u64::from(scw) / u64::from(cw.max(1))).unwrap_or(0),
            );
            let base = r * sw_u + c * 2;
            let dst = dst_row + to(cell.x / 2 + col) * 2;
            for k in 0..2 {
                if let (Some(d), Some(s)) = (uv_plane.get_mut(dst + k), suv.get(base + k)) {
                    *d = *s;
                }
            }
        }
    }
}

/// Compose one wall picture from the newest frame of each cell's source.
/// A source with no frame (or an odd/empty picture) leaves its cell black.
#[must_use]
pub fn compose(
    config: &WallConfig,
    cells: &[WallCell],
    stores: &SharedStores,
) -> Option<Nv12Image> {
    let (w, h) = (config.width(), config.height());
    let luma = usize::try_from(w)
        .ok()?
        .checked_mul(usize::try_from(h).ok()?)?;
    let mut y = vec![16u8; luma];
    let mut uv = vec![128u8; luma / 2];
    let snapshot = stores.load();
    for cell in cells {
        let Some(frame) = snapshot
            .get(&cell.source)
            .and_then(|store| store.slot().load())
        else {
            continue;
        };
        if frame.width() >= 2 && frame.height() >= 2 {
            blit_cell(&frame, w, &mut y, &mut uv, cell);
        }
    }
    let color = ColorInfo::default().resolve_defaults(w, h);
    Nv12Image::new(w, h, y, uv, color).ok()
}

/// Owner handle of the running wall worker. Dropping it stops the worker at its
/// next frame boundary; it never joins (a wedged native call cannot hang drop).
pub struct WallWorker {
    stop: Arc<AtomicBool>,
}

impl Drop for WallWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// Clears the published geometry when the worker thread ends for any reason,
/// including a panic unwinding the thread, so the route answers 503.
struct GeometryGuard(WallSlot);
impl Drop for GeometryGuard {
    fn drop(&mut self) {
        self.0.store(None);
    }
}

#[cfg(test)]
pub(crate) type FrameHook = Box<dyn FnMut(u64) + Send>;

/// Spawn the one wall worker. It follows each run's subscription factory, opens
/// one encoder per factory lifetime and publishes the geometry into `geometry`.
///
/// # Errors
/// Returns the OS error when the thread cannot be spawned.
pub fn spawn(
    config: WallConfig,
    production_cadence: Rational,
    stores: SharedStores,
    slot: WindowSubscriptionSlot,
    geometry: WallSlot,
) -> std::io::Result<WallWorker> {
    spawn_inner(
        config,
        production_cadence,
        stores,
        slot,
        geometry,
        #[cfg(test)]
        None,
    )
}

fn spawn_inner(
    config: WallConfig,
    production_cadence: Rational,
    stores: SharedStores,
    slot: WindowSubscriptionSlot,
    geometry: WallSlot,
    #[cfg(test)] hook: Option<FrameHook>,
) -> std::io::Result<WallWorker> {
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    std::thread::Builder::new()
        .name("moq-wall".into())
        .spawn(move || {
            let _guard = GeometryGuard(geometry.clone());
            let cadence = config.cadence(production_cadence);
            let mut run = Run {
                publisher: None,
                config,
                cadence,
                stores,
                geometry,
                generation: 0,
                cells: Vec::new(),
                #[cfg(test)]
                hook,
            };
            run.main(&slot, &worker_stop);
        })?;
    Ok(WallWorker { stop })
}

struct Run {
    /// One optional publisher per factory lifetime (the claim is one-shot) and
    /// the next never-reused output index for a reopened wall rendition.
    publisher: Option<(
        Arc<SubscriptionFactory>,
        crate::optional_rendition::OptionalRegistryPublisher,
        usize,
    )>,
    config: WallConfig,
    cadence: Rational,
    stores: SharedStores,
    geometry: WallSlot,
    generation: u64,
    cells: Vec<WallCell>,
    #[cfg(test)]
    hook: Option<FrameHook>,
}

/// The per-factory encoder + registry lifetime.
struct Session {
    factory: Arc<SubscriptionFactory>,
    encoder: multiview_output::VideoRenditionEncoder,
    producer: OptionalWindowProducer,
    _registration: crate::optional_rendition::OptionalRegistration,
    tick: u64,
}

impl Run {
    fn frame_period(&self) -> Duration {
        // seconds per frame = den / num, exact in integer nanoseconds.
        let num = u128::try_from(self.cadence.num.max(1)).unwrap_or(1);
        let den = u128::try_from(self.cadence.den.max(1)).unwrap_or(1);
        Duration::from_nanos(u64::try_from(den * 1_000_000_000 / num).unwrap_or(u64::MAX))
    }

    fn main(&mut self, slot: &WindowSubscriptionSlot, stop: &AtomicBool) {
        let period = self.frame_period();
        let mut session: Option<Session> = None;
        let mut failed_for: Option<usize> = None;
        let mut next = Instant::now();
        let mut frame_index: u64 = 0;
        while !stop.load(Ordering::Acquire) {
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            }
            next += period;
            // A worker that fell more than one period behind resynchronises
            // rather than bursting; PTS still comes from the frame counter.
            if Instant::now() > next + period {
                next = Instant::now() + period;
            }
            #[cfg(test)]
            if let Some(hook) = self.hook.as_mut() {
                hook(frame_index);
            }
            frame_index = frame_index.wrapping_add(1);
            let current = slot.load();
            let Some(factory) = current.as_ref() else {
                session = None;
                self.geometry.store(None);
                continue;
            };
            if session
                .as_ref()
                .is_some_and(|s| !Arc::ptr_eq(&s.factory, factory))
            {
                session = None;
                self.geometry.store(None);
            }
            // A changed cell set never changes the running encode: the session
            // (and its registration, whose dependencies are exactly the encoded
            // cells) retires, and the next frame reopens one encoder with the
            // new layout under a new generation. Viewers' leases end with the
            // old registration; the UI reopens on the generation change.
            if self.refresh_cells() && session.take().is_some() {
                self.geometry.store(None);
                failed_for = None;
            }
            let Some(picture) = compose(&self.config, &self.cells, &self.stores) else {
                continue;
            };
            if session.is_none() {
                let key = Arc::as_ptr(factory).addr();
                if failed_for == Some(key) {
                    continue;
                }
                match self.open(factory.clone(), &picture) {
                    Ok(opened) => {
                        tracing::info!(
                            width = self.config.width(),
                            height = self.config.height(),
                            encoder = %self.config.encoder,
                            "monitor wall encoder opened once for this run"
                        );
                        session = Some(opened);
                        self.publish_geometry();
                    }
                    Err(reason) => {
                        tracing::warn!(%reason, "monitor wall unavailable for this run");
                        failed_for = Some(key);
                    }
                }
                continue;
            }
            let Some(active) = session.as_mut() else {
                continue;
            };
            if Self::encode_and_offer(active, &picture, period).is_err() {
                tracing::warn!("monitor wall encoder failed; wall disabled for this run");
                failed_for = Some(Arc::as_ptr(&active.factory).addr());
                session = None;
                self.geometry.store(None);
                continue;
            }
            self.publish_geometry();
        }
    }

    /// Returns true when the cell assignment changed.
    fn refresh_cells(&mut self) -> bool {
        let ids: Vec<String> = self.stores.load().keys().cloned().collect();
        let cells = assign_cells(&self.config, &ids);
        if cells == self.cells {
            return false;
        }
        self.cells = cells;
        self.generation = self.generation.wrapping_add(1);
        true
    }

    fn publish_geometry(&self) {
        let fresh = self
            .geometry
            .load()
            .is_none_or(|current| current.generation != self.generation);
        if fresh {
            self.geometry.store(Some(Arc::new(WallGeometry {
                generation: self.generation,
                width: self.config.width(),
                height: self.config.height(),
                cadence: WallCadence {
                    num: self.cadence.num,
                    den: self.cadence.den,
                },
                cells: self.cells.clone(),
                preview_source: None,
            })));
        }
    }

    fn encoder_config(&self) -> multiview_output::EncodeConfig {
        let mut config =
            multiview_output::EncodeConfig::mpeg2(self.config.width(), self.config.height());
        config.codec_name.clone_from(&self.config.encoder);
        config.format = if self.config.encoder.ends_with("_nvenc") {
            ffmpeg_next::format::Pixel::NV12
        } else {
            ffmpeg_next::format::Pixel::YUV420P
        };
        config.cadence = self.cadence;
        // GOP of at most one second, rounded down, never zero.
        config.gop = u32::try_from(self.cadence.num / self.cadence.den.max(1))
            .unwrap_or(1)
            .max(1);
        config.bit_rate = usize::try_from(self.config.bitrate_bps).unwrap_or(0);
        config
    }

    /// Open the encoder, encode the first picture to learn the opened AVC
    /// configuration, register the one optional rendition, then offer it.
    fn open(
        &mut self,
        factory: Arc<SubscriptionFactory>,
        first: &Nv12Image,
    ) -> Result<Session, String> {
        use crate::optional_rendition::{
            OptionalRegistryPublisher, OptionalRendition, PreviewScope,
        };
        let mut encoder = multiview_output::VideoRenditionEncoder::new(&self.encoder_config())
            .map_err(|e| format!("open {}: {e}", self.config.encoder))?;
        // A low-latency encoder emits its first access unit within a few
        // frames; the opened AVC record comes from that real emission.
        let mut initial = Vec::new();
        let mut avc = None;
        let mut tick = 0;
        while avc.is_none() && tick < 4 {
            let frame = crate::pipeline::nv12_to_decoded(first).map_err(|e| e.to_string())?;
            let packets = encoder
                .encode_frame_at(frame, tick)
                .map_err(|e| e.to_string())?;
            tick += 1;
            avc = packets
                .iter()
                .filter_map(multiview_ffmpeg::EncodedPacket::payload)
                .find_map(multiview_ffmpeg::codec_metadata::AvcDecoderConfig::from_initial_rap);
            initial.extend(packets);
        }
        let avc = avc.ok_or("wall encoder emitted no H.264 random-access point")?;
        let reorder = if encoder.no_reorder_proven() {
            crate::rendition_workers::ReorderBound::NoReorder
        } else {
            return Err("wall encoder may reorder frames".into());
        };
        let mut video = VideoDescriptor::from_opened(
            encoder.codec_params(),
            encoder.time_base(),
            reorder,
            self.cadence,
        )
        .ok_or("opened wall encoder has no usable descriptor")?;
        video.codec.clone_from(&avc.codec);
        video.config = Some(avc.description.clone());
        video.framing = multiview_ffmpeg::NalFraming::AnnexB;
        video.recovery = Some(multiview_output::batch::RecoveryGate::StrictRap {
            codec: multiview_ffmpeg::CodecKind::H264,
            framing: video.framing,
        });
        if !self
            .publisher
            .as_ref()
            .is_some_and(|(owner, _, _)| Arc::ptr_eq(owner, &factory))
        {
            let publisher = OptionalRegistryPublisher::new(factory.clone())
                .map_err(|e| format!("optional registry already owned: {e:?}"))?;
            let first = factory
                .effective()
                .iter()
                .map(|b| b.output_index)
                .max()
                .map_or(Some(0), |i| i.checked_add(1))
                .ok_or("output index overflow")?;
            self.publisher = Some((factory.clone(), publisher, first));
        }
        let (_, publisher, next_index) = self.publisher.as_mut().ok_or("no publisher")?;
        let index = *next_index;
        *next_index = index.checked_add(1).ok_or("output index overflow")?;
        let producer = factory.optional_producer(index, factory.generation());
        let dependencies = self
            .cells
            .iter()
            .map(|cell| Dependency {
                kind: DependencyKind::Source,
                id: cell.source.clone(),
            })
            .chain(std::iter::once(Dependency {
                kind: DependencyKind::Output,
                id: WALL_OUTPUT_ID.to_owned(),
            }))
            .collect();
        let rendition = OptionalRendition::new(
            PreviewScope {
                profile_id: WALL_OUTPUT_ID.to_owned(),
                output_id: WALL_OUTPUT_ID.to_owned(),
                output_index: index,
                program_id: multiview_config::MAIN_PROGRAM.to_owned(),
                bus_id: WALL_BUS_ID.to_owned(),
            },
            video,
            dependencies,
            Vec::new(),
        )
        .with_producer(producer.clone());
        let registration = publisher
            .register_optional(vec![rendition])
            .map_err(|e| format!("register wall: {e:?}"))?;
        let period = self.frame_period();
        if !initial.is_empty() {
            let _ = producer.offer_complete(initial, period, Instant::now());
        }
        Ok(Session {
            factory,
            encoder,
            producer,
            _registration: registration,
            tick,
        })
    }

    fn encode_and_offer(
        session: &mut Session,
        picture: &Nv12Image,
        period: Duration,
    ) -> Result<(), ()> {
        let frame = crate::pipeline::nv12_to_decoded(picture).map_err(|_| ())?;
        let packets = session
            .encoder
            .encode_frame_at(frame, session.tick)
            .map_err(|_| ())?;
        session.tick = session.tick.saturating_add(1);
        if !packets.is_empty() {
            // Full ingress or a retired binding drops this window; the registry
            // fences recovery to the next keyframe. Never retried, never waited.
            let _ = session
                .producer
                .offer_complete(packets, period, Instant::now());
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn wall_is_off_unless_explicitly_enabled() {
        assert!(WallConfig::from_lookup(env(&[])).is_none());
        assert!(WallConfig::from_lookup(env(&[("MULTIVIEW_MOQ_WALL", "true")])).is_none());
        let on = WallConfig::from_lookup(env(&[("MULTIVIEW_MOQ_WALL", "1")])).unwrap();
        assert_eq!((on.width(), on.height()), (1280, 720));
    }

    #[test]
    fn malformed_or_out_of_range_values_disable_rather_than_default() {
        for bad in [
            ("MULTIVIEW_MOQ_WALL_COLUMNS", "x"),
            ("MULTIVIEW_MOQ_WALL_COLUMNS", "0"),
            ("MULTIVIEW_MOQ_WALL_CELL_WIDTH", "321"),
            ("MULTIVIEW_MOQ_WALL_DIVISOR", "0"),
            ("MULTIVIEW_MOQ_WALL_ENCODER", ""),
        ] {
            assert!(
                WallConfig::from_lookup(env(&[("MULTIVIEW_MOQ_WALL", "1"), bad])).is_none(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn cadence_is_an_exact_integer_division_of_production() {
        let config = WallConfig::default();
        assert_eq!(
            config.cadence(Rational::new(30_000, 1001)),
            Rational::new(30_000, 2002)
        );
        let run_period = Run {
            publisher: None,
            config: config.clone(),
            cadence: config.cadence(Rational::new(30_000, 1001)),
            stores: crate::live_sources::shared_stores(std::collections::HashMap::new()),
            geometry: crate::preview::wall_slot(),
            generation: 0,
            cells: Vec::new(),
            hook: None,
        }
        .frame_period();
        assert_eq!(run_period, Duration::from_nanos(66_733_333));
    }

    #[test]
    fn cells_are_sorted_row_major_and_capped_at_capacity() {
        let config = WallConfig {
            columns: 2,
            rows: 1,
            ..WallConfig::default()
        };
        let ids = vec!["zeta".to_owned(), "alpha".to_owned(), "mid".to_owned()];
        let cells = assign_cells(&config, &ids);
        assert_eq!(cells.len(), 2);
        assert_eq!(
            (cells[0].source.as_str(), cells[0].x, cells[0].y),
            ("alpha", 0, 0)
        );
        assert_eq!(
            (cells[1].source.as_str(), cells[1].x, cells[1].w),
            ("mid", 320, 320)
        );
    }

    #[test]
    fn compose_scales_each_source_into_its_own_cell_and_blanks_missing_ones() {
        let config = WallConfig {
            columns: 2,
            rows: 1,
            cell_width: 32,
            cell_height: 18,
            ..WallConfig::default()
        };
        let store = Arc::new(multiview_framestore::TileStore::<Nv12Image>::with_defaults(
            "a",
        ));
        let color = ColorInfo::default().resolve_defaults(64, 36);
        store.publish(
            Nv12Image::solid(64, 36, 200, 90, 160, color).unwrap(),
            multiview_core::time::MediaTime::ZERO,
        );
        let mut map = std::collections::HashMap::new();
        map.insert("a".to_owned(), store);
        map.insert(
            "b".to_owned(),
            Arc::new(multiview_framestore::TileStore::<Nv12Image>::with_defaults(
                "b",
            )),
        );
        let stores = crate::live_sources::shared_stores(map);
        let cells = assign_cells(&config, &["a".to_owned(), "b".to_owned()]);
        let wall = compose(&config, &cells, &stores).unwrap();
        assert_eq!((wall.width(), wall.height()), (64, 18));
        let y = wall.y_plane();
        assert_eq!(y[5 * 64 + 10], 200, "source a fills cell 0");
        assert_eq!(y[5 * 64 + 40], 16, "source b has no frame: black");
        let uv = wall.uv_plane();
        assert_eq!((uv[2 * 64 + 4], uv[2 * 64 + 5]), (90, 160));
        assert_eq!((uv[2 * 64 + 40], uv[2 * 64 + 41]), (128, 128));
    }

    #[test]
    fn a_dead_worker_withdraws_the_geometry() {
        let geometry = crate::preview::wall_slot();
        {
            let _guard = GeometryGuard(geometry.clone());
            geometry.store(Some(Arc::new(WallGeometry {
                generation: 1,
                width: 2,
                height: 2,
                cadence: WallCadence { num: 15, den: 1 },
                cells: Vec::new(),
                preview_source: None,
            })));
        }
        assert!(geometry.load().is_none());
    }

    const ISOLATION_CONFIG: &str = r##"
schema_version = 1
[canvas]
width = 64
height = 64
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
kind = "test"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"
[[outputs]]
kind = "hls"
path = "/tmp/multiview-wall-isolation/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;

    /// R3 chaos test (ADR-0103): the wall worker is wedged (or panics) on its
    /// first frame while the production run drives 40 ticks. The output clock
    /// must emit every tick with contiguous video PTS, and the wall must stay
    /// unavailable rather than half-published.
    fn drive_with_wedged_wall(hook: FrameHook) -> (u64, Vec<i64>, bool) {
        use crate::pipeline::{
            Pipeline, SendPolicy, StreamTestParams, TestSinkOutcome, TestSinkRunner,
        };
        let config = multiview_config::MultiviewConfig::load_from_toml(ISOLATION_CONFIG).unwrap();
        let mut pipeline = Pipeline::build(&config).unwrap();
        let stores = crate::live_sources::shared_stores(pipeline.preview_stores());
        let geometry = crate::preview::wall_slot();
        let wall = WallConfig {
            columns: 1,
            rows: 1,
            cell_width: 32,
            cell_height: 32,
            divisor: 1,
            ..WallConfig::default()
        };
        let _worker = spawn_inner(
            wall,
            Rational::new(25, 1),
            stores,
            pipeline.window_subscriptions(),
            geometry.clone(),
            Some(hook),
        )
        .unwrap();
        let pts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = pts.clone();
        let runner: TestSinkRunner = Box::new(move |rx| {
            let mut frames = 0;
            while let Ok(packet) = rx.recv() {
                frames += 1;
                if packet.kind() == multiview_ffmpeg::StreamKind::Video {
                    seen.lock().unwrap().push(packet.pts().unwrap());
                }
            }
            TestSinkOutcome { frames }
        });
        let clock = Arc::new(multiview_engine::ManualTimeSource::new());
        let stop_clock = Arc::new(AtomicBool::new(false));
        {
            let (clock, stop_clock) = (clock.clone(), stop_clock.clone());
            std::thread::spawn(move || {
                while !stop_clock.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(40));
                    clock.advance(Duration::from_millis(40));
                }
            });
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime
            .block_on(pipeline.drive_streaming_for_test(
                StreamTestParams {
                    time: clock,
                    pacer: multiview_engine::CooperativePacer,
                    max_ticks: Some(40),
                    policy: SendPolicy::BlockForExact,
                    runners: vec![runner],
                    hot_tick_observer: None,
                    consumer_fault_after_ticks: None,
                    shed_at_tick: None,
                },
                &multiview_engine::StopSignal::new(),
            ))
            .unwrap();
        stop_clock.store(true, Ordering::Release);
        let pts = pts.lock().unwrap().clone();
        (result.report.frames, pts, geometry.load().is_some())
    }

    #[test]
    fn a_blocked_wall_worker_costs_the_output_clock_zero_ticks() {
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (_release_tx, release_rx) = std::sync::mpsc::sync_channel::<()>(1);
        let hook: FrameHook = Box::new(move |_| {
            let _ = entered_tx.try_send(());
            // Wedge forever (until the test drops the sender at its end).
            let _ = release_rx.recv();
        });
        let (frames, pts, wall_published) = drive_with_wedged_wall(hook);
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the wall worker ran and is wedged");
        assert_eq!(frames, 40, "every output tick emitted");
        assert_eq!(pts, (0..40).collect::<Vec<_>>(), "contiguous PTS, no gap");
        assert!(!wall_published, "a wedged wall never publishes geometry");
    }

    #[test]
    fn a_panicking_wall_worker_disables_only_the_wall() {
        let hook: FrameHook = Box::new(|index| {
            assert!(index < 1, "injected wall worker panic");
        });
        let (frames, pts, wall_published) = drive_with_wedged_wall(hook);
        assert_eq!(frames, 40);
        assert_eq!(pts, (0..40).collect::<Vec<_>>());
        assert!(!wall_published, "the dead worker's geometry is withdrawn");
    }

    /// One real encoder, two subscribers: the wall registers once, both viewers
    /// receive the same H.264 windows, and the encoder count never changes.
    #[cfg(feature = "gpl-codecs")]
    #[test]
    #[allow(clippy::too_many_lines)] // One real-encoder scenario: two viewers, then a layout change.
    fn one_wall_encode_serves_two_subscribers() {
        use crate::output_window_registry::{EffectiveBinding, OutputWindowRegistry};
        let color = ColorInfo::default().resolve_defaults(64, 36);
        let store = Arc::new(multiview_framestore::TileStore::<Nv12Image>::with_defaults(
            "a",
        ));
        store.publish(
            Nv12Image::solid(64, 36, 120, 128, 128, color).unwrap(),
            multiview_core::time::MediaTime::ZERO,
        );
        let mut map = std::collections::HashMap::new();
        map.insert("a".to_owned(), store);
        let stores = crate::live_sources::shared_stores(map);
        let stores_handle = stores.clone();
        let (mut registry, factory) = OutputWindowRegistry::bare(Vec::<EffectiveBinding>::new(), 7);
        let slot = crate::output_window_registry::window_subscription_slot();
        let factory = Arc::new(factory);
        slot.store(Some(factory.clone()));
        let geometry = crate::preview::wall_slot();
        let wall = WallConfig {
            columns: 2,
            rows: 1,
            cell_width: 64,
            cell_height: 36,
            divisor: 1,
            bitrate_bps: 500_000,
            encoder: "libx264".to_owned(),
        };
        let _worker = spawn(wall, Rational::new(25, 1), stores, slot, geometry.clone()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while geometry.load().is_none() {
            assert!(Instant::now() < deadline, "wall never became ready");
            registry.pump(Instant::now());
            std::thread::sleep(Duration::from_millis(10));
        }
        let published = geometry.load().unwrap();
        assert_eq!((published.width, published.height), (128, 36));
        assert_eq!(published.cells.len(), 1);
        let scope = multiview_control::WhepScope::Output(WALL_OUTPUT_ID.to_owned());
        let binding = factory.resolve(&scope).unwrap();
        assert_eq!(binding.bus_id, WALL_BUS_ID);
        assert!(binding.audio.is_empty(), "the wall is video only");
        assert!(binding.video.codec.starts_with("avc1."));
        let first = factory.subscribe(&scope, factory.generation()).unwrap();
        let second = factory.subscribe(&scope, factory.generation()).unwrap();
        let mut counts = [0usize; 2];
        let deadline = Instant::now() + Duration::from_secs(10);
        while counts.iter().any(|c| *c < 5) {
            assert!(
                Instant::now() < deadline,
                "windows did not reach both viewers: {counts:?}"
            );
            registry.pump(Instant::now());
            for (sub, count) in [&first, &second].into_iter().zip(counts.iter_mut()) {
                while let Ok(Some(window)) = sub.try_recv_window() {
                    assert!(window
                        .permit
                        .packets()
                        .iter()
                        .all(|p| p.kind() == multiview_ffmpeg::StreamKind::Video));
                    *count += 1;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            factory
                .effective()
                .iter()
                .filter(|b| b.output_id == WALL_OUTPUT_ID)
                .count(),
            1,
            "two viewers, one registered wall encode"
        );
        // A new source changes the layout: the running encode retires (its
        // registration's dependencies were exactly the old cells) and one new
        // encode registers under a new generation with the new dependency set.
        let old_generation = published.generation;
        let old_index = binding.output_index;
        let added = Arc::new(multiview_framestore::TileStore::<Nv12Image>::with_defaults(
            "b",
        ));
        stores_handle.rcu(|map| {
            let mut next = (**map).clone();
            next.insert("b".to_owned(), added.clone());
            next
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(Instant::now() < deadline, "layout change never republished");
            registry.pump(Instant::now());
            if let Some(next) = geometry.load() {
                if next.generation != old_generation {
                    assert_eq!(next.cells.len(), 2);
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let rebound = factory.resolve(&scope).unwrap();
        assert!(
            rebound.output_index > old_index,
            "never reuses a routed index"
        );
        assert!(rebound
            .dependencies
            .iter()
            .any(|d| d.kind == DependencyKind::Source && d.id == "b"));
        assert_eq!(
            factory
                .effective()
                .iter()
                .filter(|b| b.output_id == WALL_OUTPUT_ID)
                .count(),
            1
        );
    }
}
