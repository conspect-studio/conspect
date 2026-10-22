//! WHIP ingest end-to-end (ADR-T014), driven **offline** with synthetic RTP —
//! no socket, no live publisher.
//!
//! Proves the ingest seam the cli `drive_webrtc` loop is built on: decrypted RTP
//! (as the `multiview-webrtc` per-session [`RtpRing`]) → the consumer's
//! [`RtpRingEngine`] `MediaEngine` → the pure keyframe-gated `WebRtcProducer` →
//! typed [`MediaEvent`]s (H.264 access units / Opus frames) the loop decodes.
//! Plus the isolation invariants the WHIP publisher must obey:
//!
//! * **Bounded (inv #2/#5):** the per-session ring is drop-oldest under a burst.
//! * **Sampled, never pacing (inv #1/#10):** an empty ring yields `Ok(None)`
//!   (the tile holds last-good — never blocks).
//! * **Dropped publisher rides the state machine (inv #2):** a closed+drained
//!   ring ends the producer stream, so the source's `drive_webrtc` loop returns
//!   and the tile rides STALE → `NO_SIGNAL` — it never stalls other tiles.
//!
//! The live browser/OBS publish + DTLS/SRTP leg is hardware-gated (the
//! `multiview-webrtc` `native_handshake` shuttle test proves the transport; this
//! proves the cli's consumer seam). The H.264/Opus *decode-to-pixels* leg is
//! validated on the box (a real bitstream needs an encoder) — the producer here
//! yields the verbatim compressed access unit the decoder consumes.
#![cfg(feature = "webrtc-native")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::cast_possible_truncation
)]

use multiview_input::webrtc::route::MediaEvent;
use multiview_input::webrtc::transport::{MediaEngine, WebRtcProducer};
use multiview_webrtc::transport::{ReceivedRtp, RtpRing, RtpRingEngine, MAX_INGRESS_RTP};

use multiview_cli::webrtc_ingest::WhipPublisher;

/// The negotiated dynamic payload types (as str0m would answer).
const VIDEO_PT: u8 = 96;
const AUDIO_PT: u8 = 111;

/// One H.264 IDR access unit as a single-NAL RTP packet (NAL header `0x65` =
/// type 5, IDR slice) — a complete, gate-opening unit in one packet.
fn idr_packet(seq: u16, ts: u32) -> ReceivedRtp {
    let mut payload = vec![0x65u8];
    payload.extend(std::iter::repeat_n(0xAB, 64));
    ReceivedRtp {
        payload_type: VIDEO_PT,
        sequence: seq,
        timestamp: ts,
        marker: true,
        ssrc: 0x1111,
        payload,
    }
}

/// One Opus RTP packet (the payload is one Opus frame; a TOC byte + body).
fn opus_packet(seq: u16, ts: u32) -> ReceivedRtp {
    ReceivedRtp {
        payload_type: AUDIO_PT,
        sequence: seq,
        timestamp: ts,
        marker: false,
        ssrc: 0x2222,
        payload: vec![0xFC, 0x01, 0x02, 0x03],
    }
}

fn publisher(ring: RtpRing) -> WhipPublisher {
    WhipPublisher {
        ring,
        video_payload_type: Some(VIDEO_PT),
        audio_payload_type: Some(AUDIO_PT),
    }
}

#[test]
fn synthetic_rtp_yields_video_access_units_and_opus_frames() {
    let ring = RtpRing::new();
    // A publisher pushes an IDR (opens the keyframe gate) then an Opus frame.
    ring.push(idr_packet(1, 90_000));
    ring.push(opus_packet(1, 48_000));

    let pubr = publisher(ring.clone());
    let session = pubr.negotiated_session();
    let engine: Box<dyn MediaEngine + Send> = Box::new(RtpRingEngine::new(ring));
    let mut producer = WebRtcProducer::new(engine, &session);

    // The first emitted event is the keyframe-gated H.264 access unit, carrying
    // the verbatim 32-bit RTP timestamp and the IDR/keyframe flag.
    let first = producer.next_event().unwrap().expect("a video AU");
    match first {
        MediaEvent::VideoAccessUnit(unit) => {
            assert!(unit.keyframe, "the IDR opens the gate as a keyframe");
            assert_eq!(unit.raw_pts, Some(90_000), "verbatim RTP timestamp");
            assert!(
                !unit.data.is_empty(),
                "the compressed access unit carries bytes"
            );
        }
        other => panic!("expected a video access unit, got {other:?}"),
    }
    // The next event is the Opus audio frame on its own 48 kHz clock.
    let second = producer.next_event().unwrap().expect("an audio frame");
    match second {
        MediaEvent::AudioFrame(unit) => {
            assert_eq!(unit.raw_pts, Some(48_000), "verbatim Opus RTP timestamp");
            assert!(!unit.data.is_empty(), "the Opus packet carries bytes");
        }
        other => panic!("expected an audio frame, got {other:?}"),
    }
    // Drained: the producer yields `Ok(None)` — sampled, never blocks (inv #1).
    assert!(producer.next_event().unwrap().is_none());
}

