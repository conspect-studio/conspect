//! GP-7 — the `GuardedPacketSource` splice seam (the `ffmpeg` feature;
//! ADR-0030 §4 "The splice seam — `GuardedPacketSource`").
//!
//! GP-7 assembles the already-merged guarded-passthrough primitives into the
//! live copy-vs-slate failover: a [`GuardedPacketSource`] wraps the LIVE input
//! packet source (a [`PacketSource`]) and the GP-4 pre-baked [`BakedSlate`], and
//! is the SOLE producer feeding `PacketMuxSink::run_av`. While the input is
//! healthy it emits the copied input packets, re-stamped monotonic; on loss it
//! flips an `AtomicU8` mode to SLATE (the GP-5 watchdog decision) and emits the
//! pre-baked slate packets, looping the slate by advancing the GP-6 restamp
//! offset per wrap; on recovery it discards input until a TRUE strict-IDR video
//! AU (GP-1 `is_idr`, NOT `is_key`) before resuming the copy.
//!
//! These tests pin that contract from the GP-7 backlog row + the task brief:
//!
//! * (a) healthy input → the copied input packets, re-stamped monotonic;
//! * (b) input goes silent past the splice threshold → mode flips to SLATE and
//!   `next_packet` returns slate packets, DTS strictly increasing across the
//!   input→slate seam (GP-6 rebase);
//! * (c) recovery: input returns but `next_packet` keeps emitting slate until a
//!   TRUE `is_idr` AU arrives (a non-IDR / recovery-point I-frame flagged `key`
//!   does NOT trigger re-entry), then resumes copy with DTS strictly increasing
//!   across the slate→input seam;
//! * (d) the slate loops (offset advances per wrap) without a DTS discontinuity
//!   during a long outage;
//! * (e) fail-safe: a stale liveness read biases to SLATE, never false-LIVE; and
//! * (f) emitted DTS is strictly increasing across an arbitrary live/slate/live
//!   sequence (property test, see `guarded_packet_source_props.rs`).
//!
//! Licensing (LGPL-clean): the slate fixture bakes `mpeg2video` (an LGPL
//! software codec in `FFmpeg`); the IDR fixtures are hand-written H.264 Annex-B
//! NAL bytes (no encode). No live network — a fake in-memory `PacketSource` + an
//! injected manual clock + a tiny in-process slate bake.
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
use multiview_output::guarded::{GuardMode, GuardedConfig, GuardedPacketSource, ManualClock};
use multiview_output::sink::PacketSource;
use multiview_output::slate::{
    BakedSlate, SlateAudio, SlateAudioSpec, SlateBaker, SlateKind, SlateSpec, SlateVideoCodec,
    SlateVideoSpec,
};
use multiview_output::Result;

/// One ~33.367 ms frame interval at ~30 fps, in nanoseconds (`T = 1/fps`).
const FRAME_NS: i64 = 33_366_666;
/// A two-second segment duration in nanoseconds (`Sd`), for the watchdog ladder.
const SEGMENT_NS: i64 = 2_000_000_000;

/// Watchdog thresholds built from the program's `(T, Sd)` — the ADR-0030 ladder
/// (`splice = max(4·T, 150 ms)`).
fn thresholds() -> PacketLivenessThresholds {
    PacketLivenessThresholds::from_frame_and_segment(FRAME_NS, SEGMENT_NS)
        .expect("valid (T, Sd) ladder")
}

/// Bake a tiny in-process `mpeg2video` slate fixture (64×64, a 2-frame closed
/// GOP, video-only): a real `BakedSlate` whose first video AU is a strict IDR,
/// built without any network and well under the < 5 MB budget.
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
    .expect("bake the tiny slate fixture")
}

/// Bake a tiny slate fixture WITH audio (mono 1 kHz tone) for the audio-seam
/// assertions.
fn tiny_slate_with_audio() -> BakedSlate {
    SlateBaker::bake_slate(&SlateSpec {
        kind: SlateKind::Black,
        video: SlateVideoSpec {
            codec: SlateVideoCodec::Mpeg2Video,
            width: 64,
            height: 64,
            cadence: Rational::FPS_30,
            gop: 2,
        },
        audio: Some(SlateAudioSpec {
            sample_rate: 48_000,
            channels: 1,
            audio: SlateAudio::Tone1k,
        }),
    })
    .expect("bake the tiny slate-with-audio fixture")
}

/// Build a video [`EncodedPacket`] carrying `data` at `(dts, pts)`, optionally
/// flagged a container keyframe.
fn video_packet(data: &[u8], dts: i64, pts: i64, key: bool) -> EncodedPacket {
    let mut p = ffmpeg::codec::packet::Packet::copy(data);
    p.set_dts(Some(dts));
    p.set_pts(Some(pts));
    if key {
        p.set_flags(PacketFlags::KEY);
    }
    EncodedPacket::from_packet(p)
}

