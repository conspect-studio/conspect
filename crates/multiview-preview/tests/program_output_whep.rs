//! PRV-5 — the **PROGRAM-output WebRTC focus path** (behind the off-by-default
//! `webrtc` feature): the program-canvas tap → preview encode → drop-oldest
//! sample-feed wiring, plus the `program`-scope focus session lifecycle.
//!
//! These exercise the **seam** with injected program frames + an in-memory fake
//! transport — no GPU blit, no real H.264 encode, no socket. The live media
//! egress (the native str0m H.264 path) lives in the `multiview-webrtc` crate's
//! `native` WHEP egress transport (ADR-0048 / ADR-P006) and is intentionally not
//! exercised here.
//!
//! What is asserted (the testable core of PRV-5 §Acceptance, program scope):
//! * conditional tap — **0 subscribers performs no blit** (ADR-P003); the first
//!   subscriber lazily starts it; the last leave auto-stops it;
//! * the program tap **samples** the canvas slot and never back-pressures the
//!   publisher (inv #1/#10) — a dead focus consumer cannot stall the source;
//! * tap → encode → bounded **drop-oldest** sample feed (the encoder pushes,
//!   never blocks; the transport drains lossily);
//! * the `program` focus session is **always** labeled `PRE-ENCODE CANVAS
//!   APPROX` (ADR-P005 — a program focus is the pre-encode canvas, never the
//!   real encoded bitstream); and
//! * the session lifecycle Created → … → Closed, with `Drop` freeing both the
//!   focus-cap slot and the tap lease (auto-stop).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
#![cfg(feature = "webrtc")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use multiview_engine::isolation::event_stream;
use multiview_preview::whep::program::{
    FidelityLabel, IdentityPreviewEncoder, ProgramFocusSource, ProgramFrame, ProgramTap,
};
use multiview_preview::whep::transport::{PreviewMediaSource, SampleFeed, SampleKind};
use multiview_preview::whep::PreviewCodec;
use multiview_preview::{FocusCaps, FocusGate};

/// A 4x2 grey NV12 program frame at the given 90 kHz timestamp.
fn frame(rtp_ts: u32) -> ProgramFrame {
    // 4*2 luma + 4*2/2 chroma = 8 + 4 = 12 bytes.
    let mut plane = vec![128u8; 12];
    plane[0] = 200; // a distinguishing luma sample so the encoder has content.
    ProgramFrame::new(4, 2, plane, rtp_ts)
}

#[tokio::test]
async fn program_tap_does_no_blit_until_first_subscriber() {
    // ADR-P003 conditional tap: the downscale blit (modeled here as the `start`
    // callback) runs ONLY when the first subscriber arrives, and is torn down on
    // the last leave — cost is ~zero while nobody watches.
    let (canvas, _seed) = event_stream::<ProgramFrame>(4);
    let blits = Arc::new(AtomicUsize::new(0));
    let teardowns = Arc::new(AtomicUsize::new(0));

    let tap = ProgramTap::new();
    assert_eq!(tap.subscriber_count(), 0);
    assert_eq!(blits.load(Ordering::SeqCst), 0, "no blit before any viewer");

    let starts = Arc::clone(&blits);
    let stops = Arc::clone(&teardowns);
    let up = canvas.clone();
    let lease = tap
        .subscribe(move || {
            // The "GPU downscale blit" the program tap appends — counted so the
            // test can prove it ran exactly once, lazily.
            starts.fetch_add(1, Ordering::SeqCst);
            let stops = Arc::clone(&stops);
            (up.subscribe(), move || {
                stops.fetch_add(1, Ordering::SeqCst);
            })
        })
        .expect("first subscribe starts the program tap");
    assert_eq!(blits.load(Ordering::SeqCst), 1, "first viewer starts blit");
    assert_eq!(tap.subscriber_count(), 1);
    assert_eq!(teardowns.load(Ordering::SeqCst), 0);

    // Last leave auto-stops the blit (ADR-P003 idle-cost).
    drop(lease);
    assert_eq!(tap.subscriber_count(), 0);
    assert_eq!(
        teardowns.load(Ordering::SeqCst),
        1,
        "last leave tears the blit down"
    );
}

#[tokio::test]
async fn program_focus_pumps_canvas_through_encode_into_drop_oldest_feed() {
    // tap → encode → bounded sample feed. The injected `IdentityPreviewEncoder`
    // stands in for the (gated) H.264 encode; the assertion is the WIRING: each
    // sampled program frame becomes exactly one encoded sample in the feed.
    let (canvas, _seed) = event_stream::<ProgramFrame>(8);
    let tap = ProgramTap::new();
    let up = canvas.clone();
    let lease = tap
        .subscribe(move || (up.subscribe(), || {}))
        .expect("subscribe");

    // depth-2 drop-oldest feed (ADR-P001 shallow ring).
    let source = ProgramFocusSource::new(lease, IdentityPreviewEncoder::new(PreviewCodec::H264), 2);
    let feed: SampleFeed = source.feed();
    assert_eq!(source.codec(), PreviewCodec::H264);

    // Publish two canvas frames; pump them through the encoder into the feed.
    canvas.publish(frame(0));
    canvas.publish(frame(900));
    let n = source.pump_available();
    assert_eq!(n, 2, "both sampled frames are encoded into the feed");

    let s0 = feed.pop().expect("first encoded sample");
    assert_eq!(s0.rtp_timestamp, 0);
    assert!(s0.keyframe, "first program sample is a keyframe");
    let s1 = feed.pop().expect("second encoded sample");
    assert_eq!(s1.rtp_timestamp, 900);
    assert!(feed.pop().is_none());
}