#[test]
fn delta_before_keyframe_is_gated_off() {
    // A delta (non-IDR) access unit before any keyframe is dropped by the gate
    // (decoding it without a reference is corruption — inv #2). A single-NAL
    // non-IDR packet (type 1 = `0x41`).
    let ring = RtpRing::new();
    ring.push(ReceivedRtp {
        payload_type: VIDEO_PT,
        sequence: 1,
        timestamp: 90_000,
        marker: true,
        ssrc: 0x1111,
        payload: {
            let mut p = vec![0x41u8];
            p.extend(std::iter::repeat_n(0xAB, 32));
            p
        },
    });
    let pubr = publisher(ring.clone());
    let session = pubr.negotiated_session();
    let mut producer = WebRtcProducer::new(Box::new(RtpRingEngine::new(ring)), &session);
    // The gated delta yields nothing; the engine is drained -> Ok(None).
    assert!(
        producer.next_event().unwrap().is_none(),
        "a delta frame before the first IDR is gated off (no corruption shown)"
    );
}

#[test]
fn the_ingress_ring_is_bounded_drop_oldest() {
    // Inv #2/#5: a publisher that outruns the consumer drops the OLDEST packet,
    // never grows the ring past its cap.
    let ring = RtpRing::new();
    for i in 0..(MAX_INGRESS_RTP as u32 + 500) {
        ring.push(idr_packet(i as u16, i));
    }
    assert!(
        ring.len() <= MAX_INGRESS_RTP,
        "the ingress ring grew past its cap ({} > {MAX_INGRESS_RTP})",
        ring.len()
    );
    assert_eq!(ring.dropped(), 500, "exactly the overflow was dropped");
}

#[test]
fn dropped_publisher_ends_the_stream_for_the_state_machine() {
    // A publisher disconnect closes the ring; the producer drains the remaining
    // buffered units, then the closed+drained ring is `is_ended()` — the cli
    // `drive_webrtc_producer` loop checks this and returns, so the source rides
    // STALE -> NO_SIGNAL via the store policy WITHOUT stalling other tiles.
    let ring = RtpRing::new();
    ring.push(idr_packet(1, 90_000));
    let producer_ring = ring.clone();
    let mut producer = WebRtcProducer::new(
        Box::new(RtpRingEngine::new(ring.clone())),
        &publisher(ring.clone()).negotiated_session(),
    );

    // Publisher drops: close the ring.
    ring.close();
    assert!(
        !producer_ring.is_ended(),
        "not ended while a unit remains buffered"
    );

    // The buffered IDR still drains (last-good is delivered, never lost).
    let drained = producer.next_event().unwrap();
    assert!(matches!(drained, Some(MediaEvent::VideoAccessUnit(_))));

    // Now the ring is closed AND drained: the drive loop's end condition.
    assert!(
        producer_ring.is_ended(),
        "a closed + drained ring signals end-of-stream (publisher gone)"
    );
    // And the producer yields nothing further (it never blocks — inv #1/#10).
    assert!(producer.next_event().unwrap().is_none());
}

#[test]
fn audio_disabled_publisher_binds_no_audio_route() {
    // `audio = false` (ADR-T014 §5): the publisher carries no audio PT, so the
    // producer binds no audio route — an Opus packet on a stray PT is sampled
    // away (counted), never an error, never decoded.
    let ring = RtpRing::new();
    ring.push(idr_packet(1, 90_000));
    ring.push(opus_packet(2, 48_000));
    let pubr = WhipPublisher {
        ring: ring.clone(),
        video_payload_type: Some(VIDEO_PT),
        audio_payload_type: None,
    };
    let session = pubr.negotiated_session();
    let mut producer = WebRtcProducer::new(Box::new(RtpRingEngine::new(ring)), &session);
    // Only the video AU emerges; the unbound Opus PT is dropped.
    let first = producer.next_event().unwrap().expect("video AU");
    assert!(matches!(first, MediaEvent::VideoAccessUnit(_)));
    assert!(
        producer.next_event().unwrap().is_none(),
        "the unbound audio PT is sampled away (no audio route when audio=false)"
    );
}
