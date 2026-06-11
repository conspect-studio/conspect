#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! Serde snapshot + round-trip + topic-routing contract tests for the Devices
//! realtime surface (DEV-A2, ADR-RT007): the coarse `devices` topic, the
//! conflated latest-wins `device.status` snapshot (managed-devices.md §2.1),
//! the lossless low-rate lifecycle events (`device.adopted` / `.removed` /
//! `.mode` / `.error` / `.sync` / `.discovered`), and the `timing.status`
//! sync-telemetry payload (ADR-M010). These prove every new `Event` variant is
//! internally-tagged (`t`/`data`, never untagged), serializes to the documented
//! wire shape under the v1 envelope, survives a JSON round-trip, honours the
//! per-event-type conflation policy (ring exclusion extends from per-topic
//! `Topic::is_high_rate` to per-event `Event::is_conflated` on this one
//! mixed-cadence topic), and that the snapshot ⊕ delta / resume-by-seq
//! ordering and the `ids` filter mechanism cover the new topic.

use multiview_core::time::{MediaTime, Rational};
use multiview_core::wallclock::WallClockRef;
use multiview_events::ordering::Accepted;
use multiview_events::{
    AchievedSync, AddressFamily, AudioMeter, ClockQuality, ClockSource, DeviceAdopted,
    DeviceCapabilities, DeviceDiscovered, DeviceError, DeviceMode, DeviceRemoved, DeviceState,
    DeviceStatus, DeviceStreamRole, DeviceStreamStatus, DeviceSync, DeviceSyncSummary, Envelope,
    Event, EventEnvelope, FrameKind, ImpactClass, ModePhase, SchemaVersion, Seq, Subscribe,
    SyncCapability, SyncChange, SyncGroupSkew, SystemMetrics, TileState, TimingStatus, Topic,
    TopicCursor,
};
use serde_json::{json, Value};

fn ts() -> MediaTime {
    MediaTime::from_nanos(920_451_123_456)
}

/// The full `device.status` row from managed-devices.md §2.1 (every field
/// populated), keyed by the registry device id.
fn sample_status() -> DeviceStatus {
    DeviceStatus {
        device_id: "dev-foyer-decoder".to_owned(),
        state: DeviceState::Online,
        mode: Some("decoder".to_owned()),
        capabilities: Some(DeviceCapabilities {
            encode: false,
            decode: true,
            display: true,
            sync: SyncCapability::OffsetOnly,
            audio: true,
            reboot: true,
            firmware_update: false,
        }),
        streams: vec![DeviceStreamStatus {
            role: DeviceStreamRole::Decode,
            output_ref: Some("out-program-srt".to_owned()),
            bitrate_bps: Some(5_980_000),
            fps: Some(25.0),
            healthy: true,
        }],
        sync: Some(DeviceSyncSummary {
            group: "lobby-wall".to_owned(),
            offset_ms: 120,
            achieved: AchievedSync::BoundedSkew,
        }),
        temperature_c: Some(47.5),
        last_seen_ts: Some(ts()),
    }
}

/// The full `timing.status` payload from ADR-M010's WS publication:
/// `{stream_id, WallClockRef, link_offset, clock_source, clock_quality}` plus
/// per-sync-group achieved-skew measurements.
fn sample_timing() -> TimingStatus {
    TimingStatus {
        stream_id: "prog-main".to_owned(),
        epoch: WallClockRef::new(1_765_432_100_000_000_000, 900_000, Rational::new(90_000, 1)),
        link_offset_ns: 150_000_000,
        clock_source: ClockSource::Ptp,
        clock_quality: ClockQuality::Locked,
        groups: vec![SyncGroupSkew {
            group: "lobby-wall".to_owned(),
            achieved: AchievedSync::BoundedSkew,
            measured_skew_ms: Some(180.0),
        }],
    }
}

#[test]
fn devices_topic_wire_string_and_ring_policy() {
    // One coarse `devices` topic (ADR-RT007): wire string `devices`, fine
    // scoping via the existing `ids` filter, never more topics.
    assert_eq!(Topic::Devices.as_str(), "devices");
    assert!(!Topic::Devices.is_control());
    // `devices` is the first MIXED-cadence topic: the lossless lifecycle lane
    // must stay in the replay ring, so the topic is NOT high-rate at topic
    // granularity — ring exclusion is per-EVENT-type (`Event::is_conflated`).
    assert!(!Topic::Devices.is_high_rate());

    let v = serde_json::to_value(Topic::Devices).unwrap();
    assert_eq!(v, json!("devices"));
    let back: Topic = serde_json::from_value(v).unwrap();
    assert_eq!(back, Topic::Devices);
}

