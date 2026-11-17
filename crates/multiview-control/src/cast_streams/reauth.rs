//! Re-authorization of live cast stream tokens when the feed of their cast
//! output changes (ADR-0111; review finding ABUSE-1).
//!
//! A token keeps its URL across a take (the receiver keeps playing), but
//! only while its owner may read what the output now carries. Before any
//! path publishes a new feed on a cast output — a routing take
//! (`/routing/output/take`, a `/patch/take` output crosspoint), the cast
//! API's own take, the take back to Program when the last stream ends, an
//! edit or delete of a layout on air — every live token on that output is
//! checked against the owner's CURRENT grants for the new feed's complete
//! source closure, with exactly the rules the take and the create route
//! apply: the output on both axes, a source (Read), a layout (Read on the id,
//! every source its cells bind and its `audio` source), a bus (Program scope
//! and the M/E grant), the output's audio choice (`program` → the bus
//! rule, a source → Read, a track → Read on the name and its route input),
//! every discrete audio track the stream packages as a rendition (#418
//! review ABUSE-2: `slots.audio` / the output's `audio` selection resolved
//! by [`multiview_config::Output::carried_audio`], the engine's own rule;
//! Read on the track name and its route input) and the source of every
//! subtitle slot (#418 review ABUSE-1: Read on the source whose captions the
//! slot carries: every entry of the engine's live route table and every
//! slot source of the accepted config). Tracks and slots are resolved in
//! the accepted config AND the run-bound one the engine started with (#418
//! review ABUSE-1-R2/ABUSE-2-R2: an output PUT is restart-pending, the
//! running sink keeps its renditions) — snapshotted when the cast engine
//! attaches, so store-only runs have one too; without any run-bound view
//! the closure fails closed (ABUSE-STORE-R3). A subtitle re-patch commits through
//! [`before_subtitle_change`] before the engine swaps its route table.
//! A token whose owner fails, or whose owner can no longer be resolved, is
//! revoked at once: its next request is the same `404` as any dead token,
//! the reason stays readable by its owner (`GET /cast/streams/{id}`), the
//! revocation is audited and logged (never the token).
//!
//! Fail-closed authorization generation (ABUSE-1, round 2). Each cast
//! output has a generation in the registry; every token records the one it
//! is armed at and `/cast/<token>/…` serves only at the CURRENT one:
//!
//! * a committed feed change ([`before_change`], before the engine hears)
//!   bumps it, records what the output may now carry, and re-authorizes
//!   every armed token for it — moved forward or revoked — in ONE critical
//!   section of the registry lock, so no request can see a half-done step;
//! * a creation arms its token only at the generation whose cleared feeds
//!   it authorized the owner for ([`arm_stream`]); a generation that moved
//!   meanwhile re-checks, then refuses (`409 cast-feed-changed`);
//! * a feed the engine reports on its outcome ([`on_outcome`], relayed at
//!   once) or shows on its readback before that outcome arrived (an engine
//!   fallback; [`on_media`], run synchronously on every media request
//!   before the token is looked up) that the gate does not clear is
//!   re-authorized the same way first.
//!
//! The re-checks run under the registry lock (key-store read + pure scope
//! checks; the grants were resolved before the lock). Control plane only:
//! no `.await` under a lock, nothing touches the engine.

use multiview_config::AudioFeed;

use crate::auth::Principal;
use crate::output_routing::{OutputOutcome, RoutedAudio, RoutedFeed, RoutedLayout};
use crate::state::AppState;

use super::{AuthOrigin, Change, ChangeMode, OwnedStream, SinkMark};

/// The reason recorded when the owner may not read the new feed.
pub(crate) const REASON_FEED: &str =
    "revoked: the cast output was taken to a feed this stream's owner may not read";
/// The reason recorded when the owner's credentials no longer resolve.
pub(crate) const REASON_OWNER: &str = "revoked: this stream's owner can no longer be resolved";
/// The reason recorded when what the output carries cannot be determined.
pub(crate) const REASON_UNKNOWN: &str =
    "revoked: what the cast output now carries could not be determined";

/// One grant a feed requires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Need {
    /// Read on this object id.
    Object(String),
    /// The switcher-bus rule (Program scope + the M/E grant).
    Bus,
}

