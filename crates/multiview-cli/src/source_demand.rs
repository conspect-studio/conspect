//! Which configured sources have a consumer, derived from the document alone
//! (`docs/architecture/demand-driven-sources.md`).
//!
//! Pure policy with no I/O: it decides what starts at boot and which standing
//! leases a source's own settings imply. Everything that appears at runtime (a
//! layout flip, a route, a preview) takes its lease where it happens.

use std::collections::BTreeMap;
#[cfg(feature = "ffmpeg")]
use std::sync::Arc;

use multiview_config::{MultiviewConfig, Source};
use multiview_core::layout::{Cell, Layout};
use multiview_engine::{
    ConsumerClass, ConsumerSet, ConsumerTag, DemandConfig, DemandManager, Rails, RequestedSize,
    SetItem,
};

/// What one source must run at boot.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct StartupDemand {
    /// The rails consumers want at boot.
    pub rails: Rails,
}

/// The manager tuning for `config`.
#[must_use]
pub fn demand_config(config: &MultiviewConfig) -> DemandConfig {
    let mut tuned = DemandConfig::default();
    if let Some(settings) = config.system.as_ref().and_then(|s| s.demand) {
        if let Some(linger) = settings.linger_secs {
            tuned.linger = std::time::Duration::from_secs(u64::from(linger));
        }
        if let Some(starts) = settings.max_concurrent_starts {
            tuned.max_concurrent_starts = usize::try_from(starts).unwrap_or(usize::MAX).max(1);
        }
    }
    tuned
}

/// The standing leases `source`'s own settings imply: `always_on`, explicitly
/// enabled journaling, and armed automation (the watched source plus its
/// replacement, so a take has a picture at once).
#[must_use]
pub fn standing_pins(source: &Source) -> Vec<(String, Rails, ConsumerTag)> {
    let mut pins = Vec::new();
    let policy = source.demand.unwrap_or_default();
    if policy.always_on {
        let tag = ConsumerTag::new(ConsumerClass::AlwaysOn, "always_on");
        pins.push((source.id.clone(), Rails::ALL, tag));
    }
    if policy.journal_events {
        let tag = ConsumerTag::new(ConsumerClass::EventJournal, "journal");
        pins.push((source.id.clone(), Rails::VIDEO, tag));
    }
    let rule = source
        .events
        .as_ref()
        .and_then(|events| events.automation.as_ref())
        .filter(|rule| rule.armed);
    if let Some(rule) = rule {
        let watch = ConsumerTag::new(ConsumerClass::EventAutomation, "watched");
        pins.push((source.id.clone(), Rails::VIDEO, watch));
        let target = ConsumerTag::new(ConsumerClass::EventAutomation, source.id.as_str());
        let rails = match rule.on_break_start.action {
            multiview_config::schema::BreakActionKind::SwapCell => Rails::VIDEO,
            _ => Rails::ALL,
        };
        pins.push((rule.on_break_start.target.clone(), rails, target));
    }
    pins
}

/// The audio-route consumers of a routing document: any route names a consumer,
/// even a muted one, because a route is also how its meter reaches an operator.
#[must_use]
pub fn audio_route_items(routing: Option<&multiview_config::AudioRouting>) -> Vec<SetItem> {
    routing
        .map(|r| r.routes.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|route| SetItem {
            id: route.input_id.as_str().into(),
            label: route.target_track.as_deref().unwrap_or("program").into(),
            size: None,
        })
        .collect()
}

