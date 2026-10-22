//! Realtime tests: the snapshot-then-delta + resume-by-seq envelope flow driven
//! against a synthetic engine event source, plus the isolation property — a
//! never-reading client lags rather than back-pressuring the publisher.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::Arc;

use multiview_control::{DeviceStatusRegistry, SessionStream};
use multiview_core::time::Rational;
use multiview_core::wallclock::WallClockRef;
use multiview_engine::EnginePublisher;
use multiview_events::{
    AddressFamily, Alert, AlertSeverity, CastSessionStarted, ClockQuality, ClockSource,
    DeviceDiscovered, DeviceState, DeviceStatus, Event, FrameKind, InputConnection, LifecycleState,
    MediaPlayerEvent, MediaPlayerState, RistLinkRole, RistLinkStats, TileState, TimingStatus,
    Topic,
};

type Publisher = EnginePublisher<serde_json::Value, Event>;

fn alert(key: &str) -> Event {
    Event::AlertRaised(Alert {
        key: key.to_owned(),
        severity: AlertSeverity::Warning,
        title: "test".to_owned(),
        detail: None,
        active: true,
    })
}

/// A `device.status` event scoped (by `event_scope_id`) to `device_id`.
fn device_status(device_id: &str) -> Event {
    Event::DeviceStatus(DeviceStatus::new(device_id, DeviceState::Online))
}

/// A `cast.session.started` event scoped (by `event_scope_id`) to `session_id`.
fn cast_started(session_id: &str) -> Event {
    Event::CastSessionStarted(CastSessionStarted {
        session_id: session_id.to_owned(),
        name: None,
        address: "[2001:db8::20]:8009".to_owned(),
        output: "out-a".to_owned(),
    })
}

/// Pull the device id off a `DeviceStatus` delta (test helper).
fn delta_device_id(frame: &multiview_control::RealtimeFrame) -> String {
    match &frame.envelope.payload {
        Event::DeviceStatus(s) => s.device_id.clone(),
        other => panic!("expected a device.status delta, got {other:?}"),
    }
}

/// A `media.player_state` event scoped (via `authorize_object`) to `player`.
fn media_player_state(player: &str) -> Event {
    Event::MediaPlayerState(MediaPlayerEvent::new(player, MediaPlayerState::Playing, 0))
}

/// A `tile.state` event bound to `input` (or unbound when `None`).
fn tile_state(input: Option<&str>) -> Event {
    Event::TileState(TileState {
        from: LifecycleState::Live,
        to: LifecycleState::NoSignal,
        input: input.map(str::to_owned),
        trigger: "nosignal_timeout".to_owned(),
    })
}

/// A `rist.link.stats` event — scoped (by `authz_scope`) to output `link_id`.
fn rist_link_stats(link_id: &str) -> Event {
    Event::RistLinkStats(RistLinkStats {
        link_id: link_id.to_owned(),
        role: RistLinkRole::Sender,
        flow_id: 1,
        cname: "peer.invalid".to_owned(),
        peer_count: 1,
        rtt_ms: 10,
        quality: 100.0,
        bandwidth_bps: 1_000_000,
        retry_bandwidth_bps: 0,
        sent: 1,
        received: 0,
        retransmitted: 0,
        lost: 0,
        recovered: 0,
        since: 0,
    })
}

/// A `timing.status` event — scoped (by `authz_scope`) to program `stream_id`.
fn timing_status(stream_id: &str) -> Event {
    Event::TimingStatus(TimingStatus {
        stream_id: stream_id.to_owned(),
        epoch: WallClockRef::new(0, 0, Rational::new(90_000, 1)),
        link_offset_ns: 0,
        clock_source: ClockSource::System,
        clock_quality: ClockQuality::Locked,
        groups: vec![],
    })
}

/// A `device.discovered` event stamped with `domain` (or unlabelled when `None`).
fn device_discovered(domain: Option<&str>) -> Event {
    let mut row = DeviceDiscovered::new(
        "zowietek".to_owned(),
        "http://[fd00:db8::42]".to_owned(),
        AddressFamily::Ipv6,
    );
    if let Some(domain) = domain {
        row = row.with_domain(domain.to_owned());
    }
    Event::DeviceDiscovered(row)
}

