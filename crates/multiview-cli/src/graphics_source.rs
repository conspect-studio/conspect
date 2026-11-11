//! Graphics (CG) sources: a prepared graphics item (`kind = "graphic"`) and a
//! playout channel bus (`kind = "graphics_channel"`).
//!
//! Efficiency rules (docs/architecture/graphics.md):
//!
//! * **Render on change, off the output clock.** An item is rendered on the
//!   source's own generator thread only when its definition (template, data
//!   binding, style) or its bound data revision changes. The prepared
//!   `Arc<Nv12Image>` is republished every tick at no cost.
//! * **Opacity-only transitions.** TAKE / CLEAR with a mix is a pure function
//!   of the generator's own tick counter ([`multiview_graphics::mix_at`]):
//!   the first tick that observes a new program `seq` latches the start tick;
//!   every later tick picks a prepared layer and an opacity. A settled frame
//!   is the prepared `Arc` itself (zero work). A mid-fade tick is exactly one
//!   opacity application on one prepared image (a 256-entry LUT over the alpha
//!   plane plus a plane copy) and is memoized while the opacity is unchanged.
//!   No template is re-rendered during a transition.
//! * **Bounded caches.** At most the two items a transition needs (from/to)
//!   plus one transparent "clear" frame are kept; everything else is pruned
//!   each tick.
//! * **Never block.** Hub and weather reads are `try_read`; on contention the
//!   last published frame is held.

use std::collections::BTreeMap;
use std::sync::Arc;

use multiview_compositor::pipeline::{AlphaPlanePool, CanvasColor, Nv12Image};
use multiview_config::{GraphicsBus, GraphicsItem, SourceKind};
use multiview_graphics::{mix_at, scale_alpha, GraphicsHub, Layer};

use crate::synth::SynthError;

/// What a graphics source shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphicsTarget {
    /// One item, full-time.
    Item(String),
    /// A channel's program or preview bus.
    Channel {
        /// Channel id.
        channel: String,
        /// Which bus.
        bus: GraphicsBus,
    },
}

impl GraphicsTarget {
    /// The graphics target of a config source kind, if any.
    #[must_use]
    pub fn from_source_kind(kind: &SourceKind) -> Option<Self> {
        match kind {
            SourceKind::Graphic { item } => Some(Self::Item(item.clone())),
            SourceKind::GraphicsChannel { channel, bus } => Some(Self::Channel {
                channel: channel.clone(),
                bus: *bus,
            }),
            _ => None,
        }
    }
}

/// The item renderer seam (production: [`render_item`]; tests: a counter).
pub type ItemRenderer =
    Box<dyn FnMut(&GraphicsItem, u32, u32, CanvasColor) -> Result<Nv12Image, SynthError> + Send>;

/// The data revision an item's picture depends on (`None` = momentarily
/// unknown: keep the cache).
pub type DataRevision = Box<dyn Fn(&GraphicsItem) -> Option<i64> + Send>;

#[derive(Debug)]
struct Prepared {
    def: GraphicsItem,
    data_rev: i64,
    image: Arc<Nv12Image>,
}

/// Per-source generator state (lives on the source's generator thread).
pub struct GraphicsGen {
    target: GraphicsTarget,
    hub: GraphicsHub,
    width: u32,
    height: u32,
    canvas: CanvasColor,
    render: ItemRenderer,
    data_rev: DataRevision,
    prepared: BTreeMap<String, Prepared>,
    clear: Option<Arc<Nv12Image>>,
    /// `(seq, start_tick)` of the latest observed program change; a `None`
    /// start means "already settled" (the state this generator booted into).
    latched: Option<(u64, Option<u64>)>,
    /// Memo of the last faded frame: `(layer item id, opacity)`.
    faded: Option<(String, u8, Arc<Nv12Image>)>,
    last: Option<Arc<Nv12Image>>,
    alpha_pool: Arc<AlphaPlanePool>,
    scratch: Vec<u8>,
    /// Template renders performed (observability + tests).
    pub renders: u64,
}