/// Convert a normalized fraction in `[0,1]` to a pixel count in `[0, extent]`,
/// `as`-cast-free (the guardrails deny `as_conversions`).
pub(crate) fn frac_to_px(frac: f32, extent: u32) -> u32 {
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

/// Convert a cell's normalized `w`/`h` into even pixel dimensions (NV12 needs
/// even extents), clamped to at least 2x2.
pub(crate) fn cell_dims(cell: &Cell, canvas_w: u32, canvas_h: u32) -> (u32, u32) {
    let to_even_px = |frac: f32, extent: u32| -> u32 {
        let raw = frac_to_px(frac, extent);
        let even = raw & !1; // round down to even
        even.max(2)
    };
    (to_even_px(cell.w, canvas_w), to_even_px(cell.h, canvas_h))
}

/// The layout consumers of `layout`: one per source-bound cell, sized to the
/// cell so a shared decode is sized to its largest consumer.
#[must_use]
pub fn layout_items_for(layout: &Layout) -> Vec<SetItem> {
    layout_items(layout.cells.iter().map(|cell| {
        let (width, height) = cell_dims(cell, layout.canvas.width, layout.canvas.height);
        (
            cell.source.as_deref(),
            Some(RequestedSize { width, height }),
        )
    }))
}

/// The command drain's demand seam: it republishes the layout consumers when
/// the active layout's bindings change. Every call is wait-free.
#[derive(Clone, Debug)]
pub struct DrainDemand<T> {
    /// The active layout's consumers.
    pub layout: ConsumerSet<T>,
    /// The subtitle routes' consumers.
    pub subtitles: ConsumerSet<T>,
    /// The manager, for restarting an edited source.
    pub manager: DemandManager<T>,
    /// The configured-source catalogue the rails read.
    pub catalogue: crate::source_rails::Catalogue,
}

/// One consumer item per source-bound cell of `cells`, in order.
pub fn layout_items<'a>(
    cells: impl Iterator<Item = (Option<&'a str>, Option<RequestedSize>)>,
) -> Vec<SetItem> {
    cells
        .enumerate()
        .filter_map(|(index, (source, size))| {
            source.map(|id| SetItem {
                id: id.into(),
                label: format!("cell-{}", index.saturating_add(1)).into(),
                size,
            })
        })
        .collect()
}

/// Everything that must run at boot. A source absent from the result costs
/// nothing: no probe, no plan, no thread.
///
/// `bound` is the solved layout's cell bindings.
#[must_use]
pub fn startup_demand<'a>(
    config: &MultiviewConfig,
    bound: impl Iterator<Item = &'a str>,
) -> BTreeMap<String, StartupDemand> {
    let mut demand: BTreeMap<String, StartupDemand> = BTreeMap::new();
    let mut want = |id: &str, rails: Rails| {
        let entry = demand.entry(id.to_owned()).or_default();
        entry.rails = entry.rails.union(rails);
    };
    // A cell's tile meter reads its source's audio, so a cell consumes both.
    for id in bound {
        want(id, Rails::ALL);
    }
    for item in audio_route_items(config.audio.as_ref()) {
        want(&item.id, Rails::AUDIO);
    }
    for source in &config.sources {
        for (id, rails, _) in standing_pins(source) {
            want(&id, rails);
        }
    }
    demand
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(extra_sources: &str, tail: &str) -> MultiviewConfig {
        let doc = format!(
            r##"schema_version = 1
[canvas]
width = 640
height = 360
fps = "25/1"
pixel_format = "nv12"
background = "#000000"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]
[[sources]]
id = "bound"
kind = "bars"
[[sources]]
id = "unused"
kind = "hls"
url = "https://example.test/u.m3u8"
{extra_sources}
[[cells]]
id = "cell"
area = "a"
[cells.source]
input_id = "bound"
{tail}
"##
        );
        MultiviewConfig::load_from_toml(&doc).expect("fixture")
    }

    fn demand(config: &MultiviewConfig) -> BTreeMap<String, StartupDemand> {
        startup_demand(config, std::iter::once("bound"))
    }

    #[test]
    fn an_unconsumed_source_has_no_demand_and_a_bound_one_gets_both_rails() {
        let demand = demand(&config("", ""));
        assert_eq!(demand["bound"].rails, Rails::ALL);
        assert!(!demand.contains_key("unused"));
    }

    #[test]
    fn a_routed_unbound_source_runs_audio_only() {
        // The production shape: msnow's audio is routed while its video is unbound.
        let tail = r#"[audio]
sample_rate_hz = 48000
[[audio.routes]]
input_id = "msnow"
channels = { kind = "stereo" }
include_in_program_bus = true
"#;
        let extra =
            "[[sources]]\nid = \"msnow\"\nkind = \"hls\"\nurl = \"https://example.test/m.m3u8\"";
        let demand = demand(&config(extra, tail));
        assert_eq!(demand["msnow"].rails, Rails::AUDIO);
        assert!(!demand.contains_key("unused"));
    }

    #[test]
    fn always_on_and_journaling_are_explicit_consumers() {
        let extra = r#"[[sources]]
id = "pinned"
kind = "hls"
url = "https://example.test/p.m3u8"
demand = { always_on = true }
[[sources]]
id = "logged"
kind = "hls"
url = "https://example.test/l.m3u8"
demand = { journal_events = true }"#;
        let demand = demand(&config(extra, ""));
        assert_eq!(demand["pinned"].rails, Rails::ALL);
        assert_eq!(demand["logged"].rails, Rails::VIDEO);
    }

    #[test]
    fn observation_alone_is_not_a_consumer_but_armed_automation_is() {
        let observed = r#"[[sources]]
id = "watched"
kind = "hls"
url = "https://example.test/w.m3u8"
events = { observe = true }"#;
        assert!(!demand(&config(observed, "")).contains_key("watched"));
        let armed = |armed: bool| {
            format!(
                r#"[[sources]]
id = "slate"
kind = "hls"
url = "https://example.test/s.m3u8"
[[sources]]
id = "watched"
kind = "hls"
url = "https://example.test/w.m3u8"
[sources.events.automation]
armed = {armed}
restore_on_end = true
max_duration_s = 60
[sources.events.automation.on_break_start]
action = "swap_cell"
target = "slate""#
            )
        };
        let disarmed = demand(&config(&armed(false), ""));
        assert!(!disarmed.contains_key("watched"));
        assert!(!disarmed.contains_key("slate"));
        let live = demand(&config(&armed(true), ""));
        assert_eq!(live["watched"].rails, Rails::VIDEO);
        assert_eq!(live["slate"].rails, Rails::VIDEO);
    }

    #[test]
    fn linger_and_start_concurrency_come_from_system_demand() {
        let tuned = demand_config(&config(
            "",
            "[system.demand]\nlinger_secs = 5\nmax_concurrent_starts = 2\n",
        ));
        assert_eq!(tuned.linger, std::time::Duration::from_secs(5));
        assert_eq!(tuned.max_concurrent_starts, 2);
        let default = demand_config(&config("", ""));
        assert_eq!(default.linger, multiview_engine::DEFAULT_LINGER);
    }

    #[test]
    fn layout_items_skip_unbound_cells_and_label_by_position() {
        let items = layout_items([(Some("a"), None), (None, None), (Some("a"), None)].into_iter());
        assert_eq!(items.len(), 2);
        assert_eq!(&*items[1].label, "cell-3");
    }
}

