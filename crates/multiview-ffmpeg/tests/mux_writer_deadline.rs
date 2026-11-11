//! Review SRT-05: a custom-writer muxer obeys an operation-wide I/O deadline
//! and the stop flag, even when every single write callback SUCCEEDS but is
//! slow (a slow-but-steady peer). The deadline does not reset per callback.
#![cfg(feature = "ffmpeg")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
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

/// Mux one large packet through a writer that SUCCEEDS on every callback but
/// takes `SLOW` each; returns how long header+packet+trailer took and how many
/// callbacks ran. `stop_after` raises the stop flag at the start of the first
/// callback that begins at least that long after arming. The flag is the
/// shared one any thread would raise; raising it from the writer removes the
/// stopper thread's own oversleep (macOS timer coalescing on shared CI VMs,
/// #265) from a bound that is about the MUXER's reaction, not that sleep.
fn run_slow(deadline: Option<Duration>, stop_after: Option<Duration>) -> (Duration, usize) {
    let mut enc = encoder();
    let packet = big_packet(&mut enc);
    let params = multiview_ffmpeg::StreamCodecParameters::from_encoder(&enc);
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_in = Arc::clone(&calls);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_in = Arc::clone(&stop);
    let armed_at = Arc::new(std::sync::OnceLock::<Instant>::new());
    let armed_in = Arc::clone(&armed_at);
    let writer: PacketWriter = make_writer(move |_data: &[u8]| {
        if let (Some(after), Some(armed)) = (stop_after, armed_in.get()) {
            if armed.elapsed() >= after {
                stop_in.store(true, Ordering::Release);
            }
        }
        calls_in.fetch_add(1, Ordering::Relaxed);
        std::thread::sleep(SLOW);
        true
    });
    let mut muxer = new_muxer(&stop, writer);
    let index = muxer
        .add_stream_from_parameters(&params, enc.time_base())
        .unwrap();
    muxer.write_header().unwrap();
    let started = Instant::now();
    armed_at.set(started).unwrap();
    muxer.arm_io_deadline(deadline);
    let _ = muxer.write_packet(index, packet);
    let _ = muxer.finish();
    let elapsed = started.elapsed();
    (elapsed, calls.load(Ordering::Relaxed))
}

#[test]
fn a_slow_but_successful_writer_cannot_outlive_the_io_deadline() {
    // Unbounded, the packet needs many slow callbacks (well over a second).
    let (_, unbounded_calls) = run_slow(None, None);
    assert!(
        unbounded_calls > 40,
        "packet too small: {unbounded_calls} callbacks"
    );
    let (elapsed, calls) = run_slow(Some(Duration::from_millis(300)), None);
    assert!(
        elapsed < Duration::from_millis(300) + SLOW * 3,
        "deadline ignored: {elapsed:?} over {calls} callbacks"
    );
}

#[test]
fn stop_aborts_a_slow_but_successful_writer() {
    // Unbounded, the packet would run far past the stop point (so this is an
    // abort, not an early finish).
    let (_, unbounded_calls) = run_slow(None, None);
    assert!(
        unbounded_calls > 40,
        "packet too small: {unbounded_calls} callbacks"
    );
    let (elapsed, calls) = run_slow(None, Some(Duration::from_millis(300)));
    assert!(
        elapsed < Duration::from_millis(300) + SLOW * 3,
        "stop ignored: {elapsed:?} over {calls} callbacks"
    );
}
