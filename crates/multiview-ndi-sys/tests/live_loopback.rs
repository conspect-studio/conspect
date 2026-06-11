//! Live NDI **loopback** test (NDI-L1 capstone): send → discover → receive in one
//! process. A real [`NdiSender`] publishes a UYVY gradient; an [`NdiFinder`]
//! discovers the local source; an [`NdiReceiver`] connects and captures it back.
//! This exercises the whole binding against the real licensed SDK: send +
//! `find_get_current_sources` + `recv_create` + `recv_capture` + `recv_free`.
//!
//! NDI carries video over `SpeedHQ` (visually lossless, **not** bit-exact), so the
//! content assertion is structural — geometry + mean luma in range — exactly the
//! GPU/codec testing tier (SSIM/PSNR thresholds, never bit-exact).
//!
//! `#[cfg(feature = "bindings")]` + `#[ignore]`: needs the SDK header at build time
//! and a resolvable NDI runtime at run time. Run on the SDK-equipped box:
//!
//! ```text
//! cargo test -p multiview-ndi-sys --features bindings --test live_loopback -- --ignored --nocapture
//! ```
#![cfg(feature = "bindings")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing
)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use multiview_ndi_sys::{NdiFinder, NdiReceiver, NdiSender, NdiVideoFourCc, RecvFourCc};

const SOURCE_NAME: &str = "Multiview NDI-L1 Loopback";
const W: u32 = 64;
const H: u32 = 64;

/// Build a UYVY frame whose luma is a vertical gradient (mean ≈ 127), chroma
/// neutral (128). Definitely-not-flat content with a known mean.
fn gradient_uyvy() -> Vec<u8> {
    let stride = (W * 2) as usize;
    let mut buf = vec![0u8; stride * H as usize];
    for row in 0..H as usize {
        let y = u8::try_from(row * 255 / (H as usize - 1)).unwrap_or(0);
        for px2 in 0..(W as usize / 2) {
            let base = row * stride + px2 * 4;
            buf[base] = 128; // U
            buf[base + 1] = y; // Y0
            buf[base + 2] = 128; // V
            buf[base + 3] = y; // Y1
        }
    }
    buf
}

/// Mean of the luma (odd) bytes within the active `W*2` region of each row.
fn mean_luma(data: &[u8], stride: u32) -> f64 {
    let stride = stride as usize;
    let row_active = (W * 2) as usize;
    let mut sum = 0u64;
    let mut n = 0u64;
    for row in 0..H as usize {
        let row_base = row * stride;
        let mut i = 1; // Y0 is at offset 1 in each UYVY quad
        while i < row_active {
            if let Some(&b) = data.get(row_base + i) {
                sum += u64::from(b);
                n += 1;
            }
            i += 2;
        }
    }
    if n == 0 {
        0.0
    } else {
        // n and sum are small; the f64 conversion is exact for these magnitudes.
        sum as f64 / n as f64
    }
}

/// What a received frame is reduced to for assertions (geometry + classified
/// packing + a luma proxy + buffer length) — all copied out before the SDK frame
/// drops.
type RecvSummary = (u32, u32, RecvFourCc, u32, f64, usize);