impl std::fmt::Debug for GraphicsGen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphicsGen")
            .field("target", &self.target)
            .field("renders", &self.renders)
            .finish_non_exhaustive()
    }
}

impl GraphicsGen {
    /// A generator over the process-global hub with the production renderer.
    #[must_use]
    pub fn production(
        target: GraphicsTarget,
        width: u32,
        height: u32,
        canvas: CanvasColor,
    ) -> Self {
        Self::new(
            target,
            GraphicsHub::global().clone(),
            width,
            height,
            canvas,
            Box::new(render_item),
            Box::new(item_data_revision),
        )
    }

    /// A generator with explicit seams.
    #[must_use]
    pub fn new(
        target: GraphicsTarget,
        hub: GraphicsHub,
        width: u32,
        height: u32,
        canvas: CanvasColor,
        render: ItemRenderer,
        data_rev: DataRevision,
    ) -> Self {
        Self {
            target,
            hub,
            width,
            height,
            canvas,
            render,
            data_rev,
            prepared: BTreeMap::new(),
            clear: None,
            latched: None,
            faded: None,
            last: None,
            alpha_pool: Arc::new(AlphaPlanePool::default()),
            scratch: Vec::new(),
            renders: 0,
        }
    }

    fn clear_frame(&mut self) -> Option<Arc<Nv12Image>> {
        if self.clear.is_none() {
            let base = Nv12Image::solid_rgb(self.width, self.height, 0, 0, 0, self.canvas).ok()?;
            let alpha = vec![0_u8; base.y_plane().len()];
            let image = self.alpha_pool.attach(base, &alpha).ok()?;
            self.clear = Some(Arc::new(image));
        }
        self.clear.clone()
    }

    /// Ensure `id` is prepared (rendering only when its definition or data
    /// changed) and return it; `None` when the item is unknown or cannot be
    /// rendered (the caller shows clear).
    fn prepared(&mut self, id: &str, def: Option<&GraphicsItem>) -> Option<Arc<Nv12Image>> {
        let def = def?;
        let rev = (self.data_rev)(def);
        if let Some(p) = self.prepared.get(id) {
            if p.def == *def && rev.is_none_or(|r| r == p.data_rev) {
                return Some(Arc::clone(&p.image));
            }
        }
        match (self.render)(def, self.width, self.height, self.canvas) {
            Ok(image) => {
                self.renders += 1;
                let image = Arc::new(image);
                self.prepared.insert(
                    id.to_owned(),
                    Prepared {
                        def: def.clone(),
                        data_rev: rev.unwrap_or(-1),
                        image: Arc::clone(&image),
                    },
                );
                if self.faded.as_ref().is_some_and(|(f, _, _)| f == id) {
                    self.faded = None;
                }
                Some(image)
            }
            Err(e) => {
                tracing::warn!(item = %id, error = %e, "graphics item render failed; holding last good");
                self.prepared.get(id).map(|p| Arc::clone(&p.image))
            }
        }
    }

    fn fade(&mut self, id: &str, image: &Arc<Nv12Image>, opacity: u8) -> Option<Arc<Nv12Image>> {
        if opacity == u8::MAX {
            return Some(Arc::clone(image));
        }
        if let Some((fid, fop, f)) = &self.faded {
            if fid == id && *fop == opacity {
                return Some(Arc::clone(f));
            }
        }
        let len = image.y_plane().len();
        scale_alpha(image.alpha_plane(), len, opacity, &mut self.scratch);
        let base = Nv12Image::new(
            image.width(),
            image.height(),
            image.y_plane().to_vec(),
            image.uv_plane().to_vec(),
            image.color(),
        )
        .ok()?;
        let faded = Arc::new(self.alpha_pool.attach(base, &self.scratch).ok()?);
        self.faded = Some((id.to_owned(), opacity, Arc::clone(&faded)));
        Some(faded)
    }

