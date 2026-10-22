//! Tests for the gated WebRTC **ingest media seam** (feature `webrtc`): the H.264
//! RTP depacketize -> access-unit seam
//! (keyframe-gated, FU-A reassembly, STAP-A, bounded), the pure Opus RTP
//! depacketizer (RFC 7587), the payload-type [`RtpRouter`], and the
//! [`WebRtcProducer`]: an honest **compressed media-event** producer driven by
//! an injected fake media engine, per ADR-T014 (decode happens at the
//! application layer; geometry comes from the decoder/SPS, never from declared
//! constructor arguments).
//!
//! Everything here is driven by injected packets/events — there is **no real
//! network, ICE, DTLS, or SRTP**. The real socket/ICE path is gated and
//! `#[ignore]`d (it needs a peer); see `live_webrtc_session_receives_rtp`.
#![cfg(feature = "webrtc")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_input::normalize::WrapBits;
use multiview_input::webrtc::opus::{OpusDepacketizer, AUDIO_CLOCK_RATE, MAX_OPUS_PACKET_BYTES};
use multiview_input::webrtc::route::{MediaEvent, MediaUnit, RtpRouter};
use multiview_input::webrtc::transport::{
    H264Depacketizer, MediaEngine, RtpFrame, WebRtcProducer, VIDEO_CLOCK_RATE,
};
use multiview_input::webrtc::{Codec, MediaKind, NegotiatedMedia, NegotiatedSession, SdpDirection};

/// A scripted media engine: yields a fixed list of injected RTP frames in order,
/// then signals clean end-of-stream. No sockets, no crypto — exactly the seam the
/// real ICE/DTLS/SRTP engine plugs into at the application layer.
struct ScriptedEngine {
    frames: std::collections::VecDeque<RtpFrame>,
}

impl ScriptedEngine {
    fn new(frames: Vec<RtpFrame>) -> Self {
        Self {
            frames: frames.into(),
        }
    }
}

impl MediaEngine for ScriptedEngine {
    fn poll_rtp(&mut self) -> Result<Option<RtpFrame>, multiview_input::Error> {
        Ok(self.frames.pop_front())
    }
}

/// The video payload type the test offers negotiate (H.264).
const VIDEO_PT: u8 = 98;
/// The audio payload type the test offers negotiate (Opus).
const AUDIO_PT: u8 = 111;

/// The standard test session as the router/producer seam receives it: H.264 video
/// on PT 98 + Opus audio on PT 111, both `recvonly` (a WHIP publish answers the
/// publisher's `sendonly` offer `recvonly`).
fn negotiated_av() -> NegotiatedSession {
    NegotiatedSession {
        sections: vec![
            NegotiatedMedia {
                kind: MediaKind::Video,
                payload_type: VIDEO_PT,
                codec: Codec::H264,
                direction: SdpDirection::RecvOnly,
            },
            NegotiatedMedia {
                kind: MediaKind::Audio,
                payload_type: AUDIO_PT,
                codec: Codec::OPUS,
                direction: SdpDirection::RecvOnly,
            },
        ],
    }
}

/// The `audio = false` ingest variant: the audio m-line is answered `inactive`
/// (ADR-T014 §5), so its section must not route.
fn negotiated_inactive_audio() -> NegotiatedSession {
    NegotiatedSession {
        sections: vec![
            NegotiatedMedia {
                kind: MediaKind::Video,
                payload_type: VIDEO_PT,
                codec: Codec::H264,
                direction: SdpDirection::RecvOnly,
            },
            NegotiatedMedia {
                kind: MediaKind::Audio,
                payload_type: AUDIO_PT,
                codec: Codec::OPUS,
                direction: SdpDirection::Inactive,
            },
        ],
    }
}

/// Build an H.264 single-NAL RTP frame (one NAL unit per packet, no fragmentation).
fn single_nal(seq: u16, timestamp: u32, marker: bool, nal: &[u8]) -> RtpFrame {
    RtpFrame {
        payload_type: VIDEO_PT,
        sequence: seq,
        timestamp,
        marker,
        payload: nal.to_vec(),
    }
}

/// Build an Opus RTP frame (RFC 7587: the payload IS one Opus packet).
fn opus_packet(seq: u16, timestamp: u32, payload: &[u8]) -> RtpFrame {
    RtpFrame {
        payload_type: AUDIO_PT,
        sequence: seq,
        timestamp,
        marker: false,
        payload: payload.to_vec(),
    }
}

