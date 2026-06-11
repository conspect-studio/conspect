//! Strict-IDR classifier (GP-1, ADR-0030 §4 boundary-2 recovery gate).
//!
//! Re-anchoring a guarded passthrough on recovery must happen **only** at a true
//! random-access point — a clean IDR whose decode needs nothing before it. The
//! obvious test, FFmpeg's `AV_PKT_FLAG_KEY` (see [`crate::demux::ReadPacket::is_key`]),
//! is **not** that: libav also flags HEVC **CRA / open-GOP** access units and
//! H.264 **recovery-point-SEI** I-frames as "key", yet their leading pictures
//! reference frames that are now absent across an outage. Re-anchoring there
//! decodes garbage at the seam (ADR-0030 boundary 2).
//!
//! [`is_idr`] is a cheap **header inspection** over the coded access-unit bytes —
//! it never decodes a sample. It is a pure function over `(&[u8], CodecKind,
//! NalFraming)` with **no** libav dependency, so the whole classifier is
//! exhaustively unit-testable in the default (pure-Rust) build. The feature-gated
//! [`crate::demux::ReadPacket::is_idr`] wires it to a demuxed packet by supplying
//! the stream's codec and extradata framing.
//!
//! ## Strict random-access points
//! * **H.264** — a NAL with `nal_unit_type == 5` (IDR slice) is present.
//!   Recovery-point SEI (type 6) over a non-IDR slice (type 1) is rejected.
//! * **HEVC** — a NAL with `nal_unit_type` in `{IDR_W_RADL = 19, IDR_N_LP = 20}`.
//!   `CRA (21)` and `BLA (16, 17, 18)` are rejected.
//! * **AV1** — the temporal unit contains a sequence-header OBU **and** a frame
//!   (or frame-header) OBU that is `frame_type == KEY_FRAME`, `show_frame == 1`,
//!   `show_existing_frame == 0`, with `temporal_id == 0`.
//!
//! Any malformed, truncated, empty, or unclassifiable input conservatively
//! returns `false` (never a false-IDR): a missed IDR merely delays recovery,
//! whereas a false IDR splices garbage.

/// The coded video codec of an access unit, as far as the IDR classifier cares.
///
/// Mirrors the libav codec id, reduced to the codecs whose random-access
/// structure this classifier understands; everything else is [`CodecKind::Other`]
/// and is conservatively never an IDR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CodecKind {
    /// H.264 / AVC (NAL-based; IDR is `nal_unit_type == 5`).
    H264,
    /// H.265 / HEVC (two-byte NAL header; IDR is type 19 or 20).
    Hevc,
    /// AV1 (OBU-based; IDR is a KEY frame in a temporal unit with a seq header).
    Av1,
    /// Any other codec — conservatively never reported as an IDR.
    Other,
}

/// How NAL / OBU units are delimited inside a coded access unit.
///
/// H.264 / HEVC packets are carried either as Annex-B (`00 00 01` / `00 00 00 01`
/// start codes — MPEG-TS, RTP, raw) or length-prefixed (avcC / hvcC — MP4 / mov /
/// fMP4) with a 1-, 2-, or 4-byte big-endian length before each NAL. AV1 uses the
/// low-overhead OBU stream (`obu_has_size_field`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NalFraming {
    /// Annex-B start-code framing (`00 00 01` or `00 00 00 01`).
    AnnexB,
    /// Length-prefixed framing (avcC / hvcC); each NAL is preceded by an
    /// `nal_length_size`-byte big-endian length.
    LengthPrefixed {
        /// The NAL length-prefix width in bytes (1, 2, or 4).
        nal_length_size: u8,
    },
    /// AV1 low-overhead OBU framing (each OBU carries its own size field).
    Obu,
}

/// Classify a coded access unit as a strict IDR / random-access point.
///
/// Returns `true` **only** for a clean random-access point that needs nothing
/// decoded before it (see the [module docs](self) for the exact per-codec rule).
/// Returns `false` for inter pictures, open-GOP CRA, BLA, recovery-point I-frames,
/// `show_existing_frame` repeats, and any malformed / truncated / unknown input.
#[must_use]
pub fn is_idr(au: &[u8], codec: CodecKind, framing: NalFraming) -> bool {
    match codec {
        CodecKind::H264 => h264_is_idr(au, framing),
        CodecKind::Hevc => hevc_is_idr(au, framing),
        CodecKind::Av1 => av1_is_idr(au),
        CodecKind::Other => false,
    }
}

