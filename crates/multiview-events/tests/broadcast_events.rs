#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! Serde round-trip + topic-routing contract tests for the broadcast wire types
//! (Wave A2): alarm lifecycle events carrying [`multiview_core::alarm::AlarmRecord`],
//! tally state events carrying [`multiview_core::tally::TallyState`], salvo
//! arm/take events, and the new `alarms`/`tally` topics. These prove the new
//! `Event` variants are internally-tagged (`t`/`data`, never untagged), survive
//! a JSON round-trip, and route to the correct [`Topic`], and that the
//! snapshot ⊕ delta / resume-by-seq ordering still holds for the new topics.

use multiview_core::alarm::{
    AckState, AlarmId, AlarmKind, AlarmRecord, AlarmScope, PerceivedSeverity,
};
use multiview_core::tally::{Brightness, BusSource, TallyColor, TallyState};
use multiview_core::time::MediaTime;
use multiview_events::event::{AlarmTransition, SalvoEvent, SalvoPhase, TallyEvent, TallyTarget};
use multiview_events::ordering::Accepted;
use multiview_events::{
    Envelope, Event, EventEnvelope, FrameKind, GpuMetrics, GpuVendor, Seq, SystemMetrics, Topic,
    TopicCursor,
};
use serde_json::{json, Value};

fn ts() -> MediaTime {
    MediaTime::from_nanos(920_451_123_456)
}

fn sample_record() -> AlarmRecord {
    AlarmRecord::new(
        AlarmId::new("alarm:tile3:black"),
        AlarmKind::Black,
        PerceivedSeverity::Major,
        AlarmScope::Tile { index: 3 },
        ts(),
    )
}

#[test]
fn alarm_topic_routes_and_is_high_rate_excluded() {
    assert_eq!(Topic::Alarms.as_str(), "alarms");
    assert_eq!(Topic::Tally.as_str(), "tally");
    assert!(!Topic::Alarms.is_control());
    assert!(!Topic::Tally.is_control());
    // Neither alarms nor tally are high-rate conflated lanes — they must stay in
    // the lossless replay ring (ADR-RT003).
    assert!(!Topic::Alarms.is_high_rate());
    assert!(!Topic::Tally.is_high_rate());
}

#[test]
fn alarm_topic_roundtrips_on_the_wire() {
    for (topic, wire) in [(Topic::Alarms, "alarms"), (Topic::Tally, "tally")] {
        let v = serde_json::to_value(topic).unwrap();
        assert_eq!(v, json!(wire));
        let back: Topic = serde_json::from_value(v).unwrap();
        assert_eq!(back, topic);
    }
}

#[test]
fn alarm_raised_event_carries_the_core_record() {
    let env: EventEnvelope = Envelope::new(
        Topic::Alarms,
        Seq::new(7001),
        ts(),
        Event::AlarmRaised(AlarmTransition::new(sample_record())),
    )
    .with_id("alarm:tile3:black");

    // Internally-tagged: discriminator flattens to top-level `t`, body under `data`.
    let v: Value = serde_json::to_value(&env).unwrap();
    let obj = v.as_object().unwrap();
    assert_eq!(obj.get("t").unwrap(), &json!("alarm.raised"));
    assert_eq!(obj.get("topic").unwrap(), &json!("alarms"));
    assert!(
        !obj.contains_key("payload"),
        "Rust field name must not leak"
    );
    let data = obj.get("data").unwrap().as_object().unwrap();
    let record = data.get("record").unwrap().as_object().unwrap();
    assert_eq!(record.get("kind").unwrap(), &json!("Black"));
    assert_eq!(record.get("severity").unwrap(), &json!("Major"));
    // Scope is tagged (`kind`), never untagged.
    let scope = record.get("scope").unwrap().as_object().unwrap();
    assert_eq!(scope.get("kind").unwrap(), &json!("tile"));
    assert_eq!(scope.get("index").unwrap(), &json!(3));

    let back: EventEnvelope = serde_json::from_value(v).unwrap();
    assert_eq!(back, env, "alarm.raised must survive a JSON round-trip");
}

