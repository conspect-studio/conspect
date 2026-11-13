//! The three-way merge of a boot-file edit onto the accepted state (#367,
//! ADR-W020 §3a).
//!
//! The boot file is NOT the accepted state: the operator builds the accepted
//! state (`desired.json`) through the API and the UI, and the boot file is
//! usually the small original document. A file edit therefore carries only
//! what the edit CHANGED: [`merge_file_edit`] compares the previous file
//! document with the new one and applies exactly those changes onto the
//! accepted document. Everything the edit did not touch keeps the accepted
//! value.
//!
//! The unit of change is a section. For the id-keyed list sections (sources,
//! outputs, overlays, probes, programs, tally profiles, salvos, devices, sync
//! groups, media players, media fills, weather locations, the graphics
//! data sources/items/channels, the audio routes and the routing crosspoint
//! tables) the unit is the item: an item the file
//! added or changed takes the file's value, an item the file removed is
//! removed, and every other accepted item stays. When the accepted section is
//! still exactly what the previous file said (nothing diverged), the new
//! file's section is taken whole, so a pure reorder still lands. The working
//! canvas is one unit, and `layout` + `cells` are one unit (cells name the
//! layout's areas).
//!
//! Pure and total: no I/O. The caller validates the merged document (a merge
//! can combine an edit with accepted content into a document that no longer
//! validates; that edit is refused, never half-applied).

use crate::audio::AudioRouting;
use crate::routing::RoutingTable;
use crate::MultiviewConfig;

/// Pick a whole section: the file's new value when the file changed it,
/// otherwise the accepted value.
fn section<T: PartialEq + Clone>(prev: &T, next: &T, accepted: &T) -> T {
    if prev == next {
        accepted.clone()
    } else {
        next.clone()
    }
}

/// Merge an id-keyed list: per item, the file's change wins; untouched
/// accepted items stay in their accepted order; items the file added are
/// appended in the file's order. When the accepted list still equals the
/// previous file's list, the new file's list is taken whole (keeps a reorder).
fn keyed<T: PartialEq + Clone, K: PartialEq>(
    prev: &[T],
    next: &[T],
    accepted: &[T],
    key: impl Fn(&T) -> K,
) -> Vec<T> {
    if prev == next {
        return accepted.to_vec();
    }
    if prev == accepted {
        return next.to_vec();
    }
    let find = |list: &[T], k: &K| list.iter().find(|item| key(item) == *k).cloned();
    let mut merged: Vec<T> = Vec::with_capacity(accepted.len().max(next.len()));
    for item in accepted {
        let k = key(item);
        let before = find(prev, &k);
        let after = find(next, &k);
        match (before, after) {
            // The file removed it: the edit removes it.
            (Some(_), None) => {}
            // The file changed it: the edit wins.
            (Some(before), Some(after)) if before != after => merged.push(after),
            // The file added an id the accepted state already has: the
            // file's explicit value wins.
            (None, Some(after)) => merged.push(after),
            // Untouched by the edit: the accepted value stays.
            _ => merged.push(item.clone()),
        }
    }
    for item in next {
        let k = key(item);
        let in_prev = prev.iter().any(|p| key(p) == k);
        let in_merged = merged.iter().any(|m| key(m) == k);
        // Added by the file (absent before), or changed by the file while the
        // accepted state had removed it: the edit re-creates it.
        let changed = prev.iter().find(|p| key(p) == k).is_some_and(|p| p != item);
        if !in_merged && (!in_prev || changed) {
            merged.push(item.clone());
        }
    }
    merged
}

/// `[audio]`: the sample rate is one value, the routes are keyed by
/// `(input_id, target_track)`. Removing the whole block in the file removes
/// the whole block (the file said so); otherwise per route.
fn merge_audio(
    prev: Option<&AudioRouting>,
    next: Option<&AudioRouting>,
    accepted: Option<&AudioRouting>,
) -> Option<AudioRouting> {
    if prev == next {
        return accepted.cloned();
    }
    let (Some(next), Some(accepted)) = (next, accepted) else {
        return next.cloned();
    };
    let empty = AudioRouting {
        sample_rate_hz: next.sample_rate_hz,
        routes: Vec::new(),
    };
    let prev = prev.unwrap_or(&empty);
    Some(AudioRouting {
        sample_rate_hz: section(
            &prev.sample_rate_hz,
            &next.sample_rate_hz,
            &accepted.sample_rate_hz,
        ),
        routes: keyed(&prev.routes, &next.routes, &accepted.routes, |r| {
            (r.input_id.clone(), r.target_track.clone())
        }),
    })
}