/// An H.264 Annex-B IDR access unit (`nal_unit_type == 5`) — a TRUE random
/// access point `is_idr` accepts. Deliberately a DIFFERENT byte length to
/// [`h264_non_idr_au`] so an emitted packet's `len()` identifies which input AU
/// it was copied from (the recovery test asserts re-entry copied the IDR, never
/// the `is_key` non-IDR).
const H264_IDR_LEN: usize = 9;
fn h264_idr_au() -> Vec<u8> {
    // start code + NAL header 0x65 (forbidden 0, ref_idc 3, type 5 = IDR slice).
    vec![0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x00, 0x00]
}

/// An H.264 Annex-B NON-IDR coded slice (`nal_unit_type == 1`) that a muxer
/// could still flag a container keyframe (a recovery-point I-frame): `is_idr`
/// must reject it so recovery does NOT re-anchor here. A different byte length
/// to [`h264_idr_au`] (see [`H264_IDR_LEN`]).
const H264_NON_IDR_LEN: usize = 6;
fn h264_non_idr_au() -> Vec<u8> {
    // start code + NAL header 0x61 (forbidden 0, ref_idc 3, type 1 = non-IDR).
    vec![0x00, 0x00, 0x00, 0x01, 0x61, 0x9a]
}

/// A fake in-memory LIVE [`PacketSource`]: yields a scripted sequence of video
/// packets, then `Ok(None)`. No network, no demuxer.
struct FakeLive {
    packets: std::collections::VecDeque<EncodedPacket>,
}

impl FakeLive {
    fn new(packets: Vec<EncodedPacket>) -> Self {
        Self {
            packets: packets.into(),
        }
    }
}

impl PacketSource for FakeLive {
    fn next_packet(&mut self) -> Result<Option<EncodedPacket>> {
        Ok(self.packets.pop_front())
    }
}

/// H.264 Annex-B video config for the IDR classifier (one frame interval).
fn h264_config() -> GuardedConfig {
    GuardedConfig::new(CodecKind::H264, NalFraming::AnnexB, FRAME_NS)
}

/// Build a guarded source over `live` + the tiny slate, with a fresh video
/// watchdog and the shared manual clock (video-only).
fn guarded(
    live: FakeLive,
    slate: BakedSlate,
    clock: Arc<ManualClock>,
) -> GuardedPacketSource<FakeLive, Arc<ManualClock>> {
    let video_liveness = Arc::new(PacketLiveness::new(thresholds()));
    GuardedPacketSource::new(live, slate, video_liveness, None, clock, h264_config())
}

/// (a) Healthy input: each `next_packet` returns the copied input packet,
/// re-stamped to a strictly-increasing DTS, while the input is live. (A guarded
/// passthrough is a degenerate clock that emits forever; with a healthy input
/// every emission is a copied input packet — so we pull exactly the three live
/// packets and assert each is LIVE + monotonic, never relying on the infinite
/// source to "end".)
#[test]
fn healthy_input_copies_through_restamped() {
    let clock = Arc::new(ManualClock::new());
    // Three live IDR-led packets at raw DTS 0, 100, 200.
    let live = FakeLive::new(vec![
        video_packet(&h264_idr_au(), 0, 0, true),
        video_packet(&h264_non_idr_au(), 100, 100, false),
        video_packet(&h264_non_idr_au(), 200, 200, false),
    ]);
    let mut src = guarded(live, tiny_slate(), Arc::clone(&clock));

    let mut last = i64::MIN;
    for _ in 0..3 {
        let pkt = src
            .next_packet()
            .expect("guarded next_packet")
            .expect("a live packet while healthy");
        // Healthy throughout: the clock never advances past STALE, so the mode
        // stays LIVE and we copy the input.
        assert_eq!(src.mode(), GuardMode::Live, "healthy input must stay LIVE");
        assert_eq!(pkt.kind(), StreamKind::Video);
        let dts = pkt.dts().expect("emitted packet carries a DTS");
        assert!(
            dts > last,
            "emitted DTS strictly increasing: {dts} > {last}"
        );
        last = dts;
        // A tiny advance, well under STALE (2·T), keeps us LIVE.
        clock.advance(FRAME_NS / 2);
    }
}

