//! The Google Cast wire payloads (DEV-D2, ADR-M011): the typed CASTV2
//! namespace messages the session actor speaks — built and parsed as pure
//! JSON-over-frame values, no socket. The shapes are implemented from the
//! BSD-3-Clause Chromium Open Screen protocol sources and community docs;
//! Google Cast and Chromecast are trademarks of Google LLC.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    // The heartbeat pair IS ping/pong — the protocol's own names.
    clippy::similar_names,
    // Test helpers take owned `serde_json::Value`s for terse call sites.
    clippy::needless_pass_by_value
)]

use multiview_control::devices::cast::media::{CastMediaTarget, HlsSegmentFormat};
use multiview_control::devices::cast::protocol::{
    self, CastFrame, InboundMessage, PlayerState, DEFAULT_MEDIA_RECEIVER_APP_ID, NS_CONNECTION,
    NS_HEARTBEAT, NS_MEDIA, NS_RECEIVER, PLATFORM_RECEIVER_ID, SENDER_ID,
};

/// Parse a frame's payload as JSON for shape assertions.
fn payload_json(frame: &CastFrame) -> serde_json::Value {
    serde_json::from_str(&frame.payload).expect("frame payload is JSON")
}

#[test]
fn connect_frame_targets_the_platform_receiver() {
    let frame = protocol::connect_frame(PLATFORM_RECEIVER_ID);
    assert_eq!(frame.namespace, NS_CONNECTION);
    assert_eq!(frame.source, SENDER_ID);
    assert_eq!(frame.destination, PLATFORM_RECEIVER_ID);
    assert_eq!(payload_json(&frame)["type"], "CONNECT");
}

#[test]
fn ping_and_pong_ride_the_heartbeat_namespace() {
    let ping = protocol::ping_frame();
    assert_eq!(ping.namespace, NS_HEARTBEAT);
    assert_eq!(payload_json(&ping)["type"], "PING");
    let pong = protocol::pong_frame();
    assert_eq!(pong.namespace, NS_HEARTBEAT);
    assert_eq!(payload_json(&pong)["type"], "PONG");
}

#[test]
fn launch_frame_carries_the_default_media_receiver_app_id() {
    // ADR-M011: the Default Media Receiver (CC1AD845) — no registration, no
    // public URL, no cloud account.
    assert_eq!(DEFAULT_MEDIA_RECEIVER_APP_ID, "CC1AD845");
    let frame = protocol::launch_frame(7, DEFAULT_MEDIA_RECEIVER_APP_ID);
    assert_eq!(frame.namespace, NS_RECEIVER);
    assert_eq!(frame.destination, PLATFORM_RECEIVER_ID);
    let body = payload_json(&frame);
    assert_eq!(body["type"], "LAUNCH");
    assert_eq!(body["appId"], "CC1AD845");
    assert_eq!(body["requestId"], 7);
}

#[test]
fn load_frame_signals_hls_live_and_the_segment_format() {
    let media = CastMediaTarget {
        url: "http://192.0.2.7:8080/hls/program/program.m3u8".to_owned(),
        format: HlsSegmentFormat::MpegTs,
    };
    let frame = protocol::load_frame(3, "transport-1", &media);
    assert_eq!(frame.namespace, NS_MEDIA);
    assert_eq!(frame.destination, "transport-1");
    let body = payload_json(&frame);
    assert_eq!(body["type"], "LOAD");
    assert_eq!(body["requestId"], 3);
    assert_eq!(body["autoplay"], true);
    assert_eq!(
        body["media"]["contentId"],
        "http://192.0.2.7:8080/hls/program/program.m3u8"
    );
    assert_eq!(
        body["media"]["contentType"],
        "application/vnd.apple.mpegurl"
    );
    // Inputs are LIVE renditions (invariant: the output is continuous).
    assert_eq!(body["media"]["streamType"], "LIVE");
    // Receivers assume MPEG-TS unless told otherwise; the format is signalled
    // explicitly either way (D1's segmenter currently writes MPEG-TS).
    assert_eq!(body["media"]["hlsVideoSegmentFormat"], "mpeg2_ts");
}

#[test]
fn load_frame_signals_fmp4_for_a_cmaf_rendition() {
    let media = CastMediaTarget {
        url: "http://[2001:db8::7]:8080/hls/out/out.m3u8".to_owned(),
        format: HlsSegmentFormat::Fmp4,
    };
    let frame = protocol::load_frame(1, "t", &media);
    let body = payload_json(&frame);
    assert_eq!(body["media"]["hlsVideoSegmentFormat"], "fmp4");
}

