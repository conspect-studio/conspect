//! DEV-C1 (ADR-M010): the **RTCP Sender Report** builder — NTP↔RTP pairs
//! stamped from the same outbound presentation epoch as the WS publication and
//! the HLS `EXT-X-PROGRAM-DATE-TIME` tags.
//!
//! Pins exact integer field math against known vectors (RFC 3550 §6.4.1):
//! * the 64-bit NTP timestamp from Unix wall ns (era offset 2 208 988 800 s,
//!   2^32 fractional scale, `i128` intermediates — never float);
//! * the RTP timestamp from the epoch's media clock at the same wall instant
//!   (`rtp = base + rescale(media_at(wall))`, modulo 2^32);
//! * the exact 28-byte wire serialization;
//! * the `SrStamper` seam over the shared epoch cell (None before an epoch
//!   exists — never a fabricated mapping).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::time::Rational;
use multiview_core::wallclock::WallClockRef;
use multiview_output::rtcp::{rtp_timestamp_at, NtpTimestamp, SenderReport, SrStamper};
use multiview_output::rtsp_server::RtspServerSink;
use multiview_output::SharedEpoch;

fn rate_ns() -> Rational {
    Rational::new(1_000_000_000, 1)
}

// ---------------------------------------------------------------------------
// NTP timestamp math (RFC 3550 §4: seconds since 1900-01-01 + 2^-32 fraction)
// ---------------------------------------------------------------------------

#[test]
fn ntp_timestamp_of_the_unix_epoch_is_the_era_offset() {
    let ts = NtpTimestamp::from_unix_ns(0);
    assert_eq!(ts.seconds, 2_208_988_800);
    assert_eq!(ts.fraction, 0);
}

#[test]
fn ntp_fraction_of_half_a_second_is_exactly_2_to_31() {
    let ts = NtpTimestamp::from_unix_ns(500_000_000);
    assert_eq!(ts.seconds, 2_208_988_800);
    assert_eq!(ts.fraction, 0x8000_0000);
}

#[test]
fn ntp_timestamp_of_a_modern_instant_is_exact() {
    // 2025-06-15T15:06:40.25Z = unix 1_750_000_000.25 s.
    let ts = NtpTimestamp::from_unix_ns(1_750_000_000_250_000_000);
    assert_eq!(ts.seconds, 1_750_000_000 + 2_208_988_800);
    assert_eq!(ts.fraction, 0x4000_0000, "0.25 s = 2^30 exactly");
}

#[test]
fn ntp_fraction_rounds_exactly_not_via_float() {
    // 1 ns = 4.294967296 fractional units: rounds to 4 (half away from zero).
    assert_eq!(NtpTimestamp::from_unix_ns(1).fraction, 4);
    // 999 999 999 ns must stay strictly below 2^32 (never carry into seconds).
    let f = NtpTimestamp::from_unix_ns(999_999_999).fraction;
    assert_eq!(f, 4_294_967_292);
}

#[test]
fn ntp_seconds_wrap_modulo_2_to_32_at_the_era_boundary() {
    // Unix 2_085_978_496 s + 2_208_988_800 = 2^32 exactly: era 1 wraps to 0
    // (the RFC 3550 timestamp is the low 64 bits; receivers handle the era).
    let ts = NtpTimestamp::from_unix_ns(2_085_978_496_000_000_000);
    assert_eq!(ts.seconds, 0);
    let ts1 = NtpTimestamp::from_unix_ns(2_085_978_497_000_000_000);
    assert_eq!(ts1.seconds, 1);
}

#[test]
fn ntp_pre_epoch_instants_use_euclidean_split() {
    // -0.5 s = 1969-12-31T23:59:59.5Z: seconds go DOWN one, fraction is +0.5.
    let ts = NtpTimestamp::from_unix_ns(-500_000_000);
    assert_eq!(ts.seconds, 2_208_988_799);
    assert_eq!(ts.fraction, 0x8000_0000);
}

// ---------------------------------------------------------------------------
// RTP timestamp from the epoch media clock
// ---------------------------------------------------------------------------

#[test]
fn rtp_timestamp_at_the_anchor_is_the_base() {
    let epoch = WallClockRef::new(1_000_000_000_000_000_000, 0, rate_ns());
    assert_eq!(
        rtp_timestamp_at(&epoch, 1_000_000_000_000_000_000, 90_000, 1_000),
        1_000
    );
}

#[test]
fn rtp_timestamp_advances_by_the_clock_rate_per_second() {
    let epoch = WallClockRef::new(1_000_000_000_000_000_000, 0, rate_ns());
    assert_eq!(
        rtp_timestamp_at(&epoch, 1_000_000_001_000_000_000, 90_000, 1_000),
        1_000 + 90_000
    );
}

#[test]
fn rtp_timestamp_wraps_modulo_2_to_32() {
    let epoch = WallClockRef::new(1_000_000_000_000_000_000, 0, rate_ns());
    // One second BEFORE the anchor: 1000 - 90000 wraps mod 2^32.
    assert_eq!(
        rtp_timestamp_at(&epoch, 999_999_999_000_000_000, 90_000, 1_000),
        1_000u32.wrapping_sub(90_000)
    );
}

