//! The guarded-passthrough / transcode program's slate honours the configurable
//! failover policy (ADR-0030 §4): the program-level `on_loss` choice selects
//! which picture the pre-baked GP-4 slate displays on input loss — `SmpteBars`
//! (the operator's "back to bars"), `NoSignal` (the signal-lost card), or
//! `Black`. The single config field drives a passthrough program identically to
//! a layout tile ("configurable the same way").
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::time::Rational;
use multiview_ffmpeg::EncodedPacket;
use multiview_output::slate::{
    BakedSlate, SlateBaker, SlateKind, SlateSpec, SlateVideoCodec, SlateVideoSpec,
};

/// A small `mpeg2video` slate spec of the given picture (LGPL-clean).
fn spec_for(kind: SlateKind) -> SlateSpec {
    SlateSpec {
        kind,
        video: SlateVideoSpec {
            codec: SlateVideoCodec::Mpeg2Video,
            width: 320,
            height: 240,
            cadence: Rational::FPS_30,
            gop: 15,
        },
        audio: None,
    }
}

fn total_video_bytes(slate: &BakedSlate) -> usize {
    slate.video().iter().map(EncodedPacket::len).sum()
}

#[test]
fn each_failover_picture_bakes_a_distinct_idr_led_slate() {
    // All three failover pictures are bakeable program-level slates; each is
    // IDR-led, > 0 packets, and records the picture it baked so the splice (and
    // the program badge) knows what the program will show on loss.
    for kind in [SlateKind::Black, SlateKind::SmpteBars, SlateKind::NoSignal] {
        let slate = SlateBaker::bake_slate(&spec_for(kind)).expect("bake the failover slate");
        assert!(
            !slate.video().is_empty(),
            "{kind:?} slate produced no video"
        );
        assert!(
            slate.video()[0].is_keyframe(),
            "{kind:?} slate's first packet must be a keyframe (closed-GOP IDR)"
        );
        assert_eq!(
            slate.params().kind,
            kind,
            "the baked slate records its failover picture ({kind:?})"
        );
    }
}

#[test]
fn bars_policy_bakes_a_heavier_slate_than_black() {
    // The bars policy ("back to bars") is a multi-band picture; a flat black card
    // encodes to far fewer bytes. This proves the program-level on_loss choice
    // actually changes WHAT is baked (not a dead option that always bakes black).
    let black = SlateBaker::bake_slate(&spec_for(SlateKind::Black)).unwrap();
    let bars = SlateBaker::bake_slate(&spec_for(SlateKind::SmpteBars)).unwrap();
    let nosignal = SlateBaker::bake_slate(&spec_for(SlateKind::NoSignal)).unwrap();

    assert!(
        total_video_bytes(&bars) > total_video_bytes(&black),
        "the bars slate ({} B) must encode heavier than flat black ({} B)",
        total_video_bytes(&bars),
        total_video_bytes(&black),
    );
    // The NoSignal card is a distinct picture from black, so its bake differs.
    assert_ne!(
        total_video_bytes(&nosignal),
        total_video_bytes(&black),
        "the NoSignal card is a distinct picture from black",
    );
}