/// The owner check every gate operation runs on each armed token.
fn check(
    state: &AppState,
) -> impl Fn(&OwnedStream, Option<&[Need]>) -> Result<(), &'static str> + '_ {
    move |stream, needs| match (resolve_owner(state, stream), needs) {
        (None, _) => Err(REASON_OWNER),
        (Some(_), None) => Err(REASON_UNKNOWN),
        (Some(owner), Some(needs)) if !permits(&owner, &stream.output, needs) => Err(REASON_FEED),
        (Some(_), Some(_)) => Ok(()),
    }
}

/// Audit and log what a gate operation revoked; end the cast sessions that
/// played it. Returns the revoked ids.
fn report(state: &AppState, revoked: Vec<(OwnedStream, &'static str)>, actor: &str) -> Vec<String> {
    let mut ids = Vec::new();
    for (stream, reason) in revoked {
        tracing::info!(stream = %stream.id, output = %stream.output, reason, "cast: stream token revoked");
        state.audit(
            actor,
            crate::audit::AuditAction::Delete,
            "cast_stream",
            &stream.id,
            Some(serde_json::json!({ "output": stream.output, "reason": reason })),
        );
        ids.push(stream.id);
    }
    if !ids.is_empty() {
        // A server-side cast session that played a revoked stream ends too.
        crate::routes::cast_sessions::reap_streamless_sessions(state);
    }
    ids
}

/// Commit the feed change cast output `config_id` is about to carry, BEFORE
/// the engine hears of it: `op` the take's operation id (`None`: a change
/// without an outcome), `video` (`None`: unchanged), `layout` (the resolved
/// body of a layout `video`, when the caller has it), `audio` (`None`: the
/// output's configured audio choice). Bumps the output's generation and
/// re-authorizes every armed token for it in one critical section. `actor`
/// is the key id the revocations are audited to. Returns the revoked ids.
pub(crate) fn before_change(
    state: &AppState,
    config_id: &str,
    op: Option<&str>,
    video: Option<&RoutedFeed>,
    layout: Option<&RoutedLayout>,
    audio: Option<&RoutedAudio>,
    actor: &str,
) -> Vec<String> {
    if state
        .cast_streams
        .slots()
        .iter()
        .all(|s| s.config_id != config_id)
    {
        return Vec::new();
    }
    let change = Change {
        op: op.map(str::to_owned),
        bus: video.map(RoutedFeed::bus_name),
        needs: closure(state, config_id, video, layout, audio, None),
    };
    let revoked =
        state
            .cast_streams
            .commit(config_id, change, sink(state, config_id), &check(state));
    report(state, revoked, actor)
}

/// Commit a subtitle re-patch of cast output `config_id` (slot → new
/// source, `None` = unpatched; #418 review ABUSE-1), BEFORE the engine
/// swaps its route table: the same fail-closed generation step as a video
/// take ([`before_change`]), with the picture unchanged and the output's
/// complete next closure — every subtitle slot at its NEW source. A token
/// whose owner may not read a new source is revoked before any cue of it
/// can be published. Returns the revoked ids.
pub(crate) fn before_subtitle_change(
    state: &AppState,
    config_id: &str,
    slots: &[(usize, Option<String>)],
    actor: &str,
) -> Vec<String> {
    if state
        .cast_streams
        .slots()
        .iter()
        .all(|s| s.config_id != config_id)
    {
        return Vec::new();
    }
    let change = Change {
        op: None,
        bus: None,
        needs: closure(state, config_id, None, None, None, Some(slots)),
    };
    let revoked =
        state
            .cast_streams
            .commit(config_id, change, sink(state, config_id), &check(state));
    report(state, revoked, actor)
}

/// An engine outcome (relayed at once): the feed it reports. When the armed
/// tokens were not authorized for it (an engine fallback, a take decided
/// otherwise) they are re-authorized at a new generation first.
pub(crate) fn on_outcome(state: &AppState, outcome: &OutputOutcome) -> Vec<String> {
    let Some(feed) = &outcome.feed else {
        return Vec::new();
    };
    if state
        .cast_streams
        .slots()
        .iter()
        .all(|s| s.config_id != outcome.output)
    {
        return Vec::new();
    }
    let needs = closure(
        state,
        &outcome.output,
        Some(feed),
        None,
        outcome.audio.as_ref(),
        None,
    );
    let op = (!outcome.operation_id.is_empty()).then_some(outcome.operation_id.as_str());
    let revoked = state.cast_streams.observe(
        &outcome.output,
        op,
        &feed.bus_name(),
        needs,
        ChangeMode::Outcome,
        sink(state, &outcome.output),
        &check(state),
    );
    report(state, revoked, "engine")
}

/// The engine status of cast output `config_id`'s sink (#418 review
/// ABUSE-HISTORY-R4): what a superseded grant set's retirement is proven
/// against. `None` (no engine, no such output): unavailable, so nothing
/// superseded is forgotten.
pub(crate) fn sink(state: &AppState, config_id: &str) -> Option<SinkMark> {
    state
        .cast_engine
        .as_ref()?
        .outputs()
        .iter()
        .find(|o| o.id == config_id)
        .map(SinkMark::of)
}

/// Every cast output's sink status (see [`sink`]; `None`: no engine).
pub(crate) fn sinks(state: &AppState) -> Option<std::collections::HashMap<String, SinkMark>> {
    Some(
        state
            .cast_engine
            .as_ref()?
            .outputs()
            .iter()
            .map(|o| (o.id.clone(), SinkMark::of(o)))
            .collect(),
    )
}

/// The media path's barrier, run before each `/cast/<token>/…` lookup: when
/// the engine readback shows output `config_id` on a picture bus the gate
/// does not clear (a fallback whose outcome is not relayed yet), or a
/// subtitle source the gate does not clear (#418 review ABUSE-1: a route
/// swapped without a commit), its tokens are re-authorized for it NOW, at a
/// new generation, before anything is served. Cheap when nothing changed
/// (two readback loads, two short locks).
pub(crate) fn on_media(state: &AppState, config_id: &str) {
    let Some(bus) = live_bus(state, config_id) else {
        return;
    };
    if state.cast_streams.clears(config_id, &bus) {
        let subtitles: Vec<Need> = live_subtitles(state, config_id)
            .unwrap_or_default()
            .into_iter()
            .flatten()
            .map(Need::Object)
            .collect();
        if subtitles.is_empty() || state.cast_streams.covers(config_id, &subtitles) {
            return;
        }
    }
    let (_, needs) = live_needs(state, config_id);
    let revoked = state.cast_streams.observe(
        config_id,
        None,
        &bus,
        needs,
        ChangeMode::Readback,
        sink(state, config_id),
        &check(state),
    );
    report(state, revoked, "engine");
}

/// Arm stream `id` on `config_id` for `principal` (a creation): the owner
/// must be authorized for every feed the gate clears and for what the
/// output carries now, at the generation it read. A generation that moves
/// meanwhile is re-checked (bounded), then refused.
///
/// # Errors
/// `Err(true)`: the owner may not read what the output may now carry;
/// `Err(false)`: the output kept changing (or the stream is gone).
pub(crate) fn arm_stream(state: &AppState, id: &str, config_id: &str) -> Result<(), bool> {
    const ATTEMPTS: usize = 3;
    let Some(stream) = state.cast_streams.owned(id) else {
        return Err(false);
    };
    let check = check(state);
    for _ in 0..ATTEMPTS {
        let snapshot = state
            .cast_streams
            .arm_snapshot(config_id, sink(state, config_id));
        state.cast_streams.run_before_arm();
        let (bus, current) = live_needs(state, config_id);
        let mut needs = snapshot.needs.clone();
        if let (Some(all), Some(current)) = (needs.as_mut(), current.as_ref()) {
            all.extend(current.iter().cloned());
        } else {
            needs = None;
        }
        if check(&stream, needs.as_deref()).is_err() {
            return Err(true);
        }
        match state
            .cast_streams
            .arm(id, snapshot.generation, bus.map(|bus| (bus, current)))
        {
            Ok(()) => return Ok(()),
            Err(super::api::ArmError::Gone) => return Err(false),
            Err(super::api::ArmError::Stale) => {}
        }
    }
    Err(false)
}

/// [`before_change`]'s re-check for every output with live tokens, on what
/// each carries now and under each owner's CURRENT grants (a re-scoped or
/// revoked key, an audio track re-pointed to another input); a feed the
/// gate did not clear moves the generation.
pub(crate) fn recheck_all(state: &AppState, actor: &str) -> Vec<String> {
    let mut ids = Vec::new();
    for slot in state
        .cast_streams
        .slots()
        .into_iter()
        .filter(|s| s.streams > 0)
    {
        let (bus, needs) = live_needs(state, &slot.config_id);
        let bus = bus.unwrap_or_else(|| slot.feed.bus_name());
        let revoked = state.cast_streams.observe(
            &slot.config_id,
            None,
            &bus,
            needs,
            ChangeMode::Recheck,
            sink(state, &slot.config_id),
            &check(state),
        );
        ids.extend(report(state, revoked, actor));
    }
    ids
}

/// The picture bus the readback shows the output on now and its grants
/// (with the output's configured audio choice; `None` grants: unknown).
fn live_needs(state: &AppState, config_id: &str) -> (Option<String>, Option<Vec<Need>>) {
    let Some(bus) = live_bus(state, config_id) else {
        return (None, closure(state, config_id, None, None, None, None));
    };
    let needs = RoutedFeed::from_bus_name(&bus)
        .and_then(|feed| closure(state, config_id, Some(&feed), None, None, None));
    (Some(bus), needs)
}

/// The picture bus the engine readback shows the output on now.
fn live_bus(state: &AppState, config_id: &str) -> Option<String> {
    let snapshot = state.output_routing.as_ref()?.routing.snapshot()?;
    Some(snapshot.outputs.get(config_id)?.bus.clone())
}

/// The owner's CURRENT principal, by how it authenticated at creation
/// (ABUSE-3): a native API key is always the key store's entry for its key
/// id (re-scoped keys take effect; a key the store no longer holds fails
/// closed, whatever else is configured); a JWT subject is the principal it
/// authenticated with, while JWT validation stays configured; the local
/// admin, while auth stays disabled. An unbound stream does not resolve.
fn resolve_owner(state: &AppState, stream: &OwnedStream) -> Option<Principal> {
    match stream.origin? {
        AuthOrigin::ApiKey => state.api_keys.principal_for_key(&stream.owner),
        AuthOrigin::Jwt => {
            let recorded = stream.principal.clone()?;
            (state.jwt.is_some() && recorded.key_id == stream.owner).then_some(recorded)
        }
        AuthOrigin::LocalAdmin => {
            let recorded = stream.principal.clone()?;
            (state.auth_disabled && recorded == Principal::local_admin()).then_some(recorded)
        }
    }
}

fn permits(owner: &Principal, output: &str, needs: &[Need]) -> bool {
    crate::routes::output_routing::authorize_output_end(owner, output).is_ok()
        && needs.iter().all(|need| match need {
            Need::Object(id) => crate::auth::authorize_object(owner, id).is_ok(),
            Need::Bus => crate::routes::output_routing::authorize_bus(owner).is_ok(),
        })
}

/// The grants the output's next feed requires (`None`: undeterminable, so
/// every token fails closed): the picture, the audio choice, every discrete
/// track the stream packages and every subtitle slot's source (`subtitles`:
/// a re-patch about to apply, slot → new source, over the current ones).
fn closure(
    state: &AppState,
    config_id: &str,
    video: Option<&RoutedFeed>,
    layout: Option<&RoutedLayout>,
    audio: Option<&RoutedAudio>,
    subtitles: Option<&[(usize, Option<String>)]>,
) -> Option<Vec<Need>> {
    let live;
    let video = if let Some(video) = video {
        video
    } else {
        live = current_video(state, config_id)?;
        &live
    };
    let config = desired(state)?;
    let run = running(state);
    // ABUSE-STORE-R3: no run-bound view declaring this output ⇒ what the
    // running sink packages cannot be established: fail closed.
    if !run.iter().any(|view| output_of(view, config_id).is_some()) {
        return None;
    }
    let views: Vec<&multiview_config::MultiviewConfig> = std::iter::once(&config)
        .chain(run.iter().map(AsRef::as_ref))
        .collect();
    let mut needs = video_needs(state, video, layout)?;
    needs.extend(audio_needs(&config, config_id, audio)?);
    needs.extend(carried_track_needs(&views, config_id)?);
    needs.extend(subtitle_needs(state, &config, &run, config_id, subtitles)?);
    Some(needs)
}

/// The accepted (desired) configuration (`None`: unreadable).
fn desired(state: &AppState) -> Option<multiview_config::MultiviewConfig> {
    serde_json::from_value(crate::boot_model::current_desired_config(state).ok()?).ok()
}

/// The run-bound views (#418 review ABUSE-1-R2/ABUSE-2-R2/ABUSE-STORE-R3):
/// what the engine STARTED with, never the mutable accepted stores. A
/// restart-pending output PUT changes the accepted document but not what
/// the running cast sink packages (its renditions and subtitle slots are
/// fixed per run), so the closure is the union of the accepted view and
/// these. Two sources, each immutable per run: the start document
/// snapshotted when the cast engine seam attached ([`AppState::cast_run`],
/// present in boot-model and store-only runs alike) and the boot model's
/// adopted snapshot (outputs and audio routing never advanced after start).
/// Empty: no run view — the closure fails closed.
fn running(state: &AppState) -> Vec<std::sync::Arc<multiview_config::MultiviewConfig>> {
    let mut views: Vec<std::sync::Arc<multiview_config::MultiviewConfig>> =
        state.cast_run.iter().cloned().collect();
    if let Some(model) = &state.boot_model {
        views.push(std::sync::Arc::new(model.adopted_config()));
    }
    views
}

/// The output `config_id` in `config` (`None`: not declared — fail closed).
fn output_of<'a>(
    config: &'a multiview_config::MultiviewConfig,
    config_id: &str,
) -> Option<&'a multiview_config::Output> {
    config.outputs.iter().find(|o| o.id() == config_id)
}

