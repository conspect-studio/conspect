//! Packet-fed H.264 access-unit decoder for WHIP/WebRTC ingest (ADR-T014).
//!
//! The WebRTC path has no container: the RTP depacketizer
//! (`multiview-input`'s keyframe-gated `H264Depacketizer`, RFC 6184) hands
//! over reassembled NAL / access-unit bytes, and [`H264PacketDecoder`] is the
//! avcodec-only (no `AVFormatContext`) decoder that turns them into the
//! standard NV12 pipeline frames:
//!
//! * **Geometry comes from the bitstream** — the in-band SPS, never a declared
//!   constructor argument (the ADR-T014 fix for the scaffold's
//!   declared-geometry dishonesty). A publisher that changes resolution
//!   mid-session simply yields new [`FrameMeta`](multiview_core::frame::FrameMeta)
//!   downstream. Note libav's per-frame colour tags are **sticky** across an
//!   in-band SPS change: when the new VUI omits colour description, frames
//!   keep the previous stream's tags rather than reverting to `Unspecified`.
//! * **`ColorInfo` comes from the H.264 VUI** via the crate's shared
//!   color-detection ([`crate::convert::color_from_ff`]); untagged axes stay
//!   `Unspecified` and the BT.709-by-geometry defaulting happens downstream in
//!   `resolve_defaults` — the same policy as every other compressed ingest
//!   (invariant #8: detect, never guess).
//! * **Framing is normalized** with [`crate::annexb::to_annexb`]: Annex-B
//!   passes through zero-copy; bare NALs, STAP-A payloads, and AVCC
//!   length-prefixed AUs are converted. SDP `sprop-parameter-sets` need no
//!   special seam — push the SPS/PPS bytes like any NAL (parameter sets are
//!   buffered into the next access unit); conforming WHIP publishers repeat
//!   them in-band anyway.
//!
//! ## Access-unit coalescing
//! libav's H.264 decoder requires each submitted packet to be one access
//! unit and rejects a packet with no VCL NAL (`AVERROR_INVALIDDATA`,
//! "no frame!") — but the depacketizer emits **per-NAL**: SPS, PPS, AUD, SEI,
//! and slices arrive as separate pushes. The decoder therefore coalesces
//! pushed NALs into access units and submits one packet per AU, detecting the
//! boundary by H.264 §7.4.1.2.3/.4 rules:
//!
//! * a pushed **raw PTS change** while a VCL NAL is pending (the RTP rule —
//!   one timestamp per access unit);
//! * a **non-VCL prefix NAL** (SEI/SPS/PPS/AUD) arriving after a pending VCL
//!   NAL;
//! * a **VCL NAL starting a new picture** (`first_mb_in_slice == 0`, read
//!   from the first slice-header bit) after a pending VCL NAL — so a
//!   multi-slice picture's slices stay in one AU.
//!
//! A whole-AU pusher (each push one complete access unit) is detected by the
//! same rules at the next push, costing at most one push of latency; the
//! pending AU is bounded by [`MAX_PENDING_AU_BYTES`] (the depacketizer's own
//! reassembly cap) — drop-never-grow (CLAUDE.md §7 rule 5).
//!
//! ## Contract (sampled, never pacing — invariants #1/#2)
//! [`push`](H264PacketDecoder::push) is non-blocking and may surface zero or
//! more frames on the subsequent [`receive_frame`](H264PacketDecoder::receive_frame)
//! drain. B-frames are tolerated defensively: libav reorders to display order,
//! so a nonconforming publisher costs latency, never corruption. The raw PTS
//! pushed with each AU (the verbatim 32-bit RTP timestamp on the 90 kHz clock)
//! rides through as [`DecodedVideoFrame::raw_pts`] for the downstream
//! `PtsNormalizer` (`WrapBits::Rtp32`, ADR-T003); nothing here paces anything.

use ffmpeg_next as ffmpeg;

use multiview_core::time::Rational;

use crate::annexb::{nal_bodies, to_annexb};
use crate::decode::ensure_initialized;
use crate::decode_stream::{DecodedVideoFrame, StreamVideoDecoder};
use crate::error::{FfmpegError, Result};

/// The 4-byte start code used when assembling the pending access unit.
const START_CODE: [u8; 4] = [0x00, 0x00, 0x00, 0x01];

/// Cap on the coalesced pending access unit — the same 8 MiB bound the RTP
/// depacketizer enforces on reassembly (`MAX_ACCESS_UNIT_BYTES`, ADR-T014).
/// A pusher that never presents an AU boundary has the oversized AU submitted
/// at the cap (the decoder conceals) rather than growing without bound.
pub const MAX_PENDING_AU_BYTES: usize = 8 * 1024 * 1024;

