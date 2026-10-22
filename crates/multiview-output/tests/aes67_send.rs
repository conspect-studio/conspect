//! AES67 / ST 2110-30 send-side tests (ADR-0033 §1/§2/§7).
//!
//! The pure `Aes67Sender` (always compiled, default build): the wire round-trip
//! against `multiview-input`'s decoder (encode here, decode there), the
//! continuous marker=0 cadence with advancing sequence/timestamp, and the
//! bounded drop-oldest / never-back-pressure isolation (invariants #1 / #10).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::float_cmp
)]

use std::time::Duration;

use multiview_audio::{AudioBlock, AudioFormat, ChannelLayout};
use multiview_output::aes67::{
    Aes67ConfigError, Aes67Sender, Aes67SenderHandle, PcmDepth, RTP_FIXED_HEADER_LEN,
};

// The decoder half lives in the sibling input crate (a dev-dependency); the
// round-trip pins this encoder byte-identical to it.
use multiview_input::st2110::{pcm_to_f32, Aes3Format, RtpPacket, SampleDepth, V30Payload};

const FS: u32 = 48_000;
/// 1 ms at 48 kHz = 48 sample groups per packet (ST 2110-30 Class A).
const FRAMES_PER_PACKET: usize = 48;

fn stereo() -> AudioFormat {
    AudioFormat::new(FS, ChannelLayout::Stereo)
}

/// A block of `FRAMES_PER_PACKET` stereo frames of distinct unit-range values.
fn ramp_block() -> (Vec<f32>, AudioBlock) {
    let samples: Vec<f32> = (0..(FRAMES_PER_PACKET * 2))
        .map(|i| (i as f32) / 200.0 - 0.24)
        .collect();
    let block = AudioBlock::from_interleaved(stereo(), samples.clone()).unwrap();
    (samples, block)
}

#[test]
fn packet_duration_derives_the_send_cadence_from_the_media_clock() {
    // P2-F1: the send cadence MUST be the media clock — ptime = frames_per_packet
    // / sample_rate — not an arbitrary interval. The RTP timestamp advances by
    // frames_per_packet per packet, so a send timer unrelated to (sample_rate,
    // frames_per_packet) makes wall-clock diverge from the advertised clock and
    // the receiver buffer drifts. The sender therefore knows its sample_rate and
    // derives the cadence itself.
    let one_ms = Aes67Sender::new(2, PcmDepth::L24, 96, 1, 48_000, 48, 4_800).expect("48@48k");
    assert_eq!(
        one_ms.packet_duration(),
        Duration::from_millis(1),
        "48 frames @ 48 kHz = 1 ms"
    );
    let two_ms = Aes67Sender::new(2, PcmDepth::L16, 96, 1, 48_000, 96, 4_800).expect("96@48k");
    assert_eq!(
        two_ms.packet_duration(),
        Duration::from_millis(2),
        "96 frames @ 48 kHz = 2 ms"
    );
    let half_ms = Aes67Sender::new(2, PcmDepth::L16, 96, 1, 96_000, 48, 4_800).expect("48@96k");
    assert_eq!(
        half_ms.packet_duration(),
        Duration::from_micros(500),
        "48 frames @ 96 kHz = 0.5 ms"
    );
    // A zero / out-of-range sample rate is rejected fail-closed, never used to
    // derive a nonsensical (or division-by-zero) cadence.
    assert!(matches!(
        Aes67Sender::new(2, PcmDepth::L16, 96, 1, 0, 48, 4_800),
        Err(Aes67ConfigError::SampleRate { .. })
    ));
    assert!(matches!(
        Aes67Sender::new(2, PcmDepth::L16, 96, 1, 1_000_000, 48, 4_800),
        Err(Aes67ConfigError::SampleRate { .. })
    ));
}