    /// The frame for output tick `tick` (the generator's own monotonically
    /// increasing counter). `None` only if even the clear frame cannot be
    /// built (geometry error).
    pub fn frame(&mut self, tick: u64) -> Option<Arc<Nv12Image>> {
        let Some((_, config)) = self.hub.try_config() else {
            return self.last.clone();
        };
        let frame = match self.target.clone() {
            GraphicsTarget::Item(id) => {
                let def = config.item(&id).cloned();
                self.prune(&[id.as_str()]);
                self.prepared(&id, def.as_ref())
                    .or_else(|| self.clear_frame())
            }
            GraphicsTarget::Channel { channel, bus } => {
                let Ok(snap) = self.hub.try_channel(&channel) else {
                    return self.last.clone();
                };
                let Some(snap) = snap else {
                    self.prune(&[]);
                    let f = self.clear_frame();
                    self.last.clone_from(&f);
                    return f;
                };
                if bus == GraphicsBus::Program {
                    let elapsed = match self.latched {
                        Some((seq, start)) if seq == snap.seq => {
                            start.map_or(u64::MAX, |s| tick.saturating_sub(s))
                        }
                        // A fresh generator shows the settled state.
                        None => {
                            self.latched = Some((snap.seq, None));
                            u64::MAX
                        }
                        // A new program change ramps from this tick.
                        Some(_) => {
                            self.latched = Some((snap.seq, Some(tick)));
                            0
                        }
                    };
                    let t = &snap.transition;
                    let from = t.from.clone();
                    let to = snap.program.clone();
                    let keep: Vec<&str> = [from.as_deref(), to.as_deref()]
                        .into_iter()
                        .flatten()
                        .collect();
                    self.prune(&keep);
                    let mix = mix_at(from.is_some(), to.is_some(), t.kind, t.frames, elapsed);
                    let layer = match mix.layer {
                        Some(Layer::From) => from,
                        Some(Layer::To) => to,
                        None => None,
                    };
                    match layer {
                        Some(id) => {
                            let def = config.item(&id).cloned();
                            match self.prepared(&id, def.as_ref()) {
                                Some(img) => self.fade(&id, &img, mix.opacity),
                                None => self.clear_frame(),
                            }
                        }
                        None => self.clear_frame(),
                    }
                } else {
                    let id = snap.preview.clone();
                    self.prune(&[id.as_deref().unwrap_or("")]);
                    id.and_then(|id| {
                        let def = config.item(&id).cloned();
                        self.prepared(&id, def.as_ref())
                    })
                    .or_else(|| self.clear_frame())
                }
            }
        };
        self.last.clone_from(&frame);
        frame
    }

    fn prune(&mut self, keep: &[&str]) {
        self.prepared.retain(|id, _| keep.contains(&id.as_str()));
        if self
            .faded
            .as_ref()
            .is_some_and(|(id, _, _)| !keep.contains(&id.as_str()))
        {
            self.faded = None;
        }
    }
}

/// Seed the process-global hub with `config`'s effective graphics and return
/// a handle to it.
#[must_use]
pub fn seed_global_hub(config: &multiview_config::MultiviewConfig) -> GraphicsHub {
    let hub = GraphicsHub::global().clone();
    hub.set_config(multiview_control::routes::graphics::effective_graphics(
        config,
    ));
    hub
}

/// The weather data revision an item depends on.
#[must_use]
pub fn item_data_revision(item: &GraphicsItem) -> Option<i64> {
    match item.data_source.as_deref() {
        Some(ds) => crate::weather_source::render_key(ds),
        None => Some(0),
    }
}

fn hex(s: Option<&str>) -> Option<u32> {
    let s = s?.strip_prefix('#')?;
    u32::from_str_radix(s, 16).ok()
}

