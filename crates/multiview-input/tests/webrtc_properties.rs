//! Property tests for the gated WebRTC media seam (feature `webrtc`).
//!
//! These pin the load-bearing robustness invariants of the pure depacketizers
//! and the payload-type router:
//!   * arbitrary payload/sequence/timestamp streams NEVER panic (empty payloads,
//!     garbage bytes, wild sequence jumps included);
//!   * the Opus depacketizer's gap flag is exactly "a packet was lost or dropped
//!     since the last emitted frame";
//!   * every emitted Opus frame surfaces the payload bytes and the 48 kHz RTP
//!     timestamp verbatim (RFC 7587: one packet == one frame).
#![cfg(feature = "webrtc")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_input::webrtc::opus::{OpusDepacketizer, MAX_OPUS_PACKET_BYTES};
use multiview_input::webrtc::route::RtpRouter;
use multiview_input::webrtc::transport::{H264Depacketizer, RtpFrame};
use multiview_input::webrtc::{Codec, MediaKind, NegotiatedMedia, NegotiatedSession, SdpDirection};
use proptest::prelude::*;

/// The standard test session (H.264 video PT 98, Opus audio PT 111), as the
/// router/producer seam receives it (both sections `recvonly`).
fn negotiated_av() -> NegotiatedSession {
    NegotiatedSession {
        sections: vec![
            NegotiatedMedia {
                kind: MediaKind::Video,
                payload_type: 98,
                codec: Codec::H264,
                direction: SdpDirection::RecvOnly,
            },
            NegotiatedMedia {
                kind: MediaKind::Audio,
                payload_type: 111,
                codec: Codec::OPUS,
                direction: SdpDirection::RecvOnly,
            },
        ],
    }
}

/// An arbitrary decrypted RTP packet: any PT, sequence, timestamp, marker, and
/// up to 300 bytes of arbitrary payload (including empty).
fn arb_rtp_frame() -> impl Strategy<Value = RtpFrame> {
    (
        any::<u8>(),
        any::<u16>(),
        any::<u32>(),
        any::<bool>(),
        prop::collection::vec(any::<u8>(), 0..300),
    )
        .prop_map(
            |(payload_type, sequence, timestamp, marker, payload)| RtpFrame {
                payload_type: payload_type & 0x7F,
                sequence,
                timestamp,
                marker,
                payload,
            },
        )
}

proptest! {
    /// The Opus depacketizer NEVER panics on arbitrary payload/sequence streams,
    /// and every emitted frame surfaces the payload + 48 kHz timestamp verbatim.
    #[test]
    fn prop_opus_depacketizer_never_panics(
        packets in prop::collection::vec(arb_rtp_frame(), 0..200),
    ) {
        let mut depack = OpusDepacketizer::new();
        for packet in &packets {
            if let Some(frame) = depack.push(packet) {
                // RFC 7587: one packet == one frame, bytes verbatim.
                prop_assert_eq!(&frame.data, &packet.payload);
                prop_assert_eq!(frame.raw_pts, Some(i64::from(packet.timestamp)));
                // Only valid (non-empty, in-cap) payloads emit.
                prop_assert!(!packet.payload.is_empty());
                prop_assert!(packet.payload.len() <= MAX_OPUS_PACKET_BYTES);
            }
        }
    }

    /// The H.264 depacketizer NEVER panics on arbitrary garbage (it must drop,
    /// gate, or reassemble — never index out of bounds or grow unboundedly).
    #[test]
    fn prop_h264_depacketizer_never_panics(
        packets in prop::collection::vec(arb_rtp_frame(), 0..200),
    ) {
        let mut depack = H264Depacketizer::new();
        for packet in &packets {
            let _ = depack.push(packet);
        }
    }

    /// The router NEVER panics and never errors on arbitrary packets: every
    /// packet either routes to a depacketizer or is counted as unknown-dropped.
    #[test]
    fn prop_router_never_panics_on_garbage(
        packets in prop::collection::vec(arb_rtp_frame(), 0..200),
    ) {
        let mut router = RtpRouter::new(&negotiated_av());
        let mut last_unknown = 0_u64;
        for packet in &packets {
            let routed = router.route(packet);
            let unknown = router.unknown_dropped();
            if packet.payload_type == 98 || packet.payload_type == 111 {
                // A negotiated PT is never counted unknown.
                prop_assert_eq!(unknown, last_unknown);
            } else {
                // An unknown PT is counted exactly once and never yields.
                prop_assert!(routed.is_none());
                prop_assert_eq!(unknown, last_unknown + 1);
            }
            last_unknown = unknown;
        }
    }

    /// Gap-flag correctness on in-order streams: an emitted Opus frame flags a
    /// discontinuity exactly when the sequence stepped by more than one (a lost
    /// packet) since the previous packet, wrap included.
    #[test]
    fn prop_opus_gap_flags_are_exact(
        first_seq in any::<u16>(),
        steps in prop::collection::vec((1_u16..1000, 1_usize..16), 1..100),
    ) {
        let mut depack = OpusDepacketizer::new();
        let mut seq = first_seq;
        let mut timestamp = 0_u32;
        for (i, &(delta, payload_len)) in steps.iter().enumerate() {
            if i > 0 {
                seq = seq.wrapping_add(delta);
            }
            timestamp = timestamp.wrapping_add(960);
            let packet = RtpFrame {
                payload_type: 111,
                sequence: seq,
                timestamp,
                marker: false,
                payload: vec![0x78; payload_len],
            };
            let frame = depack.push(&packet).expect("a valid payload always emits");
            let expect_gap = i > 0 && delta > 1;
            prop_assert_eq!(
                frame.discontinuity,
                expect_gap,
                "step {} delta {}",
                i,
                delta
            );
        }
    }
}
