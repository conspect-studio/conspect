//! The Cast **session actor** (DEV-D2, ADR-M011) driven socket-free over the
//! scripted channel seam: connect → CONNECT → LAUNCH `CC1AD845` → CONNECT to
//! the app transport → LOAD; heartbeat PING every 10 s with the session dead
//! after 20 s without inbound traffic and 5 s reconnect retries; re-LOAD on
//! media IDLE; "preempted" surfaced (never fought) when another sender takes
//! the device; receiver-namespace volume/stop controls. The lifecycle states
//! it publishes are exactly the DEV-A3 `DeviceLifecycle` outputs.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    // Test prose names lifecycle states in plain caps (ONLINE/DEGRADED) and
    // protocol verbs (LOAD/PING) for readability.
    clippy::doc_markdown,
    // Test helpers take owned `serde_json::Value`s for terse call sites.
    clippy::needless_pass_by_value
)]

use std::sync::Arc;
use std::time::Duration;

use multiview_control::devices::cast::media::{CastMediaTarget, HlsSegmentFormat};
use multiview_control::devices::cast::protocol::{
    CastFrame, NS_CONNECTION, NS_HEARTBEAT, NS_MEDIA, NS_RECEIVER, PLATFORM_RECEIVER_ID, SENDER_ID,
};
use multiview_control::devices::cast::session::{
    CastSessionActor, CastSessionConfig, ScriptedChannel, ScriptedConnector, ScriptedInbound,
    SentFrames,
};
use multiview_control::devices::{
    DeviceBroadcaster, DeviceStatusRegistry, PollerControl, PollerStep,
};
use multiview_control::EngineStateSnapshot;
use multiview_engine::EnginePublisher;
use multiview_events::{DeviceState, Event};

/// The media target every test session LOADs.
fn media() -> CastMediaTarget {
    CastMediaTarget {
        url: "http://192.0.2.7:8080/hls/program/program.m3u8".to_owned(),
        format: HlsSegmentFormat::MpegTs,
    }
}

/// Build the broadcaster + the engine publisher (to subscribe to events).
fn broadcaster() -> (
    Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    DeviceBroadcaster,
) {
    let engine = Arc::new(EnginePublisher::new(64));
    let status = Arc::new(DeviceStatusRegistry::new());
    let b = DeviceBroadcaster::new(Arc::clone(&engine), status);
    (engine, b)
}

/// An inbound frame from the device.
fn from_device(namespace: &str, payload: serde_json::Value) -> CastFrame {
    CastFrame {
        namespace: namespace.to_owned(),
        source: PLATFORM_RECEIVER_ID.to_owned(),
        destination: SENDER_ID.to_owned(),
        payload: payload.to_string(),
    }
}

/// A RECEIVER_STATUS carrying the Default Media Receiver app on transport
/// `t-1` / session `s-1`.
fn receiver_status_with_app() -> CastFrame {
    from_device(
        NS_RECEIVER,
        serde_json::json!({
            "type": "RECEIVER_STATUS",
            "requestId": 0,
            "status": { "applications": [{
                "appId": "CC1AD845",
                "sessionId": "s-1",
                "transportId": "t-1",
                "displayName": "Default Media Receiver"
            }] }
        }),
    )
}

/// A RECEIVER_STATUS with **no** applications: our app is gone and nothing
/// else is running (app idle-kill/crash — NOT a preemption).
fn receiver_status_without_app() -> CastFrame {
    from_device(
        NS_RECEIVER,
        serde_json::json!({ "type": "RECEIVER_STATUS", "requestId": 0, "status": {} }),
    )
}

/// A RECEIVER_STATUS carrying `app_id` running as `session_id` on
/// `transport_id` (another sender's app, or another session of ours).
fn receiver_status_with(app_id: &str, session_id: &str, transport_id: &str) -> CastFrame {
    from_device(
        NS_RECEIVER,
        serde_json::json!({
            "type": "RECEIVER_STATUS",
            "requestId": 0,
            "status": { "applications": [{
                "appId": app_id,
                "sessionId": session_id,
                "transportId": transport_id,
                "displayName": "Some App"
            }] }
        }),
    )
}

/// A RECEIVER_STATUS carrying only the receiver's idle screen (the backdrop
/// real hardware launches when an app dies): "nothing else running".
fn receiver_status_idle_screen_only() -> CastFrame {
    from_device(
        NS_RECEIVER,
        serde_json::json!({
            "type": "RECEIVER_STATUS",
            "requestId": 0,
            "status": { "applications": [{
                "appId": "E8C28D3C",
                "sessionId": "s-idle",
                "transportId": "t-idle",
                "displayName": "Backdrop",
                "isIdleScreen": true
            }] }
        }),
    )
}

/// A MEDIA_STATUS in `player_state` (optionally with an idle reason).
fn media_status(player_state: &str, idle_reason: Option<&str>) -> CastFrame {
    let mut entry = serde_json::json!({ "mediaSessionId": 1, "playerState": player_state });
    if let Some(reason) = idle_reason {
        entry["idleReason"] = serde_json::Value::String(reason.to_owned());
    }
    from_device(
        NS_MEDIA,
        serde_json::json!({ "type": "MEDIA_STATUS", "requestId": 0, "status": [entry] }),
    )
}

/// A MEDIA_STATUS for an explicit `media_session_id` (foreign-session tests
/// use ids other than the `1` the establishment adopts).
fn media_status_for(media_session_id: i64, player_state: &str) -> CastFrame {
    from_device(
        NS_MEDIA,
        serde_json::json!({
            "type": "MEDIA_STATUS",
            "requestId": 0,
            "status": [{ "mediaSessionId": media_session_id, "playerState": player_state }]
        }),
    )
}