#[test]
fn packet_deadlines_are_cumulative_and_never_drift_at_a_non_dividing_rate() {
    // T1 (#153): the send loop must schedule each packet at an ABSOLUTE deadline
    // derived from the cumulative packet index — packet n is due at
    // start + n×frames_per_packet/sample_rate — NOT by repeating a single
    // truncated `packet_duration`. At any rate `frames_per_packet` does not evenly
    // divide, one truncated interval loses sub-nanosecond time PER packet and that
    // error accumulates without bound over a 24/7 run, so the wire cadence drifts
    // off the RTP media clock and the receiver buffer walks (inv #1/#3).
    //
    // 48 frames @ 44_100 Hz: 48e9 / 44_100 = 1_088_435.374… ns — a non-integer, so
    // a repeated `packet_duration` (floor = 1_088_435 ns) drifts ~0.374 ns/packet.
    let sender = Aes67Sender::new(2, PcmDepth::L16, 97, 1, 44_100, 48, 4_800).expect("48@44.1k");
    let frames: u128 = 48;
    let rate: u128 = 44_100;
    let ns_per_s: u128 = 1_000_000_000;

    // Packet 0 is due immediately (offset 0).
    assert_eq!(
        sender.packet_deadline_offset(0),
        Duration::ZERO,
        "the first packet is due at the send-loop start"
    );

    // The single truncated per-packet duration (what a repeated interval would use).
    let truncated = sender.packet_duration().as_nanos();

    // For growing n: the cumulative deadline is the EXACT floor (so its error stays
    // < 1 ns of the ideal FOREVER), while the naive "n × truncated" repeated
    // interval drifts away from it by an amount that GROWS with n.
    let mut prev_drift: u128 = 0;
    for &n in &[1_000u64, 1_000_000, 1_000_000_000] {
        let exact = (u128::from(n) * frames * ns_per_s) / rate;
        assert_eq!(
            sender.packet_deadline_offset(n).as_nanos(),
            exact,
            "packet {n}'s cumulative deadline is the exact floor — no accumulated drift"
        );
        let naive = u128::from(n) * truncated;
        // floor(n·R) ≥ n·floor(R), so this never underflows; it is the drift a
        // repeated truncated interval accrues by packet n.
        let drift = exact - naive;
        assert!(
            drift > prev_drift,
            "repeated-truncation drift grows without bound (n={n}: exact={exact} \
             naive={naive} drift={drift} ns) — the T1 defect the cumulative deadline fixes"
        );
        prev_drift = drift;
    }
}

#[test]
fn next_packet_into_reuses_the_buffer_with_no_per_packet_alloc() {
    // P2-F6 (rule 22): the continuous send path must not allocate per packet. The
    // reusable-buffer drain writes into one datagram buffer; after warm-up its
    // backing allocation (pointer + capacity) never changes across packets, so no
    // Vec is allocated per tick on the data plane.
    let mut sender = Aes67Sender::new(2, PcmDepth::L24, 96, 1, 48_000, 48, 4_800).expect("valid");
    let mut buf = Vec::new();

    // Warm-up: the first call sizes the buffer once.
    sender.next_packet_into(&mut buf);
    let warm_ptr = buf.as_ptr();
    let warm_cap = buf.capacity();
    let warm_len = buf.len();
    assert!(warm_len > 0, "a full packet is written into the buffer");

    // Every subsequent packet reuses the SAME allocation — no realloc, no growth.
    for _ in 0..1_000 {
        sender.next_packet_into(&mut buf);
        assert_eq!(buf.len(), warm_len, "each packet is the same full size");
        assert_eq!(
            buf.as_ptr(),
            warm_ptr,
            "the datagram buffer is reused — no per-packet reallocation (rule 22)"
        );
        assert_eq!(
            buf.capacity(),
            warm_cap,
            "capacity is stable — the buffer never grows/reallocates"
        );
    }
}

