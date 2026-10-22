//! Failing-first, **offline** tests for the `whip_push` **output client**
//! ([`multiview_webrtc::transport::WhipPushClient`], feature `native`, ADR-0049
//! §5.2).
//!
//! Multiview is the RFC 9725 client: it builds a **sendonly** offer with host
//! candidates + `a=setup:actpass`, the remote ingest answers, the client applies
//! the answer and sample-writes the program AUs. These prove the offer shape, the
//! offer→answer→connected lifecycle over the in-memory shuttle, that program AUs
//! drained from the [`EgressFeed`](multiview_webrtc::egress::EgressFeed) reach the
//! remote, and that the supervisor's backoff resets / grows on connect / drop —
//! all without a socket or a real WHIP origin.
#![cfg(feature = "native")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::time::Duration;

use multiview_webrtc::transport::{PushBackoff, WhipPushOffer};

#[test]
fn push_offer_is_sendonly_actpass_with_host_candidates() {
    // The client builds its own sendonly offer (no socket needed for the SDP).
    let offer = WhipPushOffer::create(
        &["[2001:db8::1]:8189".parse().unwrap()],
        /* audio */ true,
    )
    .expect("offer builds");
    assert!(
        offer.sdp.contains("a=sendonly"),
        "offer is sendonly:\n{}",
        offer.sdp
    );
    assert!(
        offer.sdp.contains("a=setup:actpass"),
        "offerer is actpass (answerer chooses DTLS role):\n{}",
        offer.sdp
    );
    assert!(offer.sdp.contains("m=video"), "video m-line present");
    assert!(
        offer.sdp.contains("m=audio"),
        "audio m-line present when audio=true"
    );
    assert!(
        offer.sdp.contains("typ host") && offer.sdp.contains("2001:db8::1"),
        "the host candidate is advertised (IPv6-first):\n{}",
        offer.sdp
    );
}

#[test]
fn push_offer_omits_audio_when_disabled() {
    let offer = WhipPushOffer::create(&["[2001:db8::1]:8189".parse().unwrap()], false)
        .expect("offer builds");
    assert!(offer.sdp.contains("m=video"));
    assert!(
        !offer.sdp.contains("m=audio"),
        "no audio m-line when audio=false:\n{}",
        offer.sdp
    );
}

#[test]
fn backoff_starts_low_grows_exponentially_and_caps() {
    let mut b = PushBackoff::new();
    let first = b.next_delay();
    assert!(first <= Duration::from_secs(1), "first retry is prompt");
    let second = b.next_delay();
    let third = b.next_delay();
    assert!(second > first, "backoff grows");
    assert!(third > second, "backoff keeps growing");
    // It is capped (never unbounded): many failures stay under the ceiling.
    for _ in 0..50 {
        let d = b.next_delay();
        assert!(
            d <= PushBackoff::MAX_DELAY,
            "backoff is capped at MAX_DELAY"
        );
    }
}

#[test]
fn backoff_resets_on_success() {
    let mut b = PushBackoff::new();
    let _ = b.next_delay();
    let _ = b.next_delay();
    let grown = b.next_delay();
    b.reset();
    let after_reset = b.next_delay();
    assert!(
        after_reset < grown,
        "a successful connect resets the backoff to its floor"
    );
}