/// Push `need` unless already present.
fn push_unique(needs: &mut Vec<Need>, need: Need) {
    if !needs.contains(&need) {
        needs.push(need);
    }
}

/// ABUSE-2 (#418): every discrete track the cast stream packages as an
/// audio rendition — resolved by the engine's own rule
/// ([`multiview_config::Output::carried_audio`] against the declared
/// tracks) — needs Read on the track name and its route input, exactly as
/// an `AudioFeed::Tracks` choice does. Resolved in EVERY view (ABUSE-2-R2:
/// the running run's selection and the accepted, possibly restart-pending
/// one), each track's input in every view (a live re-point and the run's
/// own binding). An output no view declares fails closed.
fn carried_track_needs(
    views: &[&multiview_config::MultiviewConfig],
    config_id: &str,
) -> Option<Vec<Need>> {
    let mut declared_anywhere = false;
    let mut needs = Vec::new();
    for config in views {
        let Some(output) = output_of(config, config_id) else {
            continue;
        };
        declared_anywhere = true;
        let declared: Vec<&str> = config
            .audio
            .as_ref()
            .map(multiview_config::AudioRouting::declared_tracks)
            .unwrap_or_default()
            .into_iter()
            .filter(|t| *t != multiview_config::PROGRAM_TRACK)
            .collect();
        let (_, tracks) = output.carried_audio(&declared);
        for track in tracks {
            for bound in views {
                if let Some(input) = crate::routes::output_routing::track_input(bound, track) {
                    push_unique(&mut needs, Need::Object(input));
                }
            }
            push_unique(&mut needs, Need::Object(track.to_owned()));
        }
    }
    declared_anywhere.then_some(needs)
}