/// The control plane's view of a run's demand manager.
#[cfg(feature = "ffmpeg")]
pub struct ControlDemand {
    manager: multiview_engine::DemandManager<multiview_compositor::pipeline::Nv12Image>,
    catalogue: crate::source_rails::Catalogue,
}

#[cfg(feature = "ffmpeg")]
impl ControlDemand {
    /// Wrap `manager` over `catalogue`.
    #[must_use]
    pub fn new(
        manager: multiview_engine::DemandManager<multiview_compositor::pipeline::Nv12Image>,
        catalogue: crate::source_rails::Catalogue,
    ) -> Self {
        Self { manager, catalogue }
    }
}

/// How long one JPEG poll keeps a source wanted. The preview polls every
/// second, so a closed preview releases its source a few seconds later.
#[cfg(feature = "ffmpeg")]
pub const PREVIEW_POLL_LEASE: std::time::Duration = std::time::Duration::from_secs(5);

#[cfg(feature = "ffmpeg")]
struct HeldLease(
    #[allow(dead_code)] multiview_engine::DemandLease<multiview_compositor::pipeline::Nv12Image>,
);

#[cfg(feature = "ffmpeg")]
impl multiview_control::SourceDemandLease for HeldLease {}

/// Map an engine status onto the control-plane view.
#[must_use]
pub fn view_of(status: &multiview_engine::DemandStatus) -> multiview_control::SourceDemandView {
    use multiview_control::SourceDemandPhase as P;
    use multiview_engine::DemandPhase as D;
    multiview_control::SourceDemandView {
        id: status.id.clone(),
        phase: match status.phase {
            D::Idle => P::Idle,
            D::Starting => P::Starting,
            D::Live => P::Live,
            D::Failed => P::Failed,
        },
        video_wanted: status.wanted.video,
        audio_wanted: status.wanted.audio,
        video_running: status.running.video,
        audio_running: status.running.audio,
        consumers: status
            .consumers
            .iter()
            .map(|tag| multiview_control::SourceConsumerView {
                kind: tag.class.as_str().to_owned(),
                label: tag.label.to_string(),
            })
            .collect(),
        detail: status.detail.clone(),
    }
}

