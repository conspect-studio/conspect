//! ST 2110-20 video **frame assembler** (pure, always compiled).
//!
//! The depacketizers in [`crate::st2110::v20`] turn each RTP payload into a set
//! of [`SrdSegment`](crate::st2110::v20::SrdSegment)s — byte ranges of packed
//! samples addressed by picture line. This module sits *above* them: it walks the
//! stream of depacketized packets (one [`PacketUnit`] each) and reassembles them
//! into a single raster, closing a frame on the **RFC 4175 marker bit** (the
//! last packet of a frame) or, when that marker is lost, on the next RTP
//! **timestamp change** — every frame in an ST 2110-20 stream carries a distinct
//! 90 kHz media timestamp, so a timestamp step is an unambiguous frame boundary.
//!
//! ## Honest partial frames, never a stall (invariants #1 / #2)
//!
//! Packet loss and reorder are facts of an ST 2110 network. The assembler is a
//! pure state machine over *injected* packets — it never reads a socket, never
//! allocates per packet beyond the one frame buffer it fills, and never blocks:
//!
//! * **Reorder within a frame** is absorbed for free — each segment is written at
//!   its line-addressed offset, so arrival order does not matter.
//! * **A gap in the RTP sequence** (a lost packet) is detected against the last
//!   sequence seen and surfaces as [`AssembledFrame::discontinuity`]; the frame
//!   still closes, carrying only the lines that arrived.
//! * **A lost marker** is recovered by the timestamp-change flush: the previous
//!   frame closes as *partial* ([`AssembledFrame::complete`] = `false`) the
//!   instant a newer timestamp appears.
//! * **End-of-stream** drops any in-progress frame — [`FrameAssembler::finish`]
//!   yields it as a partial for the caller to discard, never awaiting a marker
//!   that will never come.
//!
//! The 90 kHz RTP timestamp is surfaced verbatim as [`AssembledFrame::raw_pts`]
//! (a producer-timebase raw tick); the downstream `PtsNormalizer`
//! ([`crate::normalize`], `WrapBits::Rtp32`) rebases it onto the internal
//! nanosecond timeline — the float-free 90 kHz→ns conversion lives there, not
//! here.

use crate::st2110::v20::V20Payload;

/// An upper bound on the raster buffer the assembler will allocate, in bytes.
///
/// ST 2110-20 tops out around UHD 4:2:2 10-bit (~25 MB/frame); this 64 MB cap
/// rejects a geometry that would force an unreasonable allocation while leaving
/// ample headroom, keeping per-frame memory bounded (invariant #5).
pub const MAX_RASTER_BYTES: usize = 64 * 1024 * 1024;

/// The pixel-buffer geometry the assembler reassembles SRD segments into.
///
/// `bytes_per_line` is the stride of the packed essence (the pgroup byte width of
/// one picture row); the total buffer is `height * bytes_per_line`. The assembler
/// addresses a segment at `line_number * bytes_per_line + byte_offset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RasterGeometry {
    width: u32,
    height: u32,
    bytes_per_line: usize,
    total_bytes: usize,
}

impl RasterGeometry {
    /// Construct a raster geometry, validating that it is non-degenerate and that
    /// the resulting buffer stays within [`MAX_RASTER_BYTES`].
    ///
    /// Returns `None` when `width`, `height`, or `bytes_per_line` is zero, or when
    /// `height * bytes_per_line` overflows `usize` or exceeds the cap.
    #[must_use]
    pub fn new(width: u32, height: u32, bytes_per_line: usize) -> Option<Self> {
        if width == 0 || height == 0 || bytes_per_line == 0 {
            return None;
        }
        let total_bytes = usize::try_from(height)
            .ok()
            .and_then(|h| h.checked_mul(bytes_per_line))?;
        if total_bytes > MAX_RASTER_BYTES {
            return None;
        }
        Some(Self {
            width,
            height,
            bytes_per_line,
            total_bytes,
        })
    }

    /// Raster width in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Raster height in picture lines.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Stride: packed bytes per picture line.
    #[must_use]
    pub const fn bytes_per_line(&self) -> usize {
        self.bytes_per_line
    }

    /// Total reassembly buffer size in bytes.
    #[must_use]
    pub const fn total_bytes(&self) -> usize {
        self.total_bytes
    }
}