/// An avcodec-only H.264 decoder fed reassembled access units / NAL bytes.
///
/// Owns its libav decoder context (`Send + !Sync`, freed in `Drop` by
/// `ffmpeg_next`); the receive path is the crate's shared
/// [`StreamVideoDecoder`] machinery, so frames leave on the canonical NV12
/// timeline (invariant #5) with the same metadata as every demuxed source.
pub struct H264PacketDecoder {
    inner: StreamVideoDecoder,
    /// The access unit being coalesced, as Annex-B bytes.
    pending: Vec<u8>,
    /// The raw PTS of the pending access unit (its first push's PTS).
    pending_pts: Option<i64>,
    /// Whether the pending access unit already holds a VCL (slice) NAL.
    pending_has_vcl: bool,
}

impl H264PacketDecoder {
    /// Open the linked libav H.264 software decoder with no stream parameters
    /// — geometry and colour are read from the in-band bitstream (SPS/VUI).
    ///
    /// `time_base` is the clock the pushed raw PTS values tick on; WebRTC
    /// video is the RTP 90 kHz clock, `Rational::new(1, 90_000)` (RFC 6184).
    /// It only rescales the convenience `meta.pts` nanosecond view; the
    /// verbatim raw PTS is surfaced separately for wrap-aware normalization.
    ///
    /// # Errors
    /// * [`FfmpegError::Init`] — global libav init failed.
    /// * [`FfmpegError::CodecNotFound`] — the linked `FFmpeg` has no H.264
    ///   decoder.
    /// * [`FfmpegError::OpenDecoder`] — the decoder could not be opened.
    pub fn new(time_base: Rational) -> Result<Self> {
        ensure_initialized()?;
        let codec = ffmpeg::decoder::find(ffmpeg::codec::Id::H264)
            .ok_or(FfmpegError::CodecNotFound("h264"))?;
        let ctx = ffmpeg::codec::context::Context::new_with_codec(codec);
        let decoder = ctx.decoder().video().map_err(FfmpegError::OpenDecoder)?;
        Ok(Self {
            inner: StreamVideoDecoder::from_parts(decoder, time_base),
            pending: Vec::new(),
            pending_pts: None,
            pending_has_vcl: false,
        })
    }

    /// Push one NAL unit or one whole access unit (the depacketizer emits
    /// per-NAL; AVCC-normalizing producers emit per-AU) with its raw PTS in
    /// `time_base` ticks.
    ///
    /// Accepts Annex-B, AVCC length-prefixed, raw STAP-A, or bare NAL bytes
    /// (normalized via [`to_annexb`]); an empty push is a no-op. Non-blocking:
    /// zero or more frames become available on the next
    /// [`receive_frame`](Self::receive_frame) drain (an access unit is
    /// submitted to libav once its boundary is seen — see the
    /// [module docs](self)). The bytes are copied once into the pending AU
    /// (the depacketizer keeps ownership of its buffer).
    ///
    /// # Errors
    /// Returns [`FfmpegError::Decode`] on a libav send error — including a
    /// structurally corrupt access unit the decoder rejects outright. For a
    /// live session that is a *recoverable* condition: warn (rate-limited by
    /// the libav log bridge), drop the AU, and ride last-good (invariant #2);
    /// the typed error is surfaced so the ingest supervisor owns that policy.
    pub fn push(&mut self, au: &[u8], raw_pts: Option<i64>) -> Result<()> {
        if au.is_empty() {
            return Ok(());
        }
        let normalized = to_annexb(au);

        // RTP access-unit rule: one timestamp per AU. A PTS change while a
        // slice is pending closes the previous access unit.
        if self.pending_has_vcl
            && raw_pts.is_some()
            && self.pending_pts.is_some()
            && raw_pts != self.pending_pts
        {
            self.submit_pending()?;
        }

        for nal in nal_bodies(&normalized) {
            if self.pending_has_vcl && starts_new_access_unit(nal) {
                self.submit_pending()?;
            }
            if self.pending.len().saturating_add(nal.len()) > MAX_PENDING_AU_BYTES {
                // Drop-never-grow: a boundary-free pusher gets the oversized
                // AU submitted at the cap (the decoder conceals). A pending
                // buffer with NO VCL cannot be submitted (libav rejects
                // slice-less packets), so an over-cap non-VCL head — SEI /
                // parameter-set spam — is DROPPED outright instead; holding it
                // would grow without bound.
                self.submit_pending()?;
                if !self.pending_has_vcl {
                    self.pending.clear();
                    self.pending_pts = None;
                }
            }
            if self.pending.is_empty() {
                self.pending_pts = raw_pts;
            }
            self.pending.extend_from_slice(&START_CODE);
            self.pending.extend_from_slice(nal);
            if is_vcl(nal) {
                self.pending_has_vcl = true;
            }
        }
        Ok(())
    }