#[test]
fn all_alarm_lifecycle_variants_roundtrip_and_route() {
    let record = sample_record();
    let acked = {
        let mut r = record.clone();
        r.ack = AckState::acked("operator:jo", ts());
        r
    };
    let cases: Vec<(Event, &str)> = vec![
        (
            Event::AlarmRaised(AlarmTransition::new(record.clone())),
            "alarm.raised",
        ),
        (
            Event::AlarmUpdated(AlarmTransition::new(record.clone())),
            "alarm.updated",
        ),
        (
            Event::AlarmCleared(AlarmTransition::new(record.clone())),
            "alarm.cleared",
        ),
        (
            Event::AlarmAcked(AlarmTransition::new(acked)),
            "alarm.acked",
        ),
    ];
    for (event, tag) in cases {
        assert_eq!(event.type_tag(), tag, "type_tag mismatch for {tag}");
        assert!(!event.is_control(), "{tag} is a data event, not control");
        let v = serde_json::to_value(&event).unwrap();
        assert_eq!(v.get("t").unwrap(), &json!(tag));
        let back: Event = serde_json::from_value(v).unwrap();
        assert_eq!(back, event, "{tag} must round-trip");
    }
}

#[test]
fn alarm_acked_event_preserves_ack_state() {
    let mut record = sample_record();
    record.ack = AckState::acked("operator:jo", ts());
    let event = Event::AlarmAcked(AlarmTransition::new(record));
    let back: Event = serde_json::from_value(serde_json::to_value(&event).unwrap()).unwrap();
    match back {
        Event::AlarmAcked(t) => assert!(
            t.record.ack.is_acked(),
            "ack state must round-trip through the alarm.acked event"
        ),
        other => panic!("expected AlarmAcked, got {other:?}"),
    }
}

#[test]
fn tally_state_event_carries_the_core_tally_state() {
    let event = Event::TallyState(TallyEvent {
        target: TallyTarget::Tile { index: 5 },
        state: TallyState {
            color: TallyColor::Red,
            brightness: Brightness::FULL,
            source: BusSource::Program,
        },
    });
    assert_eq!(event.type_tag(), "tally.state");
    assert!(!event.is_control());

    let v = serde_json::to_value(&event).unwrap();
    assert_eq!(v.get("t").unwrap(), &json!("tally.state"));
    let data = v.get("data").unwrap().as_object().unwrap();
    // TallyTarget is tagged (`kind`), never untagged.
    let target = data.get("target").unwrap().as_object().unwrap();
    assert_eq!(target.get("kind").unwrap(), &json!("tile"));
    assert_eq!(target.get("index").unwrap(), &json!(5));
    let state = data.get("state").unwrap().as_object().unwrap();
    // BusSource is tagged.
    let source = state.get("source").unwrap().as_object().unwrap();
    assert_eq!(source.get("kind").unwrap(), &json!("program"));

    let back: Event = serde_json::from_value(v).unwrap();
    assert_eq!(back, event, "tally.state must round-trip");
}

#[test]
fn salvo_lifecycle_variants_roundtrip_and_route() {
    let cases: Vec<(Event, &str, SalvoPhase)> = vec![
        (
            Event::SalvoArmed(SalvoEvent::new("salvo:evening_news", SalvoPhase::Armed)),
            "salvo.armed",
            SalvoPhase::Armed,
        ),
        (
            Event::SalvoTaken(SalvoEvent::new("salvo:evening_news", SalvoPhase::Taken)),
            "salvo.taken",
            SalvoPhase::Taken,
        ),
        (
            Event::SalvoCancelled(SalvoEvent::new("salvo:evening_news", SalvoPhase::Cancelled)),
            "salvo.cancelled",
            SalvoPhase::Cancelled,
        ),
    ];
    for (event, tag, phase) in cases {
        assert_eq!(event.type_tag(), tag);
        assert!(!event.is_control());
        let v = serde_json::to_value(&event).unwrap();
        assert_eq!(v.get("t").unwrap(), &json!(tag));
        let data = v.get("data").unwrap().as_object().unwrap();
        assert_eq!(data.get("salvo").unwrap(), &json!("salvo:evening_news"));
        // phase is tagged within the body and matches the event.
        let back: Event = serde_json::from_value(v).unwrap();
        match &back {
            Event::SalvoArmed(s) | Event::SalvoTaken(s) | Event::SalvoCancelled(s) => {
                assert_eq!(s.phase, phase);
            }
            other => panic!("expected a salvo event, got {other:?}"),
        }
        assert_eq!(back, event, "{tag} must round-trip");
    }
}

#[test]
fn salvo_head_scope_roundtrips() {
    let event = Event::SalvoTaken(
        SalvoEvent::new("salvo:wall", SalvoPhase::Taken).with_head("head:wall_a"),
    );
    let back: Event = serde_json::from_value(serde_json::to_value(&event).unwrap()).unwrap();
    match back {
        Event::SalvoTaken(s) => assert_eq!(s.head.as_deref(), Some("head:wall_a")),
        other => panic!("expected SalvoTaken, got {other:?}"),
    }
}