/// (b) Input goes silent past the splice threshold: the mode flips to SLATE and
/// `next_packet` returns slate packets, DTS strictly increasing across the
/// input→slate seam (the GP-6 rebase, no discontinuity).
#[test]
fn silence_past_threshold_splices_to_slate_monotonic() {
    let clock = Arc::new(ManualClock::new());
    // One live packet, then the input dries up (FakeLive yields None forever).
    let live = FakeLive::new(vec![video_packet(&h264_idr_au(), 0, 0, true)]);
    let mut src = guarded(live, tiny_slate(), Arc::clone(&clock));

    // Pull the one live packet (records into the watchdog at t=0).
    let first = src
        .next_packet()
        .expect("live pull")
        .expect("a live packet");
    let live_dts = first.dts().expect("live DTS");
    assert_eq!(src.mode(), GuardMode::Live);

    // Now jump the clock well past SPLICE (max(4·T, 150 ms)) with no new input.
    clock.set(SEGMENT_NS); // 2 s ≫ splice
    let slate_pkt = src
        .next_packet()
        .expect("slate pull")
        .expect("slate keeps emitting");
    assert_eq!(
        src.mode(),
        GuardMode::Slate,
        "silence past splice must flip to SLATE"
    );
    let slate_dts = slate_pkt.dts().expect("slate DTS");
    assert!(
        slate_dts > live_dts,
        "DTS strictly increasing across the input→slate seam: {slate_dts} > {live_dts}"
    );
}

/// (c) Recovery is gated on a TRUE strict IDR. After a splice, the input
/// returns — first a NON-IDR coded slice flagged a container keyframe (a
/// recovery-point I-frame), then a real IDR. `next_packet` must keep emitting
/// slate across the non-IDR packet and only resume copy at the IDR, DTS
/// strictly increasing across the slate→input seam.
#[test]
fn recovery_gated_on_strict_idr_not_is_key() {
    let clock = Arc::new(ManualClock::new());
    // Sequence the live source: one healthy packet, then (after the outage) a
    // KEY-flagged NON-IDR slice, then the true IDR, then a follow-on.
    let live = FakeLive::new(vec![
        video_packet(&h264_idr_au(), 0, 0, true), // healthy
        video_packet(&h264_non_idr_au(), 1000, 1000, true), // KEY but NOT idr
        video_packet(&h264_idr_au(), 1100, 1100, true), // the real re-entry IDR
        video_packet(&h264_non_idr_au(), 1200, 1200, false),
    ]);
    let mut src = guarded(live, tiny_slate(), Arc::clone(&clock));

    // Healthy pull.
    let healthy = src.next_packet().expect("pull").expect("healthy");
    assert_eq!(src.mode(), GuardMode::Live);
    let _ = healthy.dts();

    // Outage: flip to slate.
    clock.set(SEGMENT_NS);
    let s1 = src.next_packet().expect("pull").expect("slate");
    assert_eq!(src.mode(), GuardMode::Slate);
    let mut last = s1.dts().expect("slate dts");

    // Drive several more pulls. The recovery discards the KEY-but-non-IDR packet
    // and keeps emitting slate; it must NOT have re-entered LIVE on the is_key
    // packet. We keep the clock parked in the outage so `evaluate` would still
    // splice were it not for the recovery latch — recovery is is_idr-gated.
    let mut resumed_len = None;
    for _ in 0..16 {
        let pkt = src.next_packet().expect("pull").expect("a packet");
        let dts = pkt.dts().expect("dts");
        assert!(
            dts > last,
            "monotonic across recovery pulls: {dts} > {last}"
        );
        last = dts;
        if src.mode() == GuardMode::Live {
            // The first LIVE emission after the outage is the copied input IDR,
            // re-stamped onto the running timeline. Its payload length identifies
            // the source AU: it MUST be the IDR (raw 1100), never the is_key
            // non-IDR (raw 1000) — recovery is is_idr-gated, not is_key-gated.
            resumed_len = Some(pkt.len());
            break;
        }
    }
    assert_eq!(
        resumed_len,
        Some(H264_IDR_LEN),
        "recovery resumed copy at the IDR AU (len {H264_IDR_LEN}), \
         not the is_key non-IDR (len {H264_NON_IDR_LEN})"
    );
}