/// Render one item from its template + bound data + style.
///
/// # Errors
/// [`SynthError`] for an unknown template, an `overlay`-less build, or a
/// compositor failure.
pub fn render_item(
    item: &GraphicsItem,
    width: u32,
    height: u32,
    canvas: CanvasColor,
) -> Result<Nv12Image, SynthError> {
    let template = match item.template.as_str() {
        "weather.seven_day" => multiview_config::WeatherTemplate::SevenDay,
        "weather.today" => multiview_config::WeatherTemplate::Today,
        other => {
            return Err(SynthError::Compositor(format!(
                "unknown graphics template {other:?}"
            )))
        }
    };
    let data = item
        .data_source
        .as_deref()
        .and_then(crate::weather_source::snapshot);
    #[cfg(feature = "overlay")]
    {
        crate::weather_source::render_weather_styled(
            data.as_deref(),
            template,
            item.style.background,
            hex(item.style.accent.as_deref()),
            width,
            height,
            canvas,
        )
    }
    #[cfg(not(feature = "overlay"))]
    {
        let _ = (data, template, hex(None), width, height, canvas);
        Err(SynthError::OverlayRequired)
    }
}

/// The binary's thumbnail renderer: render the item off-clock (on a blocking
/// task) and JPEG-encode it.
#[derive(Debug, Default)]
pub struct CliThumbnailer;

impl multiview_control::routes::graphics::GraphicsThumbnailer for CliThumbnailer {
    fn item_jpeg(&self, item: &GraphicsItem) -> Option<Vec<u8>> {
        use multiview_preview::JpegEncoder;
        let image = render_item(item, 640, 360, CanvasColor::default()).ok()?;
        let mut plane = Vec::with_capacity(image.y_plane().len() + image.uv_plane().len());
        plane.extend_from_slice(image.y_plane());
        plane.extend_from_slice(image.uv_plane());
        multiview_preview::Nv12JpegEncoder::new()
            .encode_nv12(&plane, image.width(), image.height(), 80)
            .ok()
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::many_single_char_names
)]
mod tests {
    use super::*;
    use multiview_config::{
        GraphicsChannel, GraphicsConfig, GraphicsDataSource, Transition, TransitionKind,
        WeatherLocation,
    };
    use std::sync::atomic::{AtomicU64, Ordering};

    fn hub() -> GraphicsHub {
        let mut c = GraphicsConfig::default();
        c.data_sources
            .push(GraphicsDataSource::from_weather_location(
                &WeatherLocation::new("syd", "Sydney", "IDN11060", "NSW_PT131"),
            ));
        for id in ["a", "b"] {
            c.items.push(GraphicsItem::new(
                id,
                id,
                "weather.today",
                Some("syd".into()),
            ));
        }
        let mut ch = GraphicsChannel::new("gfx-channel-1", "Ch 1");
        ch.rundown = vec!["a".into(), "b".into()];
        ch.transition = Transition::new(TransitionKind::Mix, 4);
        c.channels.push(ch);
        GraphicsHub::new(c)
    }

    /// A fake renderer: an opaque solid image whose luma encodes the item.
    fn gen(hub: &GraphicsHub, target: GraphicsTarget, renders: Arc<AtomicU64>) -> GraphicsGen {
        GraphicsGen::new(
            target,
            hub.clone(),
            16,
            8,
            CanvasColor::default(),
            Box::new(move |item, w, h, canvas| {
                renders.fetch_add(1, Ordering::Relaxed);
                let v = if item.id == "a" { 200 } else { 50 };
                Nv12Image::solid_rgb(w, h, v, v, v, canvas)
                    .map_err(|e| SynthError::Compositor(e.to_string()))
            }),
            Box::new(|_| Some(7)),
        )
    }

    fn alpha0(img: &Nv12Image) -> u8 {
        img.alpha_plane().map_or(255, |a| a[0])
    }

