//! Pure strict-IDR classifier (GP-1, ADR-0030 §4 boundary-2 recovery gate).
//!
//! These tests run in the **default** (pure-Rust, no-libav) build: the
//! classifier is a byte parser over a coded access unit, so it needs no `FFmpeg`
//! feature. They craft minimal H.264 / HEVC / AV1 fixtures and assert the
//! classifier reports `is_idr` **only** for a true random-access point — never
//! for an open-GOP / recovery-point / inter access unit.
//!
//! Why this is load-bearing: `AV_PKT_FLAG_KEY` / [`ReadPacket::is_key`] is set
//! for HEVC CRA / open-GOP and H.264 recovery-point-SEI I-frames, whose leading
//! pictures reference now-absent frames. Re-anchoring a passthrough there
//! decodes garbage at the seam, so recovery must gate on this stricter test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    // Fixture builders document bitstream layout in prose; the field names that
    // trip `doc_markdown` are spec terms, not Rust items.
    clippy::doc_markdown
)]

use multiview_ffmpeg::idr::{is_idr, CodecKind, NalFraming};

/// Build an Annex-B access unit from `(nal_header_bytes, payload_len)` NAL specs.
/// Each NAL is prefixed with a 4-byte `00 00 00 01` start code, then the header
/// byte(s) the caller supplies, then `payload_len` zero bytes of body.
fn annexb_au(nals: &[(&[u8], usize)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (header, body_len) in nals {
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        out.extend_from_slice(header);
        out.extend(std::iter::repeat_n(0_u8, *body_len));
    }
    out
}

/// Build a 4-byte-length-prefixed (avcC/hvcC style) access unit from NAL specs.
fn length_prefixed_au(nals: &[(&[u8], usize)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (header, body_len) in nals {
        let nal_len = header.len() + *body_len;
        out.extend_from_slice(&u32::try_from(nal_len).unwrap().to_be_bytes());
        out.extend_from_slice(header);
        out.extend(std::iter::repeat_n(0_u8, *body_len));
    }
    out
}

// ---- H.264 ---------------------------------------------------------------

#[test]
fn h264_idr_slice_nal5_is_idr() {
    // NAL header byte: forbidden_zero=0, nal_ref_idc=3 (0x60), type=5 -> 0x65.
    let au = annexb_au(&[(&[0x65], 8)]);
    assert!(
        is_idr(&au, CodecKind::H264, NalFraming::AnnexB),
        "an H.264 IDR slice (nal_unit_type==5) must classify as IDR"
    );
}

#[test]
fn h264_p_slice_nal1_is_not_idr() {
    // Non-IDR coded slice: type 1 (0x41 = ref_idc 2, type 1).
    let au = annexb_au(&[(&[0x41], 8)]);
    assert!(
        !is_idr(&au, CodecKind::H264, NalFraming::AnnexB),
        "an H.264 non-IDR P-slice (nal_unit_type==1) must NOT classify as IDR"
    );
}

#[test]
fn h264_recovery_point_sei_plus_i_is_not_idr() {
    // The recovery-point footgun: an SEI NAL (type 6, e.g. recovery_point) plus a
    // non-IDR coded slice (type 1). FFmpeg may flag this AU as KEY, but it is an
    // open-GOP recovery point, NOT an IDR — re-anchoring here decodes garbage.
    let au = annexb_au(&[(&[0x06], 4), (&[0x41], 8)]);
    assert!(
        !is_idr(&au, CodecKind::H264, NalFraming::AnnexB),
        "an H.264 recovery-point SEI + non-IDR slice must NOT classify as IDR"
    );
}

#[test]
fn h264_idr_in_length_prefixed_framing_is_idr() {
    let au = length_prefixed_au(&[(&[0x67], 6), (&[0x68], 4), (&[0x65], 8)]);
    assert!(
        is_idr(
            &au,
            CodecKind::H264,
            NalFraming::LengthPrefixed { nal_length_size: 4 }
        ),
        "an avcC-framed AU containing an IDR slice (SPS+PPS+IDR) must classify as IDR"
    );
}

// ---- HEVC ----------------------------------------------------------------

/// HEVC two-byte NAL header for `nal_unit_type` with tid 0:
/// byte0 = (type << 1), byte1 = 0x01 (nuh_layer_id 0, nuh_temporal_id_plus1 1).
fn hevc_header(nal_type: u8) -> [u8; 2] {
    [nal_type << 1, 0x01]
}

#[test]
fn hevc_idr_w_radl_19_is_idr() {
    let au = annexb_au(&[(&hevc_header(19), 8)]);
    assert!(
        is_idr(&au, CodecKind::Hevc, NalFraming::AnnexB),
        "HEVC IDR_W_RADL (type 19) must classify as IDR"
    );
}

#[test]
fn hevc_idr_n_lp_20_is_idr() {
    let au = annexb_au(&[(&hevc_header(20), 8)]);
    assert!(
        is_idr(&au, CodecKind::Hevc, NalFraming::AnnexB),
        "HEVC IDR_N_LP (type 20) must classify as IDR"
    );
}

#[test]
fn hevc_cra_21_is_not_idr() {
    // CRA is a random-access point but has leading pictures that may reference
    // frames before the splice -> must NOT be treated as a clean IDR.
    let au = annexb_au(&[(&hevc_header(21), 8)]);
    assert!(
        !is_idr(&au, CodecKind::Hevc, NalFraming::AnnexB),
        "HEVC CRA (type 21) must NOT classify as IDR"
    );
}

#[test]
fn hevc_bla_16_17_18_is_not_idr() {
    for t in [16_u8, 17, 18] {
        let au = annexb_au(&[(&hevc_header(t), 8)]);
        assert!(
            !is_idr(&au, CodecKind::Hevc, NalFraming::AnnexB),
            "HEVC BLA (type {t}) must NOT classify as IDR"
        );
    }
}

#[test]
fn hevc_trail_r_inter_is_not_idr() {
    // TRAIL_R (type 1) is an ordinary inter slice.
    let au = annexb_au(&[(&hevc_header(1), 8)]);
    assert!(
        !is_idr(&au, CodecKind::Hevc, NalFraming::AnnexB),
        "an HEVC inter slice (TRAIL_R, type 1) must NOT classify as IDR"
    );
}

// ---- AV1 -----------------------------------------------------------------

/// Build an AV1 low-overhead OBU with `obu_has_size_field=1`, the given OBU type,
/// `temporal_id`/`spatial_id` 0, and `payload` bytes.
fn av1_obu(obu_type: u8, payload: &[u8]) -> Vec<u8> {
    // forbidden(0) | type(4 bits) | extension_flag(0) | has_size_field(1) | reserved(0)
    let header = (obu_type << 3) | 0b0000_0010;
    let mut out = vec![header];
    // LEB128 size (payloads here are < 128 bytes -> single byte).
    out.push(u8::try_from(payload.len()).unwrap());
    out.extend_from_slice(payload);
    out
}

// AV1 OBU types.
const OBU_SEQUENCE_HEADER: u8 = 1;
const OBU_FRAME: u8 = 6;

/// A frame-OBU payload whose first byte sets `show_existing_frame`,
/// `frame_type`, and `show_frame`. Layout (uncompressed header, no
/// `reduced_still_picture_header`, `frame_id_numbers_present=0`):
/// bit7 show_existing_frame, bits6..5 frame_type, bit4 show_frame, rest unused.
fn av1_frame_payload(show_existing: bool, frame_type: u8, show_frame: bool) -> Vec<u8> {
    let mut b = 0_u8;
    if show_existing {
        b |= 0b1000_0000;
    }
    b |= (frame_type & 0b11) << 5;
    if show_frame {
        b |= 0b0001_0000;
    }
    vec![b, 0x00]
}

#[test]
fn av1_key_temporal_unit_with_seq_header_is_idr() {
    let mut tu = av1_obu(OBU_SEQUENCE_HEADER, &[0x00, 0x00]);
    // frame_type KEY_FRAME=0, show_frame=1, show_existing_frame=0.
    tu.extend(av1_obu(OBU_FRAME, &av1_frame_payload(false, 0, true)));
    assert!(
        is_idr(&tu, CodecKind::Av1, NalFraming::Obu),
        "an AV1 KEY_FRAME temporal unit with a sequence header must classify as IDR"
    );
}

#[test]
fn av1_key_frame_without_seq_header_is_not_idr() {
    // KEY frame but no sequence header in the temporal unit -> not a clean RAP.
    let tu = av1_obu(OBU_FRAME, &av1_frame_payload(false, 0, true));
    assert!(
        !is_idr(&tu, CodecKind::Av1, NalFraming::Obu),
        "an AV1 KEY frame without a sequence-header OBU must NOT classify as IDR"
    );
}

#[test]
fn av1_inter_frame_is_not_idr() {
    let mut tu = av1_obu(OBU_SEQUENCE_HEADER, &[0x00, 0x00]);
    // frame_type INTER_FRAME=1, show_frame=1.
    tu.extend(av1_obu(OBU_FRAME, &av1_frame_payload(false, 1, true)));
    assert!(
        !is_idr(&tu, CodecKind::Av1, NalFraming::Obu),
        "an AV1 inter-frame temporal unit must NOT classify as IDR"
    );
}

#[test]
fn av1_show_existing_frame_is_not_idr() {
    // show_existing_frame=1 is a repeat of a previously decoded frame, never a RAP.
    let mut tu = av1_obu(OBU_SEQUENCE_HEADER, &[0x00, 0x00]);
    tu.extend(av1_obu(OBU_FRAME, &av1_frame_payload(true, 0, true)));
    assert!(
        !is_idr(&tu, CodecKind::Av1, NalFraming::Obu),
        "an AV1 show_existing_frame temporal unit must NOT classify as IDR"
    );
}

// ---- Robustness ----------------------------------------------------------

#[test]
fn empty_and_truncated_inputs_never_classify_as_idr() {
    assert!(!is_idr(&[], CodecKind::H264, NalFraming::AnnexB));
    assert!(!is_idr(&[0x00, 0x00], CodecKind::Hevc, NalFraming::AnnexB));
    assert!(!is_idr(
        &[0x00, 0x00, 0x00, 0x01],
        CodecKind::H264,
        NalFraming::AnnexB
    ));
    assert!(!is_idr(&[0xFF], CodecKind::Av1, NalFraming::Obu));
}

#[test]
fn unknown_codec_is_never_idr() {
    let au = annexb_au(&[(&[0x65], 8)]);
    assert!(
        !is_idr(&au, CodecKind::Other, NalFraming::AnnexB),
        "an unclassifiable codec must conservatively report not-IDR"
    );
}