#[test]
fn device_status_envelope_matches_documented_shape() {
    // The conflated latest-wins per-device snapshot must serialize to exactly
    // the status JSON shape in managed-devices.md §2.1, under the v1 envelope,
    // with the envelope `id` = device id (ADR-RT007).
    let env: EventEnvelope = Envelope::new(
        Topic::Devices,
        Seq::new(9001),
        ts(),
        Event::DeviceStatus(sample_status()),
    )
    .with_id("dev-foyer-decoder");

    let v: Value = serde_json::to_value(&env).unwrap();
    assert_eq!(
        v,
        json!({
            "v": 1,
            "topic": "devices",
            "id": "dev-foyer-decoder",
            "seq": 9001,
            "ts": 920_451_123_456_i64,
            "t": "device.status",
            "data": {
                "device_id": "dev-foyer-decoder",
                "state": "ONLINE",
                "mode": "decoder",
                "capabilities": {
                    "encode": false,
                    "decode": true,
                    "display": true,
                    "sync": "offset-only",
                    "audio": true,
                    "reboot": true,
                    "firmware_update": false
                },
                "streams": [{
                    "role": "decode",
                    "output_ref": "out-program-srt",
                    "bitrate_bps": 5_980_000_u64,
                    "fps": 25.0,
                    "healthy": true
                }],
                "sync": {
                    "group": "lobby-wall",
                    "offset_ms": 120,
                    "achieved": "bounded-skew"
                },
                "temperature_c": 47.5,
                "last_seen_ts": 920_451_123_456_i64
            }
        }),
        "device.status must match the managed-devices.md §2.1 wire shape"
    );

    let back: EventEnvelope = serde_json::from_value(v).unwrap();
    assert_eq!(back, env, "device.status must survive a JSON round-trip");
    assert!(back.ensure_supported(&[SchemaVersion::V1]).is_ok());
}

#[test]
fn device_status_minimal_omits_optional_fields() {
    // A pre-probe row (ADOPTING: nothing probed, nothing seen) carries only the
    // required fields; every absent optional is OMITTED, never null.
    let event = Event::DeviceStatus(DeviceStatus::new("dev-new", DeviceState::Adopting));
    let v = serde_json::to_value(&event).unwrap();
    assert_eq!(
        v,
        json!({
            "t": "device.status",
            "data": { "device_id": "dev-new", "state": "ADOPTING" }
        })
    );
    let back: Event = serde_json::from_value(v).unwrap();
    assert_eq!(back, event);
}

#[test]
fn device_state_wire_strings_are_screaming_snake() {
    // The lifecycle vocabulary from managed-devices.md §2.1/§2.2, verbatim.
    let cases = [
        (DeviceState::Discovered, "DISCOVERED"),
        (DeviceState::Adopting, "ADOPTING"),
        (DeviceState::Online, "ONLINE"),
        (DeviceState::Degraded, "DEGRADED"),
        (DeviceState::AuthFailed, "AUTH_FAILED"),
        (DeviceState::Unreachable, "UNREACHABLE"),
    ];
    for (state, wire) in cases {
        let v = serde_json::to_value(state).unwrap();
        assert_eq!(v, json!(wire));
        let back: DeviceState = serde_json::from_value(v).unwrap();
        assert_eq!(back, state);
    }
}

#[test]
fn device_enum_wire_strings_roundtrip() {
    // Every small wire enum on the Devices surface, exhaustively.
    fn check<T>(value: &T, wire: &str)
    where
        T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let v = serde_json::to_value(value).unwrap();
        assert_eq!(v, json!(wire), "wire string mismatch for {wire}");
        let back: T = serde_json::from_value(v).unwrap();
        assert_eq!(&back, value, "{wire} must round-trip");
    }
    check(&SyncCapability::FrameAccurate, "frame-accurate");
    check(&SyncCapability::OffsetOnly, "offset-only");
    check(&SyncCapability::None, "none");
    check(&AchievedSync::FrameAccurate, "frame-accurate");
    check(&AchievedSync::BoundedSkew, "bounded-skew");
    check(&AchievedSync::None, "none");
    check(&DeviceStreamRole::Encode, "encode");
    check(&DeviceStreamRole::Decode, "decode");
    check(&ModePhase::Started, "started");
    check(&ModePhase::Finished, "finished");
    check(&ModePhase::Failed, "failed");
    check(&ImpactClass::ControlPlane, "cp");
    check(&ImpactClass::Class1, "c1");
    check(&ImpactClass::Class2, "c2");
    check(&ImpactClass::Device, "dev");
    check(&AddressFamily::Ipv6, "ipv6");
    check(&AddressFamily::Ipv4Legacy, "ipv4-legacy");
    check(&ClockSource::Ptp, "ptp");
    check(&ClockSource::System, "system");
    check(&ClockQuality::Locked, "locked");
    check(&ClockQuality::Holdover, "holdover");
    check(&ClockQuality::Acquiring, "acquiring");
    check(&ClockQuality::Freerun, "freerun");
}