/// The engine's live subtitle route table of `config_id` (slot → source;
/// `None`: no run samples it).
fn live_subtitles(state: &AppState, config_id: &str) -> Option<Vec<Option<String>>> {
    state
        .output_routing
        .as_ref()?
        .routing
        .subtitle_routes(config_id)
}

/// ABUSE-1 (#418, round 2): Read on every subtitle source the stream may
/// publish — every entry of the engine's live route table (all slots,
/// whatever slot count the accepted output now declares; it follows live
/// re-patches), `patch`'s new source of a re-patched slot, and every slot
/// source of the accepted config (where re-patches persist; a
/// restart-pending slot set). When the engine reports no live table, the
/// run-bound views' slot sources stand in for it (what the run started
/// sampling), so a slot removed from the accepted config still counts;
/// with no run view declaring the output it fails closed, never empty
/// (#418 review ABUSE-STORE-R3).
fn subtitle_needs(
    state: &AppState,
    accepted: &multiview_config::MultiviewConfig,
    run: &[std::sync::Arc<multiview_config::MultiviewConfig>],
    config_id: &str,
    patch: Option<&[(usize, Option<String>)]>,
) -> Option<Vec<Need>> {
    let slot_sources = |config: &multiview_config::MultiviewConfig| {
        output_of(config, config_id).map(|output| {
            output
                .slots()
                .map(|slots| {
                    slots
                        .subtitle
                        .iter()
                        .map(|slot| slot.source.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    };
    let mut sources = slot_sources(accepted).unwrap_or_default();
    if let Some(live) = live_subtitles(state, config_id) {
        sources.extend(live);
    } else {
        let mut declared = false;
        for view in run {
            if let Some(slots) = slot_sources(view) {
                declared = true;
                sources.extend(slots);
            }
        }
        if !declared {
            return None;
        }
    }
    sources.extend(patch.unwrap_or_default().iter().map(|(_, s)| s.clone()));
    let mut needs = Vec::new();
    for source in sources.into_iter().flatten() {
        push_unique(&mut needs, Need::Object(source));
    }
    Some(needs)
}

/// What the output shows (or is about to show, when a take is pending).
fn current_video(state: &AppState, config_id: &str) -> Option<RoutedFeed> {
    let snapshot = state.output_routing.as_ref()?.routing.snapshot()?;
    let live = snapshot.outputs.get(config_id)?;
    let bus = live.pending.as_ref().map_or(&live.bus, |(bus, _)| bus);
    RoutedFeed::from_bus_name(bus)
}

fn video_needs(
    state: &AppState,
    video: &RoutedFeed,
    layout: Option<&RoutedLayout>,
) -> Option<Vec<Need>> {
    Some(match video {
        RoutedFeed::Program | RoutedFeed::Preview | RoutedFeed::Clean => vec![Need::Bus],
        RoutedFeed::Source(id) => vec![Need::Object(id.clone())],
        RoutedFeed::Layout(id) => {
            let mut needs = vec![Need::Object(id.clone())];
            let (sources, layout_audio) = if let Some(routed) = layout.filter(|l| l.id == *id) {
                let sources = routed
                    .layout
                    .cells
                    .iter()
                    .filter_map(|cell| cell.source.clone())
                    .collect::<Vec<_>>();
                (sources, routed.audio.clone())
            } else {
                let library = crate::routes::routing::layout_sources(state).ok()?;
                let stored = library.get(id)?;
                (stored.sources.clone(), stored.audio.clone())
            };
            needs.extend(sources.into_iter().map(Need::Object));
            if let multiview_config::LayoutAudio::Source(source) = layout_audio {
                needs.push(Need::Object(source));
            }
            needs
        }
    })
}

/// The audio choice's own end, as the take authorizes it (`follow` and
/// `none` add nothing beyond the video).
fn audio_needs(
    config: &multiview_config::MultiviewConfig,
    config_id: &str,
    audio: Option<&RoutedAudio>,
) -> Option<Vec<Need>> {
    let choice = match audio {
        Some(RoutedAudio::Track { name, input }) => {
            return Some(vec![
                Need::Object(name.clone()),
                Need::Object(input.clone()),
            ]);
        }
        Some(routed) => routed.audio_feed(),
        None => config
            .output_feeds()
            .into_iter()
            .find(|xp| xp.output == config_id)
            .map_or(AudioFeed::Follow, |xp| xp.audio),
    };
    Some(match choice {
        AudioFeed::Follow | AudioFeed::None => Vec::new(),
        AudioFeed::Program => vec![Need::Bus],
        AudioFeed::Source(id) => vec![Need::Object(id)],
        AudioFeed::Tracks(tracks) => {
            let mut needs = Vec::new();
            for track in tracks {
                if let Some(input) = crate::routes::output_routing::track_input(config, &track) {
                    needs.push(Need::Object(input));
                }
                needs.push(Need::Object(track));
            }
            needs
        }
        _ => return None,
    })
}