#[tokio::test]
async fn snapshot_precedes_deltas_with_monotonic_connection_seq() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));
    let sub = engine.subscribe();
    let mut session = SessionStream::new(sub, "sess-1", None);

    // Snapshot first, on the $control topic, at connection seq 0.
    let snap = session.snapshot_frame(engine.state.sequence());
    assert_eq!(snap.kind, FrameKind::Snapshot);
    assert_eq!(snap.envelope.topic, Topic::Control);
    assert_eq!(snap.envelope.seq.get(), 0);
    assert!(matches!(snap.envelope.payload, Event::Hello(_)));

    // Engine publishes two events; the session emits them as deltas with
    // strictly increasing per-connection seqs (1, 2) on their topics.
    engine.publish_event(alert("a"));
    engine.publish_event(Event::InputConnection(InputConnection::new(
        "input:cam1",
        LifecycleState::Live,
    )));

    let d1 = session
        .next_delta()
        .await
        .unwrap()
        .expect("first delta present");
    assert_eq!(d1.kind, FrameKind::Delta);
    assert_eq!(d1.envelope.seq.get(), 1);
    assert_eq!(d1.envelope.topic, Topic::Alerts);

    let d2 = session
        .next_delta()
        .await
        .unwrap()
        .expect("second delta present");
    assert_eq!(d2.envelope.seq.get(), 2);
    assert_eq!(d2.envelope.topic, Topic::Inputs);

    // The wire form round-trips through serde.
    let text = d2.to_json().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["topic"], "inputs");
    assert_eq!(parsed["t"], "input.connection");
}

#[tokio::test]
async fn resume_after_skips_already_observed_events() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));

    // Publish three events while a subscriber is live so they are buffered.
    let sub = engine.subscribe();
    let s1 = engine.publish_event(alert("one"));
    let _s2 = engine.publish_event(alert("two"));
    let s3 = engine.publish_event(alert("three"));
    assert!(s3 > s1);

    // The client resumes after the first engine seq: it should NOT re-receive
    // event one, but should receive two and three.
    let mut session = SessionStream::new(sub, "sess-resume", Some(s1));

    // First poll: event one is skipped (Ok(None)).
    assert_eq!(session.next_delta().await.unwrap(), None);
    // Next polls deliver two then three.
    let d_two = session.next_delta().await.unwrap().expect("event two");
    let two_key = match &d_two.envelope.payload {
        Event::AlertRaised(a) => a.key.clone(),
        other => panic!("expected alert, got {other:?}"),
    };
    assert_eq!(two_key, "two");

    let d_three = session.next_delta().await.unwrap().expect("event three");
    let three_key = match &d_three.envelope.payload {
        Event::AlertRaised(a) => a.key.clone(),
        other => panic!("expected alert, got {other:?}"),
    };
    assert_eq!(three_key, "three");
}