/// One depacketized ST 2110-20 packet handed to the assembler.
///
/// Carries the RTP framing fields the assembler keys on (the marker bit, the
/// 90 kHz `timestamp`, and the 16-bit `sequence`) plus the depacketized
/// [`V20Payload`] and the raw `payload` bytes its [`SrdSegment`]s point into.
///
/// [`SrdSegment`]: crate::st2110::v20::SrdSegment
#[derive(Debug, Clone)]
pub struct PacketUnit {
    /// The RFC 4175 marker bit: `true` flags the **last packet of a frame**.
    pub marker: bool,
    /// The 90 kHz RTP media timestamp (one value per video frame).
    pub timestamp: u32,
    /// The 16-bit RTP sequence number (gap detection / reorder).
    pub sequence: u16,
    /// The raw RTP payload the [`SrdSegment`] byte ranges index into.
    ///
    /// [`SrdSegment`]: crate::st2110::v20::SrdSegment
    pub payload: Vec<u8>,
    /// The depacketized -20 payload: the SRD segments this packet carried.
    pub payload_v20: V20Payload,
}

/// A reassembled raster (complete or partial).
///
/// `pixels` is the line-addressed essence buffer; `lines_written` counts the
/// distinct picture lines that received data. A frame is `complete` only when a
/// marker bit closed it; a `discontinuity` is set when a sequence gap (lost
/// packet) was observed while it was being assembled, or when an out-of-range
/// segment had to be dropped.
#[derive(Debug, Clone)]
pub struct AssembledFrame {
    /// `true` iff a marker bit closed this frame (a clean RFC 4175 end-of-frame).
    pub complete: bool,
    /// `true` iff a lost/gapped sequence or a dropped out-of-range segment was
    /// observed while assembling this frame.
    pub discontinuity: bool,
    /// The 90 kHz RTP timestamp, surfaced as a producer-timebase raw pts (the
    /// downstream normalizer rebases it onto the nanosecond timeline).
    pub raw_pts: i64,
    /// The number of distinct picture lines written into `pixels`.
    pub lines_written: usize,
    /// The line-addressed essence buffer (`geometry.total_bytes()` long).
    pub pixels: Vec<u8>,
}

/// The in-progress frame being filled.
#[derive(Debug)]
struct PartialFrame {
    timestamp: u32,
    pixels: Vec<u8>,
    /// One flag per picture line: whether that line received any data.
    lines_seen: Vec<bool>,
    lines_written: usize,
    discontinuity: bool,
    /// The highest in-frame RTP sequence accepted so far (for gap detection).
    last_sequence: Option<u16>,
}

impl PartialFrame {
    fn new(geometry: &RasterGeometry, timestamp: u32) -> Self {
        let line_count = usize::try_from(geometry.height).unwrap_or(0);
        Self {
            timestamp,
            pixels: vec![0_u8; geometry.total_bytes],
            lines_seen: vec![false; line_count],
            lines_written: 0,
            discontinuity: false,
            last_sequence: None,
        }
    }

    fn into_assembled(self, complete: bool) -> AssembledFrame {
        AssembledFrame {
            complete,
            discontinuity: self.discontinuity,
            raw_pts: i64::from(self.timestamp),
            lines_written: self.lines_written,
            pixels: self.pixels,
        }
    }
}

/// The ST 2110-20 frame assembler: a pure state machine over injected packets.
///
/// Holds at most one in-progress frame plus a single closed-but-not-yet-returned
/// frame, so its memory is bounded by two raster buffers regardless of the input
/// (invariant #5). It never reads a socket and never blocks (invariants #1 / #2).
#[derive(Debug)]
pub struct FrameAssembler {
    geometry: RasterGeometry,
    /// The frame currently being filled, if any.
    partial: Option<PartialFrame>,
    /// A frame already closed by this `push` that a single-return call could not
    /// also return; drained on the next `push`. Depth one keeps memory bounded.
    ready: Option<AssembledFrame>,
}

impl FrameAssembler {
    /// Construct an assembler that reassembles into `geometry`.
    #[must_use]
    pub fn new(geometry: RasterGeometry) -> Self {
        Self {
            geometry,
            partial: None,
            ready: None,
        }
    }

    /// The geometry this assembler reassembles into.
    #[must_use]
    pub const fn geometry(&self) -> RasterGeometry {
        self.geometry
    }

    /// Push one depacketized packet, returning a closed frame when one becomes
    /// available.
    ///
    /// A frame closes when this packet carries the marker bit (a clean frame,
    /// `complete = true`) or when its RTP timestamp differs from the in-progress
    /// frame's (the previous frame is flushed as partial, `complete = false`).
    /// At most one frame is returned per call; if a single call both flushes a
    /// previous frame *and* immediately closes a new single-packet frame, the
    /// second is buffered and returned by the next `push`.
    ///
    /// Never blocks and never panics: an out-of-range line or byte offset is
    /// dropped (flagging a discontinuity) rather than written out of bounds.
    pub fn push(&mut self, unit: &PacketUnit) -> Option<AssembledFrame> {
        // Drain any frame closed by a previous call first (bounded depth one).
        let mut emit = self.ready.take();

        // A timestamp change flushes the in-progress (now partial) frame.
        if let Some(active) = self.partial.as_ref() {
            if active.timestamp != unit.timestamp {
                if let Some(flushed) = self.partial.take() {
                    let frame = flushed.into_assembled(false);
                    Self::stage(&mut emit, &mut self.ready, frame);
                }
            }
        }

        // Ensure there is an in-progress frame for this packet's timestamp.
        if self.partial.is_none() {
            self.partial = Some(PartialFrame::new(&self.geometry, unit.timestamp));
        }

        // Accumulate this packet's segments into the in-progress frame.
        if let Some(active) = self.partial.as_mut() {
            Self::accumulate(&self.geometry, active, unit);

            // The marker bit closes the frame cleanly.
            if unit.marker {
                if let Some(done) = self.partial.take() {
                    let frame = done.into_assembled(true);
                    Self::stage(&mut emit, &mut self.ready, frame);
                }
            }
        }

        emit
    }

