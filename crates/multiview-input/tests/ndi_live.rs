//! IN-3 — NDI receive proofs.
//!
//! Two layers:
//! - [`probe_never_panics_and_reports_a_typed_status`] runs **in CI** (under
//!   `--features ndi`, no runtime needed): the probe returns a typed status, never
//!   a panic/block.
//! - The `live_ingest` submodule (under `--features ndi-bindings`, `#[ignore]`d)
//!   drives the production [`NdiProducer`] over the real `SdkNdiReceiver` against
//!   the licensed SDK: a sys `NdiSender` publishes a UYVY gradient, an `NdiFinder`
//!   discovers it, and the producer yields an NV12 `ProducedFrame`. Run on the
//!   SDK-equipped box:
//!
//! ```text
//! cargo test -p multiview-input --features ndi-bindings --test ndi_live -- --ignored --nocapture
//! ```
#![cfg(feature = "ndi")]
#![allow(
    // reason: integration test; the strict workspace lints are relaxed for
    // `tests/` per CLAUDE.md.
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::as_conversions,
    clippy::cast_possible_truncation
)]

use multiview_input::ndi::NdiLoadStatus;

#[test]
fn probe_never_panics_and_reports_a_typed_status() {
    // The runtime-absent case is the CI default: the probe must return a typed
    // status (RuntimeNotFound / Unusable / Available) and never panic or block.
    // This runs in CI (it does not require the runtime to be present — it tolerates
    // either outcome).
    let status = multiview_input::ndi::NdiCapability::probe();
    // `is_available()` must agree with the variant: true exactly when `Available`.
    assert_eq!(
        status.is_available(),
        status == NdiLoadStatus::Available,
        "is_available() must be true exactly for the Available status",
    );
}

/// The live ingest round-trip — only built with the SDK-binding feature, since it
/// needs the sys-crate sender/finder + the real `SdkNdiReceiver`.
#[cfg(feature = "ndi-bindings")]
mod live_ingest {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use multiview_core::pixel::PixelFormat;
    use multiview_input::ndi::license::LicenseAcceptance;
    use multiview_input::ndi::{NdiCapability, NdiLicense, NdiProducer, SdkNdiReceiver};
    use multiview_input::source::FrameProducer;
    use multiview_ndi_sys::{NdiFinder, NdiRuntime, NdiSender, NdiVideoFourCc};

    const SOURCE_NAME: &str = "Multiview NDI-L1 Ingest";
    const W: u32 = 64;
    const H: u32 = 64;

    fn gradient_uyvy() -> Vec<u8> {
        let stride = (W * 2) as usize;
        let mut buf = vec![0u8; stride * H as usize];
        for row in 0..H as usize {
            let y = u8::try_from(row * 255 / (H as usize - 1)).unwrap_or(0);
            for px2 in 0..(W as usize / 2) {
                let base = row * stride + px2 * 4;
                buf[base] = 128;
                buf[base + 1] = y;
                buf[base + 2] = 128;
                buf[base + 3] = y;
            }
        }
        buf
    }

    /// An accepted NDI license guard for the gated `NdiProducer::new` (ADR-0008
    /// §7.5); this live round-trip exercises the receiver, not the gate.
    fn accepted() -> NdiLicense {
        NdiLicense::accept(LicenseAcceptance {
            accepted_by: "live-test".to_owned(),
            accepted_at: "2026-06-06T00:00:00Z".to_owned(),
        })
        .expect("a complete acceptance is accepted")
    }

    #[test]
    #[ignore = "needs a resolvable NDI runtime + discovery (mDNS or NDI discovery server)"]
    fn live_ingest_yields_nv12_produced_frames() {
        // Sender side: its own runtime, on a background thread that keeps the source up.
        let send_runtime = NdiRuntime::load().expect("an NDI runtime should be resolvable");
        let sender = NdiSender::create(send_runtime.api_table(), SOURCE_NAME, false, false)
            .expect("sender creates");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_tx = Arc::clone(&stop);
        let send_thread = std::thread::spawn(move || {
            let frame = gradient_uyvy();
            let mut tick = 0i64;
            while !stop_tx.load(Ordering::Relaxed) {
                let _ = sender.send_video(
                    W,
                    H,
                    W * 2,
                    NdiVideoFourCc::Uyvy,
                    30_000,
                    1_001,
                    tick * 33_366,
                    &frame,
                );
                tick += 1;
                std::thread::sleep(Duration::from_millis(33));
            }
        });

        // All fallible work in a closure so we ALWAYS stop + join before asserting.
        let outcome: Result<(u32, u32, PixelFormat, usize), String> = (|| {
            let finder =
                NdiFinder::create(send_runtime.api_table(), true).map_err(|e| e.to_string())?;
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut source = None;
            while Instant::now() < deadline {
                if let Some(src) = finder
                    .current_sources()
                    .iter()
                    .find(|s| s.as_str().contains(SOURCE_NAME))
                {
                    source = Some(src.clone());
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            let source = source.ok_or_else(|| "source not discovered in 15s".to_owned())?;
            println!("discovered NDI source: {}", source.as_str());

            // Receiver side: its own capability, driven through the production producer.
            let capability =
                NdiCapability::load().map_err(|s| format!("capability load: {s:?}"))?;
            let receiver =
                SdkNdiReceiver::connect(capability, &source.as_str()).map_err(|e| e.to_string())?;
            let mut producer = NdiProducer::new(accepted(), Box::new(receiver));

            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                match producer.next_frame().map_err(|e| e.to_string())? {
                    Some(frame) => {
                        return Ok((
                            frame.meta.width,
                            frame.meta.height,
                            frame.meta.format,
                            frame.pixels.len(),
                        ));
                    }
                    None => std::thread::sleep(Duration::from_millis(10)),
                }
            }
            Err("no NV12 frame produced in 10s".to_owned())
        })();

        stop.store(true, Ordering::Relaxed);
        send_thread.join().expect("sender thread joins");

        let (w, h, format, pixels) = match outcome {
            Ok(t) => t,
            Err(e) => panic!("NDI ingest failed: {e}"),
        };
        println!("produced {w}x{h} {format:?} pixels={pixels}");
        assert_eq!(w, W, "produced width matches the source");
        assert_eq!(h, H, "produced height matches the source");
        assert_eq!(
            format,
            PixelFormat::Nv12,
            "ingest converts to NV12 (inv #5)"
        );
        // NV12 is 1.5 bytes/px (Y + half-size interleaved UV).
        let expected = (W * H * 3 / 2) as usize;
        assert_eq!(pixels, expected, "NV12 payload is width*height*3/2 bytes");
    }
}
