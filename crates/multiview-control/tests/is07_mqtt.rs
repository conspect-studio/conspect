//! IS-07 MQTT transport tests (SUR-2).
//!
//! The **pure** message/codec/topic/isolation core is always compiled and
//! always tested here (no broker, no `is07-mqtt` feature). The **live broker**
//! round-trip is gated behind `#[cfg(feature = "is07-mqtt")]` and runs against
//! an in-process `rumqttd` broker (no external broker in CI).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_control::is07::mqtt::{
    self, decode, encode, topic_for_message, topic_for_source, MqttDecodeError, PublishQueue, Qos,
};
use multiview_control::is07::{Is07Message, Is07Payload, Is07Timing};

fn boolean_state(source_id: &str, value: bool) -> Is07Message {
    Is07Message::State {
        source_id: source_id.to_owned(),
        flow_id: source_id.to_owned(),
        payload: Is07Payload::Boolean { value },
        timing: Is07Timing {
            creation_timestamp: "1700000000:0".to_owned(),
            origin_timestamp: None,
        },
    }
}

#[test]
fn message_round_trips_over_the_wire_codec() {
    let msg = boolean_state("src-1", true);
    let bytes = encode(&msg).expect("encode");
    let back = decode(&bytes).expect("decode");
    assert_eq!(back, msg);
}

#[test]
fn the_wire_payload_is_tagged_json_never_untagged() {
    // The on-the-wire bytes must carry the IS-07 tag discriminators so a
    // standards receiver can parse them — proves we serialise the tagged model,
    // not an `untagged` shape.
    let msg = boolean_state("src-1", true);
    let bytes = encode(&msg).expect("encode");
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(value["message_type"], "state");
    assert_eq!(value["payload"]["type"], "boolean");
    assert_eq!(value["payload"]["value"], true);
}

#[test]
fn decode_rejects_non_json_bytes_without_panicking() {
    let err = decode(b"\xff\xff not json").expect_err("must reject");
    assert!(matches!(err, MqttDecodeError::Json(_)));
}

#[test]
fn topic_follows_the_nmos_convention() {
    assert_eq!(
        topic_for_source("cam-3"),
        "x-nmos/events/v1.0/sources/cam-3"
    );
    let msg = boolean_state("cam-3", false);
    assert_eq!(topic_for_message(&msg), "x-nmos/events/v1.0/sources/cam-3");
}

#[test]
fn wildcard_subscribe_topic_matches_the_per_source_topics() {
    let filter = mqtt::SUBSCRIBE_TOPIC_FILTER;
    assert_eq!(filter, "x-nmos/events/v1.0/sources/+");
    // The single-level `+` wildcard matches a per-source topic's final segment.
    let topic = topic_for_source("abc");
    let (filter_head, _plus) = filter.rsplit_once('/').expect("filter has a /");
    let (topic_head, _src) = topic.rsplit_once('/').expect("topic has a /");
    assert_eq!(filter_head, topic_head);
}

#[test]
fn qos_maps_to_the_mqtt_wire_codes() {
    assert_eq!(Qos::AtMostOnce.code(), 0);
    assert_eq!(Qos::AtLeastOnce.code(), 1);
    assert_eq!(Qos::ExactlyOnce.code(), 2);
    // NMOS guidance: state messages are delivered at least once.
    assert_eq!(Qos::default(), Qos::AtLeastOnce);
}

// ---- invariant #10: the publish queue never back-pressures the producer ----

#[test]
fn publish_queue_is_bounded_and_drops_oldest_never_blocks_the_producer() {
    // A capacity-2 queue. The producer (engine side) pushes 5 messages without
    // ever blocking; the oldest are dropped so the newest two survive — the
    // engine is never paced by a slow/stalled broker drainer.
    let (tx, mut rx) = PublishQueue::bounded(2);
    let mut dropped = 0_u64;
    for i in 0..5 {
        // try_publish must be non-blocking and report a drop, never await.
        if !tx.try_publish(boolean_state(&format!("s{i}"), true)) {
            dropped += 1;
        }
    }
    assert_eq!(dropped, 3, "3 of 5 dropped at capacity 2");
    // The two newest survive in order (drop-oldest, not drop-newest).
    let a = rx.try_recv().expect("first survivor");
    let b = rx.try_recv().expect("second survivor");
    assert_eq!(a.source_id(), "s3");
    assert_eq!(b.source_id(), "s4");
    assert!(rx.try_recv().is_none(), "only two survive");
}