#[test]
fn device_lifecycle_variants_roundtrip_under_v1_envelope() {
    // Every lossless low-rate lifecycle event: correct `t` tag, data (not
    // control), NOT conflated (must stay in the lossless replay ring), and a
    // full envelope round-trip at schema major v1.
    let cases: Vec<(Event, &str)> = vec![
        (
            Event::DeviceAdopted(DeviceAdopted {
                device_id: "dev-foyer-decoder".to_owned(),
                driver: "zowietek".to_owned(),
                name: Some("Foyer decoder".to_owned()),
            }),
            "device.adopted",
        ),
        (
            Event::DeviceRemoved(DeviceRemoved::new("dev-foyer-decoder")),
            "device.removed",
        ),
        (
            Event::DeviceMode(DeviceMode {
                device_id: "dev-foyer-decoder".to_owned(),
                mode: "decoder".to_owned(),
                phase: ModePhase::Started,
                impact: ImpactClass::Device,
                detail: Some(
                    "device restarts its pipeline; bound sources ride the tile ladder to \
                     NO_SIGNAL; no Multiview outputs are affected"
                        .to_owned(),
                ),
            }),
            "device.mode",
        ),
        (
            Event::DeviceError(DeviceError {
                device_id: "dev-foyer-decoder".to_owned(),
                code: Some("00004".to_owned()),
                message: "workmode rejected: close decode before operating".to_owned(),
            }),
            "device.error",
        ),
        (
            Event::DeviceSync(DeviceSync {
                device_id: "dev-node-left".to_owned(),
                group: "lobby-wall".to_owned(),
                change: SyncChange::Joined { offset_ms: 0 },
            }),
            "device.sync",
        ),
        (
            Event::DeviceDiscovered(DeviceDiscovered {
                driver: "zowietek".to_owned(),
                address: "http://[fd00:db8::42]".to_owned(),
                family: AddressFamily::Ipv6,
                name: Some("ZowieBox 4K".to_owned()),
            }),
            "device.discovered",
        ),
    ];
    for (event, tag) in cases {
        assert_eq!(event.type_tag(), tag, "type_tag mismatch for {tag}");
        assert!(!event.is_control(), "{tag} is a data event, not control");
        assert!(
            !event.is_conflated(),
            "{tag} is a lossless lifecycle event — it must stay in the replay ring"
        );
        let env: EventEnvelope = Envelope::new(Topic::Devices, Seq::new(42), ts(), event.clone());
        let v = serde_json::to_value(&env).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj.get("v").unwrap(), &json!(1), "{tag} envelope major");
        assert_eq!(obj.get("topic").unwrap(), &json!("devices"));
        assert_eq!(obj.get("t").unwrap(), &json!(tag));
        assert!(
            !obj.contains_key("payload"),
            "Rust field name must not leak"
        );
        let back: EventEnvelope = serde_json::from_value(v).unwrap();
        assert_eq!(back, env, "{tag} must survive a JSON round-trip");
    }
}

#[test]
fn device_mode_declares_its_impact() {
    // `device.mode` carries the mode-convergence phase AND the declared impact
    // (managed-devices.md §10: device mode convergence is DEV-class — the
    // device pipeline restarts; program output is unaffected).
    let event = Event::DeviceMode(DeviceMode {
        device_id: "dev-foyer-decoder".to_owned(),
        mode: "decoder".to_owned(),
        phase: ModePhase::Started,
        impact: ImpactClass::Device,
        detail: Some("device restarts its pipeline".to_owned()),
    });
    let v = serde_json::to_value(&event).unwrap();
    assert_eq!(
        v,
        json!({
            "t": "device.mode",
            "data": {
                "device_id": "dev-foyer-decoder",
                "mode": "decoder",
                "phase": "started",
                "impact": "dev",
                "detail": "device restarts its pipeline"
            }
        })
    );
}

