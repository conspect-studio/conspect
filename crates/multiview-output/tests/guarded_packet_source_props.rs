//! GP-7 property test (f): emitted DTS is strictly increasing across an
//! arbitrary live → slate → live → … sequence (the `ffmpeg` feature; ADR-0030
//! §4).
//!
//! The headline invariant a `GuardedPacketSource` must hold for
//! `av_interleaved_write_frame` to never abort is **monotonic, strictly
//! increasing DTS** on its emitted video stream — across BOTH seams (input→slate
//! and slate→input recovery) and across slate loop wraps, for ANY schedule of
//! outages and recoveries. This drives the guarded source over a randomized
//! schedule of (live-run length, outage length) pairs and asserts every emitted
//! video DTS strictly exceeds the previous one.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions
)]

use std::sync::Arc;

use ffmpeg::codec::packet::flag::Flags as PacketFlags;
use ffmpeg_next as ffmpeg;
use multiview_core::time::Rational;
use multiview_ffmpeg::{CodecKind, EncodedPacket, NalFraming, StreamKind};
use multiview_framestore::{PacketLiveness, PacketLivenessThresholds};
use multiview_output::guarded::{GuardedConfig, GuardedPacketSource, ManualClock};
use multiview_output::sink::PacketSource;
use multiview_output::slate::{
    BakedSlate, SlateBaker, SlateKind, SlateSpec, SlateVideoCodec, SlateVideoSpec,
};
use multiview_output::Result;
use proptest::prelude::*;

const FRAME_NS: i64 = 33_366_666;
const SEGMENT_NS: i64 = 2_000_000_000;

fn thresholds() -> PacketLivenessThresholds {
    PacketLivenessThresholds::from_frame_and_segment(FRAME_NS, SEGMENT_NS).expect("ladder")
}

fn tiny_slate() -> BakedSlate {
    SlateBaker::bake_slate(&SlateSpec {
        kind: SlateKind::Black,
        video: SlateVideoSpec {
            codec: SlateVideoCodec::Mpeg2Video,
            width: 64,
            height: 64,
            cadence: Rational::FPS_30,
            gop: 2,
        },
        audio: None,
    })
    .expect("bake")
}

fn idr_au() -> Vec<u8> {
    vec![0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x00]
}

fn non_idr_au() -> Vec<u8> {
    vec![0x00, 0x00, 0x00, 0x01, 0x61, 0x9a, 0x00, 0x00]
}

fn video_packet(data: &[u8], dts: i64, key: bool) -> EncodedPacket {
    let mut p = ffmpeg::codec::packet::Packet::copy(data);
    p.set_dts(Some(dts));
    p.set_pts(Some(dts));
    if key {
        p.set_flags(PacketFlags::KEY);
    }
    EncodedPacket::from_packet(p)
}

struct FakeLive {
    packets: std::collections::VecDeque<EncodedPacket>,
}

impl PacketSource for FakeLive {
    fn next_packet(&mut self) -> Result<Option<EncodedPacket>> {
        Ok(self.packets.pop_front())
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// For any schedule of live-runs interleaved with outages, every emitted
    /// VIDEO DTS strictly increases.
    #[test]
    fn dts_strictly_increasing_across_arbitrary_live_slate_sequence(
        // Each (run_len, outage_pulls) pair: `run_len` live IDR-led packets
        // (each starting with a true IDR so recovery can re-enter), then an
        // outage during which we pull `outage_pulls` packets (slate).
        schedule in prop::collection::vec((1usize..=4, 1usize..=6), 1..=6),
    ) {
        let clock = Arc::new(ManualClock::new());

        // Build the scripted live stream: each run is one IDR then non-IDR
        // follow-ons, with raw DTS climbing across the whole script.
        let mut packets = std::collections::VecDeque::new();
        let mut raw = 0_i64;
        for (run_len, _) in &schedule {
            for i in 0..*run_len {
                let (data, key) = if i == 0 {
                    (idr_au(), true)
                } else {
                    (non_idr_au(), false)
                };
                packets.push_back(video_packet(&data, raw, key));
                raw += 100;
            }
        }
        let live = FakeLive { packets };
        let video_liveness = Arc::new(PacketLiveness::new(thresholds()));
        let mut src = GuardedPacketSource::new(
            live,
            tiny_slate(),
            video_liveness,
            None,
            Arc::clone(&clock),
            GuardedConfig::new(CodecKind::H264, NalFraming::AnnexB, FRAME_NS),
        );

        let mut last = i64::MIN;
        // Drive the schedule: alternate healthy pulls (clock advancing within
        // STALE) with outage pulls (clock parked past SPLICE).
        for (run_len, outage_pulls) in &schedule {
            // Healthy phase: pull up to run_len packets, advancing the clock
            // just under STALE each time so we stay LIVE.
            for _ in 0..*run_len {
                if let Some(pkt) = src.next_packet().expect("pull") {
                    if pkt.kind() == StreamKind::Video {
                        let dts = pkt.dts().expect("dts");
                        prop_assert!(dts > last, "live DTS strictly increasing: {} > {}", dts, last);
                        last = dts;
                    }
                }
                clock.advance(FRAME_NS / 2);
            }
            // Outage phase: jump the clock past SPLICE and pull slate packets.
            clock.advance(SEGMENT_NS);
            for _ in 0..*outage_pulls {
                let pkt = src.next_packet().expect("pull").expect("slate keeps emitting");
                if pkt.kind() == StreamKind::Video {
                    let dts = pkt.dts().expect("dts");
                    prop_assert!(dts > last, "slate DTS strictly increasing: {} > {}", dts, last);
                    last = dts;
                }
            }
        }
    }
}