#[test]
fn rtp_timestamp_handles_a_90khz_epoch_rate_natively() {
    // An epoch already carried in 90 kHz units maps 1:1 onto a 90 kHz RTP clock.
    let epoch = WallClockRef::new(1_000_000_000_000_000_000, 500, Rational::new(90_000, 1));
    assert_eq!(
        rtp_timestamp_at(&epoch, 1_000_000_001_000_000_000, 90_000, 0),
        90_500
    );
}

// ---------------------------------------------------------------------------
// Wire serialization (RFC 3550 §6.4.1, 28 bytes, no report blocks)
// ---------------------------------------------------------------------------

#[test]
fn sender_report_serializes_to_the_exact_28_byte_wire_form() {
    let sr = SenderReport {
        ssrc: 0x1234_5678,
        ntp: NtpTimestamp {
            seconds: 0x8385_0F1E,
            fraction: 0x4000_0000,
        },
        rtp_timestamp: 0x0001_86A0,
        packet_count: 42,
        octet_count: 4242,
    };
    let bytes = sr.to_bytes();
    let expected: [u8; 28] = [
        0x80, 0xC8, 0x00, 0x06, // V=2 P=0 RC=0, PT=200 (SR), length=6 words
        0x12, 0x34, 0x56, 0x78, // SSRC
        0x83, 0x85, 0x0F, 0x1E, // NTP seconds
        0x40, 0x00, 0x00, 0x00, // NTP fraction
        0x00, 0x01, 0x86, 0xA0, // RTP timestamp
        0x00, 0x00, 0x00, 0x2A, // sender packet count
        0x00, 0x00, 0x10, 0x92, // sender octet count
    ];
    assert_eq!(bytes, expected);
}

// ---------------------------------------------------------------------------
// The SrStamper seam over the shared epoch
// ---------------------------------------------------------------------------

#[test]
fn stamper_yields_nothing_before_an_epoch_exists() {
    let epoch = SharedEpoch::new();
    let stamper = SrStamper::new(epoch, 90_000, 0xDEAD_BEEF, 0);
    assert_eq!(
        stamper.report(1_000_000_000_000_000_000, 1, 100),
        None,
        "no epoch ⇒ no SR: a fabricated NTP↔RTP pair is worse than none"
    );
}

#[test]
fn stamper_builds_the_sr_from_the_live_epoch() {
    let cell = SharedEpoch::new();
    cell.set(WallClockRef::new(1_750_000_000_000_000_000, 0, rate_ns()));
    let stamper = SrStamper::new(cell.clone(), 90_000, 0xDEAD_BEEF, 1_000);
    // 0.25 s past the anchor.
    let sr = stamper
        .report(1_750_000_000_250_000_000, 7, 700)
        .expect("epoch present");
    assert_eq!(sr.ssrc, 0xDEAD_BEEF);
    assert_eq!(sr.ntp.seconds, 1_750_000_000 + 2_208_988_800);
    assert_eq!(sr.ntp.fraction, 0x4000_0000);
    assert_eq!(
        sr.rtp_timestamp,
        1_000 + 22_500,
        "0.25 s at 90 kHz = 22 500"
    );
    assert_eq!(sr.packet_count, 7);
    assert_eq!(sr.octet_count, 700);

    // A re-anchored epoch is picked up live (latest-wins cell).
    cell.set(WallClockRef::new(1_750_000_001_000_000_000, 0, rate_ns()));
    let sr2 = stamper
        .report(1_750_000_001_000_000_000, 8, 800)
        .expect("epoch present");
    assert_eq!(
        sr2.rtp_timestamp, 1_000,
        "the new anchor maps this wall to pts 0"
    );
}

#[test]
fn rtsp_server_sink_carries_the_stamper_seam() {
    // The always-compiled RTSP seam consumes the stamper: the serving layer
    // obtains epoch-stamped SRs from the same sink it drains packets from.
    let cell = SharedEpoch::new();
    cell.set(WallClockRef::new(1_750_000_000_000_000_000, 0, rate_ns()));
    let sink = RtspServerSink::new("rtsp0", 8).with_sr_stamper(SrStamper::new(
        cell,
        90_000,
        0x0BAD_F00D,
        0,
    ));
    let sr = sink
        .sender_report(1_750_000_000_500_000_000, 3, 333)
        .expect("stamper attached + epoch present");
    assert_eq!(sr.ssrc, 0x0BAD_F00D);
    assert_eq!(sr.rtp_timestamp, 45_000, "0.5 s at 90 kHz");
    assert_eq!(sr.ntp.fraction, 0x8000_0000);

    // A sink without a stamper reports honestly: no SR.
    let bare = RtspServerSink::new("rtsp1", 8);
    assert_eq!(bare.sender_report(0, 0, 0), None);
}
