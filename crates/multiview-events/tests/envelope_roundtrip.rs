#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! Serde round-trip and wire-shape contract tests for the envelope + event
//! discriminated union (ADR-RT002).

use multiview_core::time::MediaTime;
use multiview_events::event::LifecycleState;
use multiview_events::{
    Alert, AlertSeverity, AudioMeter, Envelope, Event, EventEnvelope, Hello, Lag, LagAction,
    OutputRunState, OutputStatus, Resync, ResyncReason, SchemaVersion, Seq, Subscribe, Subscribed,
    TileState, Topic,
};
use serde_json::{json, Value};

fn ts() -> MediaTime {
    MediaTime::from_nanos(920_451_123_456)
}

#[test]
fn event_envelope_roundtrips_through_json() {
    let env: EventEnvelope = Envelope::new(
        Topic::Tiles,
        Seq::new(184_213),
        ts(),
        Event::TileState(TileState {
            from: LifecycleState::Reconnecting,
            to: LifecycleState::NoSignal,
            input: Some("input:ndi3".to_owned()),
            trigger: "nosignal_timeout".to_owned(),
        }),
    )
    .with_id("tile:small1");

    let text = serde_json::to_string(&env).unwrap();
    let back: EventEnvelope = serde_json::from_str(&text).unwrap();
    assert_eq!(env, back, "envelope must survive a JSON round-trip");
}

#[test]
fn envelope_wire_shape_has_flattened_t_and_data() {
    // The event payload must flatten into the envelope as a `t` discriminator
    // plus a `data` body (ADR-RT002), NOT a nested `payload` object and NOT an
    // untagged shape.
    let env: EventEnvelope = Envelope::new(
        Topic::Outputs,
        Seq::new(52_310),
        ts(),
        Event::OutputStatus(OutputStatus {
            state: OutputRunState::Running,
            bitrate_bps: Some(5_980_000),
            clients: Some(12),
        }),
    )
    .with_id("output:ll_hls_main");

    let v: Value = serde_json::to_value(&env).unwrap();
    let obj = v.as_object().unwrap();

    assert_eq!(obj.get("v").unwrap(), &json!(1), "schema major `v` present");
    assert_eq!(obj.get("topic").unwrap(), &json!("outputs"));
    assert_eq!(obj.get("id").unwrap(), &json!("output:ll_hls_main"));
    assert_eq!(obj.get("seq").unwrap(), &json!(52_310));
    assert_eq!(obj.get("ts").unwrap(), &json!(920_451_123_456_i64));
    assert_eq!(
        obj.get("t").unwrap(),
        &json!("output.status"),
        "internally-tagged discriminator `t` must be flattened to the top level"
    );
    assert!(
        obj.contains_key("data"),
        "payload body must live under `data`"
    );
    assert!(
        !obj.contains_key("payload"),
        "the Rust field name `payload` must NOT leak onto the wire (it is flattened)"
    );
    // The data body carries the typed fields.
    let data = obj.get("data").unwrap().as_object().unwrap();
    assert_eq!(data.get("state").unwrap(), &json!("running"));
    assert_eq!(data.get("bitrate_bps").unwrap(), &json!(5_980_000));
}

#[test]
fn optional_fields_are_omitted_when_absent() {
    // No `id`, no `corr` -> those keys must not appear (skip_serializing_if).
    let env: EventEnvelope = Envelope::new(Topic::Control, Seq::ZERO, ts(), Event::Ping);
    let v: Value = serde_json::to_value(&env).unwrap();
    let obj = v.as_object().unwrap();
    assert!(!obj.contains_key("id"), "absent id must be omitted");
    assert!(!obj.contains_key("corr"), "absent corr must be omitted");
    assert_eq!(obj.get("t").unwrap(), &json!("$ping"));
}

#[test]
fn corr_roundtrips_for_job_correlation() {
    let env: EventEnvelope = Envelope::new(
        Topic::Jobs,
        Seq::new(33_120),
        ts(),
        Event::JobProgress(multiview_events::JobProgress {
            phase: "prewarming_inputs".to_owned(),
            pct: 60,
            message: Some("input:cam4 connected".to_owned()),
        }),
    )
    .with_id("job:apply_8821")
    .with_corr("req_5f2a");

    let v: Value = serde_json::to_value(&env).unwrap();
    assert_eq!(v.get("corr").unwrap(), &json!("req_5f2a"));

    let back: EventEnvelope = serde_json::from_value(v).unwrap();
    assert_eq!(back.corr.as_deref(), Some("req_5f2a"));
    assert_eq!(back, env);
}

#[test]
fn deserializes_brief_tile_state_fixture() {
    // The exact fixture from realtime-api.md §3 must parse (minus the
    // not-yet-modeled showing/since_ts fields, which serde ignores as unknown).
    let fixture = json!({
        "v": 1,
        "t": "tile.state",
        "topic": "tiles",
        "id": "tile:small1",
        "seq": 184_213,
        "ts": 920_451_123_456_i64,
        "data": {
            "from": "RECONNECTING",
            "to": "NO_SIGNAL",
            "input": "input:ndi3",
            "trigger": "nosignal_timeout"
        }
    });
    let env: EventEnvelope = serde_json::from_value(fixture).unwrap();
    assert_eq!(env.topic, Topic::Tiles);
    assert_eq!(env.id.as_deref(), Some("tile:small1"));
    match env.payload {
        Event::TileState(ts) => {
            assert_eq!(ts.from, LifecycleState::Reconnecting);
            assert_eq!(ts.to, LifecycleState::NoSignal);
            assert_eq!(ts.input.as_deref(), Some("input:ndi3"));
        }
        other => panic!("expected TileState, got {other:?}"),
    }
}