#[test]
fn device_sync_changes_are_tagged_never_untagged() {
    // SyncChange is a tagged union (`kind`), per repo serde conventions.
    let drift = Event::DeviceSync(DeviceSync {
        device_id: "dev-foyer-decoder".to_owned(),
        group: "lobby-wall".to_owned(),
        change: SyncChange::Drift {
            measured_skew_ms: 180.5,
            target_skew_ms: 50,
            exceeded: true,
        },
    });
    let v = serde_json::to_value(&drift).unwrap();
    assert_eq!(
        v,
        json!({
            "t": "device.sync",
            "data": {
                "device_id": "dev-foyer-decoder",
                "group": "lobby-wall",
                "change": {
                    "kind": "drift",
                    "measured_skew_ms": 180.5,
                    "target_skew_ms": 50,
                    "exceeded": true
                }
            }
        })
    );

    // Membership / tier / recovery variants all carry the `kind` tag and
    // round-trip.
    let changes = [
        (SyncChange::Joined { offset_ms: 120 }, "joined"),
        (SyncChange::Left, "left"),
        (
            SyncChange::Tier {
                achieved: AchievedSync::FrameAccurate,
            },
            "tier",
        ),
        (
            SyncChange::Drift {
                measured_skew_ms: 12.0,
                target_skew_ms: 50,
                exceeded: false,
            },
            "drift",
        ),
    ];
    for (change, kind) in changes {
        let v = serde_json::to_value(&change).unwrap();
        assert_eq!(
            v.get("kind").unwrap(),
            &json!(kind),
            "SyncChange must be tagged with kind={kind}"
        );
        let back: SyncChange = serde_json::from_value(v).unwrap();
        assert_eq!(back, change, "SyncChange {kind} must round-trip");
    }
}

#[test]
fn device_discovered_correlates_with_the_scan_operation() {
    // Discovery rows stream while a `POST /discovery/devices/scan` 202
    // operation runs, correlated via the envelope `corr` (ADR-RT007); IPv4
    // results are explicitly labelled legacy (IPv6-first, ADR-0042).
    let env: EventEnvelope = Envelope::new(
        Topic::Devices,
        Seq::new(77),
        ts(),
        Event::DeviceDiscovered(DeviceDiscovered {
            driver: "zowietek".to_owned(),
            address: "http://192.0.2.7".to_owned(),
            family: AddressFamily::Ipv4Legacy,
            name: None,
        }),
    )
    .with_corr("op:scan-1");

    let v = serde_json::to_value(&env).unwrap();
    assert_eq!(v.get("corr").unwrap(), &json!("op:scan-1"));
    let data = v.get("data").unwrap().as_object().unwrap();
    assert_eq!(data.get("family").unwrap(), &json!("ipv4-legacy"));
    assert!(!data.contains_key("name"), "absent name must be omitted");
    let back: EventEnvelope = serde_json::from_value(v).unwrap();
    assert_eq!(back, env);
}

#[test]
fn timing_status_envelope_matches_adr_m010_shape() {
    // `timing.status` carries the outbound presentation epoch — the exact
    // affine WallClockRef map — plus link offset, clock source/quality, and
    // per-sync-group achieved-skew measurements; envelope `id` = program or
    // sync-group id (ADR-RT007 / ADR-M010).
    let env: EventEnvelope = Envelope::new(
        Topic::Devices,
        Seq::new(9002),
        ts(),
        Event::TimingStatus(sample_timing()),
    )
    .with_id("prog-main");

    let v: Value = serde_json::to_value(&env).unwrap();
    assert_eq!(
        v,
        json!({
            "v": 1,
            "topic": "devices",
            "id": "prog-main",
            "seq": 9002,
            "ts": 920_451_123_456_i64,
            "t": "timing.status",
            "data": {
                "stream_id": "prog-main",
                "epoch": {
                    "wall_at_anchor_ns": 1_765_432_100_000_000_000_i64,
                    "media_at_anchor": 900_000,
                    "rate": { "num": 90_000, "den": 1 }
                },
                "link_offset_ns": 150_000_000,
                "clock_source": "ptp",
                "clock_quality": "locked",
                "groups": [{
                    "group": "lobby-wall",
                    "achieved": "bounded-skew",
                    "measured_skew_ms": 180.0
                }]
            }
        }),
        "timing.status must match the ADR-M010 WS publication shape"
    );

    let back: EventEnvelope = serde_json::from_value(v).unwrap();
    assert_eq!(back, env, "timing.status must survive a JSON round-trip");
    assert!(back.ensure_supported(&[SchemaVersion::V1]).is_ok());
}