/// A minimal H.264 IDR-slice NAL (`nal_unit_type` 5 => keyframe) and a non-IDR
/// slice NAL (type 1 => delta). The depacketizer keys keyframe-gating on the type.
const IDR_NAL: &[u8] = &[0x65, 0x88, 0x84, 0x00];
const NON_IDR_NAL: &[u8] = &[0x41, 0x9A, 0x00, 0x00];

/// Drain a producer to clean end-of-stream, collecting every typed media event.
fn collect_events(producer: &mut WebRtcProducer) -> Vec<MediaEvent> {
    let mut events = Vec::new();
    while let Some(event) = producer.next_event().expect("engine never faults") {
        events.push(event);
    }
    events
}

/// Unwrap a video access-unit event.
fn expect_video(event: &MediaEvent) -> &MediaUnit {
    match event {
        MediaEvent::VideoAccessUnit(unit) => unit,
        other => panic!("expected a video access unit, got {other:?}"),
    }
}

/// Unwrap an audio frame event.
fn expect_audio(event: &MediaEvent) -> &MediaUnit {
    match event {
        MediaEvent::AudioFrame(unit) => unit,
        other => panic!("expected an audio frame, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// H.264 depacketizer (RFC 6184): keyframe gate, FU-A, STAP-A, gaps, bounds
// ---------------------------------------------------------------------------

#[test]
fn depacketizer_gates_until_first_keyframe() {
    let mut depack = H264Depacketizer::new();
    // A delta frame before any keyframe is dropped (no SPS/PPS reference yet).
    let out = depack.push(&single_nal(1, 1000, true, NON_IDR_NAL));
    assert!(out.is_none(), "delta frame before keyframe must be gated");

    // The first keyframe opens the gate and emits an access unit.
    let out = depack
        .push(&single_nal(2, 2000, true, IDR_NAL))
        .expect("keyframe emits an access unit");
    assert_eq!(out.raw_pts, Some(2000));
    assert!(out.keyframe);
    assert!(!out.data.is_empty(), "access unit carries the NAL bytes");

    // After the gate is open, a delta frame is admitted.
    let out = depack
        .push(&single_nal(3, 3000, true, NON_IDR_NAL))
        .expect("delta after keyframe emits");
    assert!(!out.keyframe);
    assert_eq!(out.raw_pts, Some(3000));
}

#[test]
fn depacketizer_reassembles_fu_a_fragments() {
    // An FU-A fragmentation of an IDR NAL across two packets:
    //   FU indicator byte: F|NRI|type=28 (0x7C for NRI=3) ; here use 0x7C.
    //   FU header: S/E/R + original nal_unit_type (5 for IDR).
    // Start fragment: S=1 -> 0x85 ; End fragment: E=1 -> 0x45.
    let fu_indicator = 0x7C_u8;
    let start = vec![fu_indicator, 0x85, 0xDE, 0xAD];
    let end = vec![fu_indicator, 0x45, 0xBE, 0xEF];

    let mut depack = H264Depacketizer::new();
    // Start fragment alone does not complete the access unit.
    assert!(depack
        .push(&RtpFrame {
            payload_type: VIDEO_PT,
            sequence: 10,
            timestamp: 9000,
            marker: false,
            payload: start,
        })
        .is_none());
    // End fragment (marker set) completes it; the reconstructed NAL is keyframe.
    let out = depack
        .push(&RtpFrame {
            payload_type: VIDEO_PT,
            sequence: 11,
            timestamp: 9000,
            marker: true,
            payload: end,
        })
        .expect("FU-A completes on the end fragment");
    assert!(out.keyframe);
    assert_eq!(out.raw_pts, Some(9000));
    // The reassembled NAL starts with the reconstructed header (type 5) then the
    // fragment payloads concatenated.
    assert_eq!(out.data.first().copied().map(|b| b & 0x1F), Some(5));
    assert!(out.data.windows(2).any(|w| w == [0xDE, 0xAD]));
    assert!(out.data.windows(2).any(|w| w == [0xBE, 0xEF]));
}

#[test]
fn depacketizer_emits_stap_a_and_keys_on_aggregated_idr() {
    // STAP-A (RFC 6184 §5.7.1): [hdr=24|NRI][len][NAL][len][NAL]...
    // The header byte 0x78 is NRI=3, type 24.
    let stap_hdr = 0x78_u8;
    // A STAP-A holding only a non-IDR slice: gated before any keyframe.
    let delta_stap = vec![stap_hdr, 0x00, 0x02, 0x41, 0x9A];
    // A STAP-A holding SPS-ish bytes plus an IDR slice: opens the gate.
    let idr_stap = vec![stap_hdr, 0x00, 0x02, 0x67, 0x42, 0x00, 0x02, 0x65, 0x88];

    let mut depack = H264Depacketizer::new();
    assert!(
        depack
            .push(&single_nal(1, 1000, true, &delta_stap))
            .is_none(),
        "a STAP-A with no IDR is gated before the first keyframe"
    );
    assert!(!depack.gate_open());

    let out = depack
        .push(&single_nal(2, 2000, true, &idr_stap))
        .expect("a STAP-A containing an IDR opens the gate and emits");
    assert!(out.keyframe);
    assert_eq!(out.raw_pts, Some(2000));
    // The emitted unit is the whole aggregation payload, verbatim.
    assert_eq!(out.data, idr_stap);
    assert!(depack.gate_open());
}

#[test]
fn depacketizer_flags_sequence_gap_as_discontinuity() {
    let mut depack = H264Depacketizer::new();
    // First packet: never a discontinuity (nothing to gap against).
    let out = depack
        .push(&single_nal(1, 1000, true, IDR_NAL))
        .expect("keyframe emits");
    assert!(!out.discontinuity);
    // In-order successor: no gap.
    let out = depack
        .push(&single_nal(2, 2000, true, NON_IDR_NAL))
        .expect("delta emits");
    assert!(!out.discontinuity);
    // Sequence jumps 2 -> 5: packets 3..4 were lost — discontinuity.
    let out = depack
        .push(&single_nal(5, 5000, true, NON_IDR_NAL))
        .expect("delta emits");
    assert!(out.discontinuity, "a lost packet must flag a discontinuity");
    // The next in-order packet is clean again.
    let out = depack
        .push(&single_nal(6, 6000, true, NON_IDR_NAL))
        .expect("delta emits");
    assert!(!out.discontinuity);
    // A stale reordered packet (seq 4 after 6) is not a *forward* gap, and it
    // must not move the watermark backwards.
    let out = depack
        .push(&single_nal(4, 4000, true, NON_IDR_NAL))
        .expect("late delta emits (gate is open)");
    assert!(
        !out.discontinuity,
        "a reordered packet is not a forward gap"
    );
    let out = depack
        .push(&single_nal(7, 7000, true, NON_IDR_NAL))
        .expect("delta emits");
    assert!(
        !out.discontinuity,
        "watermark must have stayed at 6 across the stale packet"
    );
}

#[test]
fn depacketizer_bounds_fu_a_reassembly() {
    // Feed FU-A fragments whose total exceeds MAX_ACCESS_UNIT_BYTES (8 MiB):
    // the reassembly must be dropped, never grown (invariant #5).
    let fu_indicator = 0x7C_u8;
    let chunk = vec![0xAB_u8; 1024 * 1024]; // 1 MiB per fragment.

    let mut depack = H264Depacketizer::new();
    let mut start_payload = vec![fu_indicator, 0x85]; // S=1, type 5 (IDR).
    start_payload.extend_from_slice(&chunk);
    assert!(depack
        .push(&RtpFrame {
            payload_type: VIDEO_PT,
            sequence: 1,
            timestamp: 1000,
            marker: false,
            payload: start_payload,
        })
        .is_none());
    // Nine 1 MiB continuations push the total past the 8 MiB cap.
    for i in 0..9_u16 {
        let mut payload = vec![fu_indicator, 0x05]; // continuation, type 5.
        payload.extend_from_slice(&chunk);
        assert!(depack
            .push(&RtpFrame {
                payload_type: VIDEO_PT,
                sequence: 2 + i,
                timestamp: 1000,
                marker: false,
                payload,
            })
            .is_none());
    }
    // The end fragment must NOT emit a >8 MiB unit: the over-long reassembly was
    // abandoned, so there is nothing in progress to complete.
    let out = depack.push(&RtpFrame {
        payload_type: VIDEO_PT,
        sequence: 11,
        timestamp: 1000,
        marker: true,
        payload: vec![fu_indicator, 0x45, 0xEE], // E=1, type 5.
    });
    assert!(
        out.is_none(),
        "an over-cap FU-A reassembly must be dropped, not emitted"
    );
    // The depacketizer recovers: a fresh complete keyframe still emits.
    let out = depack
        .push(&single_nal(12, 2000, true, IDR_NAL))
        .expect("a complete keyframe after the dropped reassembly emits");
    assert!(out.keyframe);
}

// ---------------------------------------------------------------------------
// Opus depacketizer (RFC 7587)
// ---------------------------------------------------------------------------

#[test]
fn opus_depacketizer_emits_one_frame_per_packet() {
    let mut depack = OpusDepacketizer::new();
    // RFC 7587 §4.2: one RTP payload == one Opus packet, surfaced verbatim.
    let out = depack
        .push(&opus_packet(100, 4800, &[0x78, 0x01, 0x02]))
        .expect("a non-empty Opus payload emits one frame");
    assert_eq!(out.data, vec![0x78, 0x01, 0x02]);
    // The 48 kHz RTP timestamp is the raw PTS, verbatim.
    assert_eq!(out.raw_pts, Some(4800));
    assert!(!out.discontinuity, "the first packet never flags a gap");

    // In-order successor: clean.
    let out = depack
        .push(&opus_packet(101, 5760, &[0x78, 0x03]))
        .expect("emits");
    assert_eq!(out.raw_pts, Some(5760));
    assert!(!out.discontinuity);

    // Sequence jumps 101 -> 103: packet 102 was lost — discontinuity.
    let out = depack
        .push(&opus_packet(103, 7680, &[0x78, 0x04]))
        .expect("emits");
    assert!(out.discontinuity, "a lost packet must flag a discontinuity");

    // Clean again afterwards.
    let out = depack
        .push(&opus_packet(104, 8640, &[0x78, 0x05]))
        .expect("emits");
    assert!(!out.discontinuity);
}

#[test]
fn opus_depacketizer_drops_empty_payload_and_flags_the_next_frame() {
    let mut depack = OpusDepacketizer::new();
    assert!(depack.push(&opus_packet(1, 960, &[0x78])).is_some());
    // An empty payload is not a valid Opus packet (RFC 6716: at least the TOC
    // byte): dropped, counted, never panics.
    assert!(depack.push(&opus_packet(2, 1920, &[])).is_none());
    assert_eq!(depack.dropped(), 1);
    // The decode gap the drop created surfaces on the next emitted frame.
    let out = depack
        .push(&opus_packet(3, 2880, &[0x78]))
        .expect("emits after the dropped packet");
    assert!(
        out.discontinuity,
        "a dropped packet is a decode gap and must flag the next frame"
    );
}

#[test]
fn opus_depacketizer_bounds_packet_size() {
    let mut depack = OpusDepacketizer::new();
    assert!(depack.push(&opus_packet(1, 960, &[0x78])).is_some());
    // An over-cap payload is dropped, never buffered or grown (invariant #5).
    let oversize = vec![0x78_u8; MAX_OPUS_PACKET_BYTES + 1];
    assert!(depack.push(&opus_packet(2, 1920, &oversize)).is_none());
    assert_eq!(depack.dropped(), 1);
    let out = depack
        .push(&opus_packet(3, 2880, &[0x78]))
        .expect("emits after the dropped packet");
    assert!(out.discontinuity);
}

#[test]
fn opus_depacketizer_reorder_does_not_flag_or_rewind() {
    let mut depack = OpusDepacketizer::new();
    assert!(
        !depack
            .push(&opus_packet(10, 960, &[0x01]))
            .expect("emits")
            .discontinuity
    );
    assert!(
        !depack
            .push(&opus_packet(11, 1920, &[0x02]))
            .expect("emits")
            .discontinuity
    );
    // A stale reordered packet is not a forward gap...
    assert!(
        !depack
            .push(&opus_packet(9, 0, &[0x03]))
            .expect("emits")
            .discontinuity
    );
    // ...and the watermark must not rewind: 12 follows 11 cleanly.
    assert!(
        !depack
            .push(&opus_packet(12, 2880, &[0x04]))
            .expect("emits")
            .discontinuity
    );
}

// ---------------------------------------------------------------------------
// Payload-type routing (NegotiatedSession -> typed media events)
// ---------------------------------------------------------------------------

#[test]
fn router_dispatches_by_negotiated_payload_type() {
    let mut router = RtpRouter::new(&negotiated_av());

    // A video keyframe routes to the H.264 depacketizer.
    let event = router
        .route(&single_nal(1, 90_000, true, IDR_NAL))
        .expect("keyframe emits a video event");
    let unit = expect_video(&event);
    assert_eq!(unit.codec, Codec::H264);
    assert!(unit.keyframe);
    assert_eq!(unit.raw_pts, Some(90_000));
    assert_eq!(unit.data, IDR_NAL.to_vec());

    // An audio packet routes to the Opus depacketizer.
    let event = router
        .route(&opus_packet(50, 4800, &[0x78, 0x0A]))
        .expect("an Opus packet emits an audio event");
    let unit = expect_audio(&event);
    assert_eq!(unit.codec, Codec::OPUS);
    assert_eq!(unit.raw_pts, Some(4800));
    assert_eq!(unit.data, vec![0x78, 0x0A]);
    // Opus has no delta-frame gating: every frame is a decoder entry point.
    assert!(unit.keyframe);

    // An unknown payload type is counted and dropped — never an error.
    assert!(router
        .route(&RtpFrame {
            payload_type: 77,
            sequence: 1,
            timestamp: 0,
            marker: false,
            payload: vec![0x00],
        })
        .is_none());
    assert_eq!(router.unknown_dropped(), 1);
    assert!(router
        .route(&RtpFrame {
            payload_type: 96,
            sequence: 2,
            timestamp: 0,
            marker: true,
            payload: vec![],
        })
        .is_none());
    assert_eq!(router.unknown_dropped(), 2);
}

#[test]
fn router_keeps_per_stream_gap_detection_independent() {
    let mut router = RtpRouter::new(&negotiated_av());
    // Open the video gate.
    assert!(router.route(&single_nal(1, 0, true, IDR_NAL)).is_some());
    assert!(router.route(&opus_packet(1, 0, &[0x78])).is_some());
    // A video gap must NOT flag the audio stream (independent sequence spaces).
    let event = router
        .route(&single_nal(5, 3000, true, NON_IDR_NAL))
        .expect("video emits");
    assert!(expect_video(&event).discontinuity);
    let event = router
        .route(&opus_packet(2, 960, &[0x78]))
        .expect("audio emits");
    assert!(
        !expect_audio(&event).discontinuity,
        "a video-stream gap must not leak into the audio stream"
    );
}

#[test]
fn router_does_not_bind_an_inactive_media_section() {
    // `audio = false` ingest answers the audio m-line inactive (ADR-T014 §5):
    // an inactive section must not route — its PT counts as unknown.
    let negotiated = negotiated_inactive_audio();
    let mut router = RtpRouter::new(&negotiated);
    assert!(router.route(&single_nal(1, 0, true, IDR_NAL)).is_some());
    assert!(
        router.route(&opus_packet(1, 0, &[0x78])).is_none(),
        "an inactive audio section must not yield events"
    );
    assert_eq!(router.unknown_dropped(), 1);
}

// ---------------------------------------------------------------------------
// Timing surfaces (invariant #3): WrapBits::Rtp32 on both clocks
// ---------------------------------------------------------------------------

#[test]
fn timing_surfaces_are_rtp32_on_both_clocks() {
    // Both RTP clocks are 32-bit: video rides 90 kHz, audio rides 48 kHz.
    assert_eq!(WebRtcProducer::WRAP_BITS, WrapBits::Rtp32);
    assert_eq!(MediaUnit::WRAP_BITS, WrapBits::Rtp32);
    assert_eq!(VIDEO_CLOCK_RATE, 90_000);
    assert_eq!(AUDIO_CLOCK_RATE, 48_000);

    let mut router = RtpRouter::new(&negotiated_av());
    let video = router
        .route(&single_nal(1, 0, true, IDR_NAL))
        .expect("video emits");
    let tb = expect_video(&video).timebase();
    assert_eq!((tb.num, tb.den), (1, 90_000), "video timebase is 1/90000");
    let audio = router
        .route(&opus_packet(1, 0, &[0x78]))
        .expect("audio emits");
    let tb = expect_audio(&audio).timebase();
    assert_eq!((tb.num, tb.den), (1, 48_000), "audio timebase is 1/48000");
}

// ---------------------------------------------------------------------------
// WebRtcProducer: the honest compressed media-event seam
// ---------------------------------------------------------------------------

#[test]
fn producer_yields_typed_media_events_for_both_streams() {
    // Interleaved video (keyframe + deltas) and audio, fed through the producer.
    let engine = ScriptedEngine::new(vec![
        single_nal(1, 90_000, true, IDR_NAL),
        opus_packet(50, 4800, &[0x78, 0x01]),
        single_nal(2, 93_000, true, NON_IDR_NAL),
        opus_packet(51, 5760, &[0x78, 0x02]),
        single_nal(3, 96_000, true, NON_IDR_NAL),
    ]);
    let mut producer = WebRtcProducer::new(Box::new(engine), &negotiated_av());

    let events = collect_events(&mut producer);
    assert_eq!(events.len(), 5, "every admitted packet yields one event");

    let v0 = expect_video(&events[0]);
    assert!(v0.keyframe);
    assert_eq!(v0.raw_pts, Some(90_000));
    assert_eq!(v0.codec, Codec::H264);

    let a0 = expect_audio(&events[1]);
    assert_eq!(a0.raw_pts, Some(4800));
    assert_eq!(a0.codec, Codec::OPUS);
    assert_eq!(a0.data, vec![0x78, 0x01]);

    let v1 = expect_video(&events[2]);
    assert!(!v1.keyframe);
    assert_eq!(v1.raw_pts, Some(93_000));

    let a1 = expect_audio(&events[3]);
    assert_eq!(a1.raw_pts, Some(5760));

    let v2 = expect_video(&events[4]);
    assert_eq!(v2.raw_pts, Some(96_000));

    // The compressed bytes are surfaced verbatim — no fabricated pixel geometry
    // anywhere on this seam (ADR-T014: geometry comes from the decoder/SPS at
    // the application layer).
    assert_eq!(v0.data, IDR_NAL.to_vec());
}

#[test]
fn producer_drops_pre_keyframe_garbage_without_stalling() {
    // Two delta frames arrive before any keyframe: they are sampled and dropped,
    // never pacing the producer or stalling it (invariants #1/#2).
    let engine = ScriptedEngine::new(vec![
        single_nal(1, 1000, true, NON_IDR_NAL),
        single_nal(2, 2000, true, NON_IDR_NAL),
        single_nal(3, 3000, true, IDR_NAL),
    ]);
    let mut producer = WebRtcProducer::new(Box::new(engine), &negotiated_av());
    let events = collect_events(&mut producer);
    // Only the keyframe (and anything after) is emitted — the two pre-keyframe
    // deltas were dropped by the gate.
    assert_eq!(events.len(), 1);
    let unit = expect_video(&events[0]);
    assert!(unit.keyframe);
    assert_eq!(unit.raw_pts, Some(3000));
}

#[test]
fn producer_counts_and_drops_unknown_payload_types() {
    let engine = ScriptedEngine::new(vec![
        RtpFrame {
            payload_type: 33,
            sequence: 1,
            timestamp: 0,
            marker: false,
            payload: vec![0xFF, 0xFF],
        },
        single_nal(1, 1000, true, IDR_NAL),
        RtpFrame {
            payload_type: 96,
            sequence: 2,
            timestamp: 0,
            marker: true,
            payload: vec![0x00],
        },
    ]);
    let mut producer = WebRtcProducer::new(Box::new(engine), &negotiated_av());
    let events = collect_events(&mut producer);
    assert_eq!(events.len(), 1, "only the negotiated PTs yield events");
    assert!(expect_video(&events[0]).keyframe);
    assert_eq!(
        producer.router().unknown_dropped(),
        2,
        "unknown payload types are counted and dropped, never errors"
    );
}

/// Live ICE/DTLS/SRTP path — **gated, requires a real peer + application-layer
/// media engine**.
///
/// `multiview-input` deliberately links **no** native WebRTC library: the real
/// ICE/DTLS/SRTP engine is supplied at the application layer (a sans-IO driver
/// behind the binary's `webrtc` wiring), so there is nothing to drive a real
/// socket from inside this crate's test. This test is therefore `#[ignore]`d and
/// only runs when an operator points it at a real peer via
/// `MULTIVIEW_WEBRTC_PEER`. Absent that, it skips honestly (it never asserts a
/// fake pass) — the injected-engine tests above carry the depacketize/routing
/// correctness load.
#[test]
#[ignore = "needs a real WebRTC peer + application-layer ICE/DTLS/SRTP engine (set MULTIVIEW_WEBRTC_PEER)"]
fn live_webrtc_session_receives_rtp() {
    let Ok(peer) = std::env::var("MULTIVIEW_WEBRTC_PEER") else {
        // No peer configured: skip rather than fake a pass. The real engine lives
        // at the application layer and is not part of this crate.
        eprintln!(
            "skipping live webrtc test: set MULTIVIEW_WEBRTC_PEER to a reachable \
             WHIP/WebRTC ingest endpoint and supply an application-layer MediaEngine"
        );
        return;
    };
    panic!(
        "live webrtc ingest against {peer} requires an application-layer \
         ICE/DTLS/SRTP MediaEngine implementation, which is not linked into \
         multiview-input (pure/LGPL-clean by design)"
    );
}
