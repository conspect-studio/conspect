//! LIVE end-to-end NDI **output roundtrip** (OUT-4b / NDI-L2): send a composited
//! NV12 canvas through the safe [`NdiOutput`] sink (NV12→UYVY at the host-copy
//! boundary + a tick-derived timecode) and receive it back through an in-process
//! NDI [`NdiReceiver`], proving the bytes survived the conversion AND that the
//! send path never blocks the producer (invariant #10).
//!
//! `#[cfg(feature = "ndi-bindings")]` — needs the build-time `bindgen` over the
//! licensed SDK header. `#[ignore]` — needs (a) a resolvable NDI 6 runtime and
//! (b) acceptance of the proprietary SDK license, neither of which exists in CI
//! or this sandbox. Run on the SDK-equipped box:
//!
//! ```text
//! cargo test -p multiview-output --features ndi-bindings --test ndi_output_roundtrip \
//!   -- --ignored --nocapture
//! ```
#![cfg(feature = "ndi-bindings")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use multiview_ndi_sys::{NdiFinder, NdiReceiver, RecvFourCc};
use multiview_output::ndi::license::LicenseAcceptance;
use multiview_output::ndi::{
    nv12_to_uyvy, NdiCapability, NdiLicense, NdiOutput, Nv12Canvas, SdkNdiApi,
};

/// Build a `w`x`h` NV12 canvas whose Y plane is a deterministic per-pixel ramp
/// (so the received UYVY can be matched byte-for-byte against the reference
/// conversion — "content-aware ramp survival"). Chroma is a constant mid-grey.
fn ramp_nv12(w: u32, h: u32) -> (Vec<u8>, Vec<u8>) {
    let y: Vec<u8> = (0..(w * h))
        .map(|i| u8::try_from(i % 251).unwrap_or(0)) // 251: a prime ramp, never all-equal
        .collect();
    let uv = vec![128u8; usize::try_from(w * h / 2).unwrap()];
    (y, uv)
}

#[test]
#[ignore = "requires a resolvable proprietary NDI runtime + license acceptance (live, on the SDK box)"]
fn ndi_output_roundtrip_survives_the_nv12_to_uyvy_conversion() {
    const W: u32 = 64;
    const H: u32 = 32;
    let source_name = format!("Multiview Roundtrip {}", std::process::id());

    // Load the runtime ONCE; both the sender (via SdkNdiApi) and the receiver/
    // finder share its function table.
    let capability =
        NdiCapability::load().expect("an NDI runtime must be resolvable for the live roundtrip");
    let api_table = capability.runtime().api_table();
    let license = NdiLicense::accept(LicenseAcceptance {
        accepted_by: "roundtrip-test".to_owned(),
        accepted_at: "2026-06-15T00:00:00Z".to_owned(),
    })
    .expect("a complete acceptance yields an accepted license");

    // The reference UYVY the receiver must observe (the exact host-copy bytes the
    // seam produces for this ramp canvas).
    let (y, uv) = ramp_nv12(W, H);
    let canvas = Nv12Canvas::new(W, H, &y, &uv).expect("ramp canvas is valid geometry");
    let expected_uyvy = nv12_to_uyvy(&canvas);

    // The sender runs on its own thread, pushing the canvas continuously (an NDI
    // source must keep sending to be discoverable + deliver). It also re-asserts
    // invariant #10: every `send_canvas` returns promptly — the producer never
    // blocks on a (possibly absent) receiver. We record the worst-case single-send
    // latency and the count, and stop on the flag.
    let stop = Arc::new(AtomicBool::new(false));
    let sent = Arc::new(AtomicU64::new(0));
    let worst_send_micros = Arc::new(AtomicU64::new(0));
    let sender = {
        let stop = Arc::clone(&stop);
        let sent = Arc::clone(&sent);
        let worst = Arc::clone(&worst_send_micros);
        let source_name = source_name.clone();
        std::thread::spawn(move || {
            let (y, uv) = ramp_nv12(W, H);
            let mut out = NdiOutput::new(license, SdkNdiApi::new(capability), source_name)
                .expect("the NDI sender is created");
            let mut tick: u64 = 0;
            while !stop.load(Ordering::Acquire) {
                let canvas = Nv12Canvas::new(W, H, &y, &uv).expect("canvas valid");
                // Tick-derived 100 ns timecode @ 25 fps (invariant #3): 400_000/frame.
                let timecode = i64::try_from(tick).unwrap_or(0) * 400_000;
                let t0 = Instant::now();
                out.send_canvas(&canvas, timecode, 25, 1)
                    .expect("a live send succeeds");
                let micros = u64::try_from(t0.elapsed().as_micros()).unwrap_or(u64::MAX);
                worst.fetch_max(micros, Ordering::AcqRel);
                sent.fetch_add(1, Ordering::AcqRel);
                tick += 1;
                std::thread::sleep(Duration::from_millis(40)); // ~25 fps cadence
            }
            out.close();
        })
    };

    // Discover our own in-process source (show_local_sources = true), then connect
    // a receiver and capture a video frame back.
    let finder = NdiFinder::create(api_table, true).expect("the finder is created");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut connected: Option<NdiReceiver> = None;
    while Instant::now() < deadline && connected.is_none() {
        for src in finder.current_sources() {
            if src.as_str().contains(&source_name) {
                connected = Some(
                    NdiReceiver::create(api_table, &src.as_str(), Some("Multiview RT Receiver"))
                        .expect("the receiver connects to our source"),
                );
                break;
            }
        }
        if connected.is_none() {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    let receiver = connected.expect("our in-process NDI source must be discovered within 10s");

    // Capture a video frame and assert the ramp survived the NV12→UYVY conversion.
    let capture_deadline = Instant::now() + Duration::from_secs(10);
    let mut matched = false;
    while Instant::now() < capture_deadline && !matched {
        if let Some(frame) = receiver.capture_video(200).expect("capture never errors") {
            assert_eq!(frame.width(), W, "received width matches the sent canvas");
            assert_eq!(frame.height(), H, "received height matches the sent canvas");
            assert_eq!(frame.fourcc(), RecvFourCc::Uyvy, "low-latency UYVY packing");
            // Content-aware ramp survival: each received row's first `W*2` bytes must
            // equal the reference UYVY row (the SDK stride may exceed `W*2`).
            let stride = usize::try_from(frame.stride()).unwrap_or(0);
            let row_bytes = usize::try_from(W).unwrap_or(0) * 2;
            let data = frame.data();
            if stride >= row_bytes && data.len() >= stride * usize::try_from(H).unwrap_or(0) {
                let mut ok = true;
                for row in 0..usize::try_from(H).unwrap_or(0) {
                    let got = &data[row * stride..row * stride + row_bytes];
                    let want = &expected_uyvy[row * row_bytes..(row + 1) * row_bytes];
                    if got != want {
                        ok = false;
                        break;
                    }
                }
                matched = ok;
            }
        }
    }

    stop.store(true, Ordering::Release);
    sender.join().expect("the sender thread joins cleanly");

    assert!(
        matched,
        "the sent ramp canvas must be received intact as UYVY"
    );
    assert!(
        sent.load(Ordering::Acquire) > 0,
        "the sender must have published frames"
    );
    // Invariant #10: no single `send_canvas` blocked the producer. A live SDK send
    // is a host-memory queue push (sub-millisecond); allow generous slack for a
    // loaded CI box but prove it is not an unbounded stall.
    let worst = worst_send_micros.load(Ordering::Acquire);
    assert!(
        worst < 500_000,
        "a single NDI send took {worst}us — the producer must never block (invariant #10)"
    );
}