    /// Stage a freshly closed `frame`: return it now if nothing is being
    /// returned yet, otherwise buffer it in the depth-one `ready` slot. If that
    /// slot is already occupied (an extraordinary triple-close in one call), the
    /// oldest buffered partial is dropped — drop, never grow (invariant #5).
    fn stage(
        emit: &mut Option<AssembledFrame>,
        ready: &mut Option<AssembledFrame>,
        frame: AssembledFrame,
    ) {
        if emit.is_none() {
            *emit = Some(frame);
        } else {
            // Newest-wins on the single ready slot keeps memory bounded.
            *ready = Some(frame);
        }
    }

    /// Write each SRD segment of `unit` into `active` at its line-addressed
    /// offset, updating the line count and the discontinuity flag.
    fn accumulate(geometry: &RasterGeometry, active: &mut PartialFrame, unit: &PacketUnit) {
        // Detect a sequence gap against the last in-frame sequence. The marker
        // packet is included; a non-`+1` forward step means a packet was lost.
        if let Some(prev) = active.last_sequence {
            if unit.sequence != prev.wrapping_add(1)
                && crate::st2110::rtp::seq_after(prev, unit.sequence)
            {
                active.discontinuity = true;
            }
        }
        // Track the newest sequence (ignoring stale reordered packets for the
        // watermark, so a later in-order packet still detects its own gap).
        let is_stale = active
            .last_sequence
            .is_some_and(|prev| !crate::st2110::rtp::seq_after(prev, unit.sequence));
        if !is_stale {
            active.last_sequence = Some(unit.sequence);
        }

        for segment in &unit.payload_v20.segments {
            let line = usize::from(segment.line_number);

            // The source bytes for this segment live in the raw payload.
            let Some(src) = unit.payload.get(segment.data_range()) else {
                // The segment points outside the payload it claims — drop it and
                // flag the loss rather than read out of bounds.
                active.discontinuity = true;
                continue;
            };

            // Compute the in-bounds destination range, or drop the segment.
            let Some(dst_range) = Self::dest_range(geometry, line, segment.offset, src.len())
            else {
                active.discontinuity = true;
                continue;
            };
            let Some(dst) = active.pixels.get_mut(dst_range) else {
                active.discontinuity = true;
                continue;
            };
            dst.copy_from_slice(src);

            if let Some(seen) = active.lines_seen.get_mut(line) {
                if !*seen {
                    *seen = true;
                    active.lines_written = active.lines_written.saturating_add(1);
                }
            }
        }
    }

    /// Compute the destination byte range for a `len`-byte segment placed at
    /// pixel `byte_offset` of picture `line`, or `None` if it would fall outside
    /// the line stride or the buffer (so the caller drops it, never writing out
    /// of bounds). All arithmetic is checked.
    fn dest_range(
        geometry: &RasterGeometry,
        line: usize,
        byte_offset: u16,
        len: usize,
    ) -> Option<core::ops::Range<usize>> {
        if line >= usize::try_from(geometry.height).ok()? {
            return None;
        }
        let line_start = line.checked_mul(geometry.bytes_per_line)?;
        let dst_start = line_start.checked_add(usize::from(byte_offset))?;
        let dst_end = dst_start.checked_add(len)?;
        let line_limit = line_start.checked_add(geometry.bytes_per_line)?;
        if dst_end > line_limit || dst_end > geometry.total_bytes {
            return None;
        }
        Some(dst_start..dst_end)
    }

    /// At end-of-stream, drop and surface any in-progress frame as a partial.
    ///
    /// Consumes the assembler. The returned frame (if any) is always
    /// `complete = false` — the assembler never waits for a marker that will not
    /// arrive (invariant #1). A buffered `ready` frame takes precedence so no
    /// already-closed frame is lost.
    #[must_use]
    pub fn finish(mut self) -> Option<AssembledFrame> {
        if let Some(frame) = self.ready.take() {
            return Some(frame);
        }
        self.partial.take().map(|p| p.into_assembled(false))
    }
}
