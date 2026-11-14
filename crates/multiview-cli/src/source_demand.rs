//! Which configured sources have a consumer, derived from the document alone
//! (`docs/architecture/demand-driven-sources.md`).
//!
//! Pure policy with no I/O: it decides what starts at boot and which standing
//! leases a source's own settings imply. `activation = "always_on"` (the
//! default) is a standing lease, so every source starts at boot as it always
//! has; only `on_demand` sources wait for a consumer. Everything that appears
//! at runtime (a layout flip, a route, a preview) takes its lease where it
//! happens.

use std::collections::BTreeMap;

use multiview_config::{MultiviewConfig, Source};
use multiview_core::layout::Layout;
use multiview_engine::{ConsumerClass, ConsumerTag, DemandConfig, Rails, RequestedSize, SetItem};

/// What one source must run at boot.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct StartupDemand {
    /// The rails consumers want at boot.
    pub rails: Rails,
}

/// One standing lease: the leased source, its rails and its tag.
pub type StandingPin = (String, Rails, ConsumerTag);

/// The manager tuning for `config`.
#[must_use]
pub fn demand_config(config: &MultiviewConfig) -> DemandConfig {
    let mut tuned = DemandConfig::default();
    if let Some(settings) = config.system.as_ref().and_then(|s| s.demand) {
        if let Some(close_after) = settings.close_after_secs {
            tuned.close_after = std::time::Duration::from_secs(u64::from(close_after));
        }
        if let Some(timeout) = settings.start_timeout_secs {
            tuned.start_timeout = std::time::Duration::from_secs(u64::from(timeout.max(1)));
        }
        if let Some(starts) = settings.max_concurrent_starts {
            tuned.max_concurrent_starts = usize::try_from(starts).unwrap_or(usize::MAX).max(1);
        }
        if let Some(stuck) = settings.max_stuck_opens {
            tuned.max_stuck_opens = usize::try_from(stuck).unwrap_or(usize::MAX).max(1);
        }
    }
    tuned
}

/// The standing leases `source`'s own settings imply: `always_on` activation
/// (the default) and armed automation (the watched source plus its
/// replacement, so a take has a picture at once).
///
/// There is no separate journaling pin: event observation rides the video
/// rail, so it runs whenever the source is ready. An `always_on` source is
/// always ready, which keeps its event history.
#[must_use]
pub fn standing_pins(source: &Source) -> Vec<StandingPin> {
    let mut pins = Vec::new();
    if matches!(source.kind, multiview_config::SourceKind::Aes67 { .. }) {
        // AES67 is a local multicast join outside demand; it always runs.
        return pins;
    }
    if !source.is_on_demand() {
        // Exactly today's boot set: the picture of every source, and its audio
        // only where boot built audio before (audio-bearing kinds).
        let tag = ConsumerTag::new(ConsumerClass::AlwaysOn, "always_on");
        pins.push((source.id.clone(), Rails::ALL, tag));
    }
    let rule = source
        .events
        .as_ref()
        .and_then(|events| events.automation.as_ref())
        .filter(|rule| rule.armed);
    if let Some(rule) = rule {
        let watch = ConsumerTag::new(ConsumerClass::EventAutomation, "watched");
        pins.push((source.id.clone(), Rails::VIDEO, watch));
        // The replacement's holder names the watching source: an object of
        // its own, authorized separately (review DMD-04).
        let target = ConsumerTag::naming(
            ConsumerClass::EventAutomation,
            source.id.as_str(),
            source.id.as_str(),
        );
        let rails = match rule.on_break_start.action {
            multiview_config::schema::BreakActionKind::SwapCell => Rails::VIDEO,
            multiview_config::schema::BreakActionKind::RouteProgram => Rails::ALL,
        };
        pins.push((rule.on_break_start.target.clone(), rails, target));
    }
    pins
}