/// `[routing]`: each crosspoint table keyed by its destination (cell, audio
/// target, subtitle layer, output). A routing table with every list empty is
/// `None` (the same "absent" the boot splice uses).
fn merge_routing(
    prev: Option<&RoutingTable>,
    next: Option<&RoutingTable>,
    accepted: Option<&RoutingTable>,
) -> Option<RoutingTable> {
    if prev == next {
        return accepted.cloned();
    }
    let empty = RoutingTable::default();
    let (p, n, a) = (
        prev.unwrap_or(&empty),
        next.unwrap_or(&empty),
        accepted.unwrap_or(&empty),
    );
    let merged = RoutingTable {
        video: keyed(&p.video, &n.video, &a.video, |x| x.cell.clone()),
        audio: keyed(&p.audio, &n.audio, &a.audio, |x| x.target.clone()),
        subtitle: keyed(&p.subtitle, &n.subtitle, &a.subtitle, |x| x.layer.clone()),
        output: keyed(&p.output, &n.output, &a.output, |x| x.output.clone()),
    };
    let none = merged.video.is_empty()
        && merged.audio.is_empty()
        && merged.subtitle.is_empty()
        && merged.output.is_empty();
    (!none).then_some(merged)
}

/// Apply the edit `prev_file` → `next_file` onto `accepted`, section by
/// section (items for id-keyed lists). See the module docs.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn merge_file_edit(
    prev_file: &MultiviewConfig,
    next_file: &MultiviewConfig,
    accepted: &MultiviewConfig,
) -> MultiviewConfig {
    // EXHAUSTIVE destructure (no `..`): a new `MultiviewConfig` field is a
    // compile error HERE until the merge decides how a file edit carries it.
    let MultiviewConfig {
        schema_version: p_schema_version,
        canvas: p_canvas,
        layout: p_layout,
        sources: p_sources,
        cells: p_cells,
        overlays: p_overlays,
        outputs: p_outputs,
        programs: p_programs,
        probes: p_probes,
        tally_profiles: p_tally_profiles,
        salvos: p_salvos,
        walls: p_walls,
        devices: p_devices,
        sync_groups: p_sync_groups,
        control: p_control,
        api: p_api,
        placement: p_placement,
        audio: p_audio,
        switcher: p_switcher,
        routing: p_routing,
        discovery: p_discovery,
        catalogues: p_catalogues,
        network_diagnostics: p_network_diagnostics,
        timing: p_timing,
        webrtc: p_webrtc,
        network: p_network,
        system: p_system,
        media_library: p_media_library,
        media_players: p_media_players,
        media_fills: p_media_fills,
        weather: p_weather,
        iptv: p_iptv,
        graphics: p_graphics,
        events: p_events,
    } = prev_file;
    let n = next_file;
    let a = accepted;
    // `layout` + `cells` are one unit: the cells name the layout's areas.
    let layout_unit_changed = p_layout != &n.layout || p_cells != &n.cells;
    let (layout, cells) = if layout_unit_changed {
        (n.layout.clone(), n.cells.clone())
    } else {
        (a.layout.clone(), a.cells.clone())
    };
    MultiviewConfig {
        schema_version: section(p_schema_version, &n.schema_version, &a.schema_version),
        canvas: section(p_canvas, &n.canvas, &a.canvas),
        layout,
        sources: keyed(p_sources, &n.sources, &a.sources, |s| s.id.clone()),
        cells,
        overlays: keyed(p_overlays, &n.overlays, &a.overlays, |o| o.id.clone()),
        outputs: keyed(p_outputs, &n.outputs, &a.outputs, crate::Output::id),
        programs: keyed(p_programs, &n.programs, &a.programs, |p| p.id.clone()),
        probes: keyed(p_probes, &n.probes, &a.probes, |p| p.id.clone()),
        tally_profiles: keyed(
            p_tally_profiles,
            &n.tally_profiles,
            &a.tally_profiles,
            |t| t.id.clone(),
        ),
        salvos: keyed(p_salvos, &n.salvos, &a.salvos, |s| s.id.clone()),
        walls: keyed(p_walls, &n.walls, &a.walls, |w| w.name.clone()),
        devices: keyed(p_devices, &n.devices, &a.devices, |d| d.id.clone()),
        sync_groups: keyed(p_sync_groups, &n.sync_groups, &a.sync_groups, |g| {
            g.id.clone()
        }),
        control: section(p_control, &n.control, &a.control),
        api: section(p_api, &n.api, &a.api),
        placement: section(p_placement, &n.placement, &a.placement),
        audio: merge_audio(p_audio.as_ref(), n.audio.as_ref(), a.audio.as_ref()),
        switcher: section(p_switcher, &n.switcher, &a.switcher),
        routing: merge_routing(p_routing.as_ref(), n.routing.as_ref(), a.routing.as_ref()),
        discovery: section(p_discovery, &n.discovery, &a.discovery),
        catalogues: section(p_catalogues, &n.catalogues, &a.catalogues),
        network_diagnostics: section(
            p_network_diagnostics,
            &n.network_diagnostics,
            &a.network_diagnostics,
        ),
        timing: section(p_timing, &n.timing, &a.timing),
        webrtc: section(p_webrtc, &n.webrtc, &a.webrtc),
        network: section(p_network, &n.network, &a.network),
        system: section(p_system, &n.system, &a.system),
        media_library: section(p_media_library, &n.media_library, &a.media_library),
        media_players: keyed(p_media_players, &n.media_players, &a.media_players, |m| {
            m.id.clone()
        }),
        media_fills: keyed(p_media_fills, &n.media_fills, &a.media_fills, |f| {
            f.id.clone()
        }),
        weather: crate::weather::WeatherConfig {
            locations: keyed(
                &p_weather.locations,
                &n.weather.locations,
                &a.weather.locations,
                |l| l.id.clone(),
            ),
        },
        iptv: section(p_iptv, &n.iptv, &a.iptv),
        graphics: Box::new(crate::graphics::GraphicsConfig {
            data_sources: keyed(
                &p_graphics.data_sources,
                &n.graphics.data_sources,
                &a.graphics.data_sources,
                |d| d.id.clone(),
            ),
            items: keyed(
                &p_graphics.items,
                &n.graphics.items,
                &a.graphics.items,
                |i| i.id.clone(),
            ),
            channels: keyed(
                &p_graphics.channels,
                &n.graphics.channels,
                &a.graphics.channels,
                |c| c.id.clone(),
            ),
        }),
        events: section(p_events, &n.events, &a.events),
    }
}