#[test]
fn rejects_a_payload_type_outside_the_7bit_rtp_range() {
    // P2-F7: the RTP payload type is a 7-bit field (0..=127). A value >127 was
    // silently truncated with `& 0x7f`, so the transmitted PT diverged from the
    // advertised SDP (e.g. 200 -> 72). Reject it fail-closed at construction
    // rather than send a PT that does not match the announced one.
    assert!(matches!(
        Aes67Sender::new(2, PcmDepth::L24, 128, 1, 48_000, 48, 4_800),
        Err(Aes67ConfigError::PayloadType { .. })
    ));
    assert!(matches!(
        Aes67Sender::new(2, PcmDepth::L24, 200, 1, 48_000, 48, 4_800),
        Err(Aes67ConfigError::PayloadType { .. })
    ));
    // In-range dynamic payload types (96..=127 is the usual AES67 band) still
    // construct, including the 127 boundary.
    assert!(Aes67Sender::new(2, PcmDepth::L24, 96, 1, 48_000, 48, 4_800).is_ok());
    assert!(Aes67Sender::new(2, PcmDepth::L24, 127, 1, 48_000, 48, 4_800).is_ok());
}

#[test]
fn sender_payload_roundtrips_through_the_v30_decoder() {
    let mut sender = Aes67Sender::new(
        2,
        PcmDepth::L24,
        97,
        0x1234_5678,
        FS,
        FRAMES_PER_PACKET,
        4_800,
    )
    .expect("valid aes67 config");
    let (samples, block) = ramp_block();
    sender.handle().push(&block);

    let packet = sender.next_packet();
    let rtp = RtpPacket::parse(&packet).expect("a valid RTP packet");
    assert!(
        !rtp.header.marker,
        "continuous stream: marker=0 (ADR-0033 §1)"
    );
    assert_eq!(rtp.header.payload_type, 97, "dynamic payload type carried");

    let format = Aes3Format::new(2, SampleDepth::L24).expect("stereo L24");
    let payload = V30Payload::parse(rtp.payload, format).expect("whole sample groups");
    let decoded = pcm_to_f32(&payload);

    assert_eq!(
        decoded.len(),
        samples.len(),
        "one decoded f32 per encoded sample"
    );
    for (want, got) in samples.iter().zip(&decoded) {
        assert!(
            (want - got).abs() < 1.0e-4,
            "L24 encode round-trips through the decoder: want {want}, got {got}",
        );
    }
}

#[test]
fn handle_push_feeds_the_serve_side() {
    // F3: the engine-bake producer HANDLE and the serve loop share one FIFO via
    // an Arc, so a block pushed on the handle is drained by the serve side — the
    // two halves run on independent clocks with no `&mut` contention.
    let mut sender = Aes67Sender::new(
        2,
        PcmDepth::L24,
        97,
        0x1234_5678,
        FS,
        FRAMES_PER_PACKET,
        4_800,
    )
    .expect("valid aes67 config");
    let handle: Aes67SenderHandle = sender.handle();
    let (samples, block) = ramp_block();
    handle.push(&block);

    let packet = sender.next_packet();
    let rtp = RtpPacket::parse(&packet).expect("a valid RTP packet");
    let format = Aes3Format::new(2, SampleDepth::L24).expect("stereo L24");
    let payload = V30Payload::parse(rtp.payload, format).expect("whole sample groups");
    let decoded = pcm_to_f32(&payload);
    assert_eq!(
        decoded.len(),
        samples.len(),
        "the serve side drained the handle's push"
    );
    for (want, got) in samples.iter().zip(&decoded) {
        assert!(
            (want - got).abs() < 1.0e-4,
            "handle push feeds the serve drain through the shared FIFO: want {want}, got {got}",
        );
    }
}

#[test]
fn sender_emits_continuous_marker0_with_advancing_seq_and_ts() {
    // Stereo L24, 1 ms packets: ADR-0033 §2 says the payload is 288 bytes.
    let mut sender = Aes67Sender::new(2, PcmDepth::L24, 96, 0xABCD, FS, FRAMES_PER_PACKET, 4_800)
        .expect("valid aes67 config");

    let mut prev_seq: Option<u16> = None;
    let mut prev_ts: Option<u32> = None;
    for _ in 0..8 {
        let packet = sender.next_packet();
        assert_eq!(
            packet.len(),
            RTP_FIXED_HEADER_LEN + 288,
            "12-byte header + 288-byte stereo-L24 payload (ADR-0033 §2)"
        );
        let rtp = RtpPacket::parse(&packet).expect("valid RTP");
        assert!(!rtp.header.marker, "marker=0 on every packet");

        if let Some(seq) = prev_seq {
            assert_eq!(
                rtp.header.sequence,
                seq.wrapping_add(1),
                "sequence advances by exactly 1 per packet"
            );
        }
        if let Some(ts) = prev_ts {
            assert_eq!(
                rtp.header.timestamp,
                ts.wrapping_add(u32::try_from(FRAMES_PER_PACKET).unwrap()),
                "timestamp advances by sample-groups (+48), never by bytes"
            );
        }
        prev_seq = Some(rtp.header.sequence);
        prev_ts = Some(rtp.header.timestamp);
    }
}