#[test]
fn control_frames_use_dollar_prefixed_discriminators() {
    let cases: Vec<(Event, &str)> = vec![
        (
            Event::Hello(Hello {
                session_id: "s_8f3a91c2".to_owned(),
                server_v: vec![SchemaVersion::V1],
                heartbeat_ms: 15_000,
                min_rate_hz: 1,
                max_rate_hz: 30,
                default_rate_hz: 10,
                replay_ring: 1024,
            }),
            "$hello",
        ),
        (
            Event::Subscribe(Subscribe {
                topics: vec![Topic::Tiles, Topic::AudioMeters],
                ids: vec!["tile:big".to_owned()],
                rate_hz: Some(25),
                since_seq: Some(Seq::new(184_250)),
            }),
            "$subscribe",
        ),
        (
            Event::Subscribed(Subscribed {
                topic: Topic::AudioMeters,
                effective_rate_hz: 25,
                snapshot_seq: Seq::new(43),
            }),
            "$subscribed",
        ),
        (
            Event::Resync(Resync {
                reason: ResyncReason::SeqEvicted,
                resubscribe: vec![Topic::Tiles, Topic::Outputs, Topic::Alerts],
            }),
            "$resync",
        ),
        (
            Event::Lag(Lag {
                topic: Topic::AudioMeters,
                dropped_n: 143,
                action: LagAction::Conflated,
            }),
            "$lag",
        ),
        (Event::Pong, "$pong"),
    ];

    for (event, tag) in cases {
        assert_eq!(event.type_tag(), tag, "type_tag mismatch for {tag}");
        assert!(event.is_control(), "{tag} must be a control frame");
        // Round-trips through the discriminated union.
        let v = serde_json::to_value(&event).unwrap();
        assert_eq!(v.get("t").unwrap(), &json!(tag));
        let back: Event = serde_json::from_value(v).unwrap();
        assert_eq!(back, event);
    }
}

#[test]
fn unknown_discriminator_is_rejected() {
    // Internally-tagged (NOT untagged): an unknown `t` is a hard parse error,
    // not a silent fallthrough.
    let bad = json!({"t": "totally.unknown.event", "data": {}});
    let parsed: Result<Event, _> = serde_json::from_value(bad);
    assert!(
        parsed.is_err(),
        "an unknown discriminator must fail to parse (tagged, never untagged)"
    );
}

#[test]
fn data_events_are_not_control_frames() {
    let alert = Event::AlertRaised(Alert {
        key: "encoder.recycle:output:rtsp_main".to_owned(),
        severity: AlertSeverity::Warning,
        title: "Encoder recycled".to_owned(),
        detail: None,
        active: true,
    });
    assert!(!alert.is_control());
    assert_eq!(alert.type_tag(), "alert.raised");

    let meter = Event::AudioMeter(AudioMeter {
        track: 0,
        peak_db: vec![-6.2, -7.1],
        rms_db: vec![-18.4, -19.0],
        clip: false,
        overflow: false,
        sampled_hz: 25,
    });
    assert!(!meter.is_control());
    // f32 fields round-trip (PartialEq, not Eq, on AudioMeter).
    let back: AudioMeter = serde_json::from_value(
        serde_json::to_value(&AudioMeter {
            track: 0,
            peak_db: vec![-6.2, -7.1],
            rms_db: vec![-18.4, -19.0],
            clip: false,
            overflow: false,
            sampled_hz: 25,
        })
        .unwrap(),
    )
    .unwrap();
    match meter {
        Event::AudioMeter(m) => assert_eq!(m, back),
        other => panic!("expected AudioMeter, got {other:?}"),
    }
}

#[test]
fn unsupported_schema_version_is_detected() {
    let env: EventEnvelope = Envelope {
        v: SchemaVersion(2),
        topic: Topic::System,
        id: None,
        seq: Seq::new(5),
        ts: ts(),
        corr: None,
        payload: Event::Ping,
    };
    let err = env
        .ensure_supported(&[SchemaVersion::V1])
        .expect_err("v2 against a v1-only receiver must error");
    match err {
        multiview_events::Error::UnsupportedSchemaVersion { got, supported } => {
            assert_eq!(got, SchemaVersion(2));
            assert_eq!(supported, vec![SchemaVersion::V1]);
        }
        other => panic!("wrong error: {other:?}"),
    }
    // And the supported case passes.
    let ok: EventEnvelope = Envelope::new(Topic::System, Seq::new(5), ts(), Event::Ping);
    assert!(ok.ensure_supported(&[SchemaVersion::V1]).is_ok());
}

#[test]
fn lifecycle_state_maps_from_core_source_state() {
    use multiview_core::traits::SourceState;
    assert_eq!(
        LifecycleState::from(SourceState::Live),
        LifecycleState::Live
    );
    assert_eq!(
        LifecycleState::from(SourceState::Stale),
        LifecycleState::Stale
    );
    assert_eq!(
        LifecycleState::from(SourceState::Reconnecting),
        LifecycleState::Reconnecting
    );
    assert_eq!(
        LifecycleState::from(SourceState::NoSignal),
        LifecycleState::NoSignal
    );
    // Wire form is SCREAMING_SNAKE_CASE.
    assert_eq!(
        serde_json::to_value(LifecycleState::NoSignal).unwrap(),
        json!("NO_SIGNAL")
    );
}
