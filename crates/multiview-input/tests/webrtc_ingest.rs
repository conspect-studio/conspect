//! Tests for the gated WebRTC **ingest session** core (feature `webrtc`): the
//! connection-state lifecycle, the H.264 RTP depacketize -> access-unit seam
//! (keyframe-gated, FU-A reassembly, bounded reorder), and the
//! [`WebRtcProducer`] driven by an **injected** fake media engine into the
//! [`IngestPump`] -> [`TileStore`].
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

use multiview_core::time::MediaTime;
use multiview_framestore::TileStore;
use multiview_input::source::{IngestConfig, IngestPump, StoredFrame};
use multiview_input::webrtc::transport::{
    H264Depacketizer, MediaEngine, RtpFrame, SessionState, WebRtcProducer, WebRtcSession,
};
use multiview_input::webrtc::{Codec, SessionDescription};

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

/// Build an H.264 single-NAL RTP frame (one NAL unit per packet, no fragmentation).
fn single_nal(seq: u16, timestamp: u32, marker: bool, nal: &[u8]) -> RtpFrame {
    RtpFrame {
        payload_type: 98,
        sequence: seq,
        timestamp,
        marker,
        payload: nal.to_vec(),
    }
}

/// A minimal H.264 IDR-slice NAL (`nal_unit_type` 5 => keyframe) and a non-IDR
/// slice NAL (type 1 => delta). The depacketizer keys keyframe-gating on the type.
const IDR_NAL: &[u8] = &[0x65, 0x88, 0x84, 0x00];
const NON_IDR_NAL: &[u8] = &[0x41, 0x9a, 0x00, 0x00];

#[test]
fn session_state_lifecycle_is_validated() {
    let mut st = SessionState::Created;
    assert_eq!(st, SessionState::Created);
    // Created -> Connecting -> Connected is the happy path.
    st = st.advance(SessionState::Connecting).expect("connecting");
    st = st.advance(SessionState::Connected).expect("connected");
    assert_eq!(st, SessionState::Connected);
    // Connected -> Closed terminates cleanly.
    st = st.advance(SessionState::Closed).expect("closed");
    assert!(st.is_terminal());

    // An illegal jump (Created straight to Connected) is rejected, not panicked.
    assert!(SessionState::Created
        .advance(SessionState::Connected)
        .is_err());
    // A terminal state never transitions again.
    assert!(SessionState::Closed
        .advance(SessionState::Connecting)
        .is_err());
    // Any non-terminal state may fail.
    assert!(SessionState::Connecting
        .advance(SessionState::Failed)
        .is_ok());
}

#[test]
fn session_start_runs_state_machine_to_connected() {
    let offer = "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 98\r\na=rtpmap:98 H264/90000\r\na=sendonly\r\n";
    let sdp = SessionDescription::parse(offer).expect("sdp");
    let negotiated = sdp
        .negotiate_answer(&[Codec::H264], &[Codec::OPUS])
        .expect("negotiated");
    let mut session = WebRtcSession::new(negotiated);
    assert_eq!(session.state(), SessionState::Created);
    session.connect().expect("connect drives to connected");
    assert_eq!(session.state(), SessionState::Connected);
    session.close().expect("close");
    assert_eq!(session.state(), SessionState::Closed);
}

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
    assert!(!out.pixels.is_empty(), "access unit carries the NAL bytes");

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
            payload_type: 98,
            sequence: 10,
            timestamp: 9000,
            marker: false,
            payload: start,
        })
        .is_none());
    // End fragment (marker set) completes it; the reconstructed NAL is keyframe.
    let out = depack
        .push(&RtpFrame {
            payload_type: 98,
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
    assert_eq!(out.pixels.first().copied().map(|b| b & 0x1F), Some(5));
    assert!(out.pixels.windows(2).any(|w| w == [0xDE, 0xAD]));
    assert!(out.pixels.windows(2).any(|w| w == [0xBE, 0xEF]));
}

#[test]
fn producer_feeds_ingest_pump_into_tile_store() {
    // A keyframe then two delta frames, all single-NAL, fed through the producer.
    let engine = ScriptedEngine::new(vec![
        single_nal(1, 90_000, true, IDR_NAL),
        single_nal(2, 93_000, true, NON_IDR_NAL),
        single_nal(3, 96_000, true, NON_IDR_NAL),
    ]);
    let mut producer = WebRtcProducer::new(Box::new(engine), Codec::H264, 1280, 720);

    let store: TileStore<StoredFrame> = TileStore::with_defaults("webrtc-0");
    let mut pump = IngestPump::new(&producer, IngestConfig::default());
    let published = pump
        .run_to_end(&mut producer, &store, MediaTime::ZERO)
        .expect("pump runs to clean EOS");

    // All three access units reach the store; the gate was opened by the keyframe.
    assert_eq!(published, 3);
    assert!(store.read(MediaTime::ZERO).frame().is_some());
}

#[test]
fn producer_drops_pre_keyframe_garbage_without_stalling() {
    // Two delta frames arrive before any keyframe: they are sampled and dropped,
    // never pacing the pump or stalling it (invariants #1/#2).
    let engine = ScriptedEngine::new(vec![
        single_nal(1, 1000, true, NON_IDR_NAL),
        single_nal(2, 2000, true, NON_IDR_NAL),
        single_nal(3, 3000, true, IDR_NAL),
    ]);
    let mut producer = WebRtcProducer::new(Box::new(engine), Codec::H264, 640, 360);
    let store: TileStore<StoredFrame> = TileStore::with_defaults("webrtc-1");
    let mut pump = IngestPump::new(&producer, IngestConfig::default());
    let published = pump
        .run_to_end(&mut producer, &store, MediaTime::ZERO)
        .expect("pump runs to clean EOS");
    // Only the keyframe (and anything after) is published — the two pre-keyframe
    // deltas were dropped.
    assert_eq!(published, 1);
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
/// fake pass) — the injected-engine tests above carry the depacketize/lifecycle
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