#[tokio::test]
async fn program_focus_feed_drops_oldest_never_blocks_on_slow_transport() {
    // inv #1 + #10: the program tap SAMPLES the canvas and the encode→feed leg is
    // drop-oldest. Pushing far more frames than the feed depth must never block;
    // the OLDEST encoded samples are evicted and the publisher is never stalled.
    let (canvas, _seed) = event_stream::<ProgramFrame>(64);
    let tap = ProgramTap::new();
    let up = canvas.clone();
    let lease = tap
        .subscribe(move || (up.subscribe(), || {}))
        .expect("subscribe");
    let source = ProgramFocusSource::new(lease, IdentityPreviewEncoder::new(PreviewCodec::H264), 2);
    let feed = source.feed();

    // Publishing the canvas must never block on the preview path.
    let published = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        for ts in 0..1_000u32 {
            let _ = canvas.publish(frame(ts.saturating_mul(900)));
        }
        true
    })
    .await
    .expect("canvas publish never blocks on a slow preview consumer");
    assert!(published);

    // Pump everything available; the feed stays bounded at its depth and reports
    // drops (it never grows, never blocks).
    let _ = source.pump_available();
    assert!(feed.buffered() <= 2, "feed stays bounded at its depth");
    assert!(
        feed.dropped() > 0,
        "a lagging feed drops the oldest samples"
    );
}

#[tokio::test]
async fn program_focus_source_emits_video_samples_and_carries_no_audio_feed() {
    // ADR-P006: the canvas-tap program source is video-only — its samples are
    // tagged `SampleKind::Video` (90 kHz RTP clock) and its audio feed is the
    // trait default `None` (program audio rides the shared program Opus
    // rendition of ADR-0049, a different source, not this canvas tap).
    let (canvas, _seed) = event_stream::<ProgramFrame>(4);
    let tap = ProgramTap::new();
    let up = canvas.clone();
    let lease = tap
        .subscribe(move || (up.subscribe(), || {}))
        .expect("subscribe");
    let source = ProgramFocusSource::new(lease, IdentityPreviewEncoder::new(PreviewCodec::H264), 2);

    assert!(
        source.audio_feed().is_none(),
        "the canvas-tap program source has no audio feed"
    );

    let feed = source.feed();
    canvas.publish(frame(0));
    assert_eq!(source.pump_available(), 1);
    let sample = feed.pop().expect("one encoded sample");
    assert_eq!(
        sample.kind,
        SampleKind::Video,
        "canvas samples ride the 90 kHz video clock"
    );
}

#[tokio::test]
async fn program_focus_is_always_labeled_pre_encode_canvas_approx() {
    // ADR-P005: a PROGRAM focus is the pre-encode canvas downscale — it can NEVER
    // be labeled REAL ENCODED OUTPUT (that label is reserved for an OUTPUT tap of
    // a real encoded rendition). The label is non-negotiable and on-video.
    let label = FidelityLabel::program();
    assert_eq!(label, FidelityLabel::PreEncodeCanvasApprox);
    assert_eq!(label.as_str(), "PRE-ENCODE CANVAS APPROX");
    assert!(!label.is_real_encoded());
}

#[tokio::test]
async fn program_focus_session_lifecycle_and_drop_frees_cap_and_tap() {
    // The `program`-scope focus session ties a FocusGate cap lease to the tap
    // lease + the transport session lifecycle. Dropping the session frees BOTH
    // the cap slot AND auto-stops the tap (last-leave teardown).
    let (canvas, _seed) = event_stream::<ProgramFrame>(4);
    let tap = ProgramTap::new();
    let blits = Arc::new(AtomicUsize::new(0));
    let teardowns = Arc::new(AtomicUsize::new(0));

    let gate: FocusGate<String> = FocusGate::new(FocusCaps::new(1, 1));
    assert_eq!(gate.active(), 0);

    let starts = Arc::clone(&blits);
    let stops = Arc::clone(&teardowns);
    let up = canvas.clone();
    let lease = tap
        .subscribe(move || {
            starts.fetch_add(1, Ordering::SeqCst);
            let stops = Arc::clone(&stops);
            (up.subscribe(), move || {
                stops.fetch_add(1, Ordering::SeqCst);
            })
        })
        .expect("subscribe");

    let cap = gate
        .try_acquire("program".to_owned())
        .expect("program focus admitted");
    let source = ProgramFocusSource::new(lease, IdentityPreviewEncoder::new(PreviewCodec::H264), 2);

    let session = multiview_preview::whep::program::ProgramFocusSession::new(cap, source);
    assert_eq!(session.label(), FidelityLabel::PreEncodeCanvasApprox);
    assert_eq!(gate.active(), 1, "the focus cap slot is held");
    assert_eq!(tap.subscriber_count(), 1, "the program tap is running");
    assert_eq!(blits.load(Ordering::SeqCst), 1);

    // While capacity is held a second program focus is refused (cap of 1).
    assert!(gate.try_acquire("program".to_owned()).is_err());

    // Dropping the session frees the cap and auto-stops the tap.
    drop(session);
    assert_eq!(gate.active(), 0, "dropping the session frees the cap slot");
    assert_eq!(tap.subscriber_count(), 0, "tap auto-stops on last leave");
    assert_eq!(
        teardowns.load(Ordering::SeqCst),
        1,
        "the blit was torn down"
    );
}