/// What a commit of `merged` over `accepted` would remove that exists in the
/// accepted state: source ids, output ids, the switcher (or its M/E ids) and
/// audio routes. Empty when nothing is removed. Each entry is a short human
/// label (`source in_b`, `output srt-multiview`, `switcher`, `audio route
/// in_c -> clean`) for the alarm.
#[must_use]
pub fn removed_accepted_items(accepted: &MultiviewConfig, merged: &MultiviewConfig) -> Vec<String> {
    let mut removed = Vec::new();
    for source in &accepted.sources {
        if !merged.sources.iter().any(|s| s.id == source.id) {
            removed.push(format!("source {}", source.id));
        }
    }
    let merged_outputs: Vec<String> = merged.outputs.iter().map(crate::Output::id).collect();
    for output in &accepted.outputs {
        let id = output.id();
        if !merged_outputs.contains(&id) {
            removed.push(format!("output {id}"));
        }
    }
    match (&accepted.switcher, &merged.switcher) {
        (Some(_), None) => removed.push("switcher".to_owned()),
        (Some(before), Some(after)) => {
            for me in &before.mix_effects {
                if !after.mix_effects.iter().any(|m| m.id == me.id) {
                    removed.push(format!("switcher M/E {}", me.id));
                }
            }
        }
        _ => {}
    }
    let routes = |config: &MultiviewConfig| {
        config
            .audio
            .as_ref()
            .map(|audio| audio.routes.clone())
            .unwrap_or_default()
    };
    let merged_routes = routes(merged);
    for route in routes(accepted) {
        if !merged_routes
            .iter()
            .any(|r| r.input_id == route.input_id && r.target_track == route.target_track)
        {
            removed.push(match &route.target_track {
                Some(track) => format!("audio route {} -> {track}", route.input_id),
                None => format!("audio route {}", route.input_id),
            });
        }
    }
    removed
}