#[test]
fn absent_feed_emits_continuous_silence_never_a_gap() {
    // A sender that is never pushed still emits full marker=0 packets forever
    // (silence-fill), so the RTP stream is continuous (invariant #1).
    let mut sender = Aes67Sender::new(1, PcmDepth::L16, 96, 1, FS, FRAMES_PER_PACKET, 4_800)
        .expect("valid aes67 config");
    for _ in 0..4 {
        let packet = sender.next_packet();
        let rtp = RtpPacket::parse(&packet).expect("valid RTP");
        assert!(!rtp.header.marker);
        // Mono L16, 48 frames: 96-byte silence payload of zeroes.
        assert_eq!(rtp.payload.len(), FRAMES_PER_PACKET * 2);
        assert!(
            rtp.payload.iter().all(|&b| b == 0),
            "an underrun emits silence, never a short/absent packet"
        );
    }
}

#[test]
fn push_is_bounded_drop_oldest_and_never_back_pressures() {
    // A tiny FIFO flooded with far more than capacity: every push returns (never
    // blocks the engine), the fill stays bounded, and the overflow is dropped-
    // oldest and accounted (invariants #10 / #5). This is the "sender can never
    // back-pressure the program bus" property.
    let capacity = 96; // frames
    let sender = Aes67Sender::new(2, PcmDepth::L24, 96, 7, FS, FRAMES_PER_PACKET, capacity)
        .expect("valid aes67 config");
    let handle = sender.handle();
    let (_samples, block) = ramp_block(); // 48 frames per push

    for _ in 0..10_000 {
        // If push blocked or grew unbounded this loop would hang / OOM.
        handle.push(&block);
        assert!(
            sender.fill_frames() <= capacity,
            "the send FIFO never grows past capacity (never back-pressures)"
        );
    }
    assert!(
        sender.dropped_frames() > 0,
        "a flooded FIFO sheds the oldest frames rather than growing"
    );
}

#[test]
fn sender_cadence_is_independent_of_the_program_feed() {
    // Drain the same number of packets under two very different feeds and assert
    // the emitted packet count + sequence progression is identical — the RTP
    // media clock is driven by the send timer, never paced by the input feed
    // (invariant #1).
    let drain = |feed: &dyn Fn(&Aes67SenderHandle, usize)| -> (usize, u16) {
        let mut sender = Aes67Sender::new(2, PcmDepth::L24, 96, 9, FS, FRAMES_PER_PACKET, 4_800)
            .expect("valid aes67 config");
        let handle = sender.handle();
        let mut count = 0usize;
        let mut last_seq = 0u16;
        for tick in 0..64 {
            feed(&handle, tick);
            let packet = sender.next_packet();
            let rtp = RtpPacket::parse(&packet).expect("valid RTP");
            assert!(!rtp.header.marker);
            last_seq = rtp.header.sequence;
            count += 1;
        }
        (count, last_seq)
    };

    let (_samples, block) = ramp_block();
    let absent = drain(&|_h, _t| {});
    let flooded = drain(&|h, _t| {
        // A wild over-feed each tick.
        for _ in 0..50 {
            h.push(&block);
        }
    });

    assert_eq!(
        absent, flooded,
        "the send cadence (packet count + sequence) is identical whether the feed \
         is absent or wildly over-supplied — the feed never paces the output"
    );
}