/// Whether `a` and `b` define the same media: they differ at most in
/// presentation (EPG, playlist metadata) or standing policy (armed automation,
/// activation), which change leases, never the producer (reviews DMD-09,
/// R3-02). The single shared definition is [`Source::same_media`].
#[must_use]
pub fn same_media(a: &Source, b: &Source) -> bool {
    a.same_media(b)
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

use crate::display_size::cell_dims;

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
#[cfg(feature = "ffmpeg")]
#[derive(Clone, Debug)]
pub struct DrainDemand<T> {
    /// The active layout's consumers.
    pub layout: multiview_engine::ConsumerSet<T>,
    /// The subtitle routes' consumers.
    pub subtitles: multiview_engine::ConsumerSet<T>,
    /// The manager, for restarting an edited source.
    pub manager: multiview_engine::DemandManager<T>,
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

/// The control-plane view of one holder, carrying the object it names.
fn consumer_view(tag: &ConsumerTag) -> multiview_control::SourceConsumerView {
    multiview_control::SourceConsumerView::new(tag.class.as_str(), tag.label.to_string())
        .naming(tag.object.as_deref())
}

/// The last `yt-dlp` resolve failure of a `YouTube` source (issue #420), so the
/// source status says why it shows no picture instead of only "starting".
/// Already operator-safe (URLs redacted by `operator_message`).
#[cfg(feature = "youtube")]
fn youtube_reason(id: &str) -> Option<String> {
    crate::youtube_status::last_error(id).map(|reason| format!("YouTube: {reason}"))
}

#[cfg(not(feature = "youtube"))]
fn youtube_reason(_id: &str) -> Option<String> {
    None
}

/// The status detail: a recorded `YouTube` resolve failure is the specific
/// reason and leads; the generic demand detail (e.g. "no media yet; still
/// retrying") follows it.
fn with_youtube_reason(detail: Option<String>, youtube: Option<String>) -> Option<String> {
    match (youtube, detail) {
        (Some(youtube), Some(detail)) => Some(format!("{youtube} ({detail})")),
        (youtube, detail) => youtube.or(detail),
    }
}

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
            D::Ready => P::Ready,
            D::Failed => P::Failed,
        },
        video_wanted: status.wanted.video,
        audio_wanted: status.wanted.audio,
        video_running: status.running.video,
        audio_running: status.running.audio,
        consumers: status.consumers.iter().map(consumer_view).collect(),
        detail: with_youtube_reason(status.detail.clone(), youtube_reason(&status.id)),
        starting_phase: status.starting.map(|phase| match phase {
            multiview_engine::StartingPhase::Queued => {
                multiview_control::SourceStartingPhase::Queued
            }
            multiview_engine::StartingPhase::Opening => {
                multiview_control::SourceStartingPhase::Opening
            }
            multiview_engine::StartingPhase::AwaitingMedia => {
                multiview_control::SourceStartingPhase::AwaitingMedia
            }
        }),
        deadline_in_ms: status
            .deadline_in
            .map(|left| u64::try_from(left.as_millis()).unwrap_or(u64::MAX)),
        // Filled by the API from the accepted document.
        activation: multiview_control::SourceActivationView::AlwaysOn,
    }
}

/// The `input.lifecycle` event for an engine transition.
#[must_use]
pub fn lifecycle_event(event: &multiview_engine::TransitionEvent) -> multiview_events::Event {
    use multiview_engine::DemandPhase as D;
    use multiview_events::SourceLifecycle as L;
    let state = |phase: D| match phase {
        D::Idle => L::Idle,
        D::Starting => L::Starting,
        D::Ready => L::Ready,
        D::Failed => L::Failed,
    };
    let at_ms = event
        .at
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
    multiview_events::Event::InputLifecycle(multiview_events::InputLifecycle::new(
        event.source_id.clone(),
        event.generation,
        state(event.from),
        state(event.to),
        event.reason.clone(),
        at_ms,
        event
            .lease_holders
            .iter()
            .map(|tag| {
                multiview_events::LeaseHolder::new(tag.class.as_str(), tag.label.to_string())
                    .naming(tag.object.as_deref())
            })
            .collect(),
    ))
}

