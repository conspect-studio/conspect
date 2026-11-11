//! Review SRT-05: a custom-writer muxer obeys an operation-wide I/O deadline
//! and the stop flag, even when every single write callback SUCCEEDS but is
//! slow (a slow-but-steady peer). The deadline does not reset per callback.
#![cfg(feature = "ffmpeg")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ffmpeg::format::Pixel;
use ffmpeg_next as ffmpeg;
use multiview_core::time::Rational;
use multiview_ffmpeg::{Muxer, PacketWriter, VideoEncodeTarget, VideoEncoder};

fn make_writer(mut f: impl FnMut(&[u8]) -> bool + Send + 'static) -> PacketWriter {
    Box::new(move |data: &[u8], _budget: &multiview_ffmpeg::IoBudget<'_>| f(data))
}

fn new_muxer(stop: &Arc<AtomicBool>, writer: PacketWriter) -> Muxer {
    Muxer::create_with_writer("mpegts", 1316, Arc::clone(stop), writer).unwrap()
}

const SLOW: Duration = Duration::from_millis(40);

fn encoder() -> VideoEncoder {
    VideoEncoder::new(&VideoEncodeTarget {
        codec_name: "mpeg2video".to_owned(),
        width: 640,
        height: 360,
        format: Pixel::YUV420P,
        time_base: Rational::new(1, 25),
        bit_rate: 20_000_000,
        gop: 1,
        cuda_device: None,
    })
    .expect("mpeg2video")
}

/// One large intra packet: noise so it spans many 1316-byte callbacks.
fn big_packet(enc: &mut VideoEncoder) -> ffmpeg::codec::packet::Packet {
    let mut frame = ffmpeg::util::frame::Video::new(Pixel::YUV420P, 640, 360);
    let mut x: u32 = 0x1234_5678;
    for p in 0..frame.planes() {
        for byte in frame.data_mut(p).iter_mut() {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            *byte = x.to_le_bytes()[0];
        }
    }
    frame.set_pts(Some(0));
    enc.send_frame(&frame).unwrap();
    enc.send_eof().unwrap();
    let mut out = None;
    while let Some(p) = enc.receive_packet().unwrap() {
        out.get_or_insert(p);
    }
    out.expect("one packet")
}

/// What one slow mux run measured. Times are real, not assumed: a shared CI
/// VM (macOS timer coalescing especially) oversleeps both the stopper thread
/// and each `SLOW` callback, so bounds are taken from the trigger instant and
/// the longest callback actually observed.
struct SlowRun {
    /// From arming to `finish` returning.
    elapsed: Duration,
    /// From the abort trigger (stop actually raised / deadline instant) to
    /// `finish` returning.
    after_trigger: Duration,
    /// Whether the trigger fired before the mux finished (so it was aborted,
    /// not merely completed early).
    triggered_first: bool,
    /// Longest single writer callback observed (at least `SLOW`).
    longest_callback: Duration,
    calls: usize,
}

/// Mux one large packet through a writer that SUCCEEDS on every callback but
/// takes `SLOW` each. `stop_after` raises the stop flag from another thread.
fn run_slow(deadline: Option<Duration>, stop_after: Option<Duration>) -> SlowRun {
    let mut enc = encoder();
    let packet = big_packet(&mut enc);
    let params = multiview_ffmpeg::StreamCodecParameters::from_encoder(&enc);
    let calls = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let calls_in = Arc::clone(&calls);
    let longest = Arc::new(Mutex::new(SLOW));
    let longest_in = Arc::clone(&longest);
    let writer: PacketWriter = make_writer(move |_data: &[u8]| {
        calls_in.fetch_add(1, Ordering::Relaxed);
        let began = Instant::now();
        std::thread::sleep(SLOW);
        let took = began.elapsed();
        let mut longest = longest_in.lock().unwrap();
        *longest = (*longest).max(took);
        true
    });
    let mut muxer = new_muxer(&stop, writer);
    let index = muxer
        .add_stream_from_parameters(&params, enc.time_base())
        .unwrap();
    muxer.write_header().unwrap();
    let started = Instant::now();
    let stopper = stop_after.map(|after| {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            std::thread::sleep(after);
            let raised = Instant::now();
            stop.store(true, Ordering::Release);
            raised
        })
    });
    muxer.arm_io_deadline(deadline);
    let _ = muxer.write_packet(index, packet);
    let _ = muxer.finish();
    let finished = Instant::now();
    let trigger = match stopper {
        Some(stopper) => stopper.join().unwrap(),
        None => started + deadline.unwrap_or(Duration::ZERO),
    };
    let longest_callback = *longest.lock().unwrap();
    SlowRun {
        elapsed: finished - started,
        after_trigger: finished.saturating_duration_since(trigger),
        triggered_first: trigger < finished,
        longest_callback,
        calls: calls.load(Ordering::Relaxed),
    }
}

#[test]
fn a_slow_but_successful_writer_cannot_outlive_the_io_deadline() {
    // Unbounded, the packet needs many slow callbacks (well over a second).
    let unbounded = run_slow(None, None);
    assert!(
        unbounded.calls > 40,
        "packet too small: {} callbacks",
        unbounded.calls
    );
    let run = run_slow(Some(Duration::from_millis(300)), None);
    // The deadline fired well before the packet could finish...
    assert!(run.triggered_first, "packet finished before the deadline");
    assert!(
        run.calls < unbounded.calls,
        "deadline ignored: {} callbacks",
        run.calls
    );
    // ...and nothing ran on past it for more than the same 3-callback margin.
    assert!(
        run.after_trigger < run.longest_callback * 3,
        "deadline ignored: {:?} past the deadline ({:?} total, longest callback {:?}) over {} callbacks",
        run.after_trigger,
        run.elapsed,
        run.longest_callback,
        run.calls
    );
}

#[test]
fn stop_aborts_a_slow_but_successful_writer() {
    let run = run_slow(None, Some(Duration::from_millis(300)));
    assert!(
        run.triggered_first,
        "packet finished before stop: {:?}",
        run.elapsed
    );
    assert!(
        run.after_trigger < run.longest_callback * 3,
        "stop ignored: {:?} after stop was raised ({:?} total, longest callback {:?}) over {} callbacks",
        run.after_trigger,
        run.elapsed,
        run.longest_callback,
        run.calls
    );
}