    #[test]
    fn channel_program_prepares_once_and_ramps_opacity_from_ticks() {
        let hub = hub();
        let renders = Arc::new(AtomicU64::new(0));
        let mut g = gen(
            &hub,
            GraphicsTarget::Channel {
                channel: "gfx-channel-1".into(),
                bus: GraphicsBus::Program,
            },
            Arc::clone(&renders),
        );
        // Clear: transparent.
        let f = g.frame(0).unwrap();
        assert_eq!(alpha0(&f), 0);
        hub.cue("gfx-channel-1", "a").unwrap();
        hub.take("gfx-channel-1", None).unwrap();
        // Ticks 10..=14 after the take: 63, 127, 191, 255, 255.
        let ops: Vec<u8> = (10..15).map(|t| alpha0(&g.frame(t).unwrap())).collect();
        assert_eq!(ops, vec![63, 127, 191, 255, 255]);
        assert_eq!(renders.load(Ordering::Relaxed), 1, "rendered exactly once");
        // Settled: the prepared Arc itself is republished (no copy).
        let a = g.frame(20).unwrap();
        let b = g.frame(21).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert!(
            a.alpha_plane().is_none(),
            "opaque item settles without alpha"
        );
        // Same tick again → deterministic, memoized.
        hub.clear("gfx-channel-1", None).unwrap();
        let x = g.frame(30).unwrap();
        let y = g.frame(30).unwrap();
        assert!(Arc::ptr_eq(&x, &y));
        assert_eq!(alpha0(&x), 191);
        assert_eq!(alpha0(&g.frame(33).unwrap()), 0);
        assert_eq!(alpha0(&g.frame(34).unwrap()), 0, "cleared → transparent");
        assert_eq!(
            renders.load(Ordering::Relaxed),
            1,
            "fade out re-used the prepared image"
        );
    }

    #[test]
    fn preview_bus_and_item_targets() {
        let hub = hub();
        let renders = Arc::new(AtomicU64::new(0));
        let mut pvw = gen(
            &hub,
            GraphicsTarget::Channel {
                channel: "gfx-channel-1".into(),
                bus: GraphicsBus::Preview,
            },
            Arc::clone(&renders),
        );
        assert_eq!(alpha0(&pvw.frame(0).unwrap()), 0);
        hub.cue("gfx-channel-1", "b").unwrap();
        let f = pvw.frame(1).unwrap();
        assert_eq!(f.y_plane()[0], pvw.frame(2).unwrap().y_plane()[0]);
        assert_eq!(alpha0(&f), 255);
        let mut item = gen(&hub, GraphicsTarget::Item("a".into()), Arc::clone(&renders));
        let a1 = item.frame(0).unwrap();
        let a2 = item.frame(1).unwrap();
        assert!(Arc::ptr_eq(&a1, &a2));
        let mut ghost = gen(&hub, GraphicsTarget::Item("zz".into()), renders);
        assert_eq!(alpha0(&ghost.frame(0).unwrap()), 0, "unknown item is clear");
    }

    #[test]
    fn deleting_on_air_item_goes_clear() {
        let hub = hub();
        let renders = Arc::new(AtomicU64::new(0));
        let mut g = gen(
            &hub,
            GraphicsTarget::Channel {
                channel: "gfx-channel-1".into(),
                bus: GraphicsBus::Program,
            },
            renders,
        );
        hub.cue("gfx-channel-1", "a").unwrap();
        hub.take(
            "gfx-channel-1",
            Some(Transition::new(TransitionKind::Cut, 1)),
        )
        .unwrap();
        assert_eq!(alpha0(&g.frame(1).unwrap()), 255);
        let mut c = (*hub.config()).clone();
        c.items.retain(|i| i.id != "a");
        c.channels[0].rundown.retain(|i| i != "a");
        hub.set_config(c);
        assert_eq!(alpha0(&g.frame(2).unwrap()), 0);
    }

    #[cfg(feature = "overlay")]
    #[test]
    fn real_weather_template_renders_with_style() {
        let mut item = GraphicsItem::new("x", "X", "weather.today", Some("nowhere".into()));
        item.style.background = multiview_config::WeatherBackground::Transparent;
        item.style.accent = Some("#ff0000".into());
        let img = render_item(&item, 320, 180, CanvasColor::default()).unwrap();
        assert!(
            img.alpha_plane().is_some(),
            "transparent style carries alpha"
        );
        item.template = "lower_third.basic".into();
        assert!(render_item(&item, 320, 180, CanvasColor::default()).is_err());
    }
}