#[test]
fn publish_queue_reports_dropped_count_for_telemetry() {
    let (tx, _rx) = PublishQueue::bounded(1);
    tx.try_publish(boolean_state("a", true));
    tx.try_publish(boolean_state("b", true));
    tx.try_publish(boolean_state("c", true));
    // Two were dropped to make room for the newest.
    assert_eq!(tx.dropped(), 2);
}

#[cfg(feature = "is07-mqtt")]
mod live_broker {
    //! Live publish→broker→subscribe round-trip against an in-process rumqttd
    //! broker. Gated behind `is07-mqtt`; no external broker is contacted.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::collections::HashMap;
    use std::net::{Ipv6Addr, SocketAddr, TcpListener};
    use std::time::Duration;

    use multiview_control::is07::mqtt::{BrokerConfig, MqttSubscriber, Qos};
    use multiview_control::is07::{GpiEvent, Is07Message, Is07Payload, Is07Timing};

    fn free_port() -> u16 {
        let listener = TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).expect("bind ephemeral port");
        listener.local_addr().expect("local addr").port()
    }

    fn rumqttd_config(id: usize, port: u16) -> rumqttd::Config {
        let mut v4 = HashMap::new();
        v4.insert(
            "v4-1".to_owned(),
            rumqttd::ServerSettings {
                name: "v4-1".to_owned(),
                listen: SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
                tls: None,
                next_connection_delay_ms: 0,
                connections: rumqttd::ConnectionSettings {
                    connection_timeout_ms: 5_000,
                    max_payload_size: 268_435_455,
                    max_inflight_count: 200,
                    auth: None,
                    external_auth: None,
                    dynamic_filters: true,
                },
            },
        );
        rumqttd::Config {
            id,
            router: rumqttd::RouterConfig {
                max_connections: 100,
                max_outgoing_packet_count: 200,
                max_segment_size: 104_857_600,
                max_segment_count: 10,
                ..Default::default()
            },
            v4: Some(v4),
            ..Default::default()
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn boolean_state_publishes_and_decodes_to_the_right_gpi_event() {
        let port = free_port();
        let mut broker = rumqttd::Broker::new(rumqttd_config(0, port));
        // The broker's `start()` blocks; run it on a dedicated OS thread.
        std::thread::spawn(move || {
            let _ = broker.start();
        });
        // Give the listener a moment to bind.
        tokio::time::sleep(Duration::from_millis(300)).await;

        let cfg = BrokerConfig::new(Ipv6Addr::LOCALHOST.to_string(), port, "mv-sub")
            .with_qos(Qos::AtLeastOnce)
            .with_capacity(16);
        // Subscriber first so the retained-free message is seen.
        let mut subscriber = MqttSubscriber::connect(&cfg).await.expect("subscriber");
        tokio::time::sleep(Duration::from_millis(200)).await;

        let pub_cfg = BrokerConfig::new(Ipv6Addr::LOCALHOST.to_string(), port, "mv-pub")
            .with_qos(Qos::AtLeastOnce)
            .with_capacity(16);
        let publisher = multiview_control::is07::mqtt::MqttPublisher::connect(&pub_cfg)
            .await
            .expect("publisher");

        let msg = Is07Message::State {
            source_id: "preview-bus".to_owned(),
            flow_id: "preview-bus".to_owned(),
            payload: Is07Payload::Boolean { value: true },
            timing: Is07Timing {
                creation_timestamp: "1700000000:0".to_owned(),
                origin_timestamp: None,
            },
        };
        assert!(publisher.try_publish(msg.clone()), "publish accepted");

        // The subscriber decodes the inbound frame back into an Is07Message and
        // we recover the GpiEvent — assert it lands within a bounded wait.
        let received = tokio::time::timeout(Duration::from_secs(5), subscriber.recv())
            .await
            .expect("did not time out")
            .expect("a message");
        let gpi = GpiEvent::from_is07(&received).expect("boolean → gpi");
        assert_eq!(gpi.line, "preview-bus");
        assert!(gpi.asserted);
    }
}