/// (c′) Recovery NEVER resumes on an `is_key`-flagged NON-IDR. If the only
/// post-outage input is a container-keyframe-flagged recovery-point I-frame (no
/// true IDR ever arrives), the seam must keep emitting slate forever — it must
/// never mistake `AV_PKT_FLAG_KEY` for a strict RAP and re-enter on garbage.
#[test]
fn recovery_never_resumes_on_is_key_non_idr() {
    let clock = Arc::new(ManualClock::new());
    // One healthy packet, then ONLY is_key-flagged non-IDR slices forever after.
    let live = FakeLive::new(vec![
        video_packet(&h264_idr_au(), 0, 0, true),
        video_packet(&h264_non_idr_au(), 1000, 1000, true),
        video_packet(&h264_non_idr_au(), 1100, 1100, true),
        video_packet(&h264_non_idr_au(), 1200, 1200, true),
        video_packet(&h264_non_idr_au(), 1300, 1300, true),
    ]);
    let mut src = guarded(live, tiny_slate(), Arc::clone(&clock));

    // Prime + splice.
    let _ = src.next_packet().expect("pull").expect("healthy");
    clock.set(SEGMENT_NS);

    // Every subsequent pull consumes an is_key non-IDR (discarded) and emits
    // slate. The mode must stay SLATE — re-entry never triggers on is_key.
    let mut last = i64::MIN;
    for _ in 0..10 {
        let pkt = src.next_packet().expect("pull").expect("slate");
        assert_eq!(
            src.mode(),
            GuardMode::Slate,
            "is_key non-IDR must NOT resume copy"
        );
        let dts = pkt.dts().expect("dts");
        assert!(dts > last, "slate stays monotonic: {dts} > {last}");
        last = dts;
    }
}

/// (d) Long outage: the slate loops, advancing the restamp offset per wrap, with
/// no DTS discontinuity (no repeat, no decrease) across many loop wraps.
#[test]
fn long_outage_loops_slate_without_dts_discontinuity() {
    let clock = Arc::new(ManualClock::new());
    let live = FakeLive::new(vec![video_packet(&h264_idr_au(), 0, 0, true)]);
    let slate = tiny_slate();
    let slate_len = slate.video().len();
    assert!(slate_len >= 1, "slate has at least one video packet");
    let mut src = guarded(live, slate, Arc::clone(&clock));

    // Prime + splice.
    let _ = src.next_packet().expect("pull").expect("live");
    clock.set(SEGMENT_NS);

    // Pull many slate packets — several full loop wraps — and assert strict DTS
    // monotonicity throughout (the offset advances per wrap, never repeats).
    let pulls = slate_len * 5 + 3;
    let mut last = i64::MIN;
    for _ in 0..pulls {
        let pkt = src
            .next_packet()
            .expect("pull")
            .expect("slate keeps looping");
        assert_eq!(src.mode(), GuardMode::Slate);
        let dts = pkt.dts().expect("slate dts");
        assert!(
            dts > last,
            "slate loop DTS strictly increasing across wraps: {dts} > {last}"
        );
        last = dts;
    }
}

/// (e) Fail-safe: a watchdog that has never recorded a packet classifies as
/// SPLICE, never a false LIVE — so a guarded source biases to slate before any
/// byte arrives.
#[test]
fn failsafe_stale_liveness_biases_to_slate() {
    let clock = Arc::new(ManualClock::new());
    // The input yields nothing at all: the watchdog never records a packet.
    let live = FakeLive::new(vec![]);
    let mut src = guarded(live, tiny_slate(), Arc::clone(&clock));

    // Even at t=0 (no elapsed), "no packet ever recorded" is fail-safe SPLICE.
    let pkt = src
        .next_packet()
        .expect("pull")
        .expect("slate emitted on the fail-safe path");
    assert_eq!(
        src.mode(),
        GuardMode::Slate,
        "never-LIVE before any packet: fail-safe to SLATE"
    );
    assert_eq!(pkt.kind(), StreamKind::Video);
}

/// The audio slate is spliced alongside the video on loss (the audio stream is
/// independent): with an audio watchdog that has never recorded, the guarded
/// source emits both the slate video AND the slate audio AUs.
#[test]
fn audio_slate_emitted_alongside_video_on_loss() {
    let clock = Arc::new(ManualClock::new());
    let live = FakeLive::new(vec![]);
    let slate = tiny_slate_with_audio();
    assert!(
        slate.audio().is_some_and(|a| !a.is_empty()),
        "the fixture has baked audio AUs"
    );
    let video_liveness = Arc::new(PacketLiveness::new(thresholds()));
    let audio_liveness = Arc::new(PacketLiveness::new(thresholds()));
    let mut src = GuardedPacketSource::new(
        live,
        slate,
        video_liveness,
        Some(audio_liveness),
        Arc::clone(&clock),
        h264_config(),
    );

    let mut saw_video = false;
    let mut saw_audio = false;
    for _ in 0..32 {
        let pkt = src.next_packet().expect("pull").expect("slate");
        match pkt.kind() {
            StreamKind::Video => saw_video = true,
            StreamKind::Audio => saw_audio = true,
            _ => {}
        }
        if saw_video && saw_audio {
            break;
        }
    }
    assert!(saw_video, "slate video AUs emitted on loss");
    assert!(saw_audio, "slate audio AUs emitted on loss");
}