/// A MEDIA_STATUS with an **empty** status array: the media session vanished
/// receiver-side without a final IDLE.
fn empty_media_status() -> CastFrame {
    from_device(
        NS_MEDIA,
        serde_json::json!({ "type": "MEDIA_STATUS", "requestId": 0, "status": [] }),
    )
}

/// A media-namespace LOAD_FAILED (the receiver rejected our LOAD).
fn load_failed() -> CastFrame {
    from_device(
        NS_MEDIA,
        serde_json::json!({ "type": "LOAD_FAILED", "requestId": 2 }),
    )
}

/// The connection-namespace CLOSE (the app transport went away).
fn close_connection() -> CastFrame {
    from_device(NS_CONNECTION, serde_json::json!({ "type": "CLOSE" }))
}

/// Drain `events` and report whether a `device.error` naming a preemption
/// rode the lossless lane for `device_id`.
fn saw_preempted_error(
    events: &mut multiview_engine::EventSubscription<Event>,
    device_id: &str,
) -> bool {
    let mut saw = false;
    while let Ok(envelope) = events.try_recv() {
        if let Event::DeviceError(e) = &*envelope.event {
            if e.device_id == device_id && e.message.contains("preempted") {
                saw = true;
            }
        }
    }
    saw
}

/// The payload `type` tokens of the frames sent on a channel, in order.
fn sent_types(sent: &SentFrames) -> Vec<(String, String)> {
    sent.lock()
        .expect("sent log lock")
        .iter()
        .map(|f| {
            let body: serde_json::Value =
                serde_json::from_str(&f.payload).expect("sent payload is JSON");
            (
                f.namespace.clone(),
                body["type"].as_str().unwrap_or("?").to_owned(),
            )
        })
        .collect()
}

/// Count the sent frames whose payload `type` equals `kind`.
fn count_sent(sent: &SentFrames, kind: &str) -> usize {
    sent_types(sent)
        .into_iter()
        .filter(|(_, t)| t == kind)
        .count()
}

#[tokio::test]
async fn connect_step_runs_connect_launch_load_in_order() {
    let (_engine, b) = broadcaster();
    let (channel, sent) = ScriptedChannel::new(vec![
        ScriptedInbound::Frame(receiver_status_with_app()),
        // Queued behind the establishment: the first media status report.
        ScriptedInbound::Frame(media_status("PLAYING", None)),
    ]);
    let connector = ScriptedConnector::new(vec![channel]);
    let mut actor = CastSessionActor::new(
        "cast-1",
        connector,
        "[2001:db8::20]:8009",
        media(),
        b,
        CastSessionConfig::default(),
    );

    assert_eq!(actor.state(), DeviceState::Adopting, "starts ADOPTING");
    let step = actor.connect_step().await;
    assert_eq!(step, PollerStep::Online, "established session is ONLINE");
    assert_eq!(actor.state(), DeviceState::Online);

    // The exact ADR-M011 establishment sequence, in order: virtual CONNECT to
    // the platform receiver → LAUNCH the Default Media Receiver → CONNECT to
    // the app's transport → LOAD the HLS rendition.
    let types = sent_types(&sent);
    assert_eq!(
        types,
        vec![
            (NS_CONNECTION.to_owned(), "CONNECT".to_owned()),
            (NS_RECEIVER.to_owned(), "LAUNCH".to_owned()),
            (NS_CONNECTION.to_owned(), "CONNECT".to_owned()),
            (NS_MEDIA.to_owned(), "LOAD".to_owned()),
        ]
    );
    // The app-transport CONNECT and the LOAD address the app, not the platform.
    let frames = sent.lock().expect("sent log lock").clone();
    assert_eq!(frames[2].destination, "t-1");
    assert_eq!(frames[3].destination, "t-1");

    // Until a MEDIA_STATUS arrives the published mode is the honest "loading".
    let status = actor.published_status().expect("a published status");
    assert_eq!(status.state, DeviceState::Online);
    assert_eq!(status.mode.as_deref(), Some("loading"));

    // The first MEDIA_STATUS maps the player state onto the conflated status.
    let step = actor.pump_step().await;
    assert_eq!(step, PollerStep::Online);
    let status = actor.published_status().expect("a published status");
    assert_eq!(status.mode.as_deref(), Some("playing"));
}

#[tokio::test]
async fn connect_refused_is_unreachable() {
    let (_engine, b) = broadcaster();
    // No scripted channels: every connect attempt is refused.
    let connector = ScriptedConnector::new(vec![]);
    let mut actor = CastSessionActor::new(
        "cast-1",
        connector,
        "192.0.2.20:8009",
        media(),
        b,
        CastSessionConfig::default(),
    );
    let step = actor.connect_step().await;
    assert_eq!(step, PollerStep::Unreachable);
    assert_eq!(actor.state(), DeviceState::Unreachable);
}

#[tokio::test]
async fn launch_error_is_a_device_fault() {
    let (_engine, b) = broadcaster();
    let (channel, _sent) = ScriptedChannel::new(vec![ScriptedInbound::Frame(from_device(
        NS_RECEIVER,
        serde_json::json!({ "type": "LAUNCH_ERROR", "reason": "NOT_FOUND" }),
    ))]);
    let connector = ScriptedConnector::new(vec![channel]);
    let mut actor = CastSessionActor::new(
        "cast-1",
        connector,
        "192.0.2.20:8009",
        media(),
        b,
        CastSessionConfig::default(),
    );
    let step = actor.connect_step().await;
    assert_eq!(step, PollerStep::Degraded, "launch refused = device fault");
    assert_eq!(actor.state(), DeviceState::Degraded);
}

