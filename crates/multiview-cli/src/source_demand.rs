//! Which configured sources have a consumer, derived from the document alone
//! (`docs/architecture/demand-driven-sources.md`).
//!
//! Pure policy with no I/O: it decides what starts at boot and which standing
//! leases a source's own settings imply. Everything that appears at runtime (a
//! layout flip, a route, a preview) takes its lease where it happens.

use std::collections::BTreeMap;

use multiview_config::{MultiviewConfig, Source, SourceKind};
use multiview_engine::{ConsumerClass, ConsumerTag, DemandConfig, RequestedSize, Rails, SetItem};

/// What one source must run at boot.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct StartupDemand {
    /// The rails consumers want at boot.
    pub rails: Rails,
}

/// Whether the document mixes every source: with no routes declared, every
/// running source joins the program bus at unity.
fn unrouted(config: &MultiviewConfig) -> bool {
    config.audio.as_ref().is_none_or(|a| a.routes.is_empty())
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
            multiview_config::BreakActionKind::SwapCell => Rails::VIDEO,
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
    let mix_everything = unrouted(config);
    for source in &config.sources {
        // AES67 exists only for the bus, so an unrouted bus is its consumer.
        if mix_everything && matches!(source.kind, SourceKind::Aes67 { .. }) {
            want(&source.id, Rails::AUDIO);
        }
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
        let extra = "[[sources]]\nid = \"msnow\"\nkind = \"hls\"\nurl = \"https://example.test/m.m3u8\"";
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
