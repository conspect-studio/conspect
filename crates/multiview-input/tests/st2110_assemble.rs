//! Frame-assembler tests for the ST 2110-20 receive path (IN-1).
//!
//! The assembler takes the stream of depacketized [`V20Payload`] SRD segments
//! plus the RTP marker bit / 90 kHz timestamp / sequence number and reassembles
//! them into a single raster, closing a frame on the RFC 4175 marker bit (or, if
//! the marker is lost, on the next RTP timestamp change). These golden, injected
//! sequences are the testable core: a full in-order frame assembles; an
//! out-of-order or missing sequence is detected and surfaced as a discontinuity;
//! the marker bit closes the frame; a timestamp change flushes the previous
//! (possibly partial) frame. Real NIC/PTP RX (IN-2) is out of scope here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::cast_possible_truncation
)]

use multiview_input::st2110::assembler::{FrameAssembler, PacketUnit, RasterGeometry};
use multiview_input::st2110::v20::{SrdSegment, V20Payload};
use proptest::prelude::*;

/// A small 4-line raster, 8 bytes per line (toy geometry for the tests).
const W: u16 = 4;
const H: u16 = 4;
const BYTES_PER_LINE: usize = 8;

fn geometry() -> RasterGeometry {
    RasterGeometry::new(u32::from(W), u32::from(H), BYTES_PER_LINE).expect("toy geometry is valid")
}

/// Build a `PacketUnit` carrying one SRD segment for `line` filled with `fill`.
///
/// The returned unit owns its payload bytes; the embedded `V20Payload` segment
/// points back into that payload at offset `0`.
fn line_packet(marker: bool, timestamp: u32, sequence: u16, line: u16, fill: u8) -> PacketUnit {
    let payload = vec![fill; BYTES_PER_LINE];
    let payload_v20 = V20Payload {
        full_sequence: u32::from(sequence),
        segments: vec![SrdSegment {
            line_number: line,
            offset: 0,
            field: false,
            data_start: 0,
            data_len: BYTES_PER_LINE,
        }],
    };
    PacketUnit {
        marker,
        timestamp,
        sequence,
        payload,
        payload_v20,
    }
}

/// A complete in-order frame assembles into one raster on the marker bit, with
/// every line placed at its addressed position.
#[test]
fn complete_frame_assembles_in_order() {
    let mut asm = FrameAssembler::new(geometry());

    // Lines 0..H, the last one carrying the marker bit (RFC 4175 end-of-frame).
    let mut emitted = None;
    for line in 0..H {
        let marker = line == H - 1;
        let pkt = line_packet(marker, 9000, 100 + line, line, 0x10 + (line as u8));
        let out = asm.push(&pkt);
        if marker {
            emitted = out;
        } else {
            assert!(out.is_none(), "mid-frame packet must not close a frame");
        }
    }

    let frame = emitted.expect("the marker packet closes exactly one frame");
    assert!(frame.complete, "a marker-closed full frame is complete");
    assert!(!frame.discontinuity, "in-order frame has no discontinuity");
    assert_eq!(
        frame.raw_pts, 9000,
        "raw_pts carries the 90 kHz RTP timestamp"
    );
    assert_eq!(
        frame.lines_written,
        usize::from(H),
        "every line was written"
    );

    // Each line landed at its addressed offset with the right fill byte.
    for line in 0..H {
        let start = usize::from(line) * BYTES_PER_LINE;
        let end = start + BYTES_PER_LINE;
        assert_eq!(
            &frame.pixels[start..end],
            &vec![0x10 + (line as u8); BYTES_PER_LINE][..],
            "line {line} placed at its addressed byte range",
        );
    }
}

/// The marker bit is what closes the frame: without it, no frame is emitted, and
/// pushing the marker packet emits exactly one frame.
#[test]
fn marker_bit_closes_the_frame() {
    let mut asm = FrameAssembler::new(geometry());

    assert!(asm.push(&line_packet(false, 9000, 0, 0, 0xAA)).is_none());
    assert!(asm.push(&line_packet(false, 9000, 1, 1, 0xBB)).is_none());
    // Marker on this one — the frame closes here.
    let frame = asm
        .push(&line_packet(true, 9000, 2, 2, 0xCC))
        .expect("marker bit closes the frame");
    assert!(frame.complete);
    assert_eq!(frame.raw_pts, 9000);
}

