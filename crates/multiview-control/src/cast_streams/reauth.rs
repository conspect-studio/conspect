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
//! Engine-originated changes (a fallback) and anything that slipped between
//! the check and the publication are re-checked when the outcome arrives.
//! Control plane only: short locks, no `.await`, nothing touches the engine.

use multiview_config::AudioFeed;

use crate::auth::Principal;
use crate::output_routing::{RoutedAudio, RoutedFeed, RoutedLayout};
use crate::state::AppState;

use super::OwnedStream;

/// The reason recorded when the owner may not read the new feed.
pub(crate) const REASON_FEED: &str =
    "revoked: the cast output was taken to a feed this stream's owner may not read";
/// The reason recorded when the owner's credentials no longer resolve.
pub(crate) const REASON_OWNER: &str = "revoked: this stream's owner can no longer be resolved";
/// The reason recorded when what the output carries cannot be determined.
pub(crate) const REASON_UNKNOWN: &str =
    "revoked: what the cast output now carries could not be determined";

/// One grant the new feed requires.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Need {
    /// Read on this object id.
    Object(String),
    /// The switcher-bus rule (Program scope + the M/E grant).
    Bus,
}

/// Re-check every live token on cast output `config_id` for the feed it is
/// about to carry: `video` (`None`: unchanged, read back from the engine),
/// `layout` (the resolved body of a layout `video`, when the caller has
/// it), `audio` (`None`: the output's configured audio choice). `actor` is
/// the key id the revocations are audited to. Returns the revoked ids.
pub(crate) fn before_change(
    state: &AppState,
    config_id: &str,
    video: Option<&RoutedFeed>,
    layout: Option<&RoutedLayout>,
    audio: Option<&RoutedAudio>,
    actor: &str,
) -> Vec<String> {
    let streams = state.cast_streams.live_on(config_id);
    if streams.is_empty() {
        return Vec::new();
    }
    let needs = closure(state, config_id, video, layout, audio);
    let mut revoked = Vec::new();
    for stream in &streams {
        let reason = match (resolve_owner(state, stream), &needs) {
            (None, _) => REASON_OWNER,
            (Some(_), None) => REASON_UNKNOWN,
            (Some(owner), Some(needs)) if !permits(&owner, &stream.output, needs) => REASON_FEED,
            (Some(_), Some(_)) => continue,
        };
        if state.cast_streams.revoke(&stream.id, reason).is_some() {
            tracing::info!(stream = %stream.id, output = %stream.output, reason, "cast: stream token revoked");
            state.audit(
                actor,
                crate::audit::AuditAction::Delete,
                "cast_stream",
                &stream.id,
                Some(serde_json::json!({ "output": stream.output, "reason": reason })),
            );
            revoked.push(stream.id.clone());
        }
    }
    if !revoked.is_empty() {
        // A server-side cast session that played a revoked stream ends too.
        crate::routes::cast_sessions::reap_streamless_sessions(state);
    }
    revoked
}

/// [`before_change`] for every output with live tokens, on what each
/// carries now (after a change of what a feed resolves to, e.g. an audio
/// track re-pointed to another input).
pub(crate) fn recheck_all(state: &AppState, actor: &str) -> Vec<String> {
    state
        .cast_streams
        .slots()
        .into_iter()
        .filter(|slot| slot.streams > 0)
        .flat_map(|slot| before_change(state, &slot.config_id, None, None, None, actor))
        .collect()
}

/// The owner's CURRENT principal: the API-key store's entry for its key id
/// (re-scoped / revoked keys take effect), else — only for an owner the
/// store never holds — the principal it authenticated with at creation: the
/// local admin while auth is disabled, or a JWT subject (unscoped by
/// construction) while JWT validation is configured. Anything else does not
/// resolve.
fn resolve_owner(state: &AppState, stream: &OwnedStream) -> Option<Principal> {
    if let Some(principal) = state.api_keys.principal_for_key(&stream.owner) {
        return Some(principal);
    }
    let recorded = stream.principal.clone()?;
    if recorded.key_id != stream.owner {
        return None;
    }
    if state.auth_disabled && recorded == Principal::local_admin() {
        return Some(recorded);
    }
    (state.jwt.is_some() && recorded.is_global()).then_some(recorded)
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
