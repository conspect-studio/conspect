//! RIST (Reliable Internet Stream Transport, VSF `TR-06`) push-sink tests: the
//! `PushProtocol::Rist` egress selector that fans the **same** encoded packets
//! as every other push transport (invariant #7), through the libav `mpegts`
//! muxer over a `rist://` URL (ADR-0095 Tier-0).
//!
//! `PushProtocol`/`PushSink` live in the `ffmpeg`-gated `sink` module, so this
//! test compiles under `--features ffmpeg`. The protocol→muxer mapping and the
//! sink construction are pure/offline (no peer, no network); a graceful failure
//! to an unreachable peer is asserted exactly like the SRT/RTMP push. A live
//! `rist://` roundtrip is the `#[ignore]`d hardware/network-gated test.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use ffmpeg_next::format::Pixel;
use ffmpeg_next::util::frame::Video;
use multiview_core::color::ColorInfo;
use multiview_core::frame::FrameMeta;
use multiview_core::pixel::PixelFormat;
use multiview_core::time::{MediaTime, Rational};
use multiview_ffmpeg::DecodedVideoFrame;
use multiview_output::sink::{EncodeConfig, PushProtocol, PushSink, VideoFrameSource};
use multiview_output::Result;

const W: u32 = 128;
const H: u32 = 96;

fn ts_config(codec: &str) -> EncodeConfig {
    EncodeConfig {
        codec_name: codec.to_owned(),
        width: W,
        height: H,
        format: Pixel::YUV420P,
        cadence: Rational::new(25, 1),
        gop: 25,
        bit_rate: 800_000,
        audio: None,
        cuda_ordinal: None,
    }
}

/// A finite [`VideoFrameSource`] of `count` NV12 frames with a moving ramp (real
/// motion so the encoder emits non-degenerate packets). Mirrors what the
/// compositor's program output hands a sink (the sink re-stamps the PTS).
struct RampNv12Source {
    produced: u8,
    remaining: u32,
}

impl VideoFrameSource for RampNv12Source {
    fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        self.remaining -= 1;
        let phase = self.produced;
        self.produced = self.produced.wrapping_add(7);
        let mut frame = Video::new(Pixel::NV12, W, H);
        {
            let stride = frame.stride(0);
            let data = frame.data_mut(0);
            let mut row_base = phase;
            for y in 0..H {
                let row_start = usize::try_from(y).unwrap_or(0).saturating_mul(stride);
                let mut value = row_base;
                for x in 0..W {
                    let idx = row_start.saturating_add(usize::try_from(x).unwrap_or(0));
                    if let Some(b) = data.get_mut(idx) {
                        *b = value;
                    }
                    value = value.wrapping_add(1);
                }
                row_base = row_base.wrapping_add(1);
            }
        }
        {
            let data = frame.data_mut(1);
            for b in data.iter_mut() {
                *b = 128;
            }
        }
        let meta = FrameMeta {
            pts: MediaTime::ZERO,
            width: W,
            height: H,
            format: PixelFormat::Nv12,
            color: ColorInfo::default(),
        };
        Ok(Some(DecodedVideoFrame {
            frame,
            meta,
            raw_pts: None,
            a53_cc: None,
        }))
    }
}

#[test]
fn rist_push_protocol_maps_to_mpegts_muxer() {
    // RIST carries an MPEG-TS payload exactly like SRT/UDP: the URL scheme
    // selects the transport, the muxer is the container (ADR-0095 §3).
    assert_eq!(PushProtocol::Rist.muxer_name(), "mpegts");
    // The sibling transports are unchanged by adding RIST.
    assert_eq!(PushProtocol::Srt.muxer_name(), "mpegts");
    assert_eq!(PushProtocol::UdpTs.muxer_name(), "mpegts");
    assert_eq!(PushProtocol::Rtmp.muxer_name(), "flv");
    assert_eq!(PushProtocol::Rtsp.muxer_name(), "rtsp");
}

#[test]
fn rist_push_sink_construction_carries_protocol_and_url() {
    let sink = PushSink::new(
        ts_config("mpeg2video"),
        PushProtocol::Rist,
        "rist://[2001:db8::20]:6000",
    );
    assert_eq!(sink.protocol(), PushProtocol::Rist);
    assert_eq!(sink.muxer_name(), "mpegts");
    assert_eq!(sink.url(), "rist://[2001:db8::20]:6000");
}

/// Live `rist://` loopback roundtrip — the genuine end-to-end proof that the
/// `PushProtocol::Rist` sink delivers a real, `ffprobe`-decodable MPEG-TS over a
/// real RIST socket (ADR-0095 §8 conformance oracle).
///
/// `#[ignore]`d: it needs an `FFmpeg` **built `--enable-librist`** (the deploy
/// image is; the plain CI runner may not be) and binds a UDP port, so it is run
/// on hardware/network-gated runners, never in the pure-Rust CI leg:
/// `cargo test -p multiview-output --features ffmpeg -- --ignored rist_loopback`.
///
/// Shape: an `ffmpeg` **receiver** listens on `rist://@[::]:PORT` and records to
/// an MPEG-TS file; our [`PushSink`] (caller) streams `FRAMES` encoded frames to
/// `rist://[::1]:PORT`; then `ffprobe` confirms the received file decodes to a
/// real `mpeg2video` stream. RIST's ARQ recovers loss transparently — on a clean
/// loopback the frame count matches exactly.
#[test]
#[ignore = "needs FFmpeg built --enable-librist + a bindable UDP port; hardware/network-gated"]
fn rist_loopback_roundtrip_delivers_a_decodable_ts() {
    use std::process::Command;
    use std::time::Duration;

    const FRAMES: u32 = 50;
    const PORT: u16 = 5_312;

    let dir = tempfile::tempdir().expect("tempdir");
    let received = dir.path().join("received.ts");

    // The receiver listens (the `@` = librist listen mode) on the IPv6 loopback,
    // dual-stack; it records whatever the sender pushes for a bounded time.
    let recv_url = format!("rist://@[::]:{PORT}?rist_profile=1");
    let mut recv_proc = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(&recv_url)
        .args(["-c", "copy", "-f", "mpegts"])
        .arg(&received)
        .spawn()
        .expect("spawn the rist receiver (needs --enable-librist)");

    // Give the listener a moment to bind before the caller connects.
    std::thread::sleep(Duration::from_millis(500));

    let send_url = format!("rist://[::1]:{PORT}?rist_profile=1");
    let sink = PushSink::new(ts_config("mpeg2video"), PushProtocol::Rist, &send_url);
    let mut source = RampNv12Source {
        produced: 0,
        remaining: FRAMES,
    };
    sink.run(&mut source).expect("the rist push delivers");

    // Let the receiver flush, then stop it.
    std::thread::sleep(Duration::from_millis(750));
    let _ = recv_proc.kill();
    let _ = recv_proc.wait();

    assert!(received.exists(), "the receiver wrote nothing");
    let count = rist_ffprobe_frame_count(&received);
    assert!(
        count > 0,
        "the received RIST stream must decode to real frames (got {count})"
    );
}

/// Count decodable video frames in `path` via `ffprobe -count_frames` (used by
/// the `#[ignore]`d live roundtrip).
#[cfg(test)]
fn rist_ffprobe_frame_count(path: &std::path::Path) -> u64 {
    let out = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-count_frames",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=nb_read_frames",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("spawn ffprobe -count_frames");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && *l != "N/A")
        .unwrap_or("0")
        .trim_end_matches(',')
        .parse()
        .unwrap_or(0)
}