/// A new RTP timestamp arriving before a marker closes the previous (now
/// partial) frame and starts a fresh one. The flush surfaces the partial frame
/// honestly rather than discarding it silently.
#[test]
fn timestamp_change_flushes_previous_partial() {
    let mut asm = FrameAssembler::new(geometry());

    // Two lines of frame @ ts=9000, then a packet at a *new* timestamp with no
    // intervening marker: the ts change flushes the old, partial frame.
    assert!(asm.push(&line_packet(false, 9000, 0, 0, 0x11)).is_none());
    assert!(asm.push(&line_packet(false, 9000, 1, 1, 0x22)).is_none());

    let flushed = asm
        .push(&line_packet(false, 18_000, 2, 0, 0x33))
        .expect("the timestamp change flushes the previous frame");
    assert!(
        !flushed.complete,
        "a frame closed by a timestamp change (no marker) is partial",
    );
    assert_eq!(flushed.raw_pts, 9000, "the flushed frame keeps its own pts");
    assert_eq!(
        flushed.lines_written, 2,
        "only the two received lines landed"
    );

    // The new frame is now in progress at ts=18000; closing it with a marker
    // yields a fresh, distinct frame.
    let next = asm
        .push(&line_packet(true, 18_000, 3, 1, 0x44))
        .expect("marker closes the second frame");
    assert_eq!(next.raw_pts, 18_000);
}

/// A gap in the RTP sequence within a frame is detected and surfaced as a
/// discontinuity on the assembled frame (a packet was lost), never a panic and
/// never a stall.
#[test]
fn sequence_gap_marks_discontinuity() {
    let mut asm = FrameAssembler::new(geometry());

    // seq 0, then seq 2 (seq 1 missing), then the marker on seq 3.
    assert!(asm.push(&line_packet(false, 9000, 0, 0, 0x01)).is_none());
    assert!(asm.push(&line_packet(false, 9000, 2, 2, 0x03)).is_none());
    let frame = asm
        .push(&line_packet(true, 9000, 3, 3, 0x04))
        .expect("marker closes the frame");
    assert!(
        frame.discontinuity,
        "a sequence gap within the frame surfaces as a discontinuity",
    );
}

/// An out-of-order (older) sequence arriving after a newer one is still placed
/// (the line-addressed buffer tolerates reorder within a frame) and the frame
/// closes on the marker without panic.
#[test]
fn out_of_order_within_frame_is_placed() {
    let mut asm = FrameAssembler::new(geometry());

    // Receive line 2 before line 0 and line 1 (reorder within the frame).
    assert!(asm.push(&line_packet(false, 9000, 2, 2, 0xC0)).is_none());
    assert!(asm.push(&line_packet(false, 9000, 0, 0, 0xA0)).is_none());
    assert!(asm.push(&line_packet(false, 9000, 1, 1, 0xB0)).is_none());
    let frame = asm
        .push(&line_packet(true, 9000, 3, 3, 0xD0))
        .expect("marker closes the reordered frame");

    // Lines landed at their addressed positions regardless of arrival order.
    assert_eq!(&frame.pixels[0..BYTES_PER_LINE], &[0xA0; BYTES_PER_LINE]);
    assert_eq!(
        &frame.pixels[BYTES_PER_LINE..2 * BYTES_PER_LINE],
        &[0xB0; BYTES_PER_LINE]
    );
    assert_eq!(
        &frame.pixels[2 * BYTES_PER_LINE..3 * BYTES_PER_LINE],
        &[0xC0; BYTES_PER_LINE]
    );
}

/// At true end-of-stream a pending partial frame is dropped, never awaited
/// (invariant #1): `finish` consumes the assembler and yields nothing if the
/// in-progress frame never saw a marker — the caller does not block on it.
#[test]
fn eos_partial_is_dropped_not_awaited() {
    let mut asm = FrameAssembler::new(geometry());
    assert!(asm.push(&line_packet(false, 9000, 0, 0, 0x55)).is_none());
    // No marker ever arrives; dropping the assembler must not stall or emit a
    // "completed" frame.
    let leftover = asm.finish();
    assert!(
        leftover.is_none_or(|f| !f.complete),
        "an unfinished frame at EOS is never reported complete",
    );
}

/// A monotonic `raw_pts`: successive completed frames carry non-decreasing pts.
#[test]
fn raw_pts_is_monotonic_across_frames() {
    let mut asm = FrameAssembler::new(geometry());
    let mut last = None;
    for (i, ts) in [9000u32, 18_000, 27_000].into_iter().enumerate() {
        let seq = (i * 2) as u16;
        let frame = asm
            .push(&line_packet(true, ts, seq, 0, 0x01))
            .expect("each single-packet marker frame closes immediately");
        if let Some(prev) = last {
            assert!(frame.raw_pts >= prev, "raw_pts is monotonic across frames");
        }
        last = Some(frame.raw_pts);
    }
}

proptest! {
    /// The assembler never panics on arbitrary injected packet sequences:
    /// random markers, timestamps, sequence numbers, and line indices (some out
    /// of raster bounds) must surface as drops/partials, never an unwind.
    #[test]
    fn assembler_never_panics(
        ops in proptest::collection::vec(
            (any::<bool>(), any::<u32>(), any::<u16>(), any::<u16>(), any::<u8>()),
            0..64,
        ),
    ) {
        let mut asm = FrameAssembler::new(geometry());
        for (marker, ts, seq, line, fill) in ops {
            let pkt = line_packet(marker, ts, seq, line, fill);
            let _ = asm.push(&pkt);
        }
        let _ = asm.finish();
    }
}