#[tokio::test(start_paused = true)]
async fn heartbeat_pings_and_expiry_reconnects() {
    let (_engine, b) = broadcaster();
    let (ch1, sent1) = ScriptedChannel::new(vec![
        ScriptedInbound::Frame(receiver_status_with_app()),
        ScriptedInbound::Frame(media_status("PLAYING", None)),
        ScriptedInbound::Hang,
    ]);
    let (ch2, sent2) = ScriptedChannel::new(vec![
        // The reconnect asks GET_STATUS first; the receiver idled our app
        // out during the outage (nothing running), so it re-establishes.
        ScriptedInbound::Frame(receiver_status_without_app()),
        ScriptedInbound::Frame(receiver_status_with_app()),
        ScriptedInbound::Frame(media_status("PLAYING", None)),
        ScriptedInbound::Hang,
    ]);
    let connector = ScriptedConnector::new(vec![ch1, ch2]);
    let connects = connector.connect_count();
    let status = Arc::clone(b.registry());
    let actor = CastSessionActor::new(
        "cast-1",
        connector,
        "[2001:db8::20]:8009",
        media(),
        b,
        CastSessionConfig::default(),
    );
    let handle = actor.spawn();

    // After the first channel goes silent: PINGs ride every 10 s, the session
    // is declared dead 20 s after the last inbound traffic, the reconnect
    // retries 5 s later and re-establishes (LAUNCH + LOAD again).
    tokio::time::sleep(Duration::from_secs(40)).await;

    assert!(
        count_sent(&sent1, "PING") >= 1,
        "PINGs ride the first channel every 10 s: {:?}",
        sent_types(&sent1)
    );
    assert_eq!(
        connects.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "the dead session reconnected exactly once"
    );
    assert_eq!(count_sent(&sent2, "LAUNCH"), 1, "re-LAUNCH on reconnect");
    assert_eq!(count_sent(&sent2, "LOAD"), 1, "re-LOAD on reconnect");
    assert_eq!(
        status.state("cast-1"),
        Some(DeviceState::Online),
        "re-established session is ONLINE again"
    );
    drop(handle);
}

#[tokio::test(start_paused = true)]
async fn media_idle_reloads_after_the_reload_delay() {
    let (_engine, b) = broadcaster();
    let (ch1, sent1) = ScriptedChannel::new(vec![
        ScriptedInbound::Frame(receiver_status_with_app()),
        ScriptedInbound::Frame(media_status("PLAYING", None)),
        ScriptedInbound::Frame(media_status("IDLE", Some("ERROR"))),
        // The re-LOAD answer arrives well after the (5 s) reload delay.
        ScriptedInbound::Wait(Duration::from_secs(8)),
        ScriptedInbound::Frame(media_status("PLAYING", None)),
        ScriptedInbound::Hang,
    ]);
    let connector = ScriptedConnector::new(vec![ch1]);
    let status = Arc::clone(b.registry());
    let actor = CastSessionActor::new(
        "cast-1",
        connector,
        "192.0.2.20:8009",
        media(),
        b,
        CastSessionConfig::default(),
    );
    let handle = actor.spawn();

    // Shortly after the IDLE lands: DEGRADED, mode "idle", and only the
    // original LOAD so far (the supervisor waits the reload delay).
    tokio::time::sleep(Duration::from_secs(3)).await;
    let snapshot = status.snapshot("cast-1").expect("a status row");
    assert_eq!(snapshot.state, DeviceState::Degraded, "IDLE degrades");
    assert_eq!(snapshot.mode.as_deref(), Some("idle"));
    assert_eq!(count_sent(&sent1, "LOAD"), 1);

    // After the reload delay the supervisor re-LOADs; the next PLAYING
    // recovers the session to ONLINE.
    tokio::time::sleep(Duration::from_secs(7)).await;
    assert_eq!(count_sent(&sent1, "LOAD"), 2, "re-LOAD on IDLE");
    let snapshot = status.snapshot("cast-1").expect("a status row");
    assert_eq!(snapshot.state, DeviceState::Online);
    assert_eq!(snapshot.mode.as_deref(), Some("playing"));
    drop(handle);
}