/// H.264: at least one `nal_unit_type == 5` (IDR slice) NAL is present.
fn h264_is_idr(au: &[u8], framing: NalFraming) -> bool {
    for_each_nal(au, framing, |nal| {
        // NAL header byte: forbidden_zero_bit(1) | nal_ref_idc(2) | type(5).
        nal.first().is_some_and(|&b| (b & 0x1F) == 5)
    })
}

/// HEVC: at least one NAL of type `IDR_W_RADL` (19) or `IDR_N_LP` (20).
fn hevc_is_idr(au: &[u8], framing: NalFraming) -> bool {
    for_each_nal(au, framing, |nal| {
        // Two-byte NAL header: forbidden_zero(1) | type(6) | layer_id(6) | tid1(3).
        // type lives in bits 1..=6 of the first byte: (b0 >> 1) & 0x3F.
        nal.first()
            .map(|&b| (b >> 1) & 0x3F)
            .is_some_and(|t| t == 19 || t == 20)
    })
}

/// Run `pred` over each NAL unit of `au` under `framing`; `true` on the first hit.
///
/// `Obu` framing is not a NAL framing — it can never carry an H.264/HEVC NAL, so
/// it conservatively yields no units (and thus `false`).
fn for_each_nal(au: &[u8], framing: NalFraming, mut pred: impl FnMut(&[u8]) -> bool) -> bool {
    match framing {
        NalFraming::AnnexB => annexb_any_nal(au, &mut pred),
        NalFraming::LengthPrefixed { nal_length_size } => {
            length_prefixed_any_nal(au, nal_length_size, &mut pred)
        }
        NalFraming::Obu => false,
    }
}