#[cfg(feature = "ffmpeg")]
impl multiview_control::SourceDemandProvider for ControlDemand {
    fn start_source(&self, id: &str) -> bool {
        if !self.catalogue.load().contains_key(id) {
            return false;
        }
        self.manager.start_source(id);
        true
    }

    fn stop_source(&self, id: &str) -> Result<(), multiview_control::StopRefusal> {
        self.manager.stop_source(id).map_err(|holders| {
            multiview_control::StopRefusal::InUse(holders.iter().map(consumer_view).collect())
        })
    }

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

    fn switcher_preview_lease(
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
            Rails::ALL,
            tag,
            None,
        ))))
    }

    fn session_lease(
        &self,
        id: &str,
        label: &str,
    ) -> Option<Box<dyn multiview_control::SourceDemandLease>> {
        if !self.catalogue.load().contains_key(id) {
            return None;
        }
        let tag = ConsumerTag::new(ConsumerClass::HlsSession, label);
        Some(Box::new(HeldLease(self.manager.acquire(
            id,
            Rails::ALL,
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
                    phase,
                    ..multiview_engine::DemandStatus::idle(id)
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
    fn an_existing_source_starts_at_boot_whether_or_not_it_is_consumed() {
        let demand = demand(&config("", ""));
        assert_eq!(demand["bound"].rails, Rails::ALL);
        assert_eq!(demand["unused"].rails, Rails::ALL, "default is always on");
    }

    #[test]
    fn an_unconsumed_on_demand_source_has_no_demand() {
        let extra = "[[sources]]\nid = \"lazy\"\nkind = \"hls\"\nurl = \"https://example.test/z.m3u8\"\nactivation = \"on_demand\"";
        let demand = demand(&config(extra, ""));
        assert!(!demand.contains_key("lazy"));
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
            "[[sources]]\nid = \"msnow\"\nkind = \"hls\"\nurl = \"https://example.test/m.m3u8\"\nactivation = \"on_demand\"";
        let demand = demand(&config(extra, tail));
        assert_eq!(demand["msnow"].rails, Rails::AUDIO);
    }

    #[test]
    fn the_legacy_demand_alias_still_selects_activation() {
        let extra = r#"[[sources]]
id = "pinned"
kind = "hls"
url = "https://example.test/p.m3u8"
demand = { always_on = true }
[[sources]]
id = "lazy"
kind = "hls"
url = "https://example.test/l.m3u8"
demand = { always_on = false, journal_events = true }"#;
        let demand = demand(&config(extra, ""));
        assert_eq!(demand["pinned"].rails, Rails::ALL);
        assert!(
            !demand.contains_key("lazy"),
            "journal_events is no longer a pin"
        );
    }

    #[test]
    fn observation_alone_is_not_a_consumer_but_armed_automation_is() {
        let observed = r#"[[sources]]
id = "watched"
kind = "hls"
url = "https://example.test/w.m3u8"
activation = "on_demand"
events = { observe = true }"#;
        assert!(!demand(&config(observed, "")).contains_key("watched"));
        let armed = |armed: bool| {
            format!(
                r#"[[sources]]
id = "slate"
kind = "hls"
url = "https://example.test/s.m3u8"
activation = "on_demand"
[[sources]]
id = "watched"
kind = "hls"
url = "https://example.test/w.m3u8"
activation = "on_demand"
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
    fn policy_only_edits_keep_the_same_media() {
        let base: Source = serde_json::from_value(serde_json::json!({
            "id": "w", "kind": "hls", "url": "https://example.test/w.m3u8",
            "events": {"automation": {"armed": false, "on_break_start": {"action": "swap_cell", "target": "s"}}}
        }))
        .unwrap();
        let mut armed = base.clone();
        armed
            .events
            .as_mut()
            .unwrap()
            .automation
            .as_mut()
            .unwrap()
            .armed = true;
        armed.activation = Some(multiview_config::SourceActivation::OnDemand);
        assert!(same_media(&base, &armed));
        let mut moved = base.clone();
        moved.kind = multiview_config::SourceKind::Bars;
        assert!(!same_media(&base, &moved));
    }

    /// Review R2-02: a source with no events block gaining its first rule (and
    /// losing it again) keeps the same media; an observe change does not.
    #[test]
    fn the_first_automation_rule_on_a_source_without_events_keeps_the_same_media() {
        let bare: Source = serde_json::from_value(serde_json::json!({
            "id": "w", "kind": "hls", "url": "https://example.test/w.m3u8"
        }))
        .unwrap();
        assert!(bare.events.is_none());
        for armed in [true, false] {
            let ruled: Source = serde_json::from_value(serde_json::json!({
                "id": "w", "kind": "hls", "url": "https://example.test/w.m3u8",
                "events": {"automation": {"armed": armed,
                    "on_break_start": {"action": "swap_cell", "target": "s"}}}
            }))
            .unwrap();
            assert!(same_media(&bare, &ruled), "armed={armed}");
            assert!(same_media(&ruled, &bare), "armed={armed}");
        }
        let unobserved: Source = serde_json::from_value(serde_json::json!({
            "id": "w", "kind": "hls", "url": "https://example.test/w.m3u8",
            "events": {"observe": false}
        }))
        .unwrap();
        assert!(!same_media(&bare, &unobserved), "observe is media");
    }

    #[test]
    fn close_after_start_timeout_and_concurrency_come_from_system_demand() {
        let tuned = demand_config(&config(
            "",
            "[system.demand]\nclose_after_secs = 5\nstart_timeout_secs = 7\nmax_concurrent_starts = 2\nmax_stuck_opens = 3\n",
        ));
        assert_eq!(tuned.max_stuck_opens, 3);
        assert_eq!(tuned.close_after, std::time::Duration::from_secs(5));
        assert_eq!(tuned.start_timeout, std::time::Duration::from_secs(7));
        assert_eq!(tuned.max_concurrent_starts, 2);
        // The old name still loads.
        let legacy = demand_config(&config("", "[system.demand]\nlinger_secs = 9\n"));
        assert_eq!(legacy.close_after, std::time::Duration::from_secs(9));
        let default = demand_config(&config("", ""));
        assert_eq!(default.close_after, multiview_engine::DEFAULT_CLOSE_AFTER);
    }

    #[test]
    fn layout_items_skip_unbound_cells_and_label_by_position() {
        let items = layout_items([(Some("a"), None), (None, None), (Some("a"), None)].into_iter());
        assert_eq!(items.len(), 2);
        assert_eq!(&*items[1].label, "cell-3");
    }
}

#[cfg(all(test, feature = "youtube"))]
mod youtube_detail_tests {
    #[test]
    fn a_youtube_resolve_failure_is_the_status_detail() {
        let id = "demand-test-youtube-detail";
        crate::youtube_status::record(id, Some("ERROR: No video formats found!"));
        assert_eq!(
            super::youtube_reason(id).as_deref(),
            Some("YouTube: ERROR: No video formats found!")
        );
        crate::youtube_status::record(id, None);
        assert_eq!(super::youtube_reason(id), None);
    }

    #[test]
    fn the_youtube_reason_leads_a_generic_failed_detail() {
        assert_eq!(
            super::with_youtube_reason(
                Some("no media yet; still retrying".to_owned()),
                Some("YouTube: not a live stream (status: was_live)".to_owned()),
            )
            .as_deref(),
            Some("YouTube: not a live stream (status: was_live) (no media yet; still retrying)")
        );
        assert_eq!(
            super::with_youtube_reason(Some("generic".to_owned()), None).as_deref(),
            Some("generic")
        );
        assert_eq!(super::with_youtube_reason(None, None), None);
    }
}