#[tokio::test(start_paused = true)]
async fn preemption_is_surfaced_and_never_fought() {
    let (engine, b) = broadcaster();
    let mut events = engine.subscribe();
    let (ch1, sent1) = ScriptedChannel::new(vec![
        ScriptedInbound::Frame(receiver_status_with_app()),
        ScriptedInbound::Frame(media_status("PLAYING", None)),
        ScriptedInbound::Wait(Duration::from_secs(2)),
        // Another sender took the device: a foreign app replaced ours in
        // the status (our session id is gone, theirs is running).
        ScriptedInbound::Frame(receiver_status_with("DEADBEEF", "s-9", "t-9")),
        ScriptedInbound::Hang,
    ]);
    // The post-preemption reconnect (after the heartbeat dies on the silent
    // channel) only re-establishes the management channel — it must NOT
    // re-LAUNCH/re-LOAD over the other sender's session.
    let (ch2, sent2) = ScriptedChannel::new(vec![ScriptedInbound::Hang]);
    let connector = ScriptedConnector::new(vec![ch1, ch2]);
    let connects = connector.connect_count();
    let status = Arc::clone(b.registry());
    let actor = CastSessionActor::new(
        "cast-1",
        connector,
        "192.0.2.20:8009",
        media(),
        b,
        CastSessionConfig::default(),
    );
    let handle = actor.spawn();

    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(
        status.state("cast-1"),
        Some(DeviceState::Degraded),
        "preempted session is DEGRADED (the channel is up, the media is not ours)"
    );
    // Exactly the original LAUNCH/LOAD — we never fight the other sender.
    assert_eq!(count_sent(&sent1, "LAUNCH"), 1);
    assert_eq!(count_sent(&sent1, "LOAD"), 1);

    // A `device.error` naming the preemption rode the lossless lane.
    let mut saw_preempted = false;
    while let Ok(envelope) = events.try_recv() {
        if let Event::DeviceError(e) = &*envelope.event {
            if e.device_id == "cast-1" && e.message.contains("preempted") {
                saw_preempted = true;
            }
        }
    }
    assert!(saw_preempted, "device.error surfaces the preemption");

    // Ride past the heartbeat expiry: the reconnect re-establishes the
    // channel but stays hands-off (no LAUNCH/LOAD) and remains DEGRADED.
    tokio::time::sleep(Duration::from_secs(40)).await;
    assert_eq!(connects.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(
        count_sent(&sent2, "LAUNCH"),
        0,
        "never re-LAUNCH after preemption"
    );
    assert_eq!(
        count_sent(&sent2, "LOAD"),
        0,
        "never re-LOAD after preemption"
    );
    assert_eq!(status.state("cast-1"), Some(DeviceState::Degraded));
    drop(handle);
}

#[tokio::test(start_paused = true)]
async fn stop_control_stops_the_receiver_app_and_exits() {
    let (_engine, b) = broadcaster();
    let (ch1, sent1) = ScriptedChannel::new(vec![
        ScriptedInbound::Frame(receiver_status_with_app()),
        ScriptedInbound::Frame(media_status("PLAYING", None)),
        ScriptedInbound::Hang,
    ]);
    let connector = ScriptedConnector::new(vec![ch1]);
    let actor = CastSessionActor::new(
        "cast-1",
        connector,
        "192.0.2.20:8009",
        media(),
        b,
        CastSessionConfig::default(),
    );
    let handle = actor.spawn();
    tokio::time::sleep(Duration::from_secs(1)).await;

    assert!(handle.try_dispatch(PollerControl::StopCast));
    let task = handle.into_join_handle();
    // The actor STOPs the receiver app and exits voluntarily (the task
    // completes — it is not aborted).
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("the actor exits after StopCast")
        .expect("the actor exits cleanly, not by abort");
    let frames = sent1.lock().expect("sent log lock").clone();
    let stop = frames
        .iter()
        .find(|f| f.payload.contains("\"STOP\""))
        .expect("a receiver STOP was sent");
    assert_eq!(stop.namespace, NS_RECEIVER);
    let body: serde_json::Value = serde_json::from_str(&stop.payload).expect("STOP body");
    assert_eq!(body["sessionId"], "s-1", "STOP names the receiver session");
}

#[tokio::test(start_paused = true)]
async fn volume_control_rides_the_receiver_namespace() {
    let (_engine, b) = broadcaster();
    let (ch1, sent1) = ScriptedChannel::new(vec![
        ScriptedInbound::Frame(receiver_status_with_app()),
        ScriptedInbound::Frame(media_status("PLAYING", None)),
        ScriptedInbound::Hang,
    ]);
    let connector = ScriptedConnector::new(vec![ch1]);
    let actor = CastSessionActor::new(
        "cast-1",
        connector,
        "192.0.2.20:8009",
        media(),
        b,
        CastSessionConfig::default(),
    );
    let handle = actor.spawn();
    tokio::time::sleep(Duration::from_secs(1)).await;

    assert!(handle.try_dispatch(PollerControl::SetVolume { percent: 42 }));
    tokio::time::sleep(Duration::from_secs(1)).await;

    let frames = sent1.lock().expect("sent log lock").clone();
    let set = frames
        .iter()
        .find(|f| f.payload.contains("SET_VOLUME"))
        .expect("a SET_VOLUME was sent");
    assert_eq!(set.namespace, NS_RECEIVER);
    let body: serde_json::Value = serde_json::from_str(&set.payload).expect("SET_VOLUME body");
    let level = body["volume"]["level"].as_f64().expect("a unit level");
    assert!((level - 0.42).abs() < 1e-9, "42% = level 0.42, got {level}");
    drop(handle);
}

// ---------------------------------------------------------------------------
// Adversarial-review findings (DEV-D2): failed LOADs must be visible (F1),
// preemption identity keys on session ids — never the app id (F2), and a
// reconnect asks GET_STATUS before touching the receiver (F3).
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn load_failed_degrades_and_schedules_a_bounded_reload() {
    let (_engine, b) = broadcaster();
    let (ch1, sent1) = ScriptedChannel::new(vec![
        ScriptedInbound::Frame(receiver_status_with_app()),
        // The receiver answers PINGs but rejects the LOAD: without a typed
        // decode the session would sit ONLINE/"loading" with a blank TV.
        ScriptedInbound::Frame(load_failed()),
        // The re-LOAD's answer arrives well after the (5 s) reload delay.
        ScriptedInbound::Wait(Duration::from_secs(8)),
        ScriptedInbound::Frame(media_status("PLAYING", None)),
        ScriptedInbound::Hang,
    ]);
    let connector = ScriptedConnector::new(vec![ch1]);
    let status = Arc::clone(b.registry());
    let actor = CastSessionActor::new(
        "cast-1",
        connector,
        "192.0.2.20:8009",
        media(),
        b,
        CastSessionConfig::default(),
    );
    let handle = actor.spawn();

    // Shortly after the rejection: DEGRADED with an honest mode, and only
    // the original LOAD so far (the supervisor waits the reload delay —
    // an instantly-failing rendition must not drive a LOAD storm).
    tokio::time::sleep(Duration::from_secs(3)).await;
    let snapshot = status.snapshot("cast-1").expect("a status row");
    assert_eq!(
        snapshot.state,
        DeviceState::Degraded,
        "LOAD_FAILED degrades"
    );
    assert_eq!(snapshot.mode.as_deref(), Some("load-failed"));
    assert_eq!(count_sent(&sent1, "LOAD"), 1);

    // After the reload delay the supervisor re-LOADs; the next PLAYING
    // recovers the session to ONLINE.
    tokio::time::sleep(Duration::from_secs(7)).await;
    assert_eq!(count_sent(&sent1, "LOAD"), 2, "re-LOAD on LOAD_FAILED");
    let snapshot = status.snapshot("cast-1").expect("a status row");
    assert_eq!(snapshot.state, DeviceState::Online);
    assert_eq!(snapshot.mode.as_deref(), Some("playing"));
    drop(handle);
}

#[tokio::test(start_paused = true)]
async fn vanished_media_session_schedules_a_reload() {
    let (_engine, b) = broadcaster();
    let (ch1, sent1) = ScriptedChannel::new(vec![
        ScriptedInbound::Frame(receiver_status_with_app()),
        ScriptedInbound::Frame(media_status("PLAYING", None)),
        // The media session is torn down receiver-side without a final
        // IDLE: the post-LOAD MEDIA_STATUS carries an empty status array.
        ScriptedInbound::Frame(empty_media_status()),
        ScriptedInbound::Wait(Duration::from_secs(8)),
        ScriptedInbound::Frame(media_status("PLAYING", None)),
        ScriptedInbound::Hang,
    ]);
    let connector = ScriptedConnector::new(vec![ch1]);
    let status = Arc::clone(b.registry());
    let actor = CastSessionActor::new(
        "cast-1",
        connector,
        "192.0.2.20:8009",
        media(),
        b,
        CastSessionConfig::default(),
    );
    let handle = actor.spawn();

    tokio::time::sleep(Duration::from_secs(3)).await;
    let snapshot = status.snapshot("cast-1").expect("a status row");
    assert_eq!(
        snapshot.state,
        DeviceState::Degraded,
        "a vanished media session degrades"
    );
    assert_eq!(snapshot.mode.as_deref(), Some("no-media"));
    assert_eq!(count_sent(&sent1, "LOAD"), 1);

    tokio::time::sleep(Duration::from_secs(7)).await;
    assert_eq!(
        count_sent(&sent1, "LOAD"),
        2,
        "re-LOAD on a vanished media session"
    );
    let snapshot = status.snapshot("cast-1").expect("a status row");
    assert_eq!(snapshot.state, DeviceState::Online);
    assert_eq!(snapshot.mode.as_deref(), Some("playing"));
    drop(handle);
}

#[tokio::test(start_paused = true)]
async fn app_death_with_nothing_running_relaunches() {
    let (engine, b) = broadcaster();
    let mut events = engine.subscribe();
    let (ch1, _sent1) = ScriptedChannel::new(vec![
        ScriptedInbound::Frame(receiver_status_with_app()),
        ScriptedInbound::Frame(media_status("PLAYING", None)),
        ScriptedInbound::Wait(Duration::from_secs(2)),
        // Our app died (idle-kill/crash) and NOTHING replaced it: that is
        // not a preemption — the spec mandates re-LAUNCH + re-LOAD.
        ScriptedInbound::Frame(receiver_status_without_app()),
        ScriptedInbound::Hang,
    ]);
    let (ch2, sent2) = ScriptedChannel::new(vec![
        ScriptedInbound::Frame(receiver_status_with_app()),
        ScriptedInbound::Frame(media_status("PLAYING", None)),
        ScriptedInbound::Hang,
    ]);
    let connector = ScriptedConnector::new(vec![ch1, ch2]);
    let connects = connector.connect_count();
    let status = Arc::clone(b.registry());
    let actor = CastSessionActor::new(
        "cast-1",
        connector,
        "192.0.2.20:8009",
        media(),
        b,
        CastSessionConfig::default(),
    );
    let handle = actor.spawn();

    tokio::time::sleep(Duration::from_secs(12)).await;
    assert_eq!(
        connects.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "the dead app drives a re-establishment"
    );
    assert_eq!(count_sent(&sent2, "LAUNCH"), 1, "re-LAUNCH after app death");
    assert_eq!(count_sent(&sent2, "LOAD"), 1, "re-LOAD after app death");
    assert_eq!(
        status.state("cast-1"),
        Some(DeviceState::Online),
        "the re-established session is ONLINE again"
    );
    assert!(
        !saw_preempted_error(&mut events, "cast-1"),
        "app death with nothing running is NOT a preemption"
    );
    drop(handle);
}

#[tokio::test(start_paused = true)]
async fn app_death_leaving_the_idle_screen_relaunches() {
    let (engine, b) = broadcaster();
    let mut events = engine.subscribe();
    let (ch1, _sent1) = ScriptedChannel::new(vec![
        ScriptedInbound::Frame(receiver_status_with_app()),
        ScriptedInbound::Frame(media_status("PLAYING", None)),
        ScriptedInbound::Wait(Duration::from_secs(2)),
        // Real hardware launches the backdrop when an app dies: only the
        // idle screen running still means "nothing else running".
        ScriptedInbound::Frame(receiver_status_idle_screen_only()),
        ScriptedInbound::Hang,
    ]);
    let (ch2, sent2) = ScriptedChannel::new(vec![
        ScriptedInbound::Frame(receiver_status_with_app()),
        ScriptedInbound::Frame(media_status("PLAYING", None)),
        ScriptedInbound::Hang,
    ]);
    let connector = ScriptedConnector::new(vec![ch1, ch2]);
    let connects = connector.connect_count();
    let status = Arc::clone(b.registry());
    let actor = CastSessionActor::new(
        "cast-1",
        connector,
        "192.0.2.20:8009",
        media(),
        b,
        CastSessionConfig::default(),
    );
    let handle = actor.spawn();

    tokio::time::sleep(Duration::from_secs(12)).await;
    assert_eq!(connects.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(
        count_sent(&sent2, "LAUNCH"),
        1,
        "the backdrop does not count as another sender"
    );
    assert_eq!(count_sent(&sent2, "LOAD"), 1);
    assert_eq!(status.state("cast-1"), Some(DeviceState::Online));
    assert!(
        !saw_preempted_error(&mut events, "cast-1"),
        "the idle screen must never read as a preemption"
    );
    drop(handle);
}

#[tokio::test(start_paused = true)]
async fn app_death_close_first_relaunches_via_get_status() {
    let (engine, b) = broadcaster();
    let mut events = engine.subscribe();
    // The other ordering of the same app death: the transport CLOSE lands
    // before any RECEIVER_STATUS shows the app gone.
    let (ch1, _sent1) = ScriptedChannel::new(vec![
        ScriptedInbound::Frame(receiver_status_with_app()),
        ScriptedInbound::Frame(media_status("PLAYING", None)),
        ScriptedInbound::Wait(Duration::from_secs(2)),
        ScriptedInbound::Frame(close_connection()),
        ScriptedInbound::Hang,
    ]);
    let (ch2, sent2) = ScriptedChannel::new(vec![
        // The reconnect asks first: GET_STATUS answers "nothing running",
        // so the actor re-establishes (re-LAUNCH + re-LOAD).
        ScriptedInbound::Frame(receiver_status_without_app()),
        ScriptedInbound::Frame(receiver_status_with("CC1AD845", "s-2", "t-2")),
        ScriptedInbound::Frame(media_status("PLAYING", None)),
        ScriptedInbound::Hang,
    ]);
    let connector = ScriptedConnector::new(vec![ch1, ch2]);
    let connects = connector.connect_count();
    let status = Arc::clone(b.registry());
    let actor = CastSessionActor::new(
        "cast-1",
        connector,
        "192.0.2.20:8009",
        media(),
        b,
        CastSessionConfig::default(),
    );
    let handle = actor.spawn();

    tokio::time::sleep(Duration::from_secs(12)).await;
    assert_eq!(connects.load(std::sync::atomic::Ordering::SeqCst), 2);
    // The exact reconnect sequence: CONNECT → GET_STATUS (ask first) →
    // LAUNCH (our session is gone, nothing else runs) → CONNECT to the new
    // transport → LOAD.
    assert_eq!(
        sent_types(&sent2),
        vec![
            (NS_CONNECTION.to_owned(), "CONNECT".to_owned()),
            (NS_RECEIVER.to_owned(), "GET_STATUS".to_owned()),
            (NS_RECEIVER.to_owned(), "LAUNCH".to_owned()),
            (NS_CONNECTION.to_owned(), "CONNECT".to_owned()),
            (NS_MEDIA.to_owned(), "LOAD".to_owned()),
        ]
    );
    assert_eq!(status.state("cast-1"), Some(DeviceState::Online));
    assert!(
        !saw_preempted_error(&mut events, "cast-1"),
        "an app death is never surfaced as a preemption"
    );
    drop(handle);
}

#[tokio::test(start_paused = true)]
async fn same_app_new_session_is_preemption_not_ours() {
    let (engine, b) = broadcaster();
    let mut events = engine.subscribe();
    let (ch1, sent1) = ScriptedChannel::new(vec![
        ScriptedInbound::Frame(receiver_status_with_app()),
        ScriptedInbound::Frame(media_status("PLAYING", None)),
        ScriptedInbound::Wait(Duration::from_secs(2)),
        // Another sender re-LAUNCHed the SAME Default Media Receiver: the
        // app id matches ours but the session is theirs (the
        // pychromecast/Home-Assistant ecosystem does exactly this).
        ScriptedInbound::Frame(receiver_status_with("CC1AD845", "s-2", "t-2")),
        ScriptedInbound::Hang,
    ]);
    let (ch2, sent2) = ScriptedChannel::new(vec![ScriptedInbound::Hang]);
    let connector = ScriptedConnector::new(vec![ch1, ch2]);
    let connects = connector.connect_count();
    let status = Arc::clone(b.registry());
    let actor = CastSessionActor::new(
        "cast-1",
        connector,
        "192.0.2.20:8009",
        media(),
        b,
        CastSessionConfig::default(),
    );
    let handle = actor.spawn();

    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(
        status.state("cast-1"),
        Some(DeviceState::Degraded),
        "a replaced session of our own app id is a preemption"
    );
    assert!(
        saw_preempted_error(&mut events, "cast-1"),
        "device.error surfaces the same-app preemption"
    );
    // Exactly the original LAUNCH/LOAD — we never re-LAUNCH over them.
    assert_eq!(count_sent(&sent1, "LAUNCH"), 1);
    assert_eq!(count_sent(&sent1, "LOAD"), 1);

    // Past the heartbeat expiry the reconnect stays hands-off.
    tokio::time::sleep(Duration::from_secs(35)).await;
    assert_eq!(connects.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(count_sent(&sent2, "GET_STATUS"), 0);
    assert_eq!(count_sent(&sent2, "LAUNCH"), 0);
    assert_eq!(count_sent(&sent2, "LOAD"), 0);
    assert_eq!(status.state("cast-1"), Some(DeviceState::Degraded));
    drop(handle);
}

#[tokio::test(start_paused = true)]
async fn replaced_media_session_is_preemption() {
    let (engine, b) = broadcaster();
    let mut events = engine.subscribe();
    let (ch1, sent1) = ScriptedChannel::new(vec![
        ScriptedInbound::Frame(receiver_status_with_app()),
        ScriptedInbound::Frame(media_status("PLAYING", None)),
        ScriptedInbound::Wait(Duration::from_secs(2)),
        // Another sender JOINED our running app session and LOADed their
        // own media: no receiver-status change at all — the only signal is
        // an active media session that is not ours.
        ScriptedInbound::Frame(media_status_for(99, "PLAYING")),
        ScriptedInbound::Hang,
    ]);
    let connector = ScriptedConnector::new(vec![ch1]);
    let status = Arc::clone(b.registry());
    let actor = CastSessionActor::new(
        "cast-1",
        connector,
        "192.0.2.20:8009",
        media(),
        b,
        CastSessionConfig::default(),
    );
    let handle = actor.spawn();

    tokio::time::sleep(Duration::from_secs(5)).await;
    let snapshot = status.snapshot("cast-1").expect("a status row");
    assert_eq!(
        snapshot.state,
        DeviceState::Degraded,
        "a foreign active media session on our app is a preemption"
    );
    assert_eq!(
        snapshot.mode.as_deref(),
        Some("preempted"),
        "the foreign PLAYING is never read as ours"
    );
    assert!(saw_preempted_error(&mut events, "cast-1"));
    // No re-LOAD over the other sender's media.
    assert_eq!(count_sent(&sent1, "LOAD"), 1);
    drop(handle);
}

#[tokio::test(start_paused = true)]
async fn foreign_playing_does_not_recover_a_preempted_session() {
    let (engine, b) = broadcaster();
    let mut events = engine.subscribe();
    let (ch1, sent1) = ScriptedChannel::new(vec![
        ScriptedInbound::Frame(receiver_status_with_app()),
        ScriptedInbound::Frame(media_status("PLAYING", None)),
        ScriptedInbound::Wait(Duration::from_secs(2)),
        // A foreign app takes the device…
        ScriptedInbound::Frame(receiver_status_with("DEADBEEF", "s-9", "t-9")),
        // …and ITS media starts playing: the stray PLAYING must not
        // un-degrade the preempted session via Recover.
        ScriptedInbound::Frame(media_status_for(99, "PLAYING")),
        ScriptedInbound::Hang,
    ]);
    let connector = ScriptedConnector::new(vec![ch1]);
    let status = Arc::clone(b.registry());
    let actor = CastSessionActor::new(
        "cast-1",
        connector,
        "192.0.2.20:8009",
        media(),
        b,
        CastSessionConfig::default(),
    );
    let handle = actor.spawn();

    tokio::time::sleep(Duration::from_secs(5)).await;
    let snapshot = status.snapshot("cast-1").expect("a status row");
    assert_eq!(
        snapshot.state,
        DeviceState::Degraded,
        "preempted stays DEGRADED through foreign media traffic"
    );
    assert_eq!(
        snapshot.mode.as_deref(),
        Some("preempted"),
        "the foreign PLAYING never becomes our mode"
    );
    assert!(saw_preempted_error(&mut events, "cast-1"));
    assert_eq!(count_sent(&sent1, "LOAD"), 1, "still no fight");
    drop(handle);
}

#[tokio::test(start_paused = true)]
async fn reconnect_with_a_surviving_session_skips_launch_and_load() {
    let (_engine, b) = broadcaster();
    let (ch1, _sent1) = ScriptedChannel::new(vec![
        ScriptedInbound::Frame(receiver_status_with_app()),
        ScriptedInbound::Frame(media_status("PLAYING", None)),
        ScriptedInbound::Wait(Duration::from_secs(2)),
        // A network blip drops the channel; the receiver keeps playing.
        ScriptedInbound::Drop,
    ]);
    let (ch2, sent2) = ScriptedChannel::new(vec![
        // GET_STATUS answers that OUR session (s-1) is still running: the
        // reconnect must not restart playback.
        ScriptedInbound::Frame(receiver_status_with_app()),
        ScriptedInbound::Frame(media_status("PLAYING", None)),
        ScriptedInbound::Hang,
    ]);
    let connector = ScriptedConnector::new(vec![ch1, ch2]);
    let connects = connector.connect_count();
    let status = Arc::clone(b.registry());
    let actor = CastSessionActor::new(
        "cast-1",
        connector,
        "192.0.2.20:8009",
        media(),
        b,
        CastSessionConfig::default(),
    );
    let handle = actor.spawn();

    tokio::time::sleep(Duration::from_secs(12)).await;
    assert_eq!(connects.load(std::sync::atomic::Ordering::SeqCst), 2);
    // The whole reconnect conversation: CONNECT → GET_STATUS → CONNECT to
    // the surviving app transport. NO LAUNCH, NO LOAD — playback untouched.
    assert_eq!(
        sent_types(&sent2),
        vec![
            (NS_CONNECTION.to_owned(), "CONNECT".to_owned()),
            (NS_RECEIVER.to_owned(), "GET_STATUS".to_owned()),
            (NS_CONNECTION.to_owned(), "CONNECT".to_owned()),
        ]
    );
    let frames = sent2.lock().expect("sent log lock").clone();
    assert_eq!(
        frames[2].destination, "t-1",
        "the re-CONNECT addresses the surviving app transport"
    );
    assert_eq!(status.state("cast-1"), Some(DeviceState::Online));
    let snapshot = status.snapshot("cast-1").expect("a status row");
    assert_eq!(snapshot.mode.as_deref(), Some("playing"));
    drop(handle);
}

#[tokio::test(start_paused = true)]
async fn preemption_during_a_blip_is_not_stomped_on_reconnect() {
    let (engine, b) = broadcaster();
    let mut events = engine.subscribe();
    let (ch1, _sent1) = ScriptedChannel::new(vec![
        ScriptedInbound::Frame(receiver_status_with_app()),
        ScriptedInbound::Frame(media_status("PLAYING", None)),
        ScriptedInbound::Wait(Duration::from_secs(2)),
        ScriptedInbound::Drop,
    ]);
    let (ch2, sent2) = ScriptedChannel::new(vec![
        // During the blip another sender took the device (a new session of
        // the same app): GET_STATUS reveals it — re-LAUNCHing over them is
        // exactly the fight ADR-M011 forbids.
        ScriptedInbound::Frame(receiver_status_with("CC1AD845", "s-2", "t-2")),
        ScriptedInbound::Hang,
    ]);
    let connector = ScriptedConnector::new(vec![ch1, ch2]);
    let connects = connector.connect_count();
    let status = Arc::clone(b.registry());
    let actor = CastSessionActor::new(
        "cast-1",
        connector,
        "192.0.2.20:8009",
        media(),
        b,
        CastSessionConfig::default(),
    );
    let handle = actor.spawn();

    tokio::time::sleep(Duration::from_secs(12)).await;
    assert_eq!(connects.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(
        count_sent(&sent2, "LAUNCH"),
        0,
        "never re-LAUNCH over the sender that took the device mid-blip"
    );
    assert_eq!(count_sent(&sent2, "LOAD"), 0);
    assert_eq!(status.state("cast-1"), Some(DeviceState::Degraded));
    assert!(
        saw_preempted_error(&mut events, "cast-1"),
        "the blip-time preemption is surfaced"
    );
    drop(handle);
}

#[tokio::test]
async fn inbound_device_ping_is_answered_with_pong() {
    let (_engine, b) = broadcaster();
    let (channel, sent) = ScriptedChannel::new(vec![
        ScriptedInbound::Frame(receiver_status_with_app()),
        ScriptedInbound::Frame(from_device(
            NS_HEARTBEAT,
            serde_json::json!({"type": "PING"}),
        )),
    ]);
    let connector = ScriptedConnector::new(vec![channel]);
    let mut actor = CastSessionActor::new(
        "cast-1",
        connector,
        "192.0.2.20:8009",
        media(),
        b,
        CastSessionConfig::default(),
    );
    assert_eq!(actor.connect_step().await, PollerStep::Online);
    let _ = actor.pump_step().await;
    assert_eq!(
        count_sent(&sent, "PONG"),
        1,
        "the device's own PING is answered: {:?}",
        sent_types(&sent)
    );
}

#[tokio::test]
async fn the_load_accepted_hook_fires_at_the_first_active_media_status() {
    // DEV-D3.1: the session's started-at stamp must mark the moment the cast
    // became REAL — the receiver accepted our LOAD and created a media
    // session — never the REST accept (which would lie about failed loads)
    // and never the bare LOAD dispatch.
    let (_engine, b) = broadcaster();
    let fired = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let probe = Arc::clone(&fired);
    let (channel, _sent) = ScriptedChannel::new(vec![
        ScriptedInbound::Frame(receiver_status_with_app()),
        ScriptedInbound::Frame(media_status("PLAYING", None)),
        ScriptedInbound::Frame(media_status("PLAYING", None)),
    ]);
    let connector = ScriptedConnector::new(vec![channel]);
    let mut actor = CastSessionActor::new(
        "cast-1",
        connector,
        "[2001:db8::20]:8009",
        media(),
        b,
        CastSessionConfig::default(),
    )
    .with_load_accepted_hook(Arc::new(move || {
        probe.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }));

    // Establishment merely DISPATCHES the LOAD: not yet accepted.
    assert_eq!(actor.connect_step().await, PollerStep::Online);
    assert_eq!(
        fired.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "LOAD dispatched is not LOAD accepted — no stamp yet"
    );

    // The first MEDIA_STATUS attributing an ACTIVE media session to our LOAD
    // is the accept point (the receiver created the session; the TV shows it).
    assert_eq!(actor.pump_step().await, PollerStep::Online);
    assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Later statuses for the already-adopted session never re-fire it.
    assert_eq!(actor.pump_step().await, PollerStep::Online);
    assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_rejected_load_never_fires_the_load_accepted_hook() {
    let (_engine, b) = broadcaster();
    let fired = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let probe = Arc::clone(&fired);
    let (channel, _sent) = ScriptedChannel::new(vec![
        ScriptedInbound::Frame(receiver_status_with_app()),
        ScriptedInbound::Frame(load_failed()),
    ]);
    let connector = ScriptedConnector::new(vec![channel]);
    let mut actor = CastSessionActor::new(
        "cast-1",
        connector,
        "[2001:db8::20]:8009",
        media(),
        b,
        CastSessionConfig::default(),
    )
    .with_load_accepted_hook(Arc::new(move || {
        probe.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }));

    assert_eq!(actor.connect_step().await, PollerStep::Online);
    // The receiver rejected the LOAD: the session degrades and the hook must
    // never fire — a session whose TV stayed blank never "started".
    assert_eq!(actor.pump_step().await, PollerStep::Degraded);
    assert_eq!(
        fired.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a rejected LOAD must not stamp a start"
    );
}