/// Iterate Annex-B NAL units (split on `00 00 01` start codes, tolerating a
/// 4-byte `00 00 00 01` variant), returning `true` on the first `pred` hit.
fn annexb_any_nal(au: &[u8], pred: &mut impl FnMut(&[u8]) -> bool) -> bool {
    let mut starts: Vec<usize> = Vec::new();
    let mut i = 0_usize;
    // Find every `00 00 01` prefix; the NAL body begins just after it.
    while let Some(window) = au.get(i..i + 3) {
        if window == [0x00, 0x00, 0x01] {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    for (n, &body_start) in starts.iter().enumerate() {
        // The NAL runs to the next start code (minus any trailing 00 of a
        // 4-byte start prefix) or the end of the buffer.
        let body_end = starts
            .get(n + 1)
            .map_or(au.len(), |&next_start| next_start.saturating_sub(3));
        if let Some(nal) = au.get(body_start..body_end) {
            if !nal.is_empty() && pred(nal) {
                return true;
            }
        }
    }
    false
}

/// Iterate length-prefixed NAL units (avcC / hvcC), returning `true` on the
/// first `pred` hit. A `nal_length_size` outside 1..=4, or a length that runs
/// past the buffer, ends iteration safely.
fn length_prefixed_any_nal(
    au: &[u8],
    nal_length_size: u8,
    pred: &mut impl FnMut(&[u8]) -> bool,
) -> bool {
    let size = usize::from(nal_length_size);
    if !(1..=4).contains(&size) {
        return false;
    }
    let mut offset = 0_usize;
    while let Some(len_bytes) = au.get(offset..offset + size) {
        let mut nal_len = 0_usize;
        for &b in len_bytes {
            nal_len = (nal_len << 8) | usize::from(b);
        }
        let body_start = offset + size;
        let Some(body_end) = body_start.checked_add(nal_len) else {
            return false;
        };
        let Some(nal) = au.get(body_start..body_end) else {
            return false;
        };
        if !nal.is_empty() && pred(nal) {
            return true;
        }
        offset = body_end;
        if nal_len == 0 {
            // A zero length would loop forever; a real stream never has one.
            return false;
        }
    }
    false
}

/// AV1 OBU type constants (`obu_type`, the 4 bits after the forbidden bit).
const OBU_SEQUENCE_HEADER: u8 = 1;
const OBU_FRAME_HEADER: u8 = 3;
const OBU_FRAME: u8 = 6;

/// AV1: the temporal unit carries a sequence-header OBU **and** a clean KEY frame.
///
/// Parses the low-overhead OBU stream (every OBU has `obu_has_size_field == 1`),
/// requiring `temporal_id == 0` on the frame OBU, and reads the first byte of the
/// frame's uncompressed header for `show_existing_frame`, `frame_type`, and
/// `show_frame`.
fn av1_is_idr(au: &[u8]) -> bool {
    let mut seen_seq_header = false;
    let mut clean_key_frame = false;

    let mut offset = 0_usize;
    while let Some(&header) = au.get(offset) {
        // OBU header: forbidden(1) | type(4) | extension_flag(1) | has_size(1) | reserved(1).
        let obu_type = (header >> 3) & 0x0F;
        let extension_flag = (header & 0b0000_0100) != 0;
        let has_size_field = (header & 0b0000_0010) != 0;
        let mut cursor = offset + 1;

        let mut temporal_id = 0_u8;
        if extension_flag {
            let Some(&ext) = au.get(cursor) else {
                return false;
            };
            temporal_id = (ext >> 5) & 0x07;
            cursor += 1;
        }

        // Without a size field we cannot safely advance to the next OBU.
        if !has_size_field {
            return false;
        }
        let Some((obu_size, after_leb)) = read_leb128(au, cursor) else {
            return false;
        };
        let payload_start = after_leb;
        let Some(payload_end) = payload_start.checked_add(obu_size) else {
            return false;
        };
        let Some(payload) = au.get(payload_start..payload_end) else {
            return false;
        };

        match obu_type {
            OBU_SEQUENCE_HEADER => seen_seq_header = true,
            OBU_FRAME | OBU_FRAME_HEADER
                if temporal_id == 0 && frame_header_is_clean_key(payload) =>
            {
                clean_key_frame = true;
            }
            _ => {}
        }

        offset = payload_end;
    }

    seen_seq_header && clean_key_frame
}

/// Read whether an AV1 frame/frame-header OBU payload is a clean KEY frame.
///
/// The uncompressed header (no `reduced_still_picture_header`, the common live
/// encode) begins with `show_existing_frame` (1 bit); if 0, `frame_type` (2 bits)
/// and `show_frame` (1 bit) follow. A KEY frame is `frame_type == 0`.
fn frame_header_is_clean_key(payload: &[u8]) -> bool {
    let Some(&first) = payload.first() else {
        return false;
    };
    let show_existing_frame = (first & 0b1000_0000) != 0;
    if show_existing_frame {
        return false;
    }
    let frame_type = (first >> 5) & 0b11;
    let show_frame = (first & 0b0001_0000) != 0;
    frame_type == 0 && show_frame
}

/// Decode an unsigned LEB128 value starting at `offset`, returning the value and
/// the offset just past it. AV1 LEB128 is at most 8 bytes; a value that would not
/// fit a `usize`, or a buffer that ends mid-value, yields `None`.
fn read_leb128(buf: &[u8], offset: usize) -> Option<(usize, usize)> {
    let mut value = 0_u64;
    let mut i = offset;
    for shift in 0..8_u32 {
        let &byte = buf.get(i)?;
        value |= u64::from(byte & 0x7F).checked_shl(shift * 7)?;
        i += 1;
        if byte & 0x80 == 0 {
            let value = usize::try_from(value).ok()?;
            return Some((value, i));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{is_idr, read_leb128, CodecKind, NalFraming};

    #[test]
    fn leb128_single_and_multi_byte() {
        assert_eq!(read_leb128(&[0x00], 0), Some((0, 1)));
        assert_eq!(read_leb128(&[0x7F], 0), Some((127, 1)));
        // 0x80 0x01 -> 128
        assert_eq!(read_leb128(&[0x80, 0x01], 0), Some((128, 2)));
        // truncated continuation byte -> None
        assert_eq!(read_leb128(&[0x80], 0), None);
    }

    #[test]
    fn annexb_three_byte_start_code_idr_is_detected() {
        // 3-byte start code variant (00 00 01) with an H.264 IDR NAL.
        let au = [0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB];
        assert!(is_idr(&au, CodecKind::H264, NalFraming::AnnexB));
    }

    #[test]
    fn length_prefixed_bad_size_is_safe() {
        // nal_length_size of 0 or 8 is invalid -> never IDR, never panics.
        let au = [0x00, 0x00, 0x00, 0x01, 0x65];
        assert!(!is_idr(
            &au,
            CodecKind::H264,
            NalFraming::LengthPrefixed { nal_length_size: 0 }
        ));
        assert!(!is_idr(
            &au,
            CodecKind::H264,
            NalFraming::LengthPrefixed { nal_length_size: 8 }
        ));
    }

    #[test]
    fn obu_framing_never_carries_an_h264_nal() {
        let au = [0x65, 0xAA];
        assert!(!is_idr(&au, CodecKind::H264, NalFraming::Obu));
    }
}