#[tokio::test]
async fn slow_client_lags_without_back_pressuring_the_publisher() {
    // A small ring; the engine publishes far more than capacity while the
    // session never drains. Each publish must return promptly (non-blocking)
    // and the lagging session recovers via resubscribe (lagged-skip), never
    // forcing the engine to wait. This is the invariant #10 chaos property.
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(4));
    let sub = engine.subscribe();
    let mut session = SessionStream::new(sub, "sess-slow", None);

    // Overflow the ring many times over. publish_event is wait-free; if it
    // could block on a slow consumer this whole loop would hang — completing it
    // is the proof the engine is never back-pressured (invariant #10).
    for i in 0..1000 {
        let seq = engine.publish_event(alert(&format!("evt-{i}")));
        assert_eq!(seq, u64::try_from(i + 1).unwrap());
    }

    // The far-behind session recovers via lagged-skip: its next poll observes
    // the overflow and re-subscribes (Ok(None)) rather than erroring or hanging.
    // A timeout guards against a regression that would block here.
    let recovery = tokio::time::timeout(std::time::Duration::from_secs(5), session.next_delta())
        .await
        .expect("lagged recovery must not block")
        .expect("lagged recovery is not a stream error");
    assert_eq!(
        recovery, None,
        "a far-behind client observes a lagged-skip recovery, not back-pressure"
    );

    // After recovery the session resumes cleanly: an event published now is
    // delivered as the next delta.
    engine.publish_event(alert("after-recovery"));
    let next = tokio::time::timeout(std::time::Duration::from_secs(5), session.next_delta())
        .await
        .expect("post-recovery delivery must not block")
        .expect("post-recovery delivery is not a stream error")
        .expect("an event published after recovery is delivered");
    match &next.envelope.payload {
        Event::AlertRaised(a) => assert_eq!(a.key, "after-recovery"),
        other => panic!("expected the post-recovery alert, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Object-scope realtime filtering (BOLA, ADR-W005 / ADR-W025): a scoped
// principal's realtime stream is a read-side projection that delivers only the
// Devices-domain object events (device.* / cast.session.*) whose scope id is in
// its allowlist — by parity with `GET /{id}` returning 403 out of scope. The
// filter is a pure per-client read decision: it NEVER blocks or touches the
// engine publish path (invariant #10).
// ---------------------------------------------------------------------------

/// A scoped session drops device/cast deltas whose scope id is outside its
/// object allowlist, and delivers the in-scope ones — so a scoped client cannot
/// enumerate (or read the status of) devices/sessions it could not `GET`.
#[tokio::test]
async fn scoped_session_filters_device_and_cast_deltas_to_the_allowlist() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));
    let sub = engine.subscribe();
    // Scoped to a single object id: only `dev-mine` / `cast-mine` are visible.
    let mut session = SessionStream::new(sub, "sess-scoped", None)
        .with_object_scope(Some(vec!["dev-mine".to_owned()]));

    // The engine publishes, in order: an out-of-scope device status, an in-scope
    // device status, an out-of-scope cast-session-start, then an unscoped alert.
    engine.publish_event(device_status("dev-other"));
    engine.publish_event(device_status("dev-mine"));
    engine.publish_event(cast_started("cast-other"));
    engine.publish_event(alert("global"));

    // The out-of-scope device status is dropped (Ok(None)) — never leaked.
    assert_eq!(
        session.next_delta().await.unwrap(),
        None,
        "an out-of-scope device.status must be filtered out"
    );
    // The in-scope device status is delivered.
    let d = session
        .next_delta()
        .await
        .unwrap()
        .expect("the in-scope device.status is delivered");
    assert_eq!(delta_device_id(&d), "dev-mine");
    // The out-of-scope cast session start is dropped.
    assert_eq!(
        session.next_delta().await.unwrap(),
        None,
        "an out-of-scope cast.session.started must be filtered out"
    );
    // The unscoped firehose event (an alert, no object scope) is still delivered
    // — object scope filters object-bearing Devices-domain events only, not the
    // role-gated firehose.
    let a = session
        .next_delta()
        .await
        .unwrap()
        .expect("a non-object event is unaffected by object scope");
    match &a.envelope.payload {
        Event::AlertRaised(alert) => assert_eq!(alert.key, "global"),
        other => panic!("expected the global alert, got {other:?}"),
    }
}

/// An unscoped session (the default admin/operator/viewer) delivers EVERY
/// device/cast delta — the filter must not over-restrict.
#[tokio::test]
async fn unscoped_session_delivers_all_device_deltas() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));
    let sub = engine.subscribe();
    let mut session = SessionStream::new(sub, "sess-unscoped", None).with_object_scope(None);

    engine.publish_event(device_status("dev-a"));
    engine.publish_event(device_status("dev-b"));

    let d1 = session
        .next_delta()
        .await
        .unwrap()
        .expect("dev-a delivered");
    assert_eq!(delta_device_id(&d1), "dev-a");
    let d2 = session
        .next_delta()
        .await
        .unwrap()
        .expect("dev-b delivered");
    assert_eq!(delta_device_id(&d2), "dev-b");
}

/// The connect-time device-status SNAPSHOT frames are filtered too: a scoped
/// session rebuilds its device cache from ONLY its in-scope devices, so the
/// snapshot cannot leak an out-of-scope device the deltas then hide.
#[tokio::test]
async fn scoped_session_filters_device_snapshot_frames_to_the_allowlist() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));
    let registry = DeviceStatusRegistry::new();
    registry.set_status(DeviceStatus::new("dev-mine", DeviceState::Online));
    registry.set_status(DeviceStatus::new("dev-other", DeviceState::Online));

    // Scoped: only `dev-mine` may appear in the connect snapshot.
    let mut scoped = SessionStream::new(engine.subscribe(), "sess-scoped", None)
        .with_object_scope(Some(vec!["dev-mine".to_owned()]));
    let frames = scoped.devices_snapshot_frames(&registry, 0);
    let ids: Vec<String> = frames.iter().map(delta_device_id).collect();
    assert_eq!(
        ids,
        vec!["dev-mine".to_owned()],
        "the connect snapshot must carry ONLY in-scope devices (no enumeration leak)"
    );

    // Unscoped: both devices appear (id-sorted), unchanged.
    let mut unscoped =
        SessionStream::new(engine.subscribe(), "sess-unscoped", None).with_object_scope(None);
    let frames = unscoped.devices_snapshot_frames(&registry, 0);
    let ids: Vec<String> = frames.iter().map(delta_device_id).collect();
    assert_eq!(ids, vec!["dev-mine".to_owned(), "dev-other".to_owned()]);
}