#[test]
fn unknown_broadcast_discriminator_is_rejected() {
    // Tagged, never untagged: a near-miss tag must hard-fail, not fall through.
    let bad = json!({"t": "alarm.exploded", "data": {}});
    let parsed: Result<Event, _> = serde_json::from_value(bad);
    assert!(parsed.is_err());
}

#[test]
fn alarms_topic_obeys_snapshot_then_delta_with_resume() {
    // The new topics ride the same lossless snapshot ⊕ delta / resume-by-seq
    // contract: snapshot establishes a baseline, deltas strictly advance, gaps
    // are reported, and a fresh snapshot ($resync) rebuilds.
    let mut cur = TopicCursor::new(Topic::Alarms);
    assert_eq!(
        cur.accept(FrameKind::Snapshot, Seq::new(7000)).unwrap(),
        Accepted::SnapshotBaseline {
            seq: Seq::new(7000)
        }
    );
    assert_eq!(
        cur.accept(FrameKind::Delta, Seq::new(7001)).unwrap(),
        Accepted::Delta {
            seq: Seq::new(7001)
        }
    );
    // A gap warrants a re-snapshot.
    assert_eq!(
        cur.accept(FrameKind::Delta, Seq::new(7005)).unwrap(),
        Accepted::DeltaWithGap {
            seq: Seq::new(7005),
            gap: 3
        }
    );
    // A non-advancing delta is rejected and never moves the cursor.
    assert!(cur.accept(FrameKind::Delta, Seq::new(7005)).is_err());
    assert_eq!(cur.last_seq(), Some(Seq::new(7005)));
    // Resync rebuilds the baseline (error message carries the wire topic name).
    assert_eq!(
        cur.accept(FrameKind::Snapshot, Seq::new(1)).unwrap(),
        Accepted::SnapshotBaseline { seq: Seq::new(1) }
    );
}

#[test]
fn system_metrics_event_is_high_rate_conflated_and_roundtrips() {
    // `system` is a high-rate conflated lane (cpu/gpu/encoder telemetry): latest-
    // only, excluded from the lossless replay ring — pushed, never polled (#10).
    assert_eq!(Topic::System.as_str(), "system");
    assert!(Topic::System.is_high_rate());
    assert!(!Topic::System.is_control());

    let event = Event::SystemMetrics(SystemMetrics {
        cpu_util: 0.41,
        mem_used_bytes: Some(8_000_000_000),
        mem_total_bytes: Some(32_000_000_000),
        self_cpu_util: Some(0.12),
        self_mem_used_bytes: Some(900_000_000),
        gpus: vec![GpuMetrics {
            id: "GPU-abc".to_owned(),
            vendor: GpuVendor::Nvidia,
            name: Some("NVIDIA GeForce RTX 4060".to_owned()),
            compute_util: 0.63,
            mem_used_bytes: 4_100_000_000,
            mem_total_bytes: 12_000_000_000,
            encoder_util: Some(0.15),
            decoder_util: Some(0.0),
            encoder_sessions: Some(6),
            encoder_session_ceiling: Some(8),
            // Our share of the device-wide totals (the GPU is shared with a co-tenant).
            self_compute_util: Some(0.18),
            self_encoder_util: Some(0.05),
            self_decoder_util: Some(0.0),
            self_mem_used_bytes: Some(1_200_000_000),
            self_encoder_sessions: Some(2),
        }],
        program_fps: Some(50.0),
        sampled_hz: 2,
    });

    assert_eq!(event.type_tag(), "system.metrics");
    assert!(
        !event.is_control(),
        "system.metrics is a data event, not control"
    );

    // Internally-tagged (`t`/`data`); the vendor enum is snake_case; the NVENC
    // session counts (exact integers) render under their wire names.
    let v = serde_json::to_value(&event).unwrap();
    assert_eq!(v.get("t").unwrap(), &json!("system.metrics"));
    let data = v.get("data").unwrap().as_object().unwrap();
    let gpu = data.get("gpus").unwrap().as_array().unwrap()[0]
        .as_object()
        .unwrap();
    assert_eq!(gpu.get("vendor").unwrap(), &json!("nvidia"));
    // Device-wide totals vs our-process share both ride the wire (integer fields
    // compared exactly; the f32 util fields are covered by the full round-trip
    // below — f32→JSON→f32 is lossless, but f32 0.18 ≠ f64 0.18 widened).
    assert_eq!(gpu.get("encoder_sessions").unwrap(), &json!(6));
    assert_eq!(gpu.get("encoder_session_ceiling").unwrap(), &json!(8));
    assert_eq!(gpu.get("self_encoder_sessions").unwrap(), &json!(2));
    assert!(gpu.contains_key("self_compute_util"));
    assert!(data.contains_key("self_cpu_util"));

    // Round-trip preserves the f32 utilisations exactly (f32→JSON→f32 is lossless).
    let back: Event = serde_json::from_value(v).unwrap();
    assert_eq!(back, event, "system.metrics must survive a JSON round-trip");
}