#[test]
fn set_volume_frame_maps_percent_to_unit_level() {
    let frame = protocol::set_volume_frame(9, 42);
    assert_eq!(frame.namespace, NS_RECEIVER);
    let body = payload_json(&frame);
    assert_eq!(body["type"], "SET_VOLUME");
    assert_eq!(body["requestId"], 9);
    let level = body["volume"]["level"].as_f64().expect("a float level");
    assert!(
        (level - 0.42).abs() < 1e-9,
        "42% maps to level 0.42, got {level}"
    );
}

#[test]
fn stop_frame_names_the_receiver_session() {
    let frame = protocol::stop_frame(11, "session-abc");
    assert_eq!(frame.namespace, NS_RECEIVER);
    let body = payload_json(&frame);
    assert_eq!(body["type"], "STOP");
    assert_eq!(body["sessionId"], "session-abc");
    assert_eq!(body["requestId"], 11);
}

/// An inbound frame helper for decode tests.
fn inbound(namespace: &str, payload: serde_json::Value) -> CastFrame {
    CastFrame {
        namespace: namespace.to_owned(),
        source: PLATFORM_RECEIVER_ID.to_owned(),
        destination: SENDER_ID.to_owned(),
        payload: payload.to_string(),
    }
}

#[test]
fn decodes_pong_ping_and_close() {
    assert!(matches!(
        protocol::decode(&inbound(NS_HEARTBEAT, serde_json::json!({"type": "PONG"}))),
        InboundMessage::Pong
    ));
    assert!(matches!(
        protocol::decode(&inbound(NS_HEARTBEAT, serde_json::json!({"type": "PING"}))),
        InboundMessage::Ping
    ));
    assert!(matches!(
        protocol::decode(&inbound(
            NS_CONNECTION,
            serde_json::json!({"type": "CLOSE"})
        )),
        InboundMessage::CloseConnection
    ));
}

#[test]
fn decodes_receiver_status_applications() {
    let frame = inbound(
        NS_RECEIVER,
        serde_json::json!({
            "type": "RECEIVER_STATUS",
            "requestId": 0,
            "status": {
                "applications": [{
                    "appId": "CC1AD845",
                    "sessionId": "s-1",
                    "transportId": "t-1",
                    "displayName": "Default Media Receiver"
                }]
            }
        }),
    );
    let InboundMessage::ReceiverStatus(status) = protocol::decode(&frame) else {
        panic!("expected a receiver status");
    };
    assert_eq!(status.applications.len(), 1);
    assert_eq!(status.applications[0].app_id, "CC1AD845");
    assert_eq!(status.applications[0].session_id, "s-1");
    assert_eq!(status.applications[0].transport_id, "t-1");
}

#[test]
fn receiver_status_with_no_applications_decodes_empty() {
    let frame = inbound(
        NS_RECEIVER,
        serde_json::json!({ "type": "RECEIVER_STATUS", "requestId": 0, "status": {} }),
    );
    let InboundMessage::ReceiverStatus(status) = protocol::decode(&frame) else {
        panic!("expected a receiver status");
    };
    assert!(status.applications.is_empty());
}

#[test]
fn decodes_media_status_player_states() {
    let frame = inbound(
        NS_MEDIA,
        serde_json::json!({
            "type": "MEDIA_STATUS",
            "requestId": 0,
            "status": [{ "mediaSessionId": 1, "playerState": "PLAYING" }]
        }),
    );
    let InboundMessage::MediaStatus(entries) = protocol::decode(&frame) else {
        panic!("expected a media status");
    };
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].player_state, PlayerState::Playing);
    assert_eq!(entries[0].idle_reason, None);

    let frame = inbound(
        NS_MEDIA,
        serde_json::json!({
            "type": "MEDIA_STATUS",
            "requestId": 0,
            "status": [{ "mediaSessionId": 1, "playerState": "IDLE", "idleReason": "ERROR" }]
        }),
    );
    let InboundMessage::MediaStatus(entries) = protocol::decode(&frame) else {
        panic!("expected a media status");
    };
    assert_eq!(entries[0].player_state, PlayerState::Idle);
    assert_eq!(entries[0].idle_reason.as_deref(), Some("ERROR"));
}