    /// Bytes currently buffered in the pending (not-yet-submitted) access
    /// unit. Observability for telemetry and the bounded-memory contract:
    /// always `<=` [`MAX_PENDING_AU_BYTES`] plus one in-flight NAL.
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Pull the next decoded frame (NV12, SPS geometry, VUI colour), or
    /// `Ok(None)` when the decoder needs more input / is fully drained.
    ///
    /// # Errors
    /// Returns [`FfmpegError::Decode`] for a real libav error, or
    /// [`FfmpegError::Convert`] if the NV12 conversion fails.
    pub fn receive_frame(&mut self) -> Result<Option<DecodedVideoFrame>> {
        self.inner.receive_frame()
    }

    /// Flush (session teardown): submit the final pending access unit and
    /// signal end-of-stream so buffered frames can be drained with
    /// [`receive_frame`](Self::receive_frame).
    ///
    /// # Errors
    /// Returns [`FfmpegError::Decode`] on a libav error.
    pub fn send_eof(&mut self) -> Result<()> {
        self.submit_pending()?;
        self.inner.send_eof()
    }

    /// Submit the pending access unit to libav as one packet, stamped with the
    /// AU's raw PTS. A pending buffer with **no** VCL NAL is *not* submitted:
    /// libav rejects slice-less packets (`"no frame!"`), and loose parameter
    /// sets belong to the *next* access unit — they stay pending up to
    /// [`MAX_PENDING_AU_BYTES`] (past the cap, [`Self::push`] drops the
    /// non-VCL head; at EOF they are dropped: there is nothing they could
    /// decode).
    fn submit_pending(&mut self) -> Result<()> {
        if !self.pending_has_vcl {
            // Parameter sets / SEI only: keep them as the head of the next AU
            // (PTS will be adopted from the AU's first slice-carrying push).
            return Ok(());
        }
        let mut packet = ffmpeg::codec::packet::Packet::copy(&self.pending);
        // PTS only: with B-frame publishers the DTS is unknowable here and the
        // H.264 decoder does not need it (it reorders by POC/PTS itself).
        packet.set_pts(self.pending_pts);
        self.pending.clear();
        self.pending_pts = None;
        self.pending_has_vcl = false;
        self.inner.send_packet(&packet)
    }
}

/// Whether a NAL body is a VCL (coded slice) NAL: types 1–5.
fn is_vcl(nal: &[u8]) -> bool {
    nal.first()
        .is_some_and(|&header| matches!(header & 0x1F, 1..=5))
}

/// Whether this NAL starts a **new** access unit when it follows a VCL NAL
/// (H.264 §7.4.1.2.3/.4):
///
/// * SEI (6), SPS (7), PPS (8), AUD (9) always precede the next AU's slices;
///   end-of-sequence/stream (10/11) *terminate the current* AU (§7.4.1.2.3) —
///   grouping them with the close boundary yields the same AU submissions;
/// * a VCL NAL whose `first_mb_in_slice == 0` starts a new picture. The field
///   is the slice header's leading `ue(v)`; the value 0 codes as a single `1`
///   bit, so the test is the MSB of the first payload byte.
fn starts_new_access_unit(nal: &[u8]) -> bool {
    let Some(&header) = nal.first() else {
        return false;
    };
    match header & 0x1F {
        6..=11 => true,
        1..=5 => nal.get(1).is_some_and(|&b| (b & 0x80) != 0),
        _ => false,
    }
}

// `H264PacketDecoder` owns a libav decoder context that must not be shared
// across threads without synchronization: like the other decoders here it is
// `Send` (it moves to the WHIP ingest thread) and intentionally `!Sync`.

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{is_vcl, starts_new_access_unit};

    #[test]
    fn vcl_classification_covers_slice_types_only() {
        assert!(is_vcl(&[0x65, 0x88])); // IDR slice (type 5)
        assert!(is_vcl(&[0x41, 0x9A])); // non-IDR slice (type 1)
        assert!(!is_vcl(&[0x67, 0x42])); // SPS
        assert!(!is_vcl(&[0x68, 0xCE])); // PPS
        assert!(!is_vcl(&[0x09, 0x10])); // AUD
        assert!(!is_vcl(&[0x06, 0x05])); // SEI
        assert!(!is_vcl(&[]));
    }

    #[test]
    fn au_boundary_rules_match_h264_semantics() {
        // Non-VCL prefix NALs start a new AU after slices.
        assert!(starts_new_access_unit(&[0x09, 0x10])); // AUD
        assert!(starts_new_access_unit(&[0x67, 0x42])); // SPS
        assert!(starts_new_access_unit(&[0x68, 0xCE])); // PPS
        assert!(starts_new_access_unit(&[0x06, 0x05])); // SEI
                                                        // First slice of a picture: first_mb_in_slice == 0 -> leading bit set.
        assert!(starts_new_access_unit(&[0x65, 0x88]));
        // A continuation slice (first_mb_in_slice > 0): leading bit clear.
        assert!(!starts_new_access_unit(&[0x65, 0x42]));
        // Unknown/reserved types never split.
        assert!(!starts_new_access_unit(&[0x1F, 0xFF]));
        assert!(!starts_new_access_unit(&[]));
    }
}