#[test]
fn system_metrics_gpu_free_host_omits_empty_collections() {
    // A GPU-free host: `gpus` is empty (skipped on the wire) and the optional
    // host-memory + fps fields are absent. Still a valid, round-tripping sample.
    let event = Event::SystemMetrics(SystemMetrics {
        cpu_util: 0.12,
        mem_used_bytes: None,
        mem_total_bytes: None,
        self_cpu_util: None,
        self_mem_used_bytes: None,
        gpus: vec![],
        program_fps: None,
        sampled_hz: 1,
    });
    let v = serde_json::to_value(&event).unwrap();
    let data = v.get("data").unwrap().as_object().unwrap();
    assert!(
        !data.contains_key("gpus"),
        "an empty gpus list must be skipped on the wire"
    );
    assert!(!data.contains_key("program_fps"));
    let back: Event = serde_json::from_value(v).unwrap();
    assert_eq!(back, event);
}

#[test]
fn tally_topic_ordering_error_names_the_wire_topic() {
    let mut cur = TopicCursor::new(Topic::Tally);
    let err = cur.accept(FrameKind::Delta, Seq::new(1)).unwrap_err();
    match err {
        multiview_events::Error::NonMonotonic { topic, .. } => assert_eq!(topic, "tally"),
        other => panic!("wrong error: {other:?}"),
    }
}

#[test]
fn shed_load_event_roundtrips_and_is_a_data_event() {
    use multiview_events::{ShedLoad, ShedReason, ShedScope};
    // A shed-load decision is a discrete, lossless degradation occurrence: it
    // must NOT be a control frame and must NOT be conflated (the retention store
    // + replay ring need every shed).
    let event = Event::ShedLoad(ShedLoad {
        reason: ShedReason::NoBetterHome,
        scope: ShedScope::Program,
        level: 2,
        dropped: 17,
    });
    assert_eq!(event.type_tag(), "shed.load");
    assert!(
        !event.is_control(),
        "shed.load is a data event, not control"
    );
    assert!(
        !event.is_conflated(),
        "a shed is a discrete lossless event, never conflated"
    );

    // Internally-tagged (`t`/`data`, never untagged); the reason + scope are
    // tagged/snake_case enums on the wire.
    let v = serde_json::to_value(&event).unwrap();
    let obj = v.as_object().unwrap();
    assert_eq!(obj.get("t").unwrap(), &json!("shed.load"));
    assert!(
        !obj.contains_key("payload"),
        "Rust field name must not leak"
    );
    let data = obj.get("data").unwrap().as_object().unwrap();
    assert_eq!(data.get("reason").unwrap(), &json!("no_better_home"));
    let scope = data.get("scope").unwrap().as_object().unwrap();
    assert_eq!(scope.get("kind").unwrap(), &json!("program"));
    assert_eq!(data.get("level").unwrap(), &json!(2));
    assert_eq!(data.get("dropped").unwrap(), &json!(17));

    let back: Event = serde_json::from_value(v).unwrap();
    assert_eq!(back, event, "shed.load must survive a JSON round-trip");
}

#[test]
fn shed_load_input_scope_carries_the_input_id() {
    use multiview_events::{ShedLoad, ShedReason, ShedScope};
    let event = Event::ShedLoad(ShedLoad {
        reason: ShedReason::Pinned,
        scope: ShedScope::Input {
            id: "cam-3".to_owned(),
        },
        level: 1,
        dropped: 0,
    });
    let v = serde_json::to_value(&event).unwrap();
    let scope = v.pointer("/data/scope").unwrap().as_object().unwrap();
    assert_eq!(scope.get("kind").unwrap(), &json!("input"));
    assert_eq!(scope.get("id").unwrap(), &json!("cam-3"));
    let back: Event = serde_json::from_value(v).unwrap();
    assert_eq!(back, event, "input-scoped shed must round-trip");
}

#[test]
fn shed_load_reason_labels_are_stable() {
    use multiview_events::ShedReason;
    assert_eq!(ShedReason::Pinned.label(), "pinned");
    assert_eq!(ShedReason::DisplayBound.label(), "display_bound");
    assert_eq!(ShedReason::NoBetterHome.label(), "no_better_home");
    assert_eq!(ShedReason::AntiStorm.label(), "anti_storm");
    assert_eq!(ShedReason::EncoderOverload.label(), "encoder_overload");
}