// ---- ADR-W026 output / program / discovery-domain axis parity ----------
// The realtime delta filter routes every event's `authz_scope()` through the
// SAME `scope_permits` predicate REST uses, so the firehose the pre-W026
// `_ => None` classifier delivered to scoped principals is closed.

/// An OUTPUT-scoped session drops a `rist.link.stats` for an out-of-scope output
/// (its `cname` leaks peer topology) and a `timing.status` for an un-granted
/// program, and delivers the in-scope link plus the `program:`-granted timing —
/// the realtime twin of REST output/program authorization.
#[tokio::test]
async fn output_scoped_session_filters_rist_and_timing_by_output_and_program() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));
    let mut session = SessionStream::new(engine.subscribe(), "sess-out", None).with_scopes(
        None,
        Some(vec!["out-a".to_owned(), "program:main".to_owned()]),
        None,
    );

    engine.publish_event(rist_link_stats("out-b")); // out-of-scope output
    engine.publish_event(rist_link_stats("out-a")); // in-scope output
    engine.publish_event(timing_status("other")); // un-granted program
    engine.publish_event(timing_status("main")); // program:main granted
    engine.publish_event(alert("global")); // public

    // Out-of-scope output rist stats dropped (no seq gap).
    assert_eq!(session.next_delta().await.unwrap(), None);
    let d = session
        .next_delta()
        .await
        .unwrap()
        .expect("in-scope output rist stats delivered");
    assert!(matches!(&d.envelope.payload, Event::RistLinkStats(s) if s.link_id == "out-a"));
    // Un-granted program timing dropped.
    assert_eq!(session.next_delta().await.unwrap(), None);
    let d = session
        .next_delta()
        .await
        .unwrap()
        .expect("program:main-granted timing delivered");
    assert!(matches!(&d.envelope.payload, Event::TimingStatus(t) if t.stream_id == "main"));
    let d = session
        .next_delta()
        .await
        .unwrap()
        .expect("public alert delivered regardless of scope");
    assert!(matches!(&d.envelope.payload, Event::AlertRaised(_)));
}

/// A DISCOVERY-scoped session drops a `device.discovered` row for another domain
/// AND an unlabelled row (fail-closed), and delivers only its own domain's rows.
#[tokio::test]
async fn discovery_scoped_session_filters_device_discovered_by_domain() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));
    let mut session = SessionStream::new(engine.subscribe(), "sess-disc", None).with_scopes(
        None,
        None,
        Some(vec!["site-a".to_owned()]),
    );

    engine.publish_event(device_discovered(Some("site-b"))); // other domain
    engine.publish_event(device_discovered(None)); // unlabelled — fail-closed
    engine.publish_event(device_discovered(Some("site-a"))); // in-domain
    engine.publish_event(alert("global")); // public

    assert_eq!(session.next_delta().await.unwrap(), None);
    assert_eq!(session.next_delta().await.unwrap(), None);
    let d = session
        .next_delta()
        .await
        .unwrap()
        .expect("in-domain discovery row delivered");
    assert!(
        matches!(&d.envelope.payload, Event::DeviceDiscovered(x) if x.domain.as_deref() == Some("site-a"))
    );
    let d = session
        .next_delta()
        .await
        .unwrap()
        .expect("public alert delivered");
    assert!(matches!(&d.envelope.payload, Event::AlertRaised(_)));
}

/// An UNSCOPED session still receives every new-axis event — including an
/// unlabelled discovery row — so the filter never over-restricts the default
/// admin/operator/viewer (regression against the pre-W026 firehose behavior).
#[tokio::test]
async fn unscoped_session_delivers_new_axis_events() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));
    let mut session = SessionStream::new(engine.subscribe(), "sess-unscoped-new", None);

    engine.publish_event(rist_link_stats("out-x"));
    engine.publish_event(timing_status("prog-x"));
    engine.publish_event(device_discovered(None));

    let d = session
        .next_delta()
        .await
        .unwrap()
        .expect("rist delivered to unscoped");
    assert!(matches!(&d.envelope.payload, Event::RistLinkStats(_)));
    let d = session
        .next_delta()
        .await
        .unwrap()
        .expect("timing delivered to unscoped");
    assert!(matches!(&d.envelope.payload, Event::TimingStatus(_)));
    let d = session
        .next_delta()
        .await
        .unwrap()
        .expect("unlabelled discovery delivered to unscoped");
    assert!(matches!(&d.envelope.payload, Event::DeviceDiscovered(_)));
}