#[test]
#[ignore = "needs a resolvable NDI runtime + discovery (mDNS or NDI_DISCOVERY_SERVER)"]
fn live_loopback_send_discover_receive() {
    // `runtime` is declared first so it drops LAST — the Library stays mapped until
    // after we join the sender thread, so the thread never calls an unmapped fn.
    let runtime = NdiRuntimeHandle::load();

    // Sender on a background thread keeps the source live while we discover + receive.
    let sender = NdiSender::create(runtime.table(), SOURCE_NAME, false, false)
        .expect("the runtime should create a sender");
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
        // `sender` drops here (NDIlib_send_destroy) when the thread ends.
    });

    // Do ALL fallible work in a closure that returns an outcome — never panicking —
    // so the sender thread is ALWAYS stopped + joined before any assertion (a panic
    // mid-flight would drop the runtime under the still-running thread → UB).
    let outcome: Result<RecvSummary, String> = (|| {
        let finder = NdiFinder::create(runtime.table(), true).map_err(|e| e.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut source = None;
        while Instant::now() < deadline {
            let sources = finder.current_sources();
            if let Some(src) = sources.iter().find(|s| s.as_str().contains(SOURCE_NAME)) {
                source = Some(src.clone());
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let source =
            source.ok_or_else(|| "the local sender was not discovered in 15s".to_owned())?;
        println!("discovered NDI source: {}", source.as_str());

        let receiver = NdiReceiver::create(
            runtime.table(),
            &source.as_str(),
            Some("Multiview Loopback Recv"),
        )
        .map_err(|e| e.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Some(frame) = receiver.capture_video(200).map_err(|e| e.to_string())? {
                return Ok((
                    frame.width(),
                    frame.height(),
                    frame.fourcc(),
                    frame.stride(),
                    mean_luma(frame.data(), frame.stride()),
                    frame.data().len(),
                ));
            }
        }
        Err("no video frame arrived in 10s".to_owned())
    })();

    // Always stop + join before asserting / dropping the runtime.
    stop.store(true, Ordering::Relaxed);
    send_thread.join().expect("sender thread joins");

    let (rw, rh, rfourcc, rstride, luma, len) = match outcome {
        Ok(summary) => summary,
        Err(e) => panic!("NDI loopback failed: {e}"),
    };
    println!("received {rw}x{rh} {rfourcc:?} stride={rstride} len={len} mean_luma={luma:.1}");
    assert_eq!(rw, W, "received width matches sent");
    assert_eq!(rh, H, "received height matches sent");
    assert_eq!(rfourcc, RecvFourCc::Uyvy, "opaque UYVY round-trips as UYVY");
    assert!(len > 0, "received a non-empty buffer");
    // Sent mean luma ≈ 127; SpeedHQ is visually lossless, so a wide-but-real band.
    assert!(
        (80.0..=175.0).contains(&luma),
        "received luma {luma:.1} reflects the sent gradient (not black/white)"
    );
}

/// Audio summary copied out before the SDK frame drops: rate, channels, FLTP?,
/// first sample value (a content proxy), and samples-this-frame.
type AudioSummary = (u32, u32, bool, f32, u32);

const A_RATE: u32 = 48_000;
const A_CHANNELS: u32 = 2;
const A_SAMPLES: u32 = 480; // 10 ms @ 48 kHz
const A_AMPLITUDE: f32 = 0.5;

/// Planar FLTP buffer: `A_CHANNELS` planes of `A_SAMPLES` f32, all `A_AMPLITUDE`.
fn constant_fltp() -> Vec<f32> {
    vec![A_AMPLITUDE; (A_CHANNELS * A_SAMPLES) as usize]
}

#[test]
#[ignore = "needs a resolvable NDI runtime + discovery (mDNS or NDI discovery server)"]
fn live_loopback_audio_round_trips() {
    const AUDIO_SOURCE: &str = "Multiview NDI-L1 Audio Loopback";
    let runtime = NdiRuntimeHandle::load();

    // Sender sends both a video keep-alive (discoverability) and the audio under test.
    let sender = NdiSender::create(runtime.table(), AUDIO_SOURCE, false, false)
        .expect("the runtime should create a sender");
    let stop = Arc::new(AtomicBool::new(false));
    let stop_tx = Arc::clone(&stop);
    let send_thread = std::thread::spawn(move || {
        let video = gradient_uyvy();
        let audio = constant_fltp();
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
                &video,
            );
            let _ = sender.send_audio(A_RATE, A_CHANNELS, A_SAMPLES, tick * 33_366, &audio);
            tick += 1;
            std::thread::sleep(Duration::from_millis(10));
        }
    });

    let outcome: Result<AudioSummary, String> = (|| {
        let finder = NdiFinder::create(runtime.table(), true).map_err(|e| e.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut source = None;
        while Instant::now() < deadline {
            if let Some(src) = finder
                .current_sources()
                .iter()
                .find(|s| s.as_str().contains(AUDIO_SOURCE))
            {
                source = Some(src.clone());
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let source = source.ok_or_else(|| "audio sender not discovered in 15s".to_owned())?;
        println!("discovered NDI source: {}", source.as_str());

        let receiver = NdiReceiver::create(
            runtime.table(),
            &source.as_str(),
            Some("Multiview Audio Recv"),
        )
        .map_err(|e| e.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Some(frame) = receiver.capture_audio(200).map_err(|e| e.to_string())? {
                let bytes = frame.data_bytes();
                // First f32 of plane 0 (channel 0) — native-endian per the SDK.
                let first = bytes
                    .get(0..4)
                    .map_or(f32::NAN, |b| f32::from_ne_bytes([b[0], b[1], b[2], b[3]]));
                return Ok((
                    frame.sample_rate(),
                    frame.channels(),
                    frame.is_fltp(),
                    first,
                    frame.samples(),
                ));
            }
        }
        Err("no audio frame arrived in 10s".to_owned())
    })();

    stop.store(true, Ordering::Relaxed);
    send_thread.join().expect("sender thread joins");

    let (rate, channels, fltp, first, samples) = match outcome {
        Ok(s) => s,
        Err(e) => panic!("NDI audio loopback failed: {e}"),
    };
    println!("received audio: {rate} Hz, {channels} ch, fltp={fltp}, samples={samples}, first={first:.4}");
    assert_eq!(rate, A_RATE, "sample rate round-trips");
    assert_eq!(channels, A_CHANNELS, "channel count round-trips");
    assert!(fltp, "NDI delivers planar float (FLTP)");
    assert!(samples > 0, "a non-empty audio frame");
    // NDI Full audio is lossless float, so the constant amplitude round-trips closely.
    assert!(
        (first - A_AMPLITUDE).abs() < 0.01,
        "received sample {first:.4} ~= sent {A_AMPLITUDE}"
    );
}

/// Tiny owner that keeps the loaded runtime alive for the test's duration and
/// hands out its `Copy` table — so the sender thread + finder + receiver all
/// resolve against a runtime that outlives them (we join the thread before drop).
struct NdiRuntimeHandle {
    runtime: multiview_ndi_sys::NdiRuntime,
}

impl NdiRuntimeHandle {
    fn load() -> Self {
        Self {
            runtime: multiview_ndi_sys::NdiRuntime::load()
                .expect("an NDI runtime should be resolvable on this host"),
        }
    }
    fn table(&self) -> multiview_ndi_sys::NdiApiTable {
        self.runtime.api_table()
    }
}
