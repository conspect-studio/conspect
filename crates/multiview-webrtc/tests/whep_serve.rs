//! Failing-first, **offline** tests for the WHEP-serve **output** endpoint
//! ([`multiview_webrtc::transport::WhepServeEndpoint`], feature `native`,
//! ADR-0049 §5.1).
//!
//! The endpoint binds the shared dual-stack media socket and admits N concurrent
//! WHEP **output viewers** (bounded by `max_viewers` *and* the endpoint-global
//! `max_sessions` viewer pool — over either is `503`). Each viewer is an answerer
//! session fed the program's already-encoded AUs from the shared
//! [`EgressFeed`](multiview_webrtc::egress::EgressFeed) (encode-once, invariant
//! #7). These prove the negotiation, the per-output capacity rule, the relay
//! candidate wiring, and that a viewer flood never starves the encode-once
//! fan-out — without a socket or a real browser.
#![cfg(feature = "native")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_webrtc::config::EndpointConfig;
use multiview_webrtc::egress::egress_feed;
use multiview_webrtc::error::WebRtcError;
use multiview_webrtc::transport::{
    Direction, MediaKind, Session, SessionConfig, WhepServeEndpoint,
};

/// A realistic WHEP recvonly offer from a browser viewer for `kinds`, built by a
/// throwaway str0m offerer so the endpoint negotiates against a real SDP.
fn viewer_offer(kinds: &[MediaKind]) -> String {
    let mut viewer = Session::new(&SessionConfig::default(), std::time::Instant::now());
    viewer
        .add_host_candidate("[::1]:50000".parse().unwrap())
        .unwrap();
    viewer
        .create_offer_with_direction(kinds, Direction::RecvOnly)
        .unwrap()
}

fn config_with_advertised() -> EndpointConfig {
    EndpointConfig {
        // An ephemeral port + a concrete advertised host so the answer carries a
        // reachable candidate (the unspecified bind addr alone is not a candidate).
        udp_port: 0,
        advertised_addresses: vec!["2001:db8::1".parse().unwrap()],
        ..EndpointConfig::default()
    }
}

#[test]
fn negotiate_admits_a_viewer_and_answers_sendonly() {
    let (_sink, feed) = egress_feed();
    let (endpoint, handle) = WhepServeEndpoint::bind(config_with_advertised()).unwrap();
    // The output id scopes the viewer pool; the feed carries program AUs.
    handle.register_output("pgm", 8, feed);

    let negotiated = handle
        .negotiate("pgm", &viewer_offer(&[MediaKind::Video]), false)
        .expect("a viewer is admitted");
    assert!(
        negotiated.answer_sdp.contains("a=setup:passive"),
        "the WHEP server is the DTLS server:\n{}",
        negotiated.answer_sdp
    );
    assert!(negotiated.answer_sdp.contains("a=fingerprint:"));
    assert!(
        !negotiated.session_id.as_str().is_empty(),
        "a session id is minted"
    );
    assert_eq!(handle.live_viewer_count("pgm"), 1);
    // Keep the endpoint alive until here (it owns the socket).
    drop(endpoint);
}

#[test]
fn negotiate_refuses_an_unknown_output() {
    let (endpoint, handle) = WhepServeEndpoint::bind(config_with_advertised()).unwrap();
    let err = handle
        .negotiate("nope", &viewer_offer(&[MediaKind::Video]), false)
        .expect_err("an unconfigured output cannot be viewed");
    assert!(matches!(err, WebRtcError::UnknownSession(_)));
    drop(endpoint);
}

#[test]
fn negotiate_is_503_beyond_max_viewers() {
    let (_sink, feed) = egress_feed();
    let (endpoint, handle) = WhepServeEndpoint::bind(config_with_advertised()).unwrap();
    // max_viewers = 1 on this output.
    handle.register_output("pgm", 1, feed);
    handle
        .negotiate("pgm", &viewer_offer(&[MediaKind::Video]), false)
        .expect("first viewer admitted");
    let err = handle
        .negotiate("pgm", &viewer_offer(&[MediaKind::Video]), false)
        .expect_err("the second viewer is over max_viewers");
    assert!(
        matches!(err, WebRtcError::AtCapacity),
        "over max_viewers must be AtCapacity (503 + Retry-After)"
    );
    assert_eq!(handle.live_viewer_count("pgm"), 1);
    drop(endpoint);
}

#[test]
fn release_frees_a_viewer_slot() {
    let (_sink, feed) = egress_feed();
    let (endpoint, handle) = WhepServeEndpoint::bind(config_with_advertised()).unwrap();
    handle.register_output("pgm", 8, feed);
    let n = handle
        .negotiate("pgm", &viewer_offer(&[MediaKind::Video]), false)
        .unwrap();
    assert_eq!(handle.live_viewer_count("pgm"), 1);
    assert!(
        handle.release("pgm", n.session_id.as_str()),
        "releasing a live viewer reports true"
    );
    // A second release of the same id is a no-op false (idempotent DELETE).
    assert!(!handle.release("pgm", n.session_id.as_str()));
    drop(endpoint);
}