/// A scoped session drops `media.player_state` (player id) and `tile.state`
/// (bound input id) deltas outside its object allowlist — by parity with
/// `GET /media/players/{id}` and `GET /inputs/{id}/streams` returning 403 — and
/// delivers the in-scope ones plus any tile carrying no input.
#[tokio::test]
async fn scoped_session_filters_media_player_and_tile_deltas_to_the_allowlist() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));
    let sub = engine.subscribe();
    // Scoped to one object id, shared by a player and an input named the same.
    let mut session = SessionStream::new(sub, "sess-scoped", None)
        .with_object_scope(Some(vec!["mine".to_owned()]));

    // In order: out-of-scope player, in-scope player, out-of-scope tile input,
    // in-scope tile input, then a placeholder tile with no input.
    engine.publish_event(media_player_state("vt-other"));
    engine.publish_event(media_player_state("mine"));
    engine.publish_event(tile_state(Some("cam-other")));
    engine.publish_event(tile_state(Some("mine")));
    engine.publish_event(tile_state(None));

    assert_eq!(
        session.next_delta().await.unwrap(),
        None,
        "an out-of-scope media.player_state must be filtered out"
    );
    let p = session
        .next_delta()
        .await
        .unwrap()
        .expect("the in-scope media.player_state is delivered");
    match &p.envelope.payload {
        Event::MediaPlayerState(e) => assert_eq!(e.player, "mine"),
        other => panic!("expected a media.player_state delta, got {other:?}"),
    }
    assert_eq!(
        session.next_delta().await.unwrap(),
        None,
        "an out-of-scope tile.state (bound input) must be filtered out"
    );
    let t = session
        .next_delta()
        .await
        .unwrap()
        .expect("the in-scope tile.state is delivered");
    match &t.envelope.payload {
        Event::TileState(e) => assert_eq!(e.input.as_deref(), Some("mine")),
        other => panic!("expected a tile.state delta, got {other:?}"),
    }
    // A placeholder tile carries no object id — it rides the firehose.
    let placeholder = session
        .next_delta()
        .await
        .unwrap()
        .expect("a tile.state with no bound input is unaffected by object scope");
    match &placeholder.envelope.payload {
        Event::TileState(e) => assert_eq!(e.input, None),
        other => panic!("expected the placeholder tile.state, got {other:?}"),
    }
}

/// The connect-time tiles SNAPSHOT is filtered too: a scoped session rebuilds
/// its tile cache from ONLY tiles whose bound input is in scope (placeholder
/// tiles with no input are kept), so the snapshot cannot leak an out-of-scope
/// input the deltas then hide.
#[tokio::test]
async fn scoped_session_filters_tiles_snapshot_to_the_allowlist() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));
    // The engine latest-state blob the snapshot reads: three tiles — one bound
    // to an in-scope input, one to an out-of-scope input, one unbound.
    let snapshot = serde_json::json!({
        "tiles": [
            { "id": "tile-mine", "state": "LIVE", "input": "mine" },
            { "id": "tile-other", "state": "LIVE", "input": "cam-other" },
            { "id": "tile-empty", "state": "NO_SIGNAL" }
        ]
    });

    let tile_ids = |frame: &multiview_control::RealtimeFrame| -> Vec<String> {
        match &frame.envelope.payload {
            Event::TilesSnapshot(s) => s.tiles.iter().map(|t| t.id.clone()).collect(),
            other => panic!("expected a tiles snapshot, got {other:?}"),
        }
    };

    // Scoped: only the in-scope-input tile and the unbound tile survive.
    let mut scoped = SessionStream::new(engine.subscribe(), "sess-scoped", None)
        .with_object_scope(Some(vec!["mine".to_owned()]));
    let frame = scoped
        .tiles_snapshot_frame(&snapshot, 0)
        .expect("a tiles snapshot frame is built");
    assert_eq!(
        tile_ids(&frame),
        vec!["tile-mine".to_owned(), "tile-empty".to_owned()],
        "the tiles snapshot must drop tiles bound to an out-of-scope input (no enumeration leak)"
    );

    // Unscoped: all three tiles appear, unchanged.
    let mut unscoped =
        SessionStream::new(engine.subscribe(), "sess-unscoped", None).with_object_scope(None);
    let frame = unscoped
        .tiles_snapshot_frame(&snapshot, 0)
        .expect("a tiles snapshot frame is built");
    assert_eq!(
        tile_ids(&frame),
        vec![
            "tile-mine".to_owned(),
            "tile-other".to_owned(),
            "tile-empty".to_owned()
        ]
    );
}
