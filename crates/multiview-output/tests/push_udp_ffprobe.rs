//! End-to-end verification of the real [`PushSink`] live-transport egress over a
//! genuine network transport: **MPEG-TS over UDP**.
//!
//! Unlike `push_and_gpl.rs` (which asserts the protocol→muxer mapping and the
//! graceful connect-failure path, but cannot complete a live push in CI because
//! RTMP/RTSP need a running server and the SRT loopback handshake is unavailable
//! in the sandbox), this test exercises the **whole** push code path on a real
//! socket: it binds a `UdpSocket`, drains it on a background thread, runs
//! [`PushSink`] with [`PushProtocol::UdpTs`] (the `mpegts` muxer over `udp://`,
//! the *identical* encode-once-mux-many drive loop the RTMP/SRT push uses — only
//! the URL scheme and muxer name differ), reassembles the received datagrams into
//! an MPEG-TS, and **`ffprobe`s it** to confirm a real, decodable stream of the
//! right codec/geometry and the exact frame count the source produced.
//!
//! Licensing: the test encodes with `mpeg2video` (LGPL, in-tree) so it passes
//! under the plain `ffmpeg` feature with no GPL escalation.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::net::UdpSocket;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use ffmpeg_next::format::Pixel;
use ffmpeg_next::util::frame::Video;
use multiview_core::color::ColorInfo;
use multiview_core::frame::FrameMeta;
use multiview_core::pixel::PixelFormat;
use multiview_core::time::{MediaTime, Rational};
use multiview_ffmpeg::DecodedVideoFrame;
use multiview_output::sink::{EncodeConfig, PushProtocol, PushSink, VideoFrameSource};
use multiview_output::Result;

const W: u32 = 320;
const H: u32 = 240;
const FRAMES: u32 = 50;

/// A finite [`VideoFrameSource`] of `count` NV12 frames with a moving ramp, so
/// the encoder produces real, non-degenerate packets (a flat frame can collapse
/// to almost nothing). Mirrors what the compositor's program output hands a sink:
/// NV12 frames whose own timestamps the sink ignores and re-stamps.
struct RampNv12Source {
    produced: u8,
    remaining: u32,
}

impl RampNv12Source {
    fn new(count: u32) -> Self {
        Self {
            produced: 0,
            remaining: count,
        }
    }
}

impl VideoFrameSource for RampNv12Source {
    fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        self.remaining -= 1;
        // A per-frame phase byte (wraps) so each frame differs -> real motion the
        // inter-frame encoder must actually code (no `as` casts: pure u8 math).
        let phase = self.produced;
        self.produced = self.produced.wrapping_add(7);
        // NV12: full-size Y plane (a per-frame-varying ramp) then a half-height
        // interleaved UV plane (neutral 128). `Video::new` allocates both planes.
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
            // All four axes default to Unspecified (the detect step never guesses
            // -- invariant #8); the sink re-stamps and tags downstream.
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

fn ts_config(fps: i64, gop: u32) -> EncodeConfig {
    EncodeConfig {
        codec_name: "mpeg2video".to_owned(),
        width: W,
        height: H,
        format: Pixel::YUV420P,
        cadence: Rational::new(fps, 1),
        gop,
        bit_rate: 1_500_000,
        audio: None,
        cuda_ordinal: None,
    }
}

/// `ffprobe` the first non-empty value of a single `stream=<entry>` field for the
/// first video stream of `path`.
fn ffprobe_v(path: &Path, entry: &str) -> String {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            &format!("stream={entry}"),
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("spawn ffprobe");
    assert!(
        out.status.success(),
        "ffprobe failed for {}",
        path.display()
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .trim_end_matches(',')
        .to_owned()
}

/// Count decodable video frames in `path` via `ffprobe -count_frames`.
fn ffprobe_frame_count(path: &Path) -> u64 {
    let out = Command::new("ffprobe")
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
    assert!(out.status.success(), "ffprobe -count_frames failed");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().trim_end_matches(','))
        .find(|l| !l.is_empty() && *l != "N/A")
        .unwrap_or("0")
        .parse()
        .expect("frame count is an integer")
}

#[test]
fn push_sink_streams_a_real_mpegts_over_udp_that_ffprobe_decodes() {
    // Bind the receiver FIRST (so no datagram is lost to a closed port), on an
    // OS-assigned free port, and drain it on a background thread into a buffer.
    // IPv6-first: receive on the IPv6 loopback `[::1]`.
    let socket = UdpSocket::bind("[::1]:0").expect("bind udp receiver");
    socket
        .set_read_timeout(Some(Duration::from_millis(1500)))
        .expect("set recv timeout");
    let port = socket.local_addr().expect("local addr").port();

    // Drain on a background thread: collect datagrams until the read times out
    // (the push completes well inside the window, then the next recv times out and
    // the thread stops). The timeout — not a count — bounds the wait, so a short
    // clip never hangs the test.
    let recv_thread = std::thread::spawn(move || {
        let mut packets = Vec::new();
        let mut datagram = vec![0u8; 65_536];
        loop {
            match socket.recv(&mut datagram) {
                Ok(n) => {
                    let slice = datagram.get(..n).unwrap_or(&[]);
                    packets.push(slice.to_vec());
                }
                // Timeout = the push finished and no more datagrams arrive.
                Err(ref e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    break
                }
                Err(e) => panic!("udp recv failed: {e}"),
            }
        }
        packets
    });

    // Run the REAL PushSink over UDP-TS. This is the same encode-once-mux-many
    // drive loop (PushSink::run -> drive_to_single_muxer) the wired RTMP/SRT push
    // uses; only the URL scheme + forced muxer (mpegts) differ. `pkt_size` keeps
    // each datagram inside a single MPEG-TS-friendly UDP packet.
    let url = format!("udp://[::1]:{port}?pkt_size=1316");
    let sink = PushSink::new(ts_config(25, 25), PushProtocol::UdpTs, url);
    let mut source = RampNv12Source::new(FRAMES);
    let stats = sink.run(&mut source).expect("udp-ts push run");
    assert_eq!(
        stats.packets,
        u64::from(FRAMES),
        "the push must encode exactly one packet per source frame"
    );
    assert!(
        stats.keyframes >= 1,
        "at least the first packet is a keyframe"
    );

    // Join the receiver and reassemble the datagrams in arrival order into one
    // MPEG-TS byte stream (UDP-TS is an ordered, lossless stream on loopback for a
    // short clip).
    let packets = recv_thread.join().expect("join udp receiver");
    let mut ts_bytes = Vec::new();
    for p in &packets {
        ts_bytes.extend_from_slice(p);
    }
    assert!(
        !ts_bytes.is_empty(),
        "no MPEG-TS datagrams were received over UDP"
    );

    // Persist and ffprobe: a real, decodable mpeg2video 320x240 stream with the
    // exact frame count the source produced -- proving the push delivered the
    // whole program over the wire, not a truncated/garbled fragment.
    let dir = tempfile::tempdir().expect("tempdir");
    let recv_path = dir.path().join("udp_recv.ts");
    std::fs::write(&recv_path, &ts_bytes).expect("write received ts");

    assert_eq!(ffprobe_v(&recv_path, "codec_name"), "mpeg2video");
    assert_eq!(ffprobe_v(&recv_path, "width"), "320");
    assert_eq!(ffprobe_v(&recv_path, "height"), "240");
    assert_eq!(
        ffprobe_frame_count(&recv_path),
        u64::from(FRAMES),
        "the received UDP-TS must decode to exactly the frames the push sent"
    );
}
