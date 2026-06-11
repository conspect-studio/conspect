//! Live NDI **sender** test (NDI-L1): create a real sender over the resolved v6
//! table, push a sequence of UYVY frames, and tear it down — proving the
//! `NDIlib_send_create` / `NDIlib_send_send_video_v2` / `NDIlib_send_destroy` ABI
//! (struct layouts + resolved fn-pointer indices) against the real licensed SDK.
//!
//! `#[ignore]` — it needs a resolvable NDI runtime (`libndi_advanced.so.6` /
//! `libndi.so.6`), which CI does not have. Run on the SDK-equipped box:
//!
//! ```text
//! cargo test -p multiview-ndi-sys --features bindings --test live_send -- --ignored --nocapture
//! ```
#![cfg(feature = "bindings")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use multiview_ndi_sys::{NdiError, NdiRuntime, NdiSender, NdiVideoFourCc};

#[test]
#[ignore = "requires a resolvable NDI runtime (libndi_advanced.so.6 / libndi.so.6)"]
fn live_sender_creates_and_sends_frames() {
    let runtime = NdiRuntime::load().expect("an NDI runtime should be resolvable on this host");

    // A small UYVY canvas (4:2:2 packed = 2 bytes/px → stride = width * 2).
    let (width, height) = (64u32, 64u32);
    let stride = width * 2;
    let frame: Vec<u8> = (0..stride * height)
        .map(|i| u8::try_from(i % 256).unwrap())
        .collect();

    let sender = NdiSender::create(
        runtime.api_table(),
        "Multiview NDI-L1 Live Test",
        // clock_video / clock_audio = false: NDI never paces Multiview (inv #1/#3).
        false,
        false,
    )
    .expect("the runtime should create a sender");

    // Push 30 frames with tick-derived (synthesised here) timecodes at 30000/1001.
    for tick in 0..30i64 {
        sender
            .send_video(
                width,
                height,
                stride,
                NdiVideoFourCc::Uyvy,
                30_000,
                1_001,
                // NDI 100 ns units; monotone from a synthetic tick (stand-in for
                // the engine's tick counter — inv #3).
                tick * 33_366,
                &frame,
            )
            .expect("each synchronous send should be accepted");
    }
    println!("sent 30 UYVY frames through a live NDI sender ({width}x{height})");

    // A too-short buffer is a typed refusal, never a crash (inv #1).
    let short = vec![0u8; 16];
    match sender.send_video(
        width,
        height,
        stride,
        NdiVideoFourCc::Uyvy,
        30_000,
        1_001,
        0,
        &short,
    ) {
        Err(NdiError::ShortBuffer { .. }) => {}
        other => panic!("expected ShortBuffer refusal, got {other:?}"),
    }

    // An interior-NUL name is refused before any SDK handle is created.
    match NdiSender::create(runtime.api_table(), "bad\0name", false, false) {
        Err(NdiError::InvalidCString { .. }) => {}
        other => panic!("expected InvalidCString refusal, got {other:?}"),
    }

    // `sender` drops here → NDIlib_send_destroy, exactly once.
}