#[cfg(feature = "ffmpeg")]
impl multiview_control::SourceDemandProvider for ControlDemand {
    fn status(&self, id: &str) -> multiview_control::SourceDemandView {
        view_of(&self.manager.status(id))
    }

    fn active(&self) -> Vec<multiview_control::SourceDemandView> {
        self.manager.snapshot().iter().map(view_of).collect()
    }

    fn preview_lease(
        &self,
        id: &str,
        label: &str,
    ) -> Option<Box<dyn multiview_control::SourceDemandLease>> {
        if !self.catalogue.load().contains_key(id) {
            return None;
        }
        let tag = ConsumerTag::new(ConsumerClass::Preview, label);
        Some(Box::new(HeldLease(self.manager.acquire(
            id,
            Rails::VIDEO,
            tag,
            None,
        ))))
    }

    fn touch_preview(&self, id: &str) -> Option<multiview_control::SourceDemandPhase> {
        if !self.catalogue.load().contains_key(id) {
            return None;
        }
        let tag = ConsumerTag::new(ConsumerClass::Preview, "jpeg");
        self.manager
            .touch(id, Rails::VIDEO, tag, PREVIEW_POLL_LEASE)
            .map(|phase| {
                view_of(&multiview_engine::DemandStatus {
                    id: id.to_owned(),
                    phase,
                    wanted: Rails::VIDEO,
                    running: Rails::NONE,
                    consumers: Vec::new(),
                    detail: None,
                })
                .phase
            })
    }
}

/// The control-plane demand provider of the current run, installed by the
/// binary before it serves. Process-wide: one run per process.
fn control_demand_slot() -> &'static std::sync::Mutex<Option<multiview_control::SharedSourceDemand>>
{
    static SLOT: std::sync::OnceLock<
        std::sync::Mutex<Option<multiview_control::SharedSourceDemand>>,
    > = std::sync::OnceLock::new();
    SLOT.get_or_init(|| std::sync::Mutex::new(None))
}

/// Install (or with `None` clear) the run's control-plane demand provider.
pub fn install_control_demand(demand: Option<multiview_control::SharedSourceDemand>) {
    *control_demand_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = demand;
}

/// The installed control-plane demand provider, if any.
#[must_use]
pub fn installed_control_demand() -> Option<multiview_control::SharedSourceDemand> {
    control_demand_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// The running pipeline's drain seam, installed when its run starts, for drains
/// built without one. Process-wide: one run per process.
#[cfg(feature = "ffmpeg")]
fn drain_demand_slot() -> &'static multiview_framestore::SwapCell<
    Option<Arc<DrainDemand<multiview_compositor::pipeline::Nv12Image>>>,
> {
    static SLOT: std::sync::OnceLock<
        multiview_framestore::SwapCell<
            Option<Arc<DrainDemand<multiview_compositor::pipeline::Nv12Image>>>,
        >,
    > = std::sync::OnceLock::new();
    SLOT.get_or_init(multiview_framestore::SwapCell::empty)
}

/// Install (or with `None` clear) the running pipeline's drain seam.
#[cfg(feature = "ffmpeg")]
pub fn install_drain_demand(
    demand: Option<DrainDemand<multiview_compositor::pipeline::Nv12Image>>,
) {
    drain_demand_slot().store(demand.map(Arc::new));
}

/// The running pipeline's drain seam. A wait-free load.
#[cfg(feature = "ffmpeg")]
#[must_use]
pub fn installed_drain_demand() -> Option<DrainDemand<multiview_compositor::pipeline::Nv12Image>> {
    drain_demand_slot().load().map(|demand| (*demand).clone())
}
