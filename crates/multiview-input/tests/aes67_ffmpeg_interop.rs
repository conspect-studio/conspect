//! AES67 / ST 2110-30 **ffmpeg interop** on loopback (ADR-0033, feature `st2110`).
//!
//! Validates our receive path against a real, independent AES67 sender: ffmpeg
//! emits an `L24/48000/2` RTP stream (`a=rtpmap:97 L24/48000/2`) to a loopback
//! UDP port, and our [`Aes67AudioProducer`] depacketizes it into non-silent
//! interleaved `f32`. This is the interop counterpart to the pure golden-vector
//! and self-round-trip tests.
//!
//! `#[ignore]`d: it needs an `ffmpeg` binary and a UDP socket, so it is run on
//! demand (`cargo test -p multiview-input --features st2110 -- --ignored
//! aes67_ffmpeg`), never on the socket-free default CI leg.

#![cfg(feature = "st2110")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::float_cmp
)]

use std::process::{Command, Stdio};
use std::time::Duration;

use multiview_input::st2110::transport::{PacketSource, St2110Packet};
use multiview_input::st2110::v30::{Aes3Format, SampleDepth};
use multiview_input::st2110::{Aes67AudioProducer, RtpPacket};

/// A one-shot [`PacketSource`] over already-received packet units.
struct Collected {
    packets: std::collections::VecDeque<St2110Packet>,
}

impl PacketSource for Collected {
    fn poll_packet(&mut self) -> Result<Option<St2110Packet>, multiview_input::Error> {
        Ok(self.packets.pop_front())
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "needs an ffmpeg binary + a UDP socket; run on demand for AES67 interop evidence"]
async fn aes67_producer_decodes_ffmpeg_l24_rtp() {
    // Bind the receive socket first so the kernel buffers ffmpeg's datagrams.
    let rx = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("bind rx");
    let port = rx.local_addr().expect("rx addr").port();

    // ffmpeg: a 1 kHz sine, stereo, 24-bit big-endian PCM, RTP to our port.
    let spawn = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-re",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=1000:sample_rate=48000:duration=2",
            "-ac",
            "2",
            "-c:a",
            "pcm_s24be",
            "-f",
            "rtp",
            &format!("rtp://127.0.0.1:{port}"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match spawn {
        Ok(child) => child,
        Err(e) => {
            eprintln!("skipping AES67 ffmpeg interop: ffmpeg not runnable ({e})");
            return;
        }
    };

    // Collect a handful of RTP datagrams (with a timeout so a missing/duplex
    // failure surfaces as a clear error, not a hang).
    let format = Aes3Format::new(2, SampleDepth::L24).expect("stereo L24");
    let mut packets = std::collections::VecDeque::new();
    let mut buf = vec![0u8; 4096];
    for _ in 0..16 {
        match tokio::time::timeout(Duration::from_secs(3), rx.recv(&mut buf)).await {
            Ok(Ok(n)) => {
                if let Ok(rtp) = RtpPacket::parse(&buf[..n]) {
                    packets.push_back(St2110Packet::from_rtp(&rtp));
                }
            }
            Ok(Err(e)) => panic!("recv failed: {e}"),
            Err(_) => break, // ffmpeg finished sending
        }
    }
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        packets.len() >= 4,
        "expected several L24 RTP packets from ffmpeg, got {}",
        packets.len()
    );

    // Our producer decodes ffmpeg's real AES67 L24 into non-silent audio. ffmpeg
    // advertises `a=rtpmap:97 L24/48000/2`, so the RTP payload type is 97.
    let mut producer = Aes67AudioProducer::new(Box::new(Collected { packets }), format, 97);
    let mut total_frames = 0usize;
    let mut peak = 0.0_f32;
    while let Some(frame) = producer.next_audio().expect("decode never faults") {
        total_frames += frame.samples.len() / 2;
        for &s in &frame.samples {
            peak = peak.max(s.abs());
        }
    }
    assert!(
        total_frames > 0,
        "decoded at least one audio frame from ffmpeg"
    );
    assert!(
        peak > 0.05,
        "a 1 kHz sine decodes as non-silent audio (peak {peak} — a mis-parse would read ~silence)"
    );
}