#[test]
fn timing_status_minimal_omits_empty_groups() {
    // A program with no sync groups publishes the epoch alone; the empty
    // `groups` array is omitted, never `[]`-noise on every conflated sample.
    let status = TimingStatus {
        stream_id: "prog-main".to_owned(),
        epoch: WallClockRef::new(0, 0, Rational::new(90_000, 1)),
        link_offset_ns: 0,
        clock_source: ClockSource::System,
        clock_quality: ClockQuality::Holdover,
        groups: vec![],
    };
    let v = serde_json::to_value(Event::TimingStatus(status.clone())).unwrap();
    let data = v.get("data").unwrap().as_object().unwrap();
    assert!(!data.contains_key("groups"), "empty groups must be omitted");
    let back: Event = serde_json::from_value(v).unwrap();
    assert_eq!(back, Event::TimingStatus(status));
}

#[test]
fn conflation_policy_is_per_event_type_on_devices() {
    // ADR-RT007 extends ADR-RT003's ring-exclusion rule from per-topic
    // granularity (`Topic::is_high_rate`) to per-event-type granularity for the
    // one mixed-cadence topic: a frame is excluded from the lossless replay
    // ring when `topic.is_high_rate() || event.is_conflated()`.
    assert!(
        Event::DeviceStatus(sample_status()).is_conflated(),
        "device.status is conflated latest-wins (re-snapshot heals it)"
    );
    assert!(
        Event::TimingStatus(sample_timing()).is_conflated(),
        "timing.status is latest-wins (the affine epoch stays valid when stale)"
    );
    // The conflated telemetry lanes that already exist stay honest under the
    // same per-event predicate.
    assert!(Event::AudioMeter(AudioMeter {
        track: 0,
        peak_db: vec![-6.0],
        rms_db: vec![-12.0],
        clip: false,
        overflow: false,
        sampled_hz: 25,
    })
    .is_conflated());
    assert!(Event::SystemMetrics(SystemMetrics {
        cpu_util: 0.5,
        mem_used_bytes: None,
        mem_total_bytes: None,
        self_cpu_util: None,
        self_mem_used_bytes: None,
        gpus: vec![],
        program_fps: None,
        sampled_hz: 1,
    })
    .is_conflated());
    // Lossless events are never conflated.
    assert!(!Event::DeviceRemoved(DeviceRemoved::new("dev-x")).is_conflated());
    assert!(!Event::TileState(TileState {
        from: multiview_events::LifecycleState::Live,
        to: multiview_events::LifecycleState::Stale,
        input: None,
        trigger: "stale_timeout".to_owned(),
    })
    .is_conflated());
    assert!(!Event::Ping.is_conflated());
}

#[test]
fn devices_subscribe_ids_filter_roundtrips() {
    // Fine scoping on the coarse topic uses the EXISTING `ids` filter
    // (ADR-RT007: scoped finer with the id filter — never with more topics).
    let sub = Subscribe {
        topics: vec![Topic::Devices],
        ids: vec!["dev-foyer-decoder".to_owned()],
        rate_hz: None,
        since_seq: None,
    };
    let v = serde_json::to_value(&sub).unwrap();
    assert_eq!(
        v,
        json!({ "topics": ["devices"], "ids": ["dev-foyer-decoder"] })
    );
    let back: Subscribe = serde_json::from_value(v).unwrap();
    assert_eq!(back, sub);
}

#[test]
fn devices_topic_obeys_snapshot_then_delta_with_resume() {
    // The lossless lifecycle lane rides the same snapshot ⊕ delta /
    // resume-by-seq contract as every other topic (ADR-RT003).
    let mut cur = TopicCursor::new(Topic::Devices);
    assert_eq!(
        cur.accept(FrameKind::Snapshot, Seq::new(100)).unwrap(),
        Accepted::SnapshotBaseline { seq: Seq::new(100) }
    );
    assert_eq!(
        cur.accept(FrameKind::Delta, Seq::new(101)).unwrap(),
        Accepted::Delta { seq: Seq::new(101) }
    );
    assert_eq!(
        cur.accept(FrameKind::Delta, Seq::new(105)).unwrap(),
        Accepted::DeltaWithGap {
            seq: Seq::new(105),
            gap: 3
        }
    );
    assert!(cur.accept(FrameKind::Delta, Seq::new(105)).is_err());
    assert_eq!(
        cur.accept(FrameKind::Snapshot, Seq::new(1)).unwrap(),
        Accepted::SnapshotBaseline { seq: Seq::new(1) }
    );
}

#[test]
fn unknown_device_discriminator_is_rejected() {
    // Tagged, never untagged: a near-miss tag must hard-fail, not fall through.
    let bad = json!({ "t": "device.exploded", "data": {} });
    let parsed: Result<Event, _> = serde_json::from_value(bad);
    assert!(parsed.is_err());
}
