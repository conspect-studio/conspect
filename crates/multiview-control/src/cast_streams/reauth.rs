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
//! and the M/E grant), and the output's audio choice (`program` → the bus
//! rule, a source → Read, a track → Read on the name and its route input).
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

use super::{AuthOrigin, Change, ChangeMode, OwnedStream};

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
        needs: closure(state, config_id, video, layout, audio),
    };
    let revoked = state.cast_streams.commit(config_id, change, &check(state));
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
    );
    let op = (!outcome.operation_id.is_empty()).then_some(outcome.operation_id.as_str());
    let revoked = state.cast_streams.observe(
        &outcome.output,
        op,
        &feed.bus_name(),
        needs,
        ChangeMode::Outcome,
        &check(state),
    );
    report(state, revoked, "engine")
}

/// The media path's barrier, run before each `/cast/<token>/…` lookup: when
/// the engine readback shows output `config_id` on a picture bus the gate
/// does not clear (a fallback whose outcome is not relayed yet), its tokens
/// are re-authorized for it NOW, at a new generation, before anything is
/// served. Cheap when nothing changed (one readback load, one short lock).
pub(crate) fn on_media(state: &AppState, config_id: &str) {
    let Some(bus) = live_bus(state, config_id) else {
        return;
    };
    if state.cast_streams.clears(config_id, &bus) {
        return;
    }
    let (_, needs) = live_needs(state, config_id);
    let revoked = state.cast_streams.observe(
        config_id,
        None,
        &bus,
        needs,
        ChangeMode::Readback,
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
        let snapshot = state.cast_streams.arm_snapshot(config_id);
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
        return (None, closure(state, config_id, None, None, None));
    };
    let needs = RoutedFeed::from_bus_name(&bus)
        .and_then(|feed| closure(state, config_id, Some(&feed), None, None));
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
/// every token fails closed).
fn closure(
    state: &AppState,
    config_id: &str,
    video: Option<&RoutedFeed>,
    layout: Option<&RoutedLayout>,
    audio: Option<&RoutedAudio>,
) -> Option<Vec<Need>> {
    let live;
    let video = if let Some(video) = video {
        video
    } else {
        live = current_video(state, config_id)?;
        &live
    };
    let mut needs = video_needs(state, video, layout)?;
    needs.extend(audio_needs(state, config_id, audio)?);
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
    state: &AppState,
    config_id: &str,
    audio: Option<&RoutedAudio>,
) -> Option<Vec<Need>> {
    let mut config: Option<multiview_config::MultiviewConfig> = None;
    let mut load = || -> Option<multiview_config::MultiviewConfig> {
        if config.is_none() {
            config =
                serde_json::from_value(crate::boot_model::current_desired_config(state).ok()?).ok();
        }
        config.clone()
    };
    let choice = match audio {
        Some(RoutedAudio::Track { name, input }) => {
            return Some(vec![
                Need::Object(name.clone()),
                Need::Object(input.clone()),
            ]);
        }
        Some(routed) => routed.audio_feed(),
        None => load()?
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
            let config = load()?;
            let mut needs = Vec::new();
            for track in tracks {
                if let Some(input) = crate::routes::output_routing::track_input(&config, &track) {
                    needs.push(Need::Object(input));
                }
                needs.push(Need::Object(track));
            }
            needs
        }
        _ => return None,
    })
}