#[test]
fn player_state_mode_tokens_are_lowercase() {
    assert_eq!(PlayerState::Playing.mode_token(), "playing");
    assert_eq!(PlayerState::Buffering.mode_token(), "buffering");
    assert_eq!(PlayerState::Paused.mode_token(), "paused");
    assert_eq!(PlayerState::Idle.mode_token(), "idle");
}

#[test]
fn load_failures_decode_as_typed_errors_not_unknown() {
    // Adversarial-review finding (DEV-D2): a receiver that answers PINGs but
    // rejects the LOAD must be visible — LOAD_FAILED / LOAD_CANCELLED /
    // INVALID_REQUEST on the media namespace are typed messages, never
    // `Unknown` (which the session actor ignores by design).
    let failed = inbound(
        NS_MEDIA,
        serde_json::json!({ "type": "LOAD_FAILED", "requestId": 2 }),
    );
    let InboundMessage::LoadError { kind, reason } = protocol::decode(&failed) else {
        panic!("LOAD_FAILED decodes as a typed load error");
    };
    assert_eq!(kind, "LOAD_FAILED");
    assert_eq!(reason, None);

    let cancelled = inbound(
        NS_MEDIA,
        serde_json::json!({ "type": "LOAD_CANCELLED", "requestId": 2 }),
    );
    let InboundMessage::LoadError { kind, .. } = protocol::decode(&cancelled) else {
        panic!("LOAD_CANCELLED decodes as a typed load error");
    };
    assert_eq!(kind, "LOAD_CANCELLED");

    let invalid = inbound(
        NS_MEDIA,
        serde_json::json!({
            "type": "INVALID_REQUEST",
            "requestId": 2,
            "reason": "INVALID_COMMAND"
        }),
    );
    let InboundMessage::LoadError { kind, reason } = protocol::decode(&invalid) else {
        panic!("a media-namespace INVALID_REQUEST decodes as a typed load error");
    };
    assert_eq!(kind, "INVALID_REQUEST");
    assert_eq!(reason.as_deref(), Some("INVALID_COMMAND"));
}

#[test]
fn receiver_status_decodes_the_idle_screen_flag() {
    // The backdrop (the receiver's own idle screen) reports
    // `isIdleScreen: true`; a sender app omits the flag (= false). The
    // session actor's "nothing else running" checks rely on the distinction.
    let frame = inbound(
        NS_RECEIVER,
        serde_json::json!({
            "type": "RECEIVER_STATUS",
            "requestId": 0,
            "status": { "applications": [
                {
                    "appId": "E8C28D3C",
                    "sessionId": "s-idle",
                    "transportId": "t-idle",
                    "displayName": "Backdrop",
                    "isIdleScreen": true
                },
                {
                    "appId": "CC1AD845",
                    "sessionId": "s-1",
                    "transportId": "t-1",
                    "displayName": "Default Media Receiver"
                }
            ] }
        }),
    );
    let InboundMessage::ReceiverStatus(status) = protocol::decode(&frame) else {
        panic!("expected a receiver status");
    };
    assert_eq!(status.applications.len(), 2);
    assert!(status.applications[0].is_idle_screen);
    assert!(!status.applications[1].is_idle_screen);
}

#[test]
fn decodes_launch_error_and_unknown_types() {
    let frame = inbound(
        NS_RECEIVER,
        serde_json::json!({ "type": "LAUNCH_ERROR", "reason": "NOT_FOUND" }),
    );
    let InboundMessage::LaunchError { reason } = protocol::decode(&frame) else {
        panic!("expected a launch error");
    };
    assert_eq!(reason.as_deref(), Some("NOT_FOUND"));

    // Unknown message types are tolerated, never an error (the receiver may
    // grow new messages at any time — proprietary-protocol drift, ADR-M011).
    let frame = inbound(
        NS_RECEIVER,
        serde_json::json!({ "type": "SOME_FUTURE_THING" }),
    );
    assert!(matches!(protocol::decode(&frame), InboundMessage::Unknown));
    // Garbage payloads are tolerated too.
    let garbage = CastFrame {
        namespace: NS_MEDIA.to_owned(),
        source: PLATFORM_RECEIVER_ID.to_owned(),
        destination: SENDER_ID.to_owned(),
        payload: "not json".to_owned(),
    };
    assert!(matches!(
        protocol::decode(&garbage),
        InboundMessage::Unknown
    ));
}
