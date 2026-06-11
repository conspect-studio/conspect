//! Per-AU parameter-set probe + drift detection (GP-2, ADR-0030 §4).
//!
//! These tests run in the **default** (pure-Rust, no-libav) build: the probe is
//! a byte parser over a coded access unit / codec extradata, so it needs no
//! `ffmpeg` feature. They craft minimal H.264 / HEVC / AV1 fixtures and assert
//! that:
//!
//! * two access units carrying **bit-identical** in-band parameter sets report
//!   **no drift** (the cached slate stays valid);
//! * an access unit whose active SPS / sequence-header bytes **change** reports
//!   **drift** (the slate must be invalidated and re-baked);
//! * an access unit carrying **no** in-band parameter sets carries the previous
//!   snapshot forward — never a false positive;
//! * an initial snapshot can be parsed out of avcC / hvcC **extradata**.
//!
//! Why this is load-bearing (ADR-0030 §4 "No clean IDR / mid-stream param
//! change"): `Demuxer::stream_parameters()` snapshots the container's view at
//! open time and does **not** update mid-stream. A mid-stream resolution /
//! profile / chroma / bit-depth change is only visible as a change in the
//! **in-band** SPS / VPS / sequence-header bytes. GP-2 is the primitive that
//! detects that drift so a later slice can invalidate + re-bake the param-matched
//! slate.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    // Fixture builders document bitstream layout in prose; the field names that
    // trip `doc_markdown` are spec terms, not Rust items.
    clippy::doc_markdown
)]

use multiview_ffmpeg::idr::{CodecKind, NalFraming};
use multiview_input::param_probe::{diff, ParamSetClass, ParamSnapshot, StreamParamProbe};

// ---- fixture builders ----------------------------------------------------

/// Build an Annex-B access unit from `(nal_header_bytes, payload)` NAL specs.
/// Each NAL is prefixed with a 4-byte `00 00 00 01` start code, then the header
/// byte(s), then the body bytes the caller supplies.
fn annexb_au(nals: &[(&[u8], &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (header, body) in nals {
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        out.extend_from_slice(header);
        out.extend_from_slice(body);
    }
    out
}

/// Build a 4-byte-length-prefixed (avcC/hvcC style) access unit from NAL specs.
fn length_prefixed_au(nals: &[(&[u8], &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (header, body) in nals {
        let nal_len = header.len() + body.len();
        out.extend_from_slice(&u32::try_from(nal_len).unwrap().to_be_bytes());
        out.extend_from_slice(header);
        out.extend_from_slice(body);
    }
    out
}

/// HEVC two-byte NAL header for `nal_unit_type` with tid 0:
/// byte0 = (type << 1), byte1 = 0x01 (nuh_layer_id 0, nuh_temporal_id_plus1 1).
fn hevc_header(nal_type: u8) -> [u8; 2] {
    [nal_type << 1, 0x01]
}

/// Build an AV1 low-overhead OBU with `obu_has_size_field=1`, the given OBU
/// type, `temporal_id`/`spatial_id` 0, and `payload` bytes.
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

// H.264 NAL header bytes: forbidden=0, ref_idc=3 (0x60) | type.
const H264_SPS: u8 = 0x67; // type 7
const H264_PPS: u8 = 0x68; // type 8
const H264_IDR: u8 = 0x65; // type 5
const H264_NONIDR: u8 = 0x41; // ref_idc 2, type 1

// HEVC NAL types.
const HEVC_VPS: u8 = 32;
const HEVC_SPS: u8 = 33;
const HEVC_PPS: u8 = 34;
const HEVC_IDR_W_RADL: u8 = 19;
const HEVC_TRAIL_R: u8 = 1;

// ---- H.264 ---------------------------------------------------------------

#[test]
fn h264_identical_sps_pps_across_two_aus_is_no_drift() {
    let sps_body: &[u8] = &[0x42, 0x00, 0x1F, 0xAB, 0xCD];
    let pps_body: &[u8] = &[0xCE, 0x3C, 0x80];
    let au1 = annexb_au(&[
        (&[H264_SPS], sps_body),
        (&[H264_PPS], pps_body),
        (&[H264_IDR], &[0x88, 0x99]),
    ]);
    let au2 = annexb_au(&[
        (&[H264_SPS], sps_body),
        (&[H264_PPS], pps_body),
        (&[H264_IDR], &[0x11, 0x22, 0x33]),
    ]);

    let snap = StreamParamProbe::snapshot_from_au(&au1, CodecKind::H264, NalFraming::AnnexB)
        .expect("au1 carries an SPS+PPS");
    let drift = diff(&snap, &au2, CodecKind::H264, NalFraming::AnnexB);
    assert!(
        !drift.changed,
        "bit-identical SPS/PPS in the second AU must report NO drift"
    );
}

#[test]
fn h264_changed_sps_profile_idc_is_drift() {
    // profile_idc lives in the first SPS body byte; flip it 0x42 -> 0x64.
    let sps_v1: &[u8] = &[0x42, 0x00, 0x1F, 0xAB, 0xCD];
    let sps_v2: &[u8] = &[0x64, 0x00, 0x1F, 0xAB, 0xCD];
    let pps_body: &[u8] = &[0xCE, 0x3C, 0x80];
    let au1 = annexb_au(&[
        (&[H264_SPS], sps_v1),
        (&[H264_PPS], pps_body),
        (&[H264_IDR], &[0xAA]),
    ]);
    let au2 = annexb_au(&[
        (&[H264_SPS], sps_v2),
        (&[H264_PPS], pps_body),
        (&[H264_IDR], &[0xAA]),
    ]);

    let snap = StreamParamProbe::snapshot_from_au(&au1, CodecKind::H264, NalFraming::AnnexB)
        .expect("au1 carries an SPS");
    let drift = diff(&snap, &au2, CodecKind::H264, NalFraming::AnnexB);
    assert!(
        drift.changed,
        "a changed SPS (profile_idc) must report drift=true"
    );
    assert!(
        drift.which.contains(&ParamSetClass::Sps),
        "the drift report must name the SPS as the changed parameter set"
    );
}

#[test]
fn h264_changed_pps_only_is_drift_naming_pps() {
    let sps_body: &[u8] = &[0x42, 0x00, 0x1F];
    let pps_v1: &[u8] = &[0xCE, 0x3C, 0x80];
    let pps_v2: &[u8] = &[0xCE, 0x3C, 0x81];
    let au1 = annexb_au(&[(&[H264_SPS], sps_body), (&[H264_PPS], pps_v1)]);
    let au2 = annexb_au(&[(&[H264_SPS], sps_body), (&[H264_PPS], pps_v2)]);

    let snap = StreamParamProbe::snapshot_from_au(&au1, CodecKind::H264, NalFraming::AnnexB)
        .expect("au1 carries SPS+PPS");
    let drift = diff(&snap, &au2, CodecKind::H264, NalFraming::AnnexB);
    assert!(drift.changed, "a changed PPS must report drift");
    assert!(
        drift.which.contains(&ParamSetClass::Pps),
        "the drift report must name the PPS"
    );
    assert!(
        !drift.which.contains(&ParamSetClass::Sps),
        "an unchanged SPS must NOT be reported as drifted"
    );
}

#[test]
fn h264_au_with_no_parameter_sets_carries_forward_no_drift() {
    let sps_body: &[u8] = &[0x42, 0x00, 0x1F];
    let pps_body: &[u8] = &[0xCE, 0x3C, 0x80];
    let au_with_ps = annexb_au(&[
        (&[H264_SPS], sps_body),
        (&[H264_PPS], pps_body),
        (&[H264_IDR], &[0x01]),
    ]);
    // A plain inter AU: no SPS/PPS, just a non-IDR slice.
    let inter_au = annexb_au(&[(&[H264_NONIDR], &[0xDE, 0xAD, 0xBE, 0xEF])]);

    let snap = StreamParamProbe::snapshot_from_au(&au_with_ps, CodecKind::H264, NalFraming::AnnexB)
        .expect("first AU carries SPS+PPS");
    let drift = diff(&snap, &inter_au, CodecKind::H264, NalFraming::AnnexB);
    assert!(
        !drift.changed,
        "an AU with NO in-band parameter sets must carry the snapshot forward (no false positive)"
    );
}

#[test]
fn h264_multiple_pps_ids_tracked_independently() {
    // Two PPS with distinct pps_id (first body byte ~ ue(v) id in real streams;
    // here we just key by the parsed id our probe assigns). Changing PPS#1 only
    // must report drift on PPS, not on PPS#0.
    let sps_body: &[u8] = &[0x42, 0x00, 0x1F];
    let pps0_v1: &[u8] = &[0x00, 0x3C, 0x80];
    let pps1_v1: &[u8] = &[0x01, 0x3C, 0x80];
    let pps1_v2: &[u8] = &[0x01, 0x3C, 0x81];
    let au1 = annexb_au(&[
        (&[H264_SPS], sps_body),
        (&[H264_PPS], pps0_v1),
        (&[H264_PPS], pps1_v1),
    ]);
    let au2 = annexb_au(&[
        (&[H264_SPS], sps_body),
        (&[H264_PPS], pps0_v1),
        (&[H264_PPS], pps1_v2),
    ]);

    let snap = StreamParamProbe::snapshot_from_au(&au1, CodecKind::H264, NalFraming::AnnexB)
        .expect("au1 carries SPS + 2 PPS");
    let drift = diff(&snap, &au2, CodecKind::H264, NalFraming::AnnexB);
    assert!(
        drift.changed,
        "changing one of two PPS payloads must report drift"
    );
    assert!(drift.which.contains(&ParamSetClass::Pps));
}

#[test]
fn h264_length_prefixed_framing_probes_the_same() {
    let sps_body: &[u8] = &[0x42, 0x00, 0x1F];
    let pps_body: &[u8] = &[0xCE, 0x3C, 0x80];
    let framing = NalFraming::LengthPrefixed { nal_length_size: 4 };
    let au1 = length_prefixed_au(&[
        (&[H264_SPS], sps_body),
        (&[H264_PPS], pps_body),
        (&[H264_IDR], &[0x01]),
    ]);
    let au2 = length_prefixed_au(&[
        (&[H264_SPS], sps_body),
        (&[H264_PPS], pps_body),
        (&[H264_IDR], &[0x02]),
    ]);

    let snap = StreamParamProbe::snapshot_from_au(&au1, CodecKind::H264, framing)
        .expect("avcC-framed au1 carries SPS+PPS");
    assert!(
        !diff(&snap, &au2, CodecKind::H264, framing).changed,
        "identical SPS/PPS in avcC framing must report no drift"
    );

    let sps_v2: &[u8] = &[0x64, 0x00, 0x1F];
    let au3 = length_prefixed_au(&[(&[H264_SPS], sps_v2), (&[H264_PPS], pps_body)]);
    assert!(
        diff(&snap, &au3, CodecKind::H264, framing).changed,
        "a changed SPS in avcC framing must report drift"
    );
}

// ---- HEVC ----------------------------------------------------------------

#[test]
fn hevc_identical_vps_sps_pps_is_no_drift() {
    let vps_body: &[u8] = &[0x0C, 0x01, 0xFF];
    let sps_body: &[u8] = &[0x01, 0x60, 0x00];
    let pps_body: &[u8] = &[0xC1, 0x73, 0xC0];
    let au1 = annexb_au(&[
        (&hevc_header(HEVC_VPS), vps_body),
        (&hevc_header(HEVC_SPS), sps_body),
        (&hevc_header(HEVC_PPS), pps_body),
        (&hevc_header(HEVC_IDR_W_RADL), &[0xAA, 0xBB]),
    ]);
    let au2 = annexb_au(&[
        (&hevc_header(HEVC_VPS), vps_body),
        (&hevc_header(HEVC_SPS), sps_body),
        (&hevc_header(HEVC_PPS), pps_body),
        (&hevc_header(HEVC_TRAIL_R), &[0x11]),
    ]);

    let snap = StreamParamProbe::snapshot_from_au(&au1, CodecKind::Hevc, NalFraming::AnnexB)
        .expect("au1 carries VPS+SPS+PPS");
    assert!(
        !diff(&snap, &au2, CodecKind::Hevc, NalFraming::AnnexB).changed,
        "bit-identical HEVC VPS/SPS/PPS must report no drift"
    );
}

#[test]
fn hevc_changed_sps_is_drift_naming_sps() {
    let vps_body: &[u8] = &[0x0C, 0x01, 0xFF];
    let sps_v1: &[u8] = &[0x01, 0x60, 0x00];
    let sps_v2: &[u8] = &[0x01, 0x60, 0x10];
    let pps_body: &[u8] = &[0xC1, 0x73, 0xC0];
    let au1 = annexb_au(&[
        (&hevc_header(HEVC_VPS), vps_body),
        (&hevc_header(HEVC_SPS), sps_v1),
        (&hevc_header(HEVC_PPS), pps_body),
    ]);
    let au2 = annexb_au(&[
        (&hevc_header(HEVC_VPS), vps_body),
        (&hevc_header(HEVC_SPS), sps_v2),
        (&hevc_header(HEVC_PPS), pps_body),
    ]);

    let snap = StreamParamProbe::snapshot_from_au(&au1, CodecKind::Hevc, NalFraming::AnnexB)
        .expect("au1 carries VPS+SPS+PPS");
    let drift = diff(&snap, &au2, CodecKind::Hevc, NalFraming::AnnexB);
    assert!(drift.changed, "a changed HEVC SPS must report drift");
    assert!(
        drift.which.contains(&ParamSetClass::Sps),
        "the drift report must name the SPS"
    );
    assert!(
        !drift.which.contains(&ParamSetClass::Vps),
        "the unchanged VPS must NOT be reported"
    );
}

#[test]
fn hevc_au_with_no_parameter_sets_is_no_drift() {
    let vps_body: &[u8] = &[0x0C, 0x01, 0xFF];
    let sps_body: &[u8] = &[0x01, 0x60, 0x00];
    let pps_body: &[u8] = &[0xC1, 0x73, 0xC0];
    let au1 = annexb_au(&[
        (&hevc_header(HEVC_VPS), vps_body),
        (&hevc_header(HEVC_SPS), sps_body),
        (&hevc_header(HEVC_PPS), pps_body),
    ]);
    let inter = annexb_au(&[(&hevc_header(HEVC_TRAIL_R), &[0xDE, 0xAD])]);

    let snap = StreamParamProbe::snapshot_from_au(&au1, CodecKind::Hevc, NalFraming::AnnexB)
        .expect("au1 carries VPS+SPS+PPS");
    assert!(
        !diff(&snap, &inter, CodecKind::Hevc, NalFraming::AnnexB).changed,
        "an HEVC inter AU with no in-band parameter sets must carry forward (no drift)"
    );
}

// ---- AV1 -----------------------------------------------------------------

#[test]
fn av1_identical_sequence_header_is_no_drift() {
    let seq_body: &[u8] = &[0x00, 0x11, 0x22, 0x33];
    let au1 = {
        let mut tu = av1_obu(OBU_SEQUENCE_HEADER, seq_body);
        tu.extend(av1_obu(OBU_FRAME, &[0x10, 0x00]));
        tu
    };
    let au2 = {
        let mut tu = av1_obu(OBU_SEQUENCE_HEADER, seq_body);
        tu.extend(av1_obu(OBU_FRAME, &[0x30, 0x00])); // a different frame OBU
        tu
    };

    let snap = StreamParamProbe::snapshot_from_au(&au1, CodecKind::Av1, NalFraming::Obu)
        .expect("au1 carries a sequence-header OBU");
    assert!(
        !diff(&snap, &au2, CodecKind::Av1, NalFraming::Obu).changed,
        "an AV1 AU with a bit-identical sequence header must report no drift"
    );
}

#[test]
fn av1_changed_sequence_header_is_drift() {
    let seq_v1: &[u8] = &[0x00, 0x11, 0x22, 0x33];
    let seq_v2: &[u8] = &[0x00, 0x11, 0x22, 0x44]; // a changed seq-hdr byte
    let au1 = av1_obu(OBU_SEQUENCE_HEADER, seq_v1);
    let au2 = av1_obu(OBU_SEQUENCE_HEADER, seq_v2);

    let snap = StreamParamProbe::snapshot_from_au(&au1, CodecKind::Av1, NalFraming::Obu)
        .expect("au1 carries a sequence-header OBU");
    let drift = diff(&snap, &au2, CodecKind::Av1, NalFraming::Obu);
    assert!(
        drift.changed,
        "a changed AV1 sequence header must report drift"
    );
    assert!(
        drift.which.contains(&ParamSetClass::SequenceHeader),
        "the drift report must name the sequence header"
    );
}

#[test]
fn av1_au_with_no_sequence_header_is_no_drift() {
    let seq_body: &[u8] = &[0x00, 0x11, 0x22, 0x33];
    let au1 = av1_obu(OBU_SEQUENCE_HEADER, seq_body);
    // A temporal unit with only a frame OBU (no seq header) — common steady state.
    let frame_only = av1_obu(OBU_FRAME, &[0x10, 0x00]);

    let snap = StreamParamProbe::snapshot_from_au(&au1, CodecKind::Av1, NalFraming::Obu)
        .expect("au1 carries a sequence-header OBU");
    assert!(
        !diff(&snap, &frame_only, CodecKind::Av1, NalFraming::Obu).changed,
        "an AV1 AU with no sequence-header OBU must carry forward (no drift)"
    );
}

// ---- extradata-only initial snapshot -------------------------------------

/// Build an avcC config record (ISO 14496-15) carrying one SPS and one PPS.
fn avcc_extradata(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut out = vec![
        0x01,                                // configurationVersion
        sps.get(1).copied().unwrap_or(0x42), // AVCProfileIndication
        0x00,                                // profile_compatibility
        sps.get(3).copied().unwrap_or(0x1F), // AVCLevelIndication
        0xFF,                                // 0b111111 | lengthSizeMinusOne(3) -> 4-byte lengths
        0xE1,                                // 0b111 | numOfSequenceParameterSets(1)
    ];
    out.extend_from_slice(&u16::try_from(sps.len()).unwrap().to_be_bytes());
    out.extend_from_slice(sps);
    out.push(0x01); // numOfPictureParameterSets
    out.extend_from_slice(&u16::try_from(pps.len()).unwrap().to_be_bytes());
    out.extend_from_slice(pps);
    out
}

#[test]
fn h264_initial_snapshot_from_avcc_extradata_matches_inband() {
    // SPS/PPS NALs INCLUDING the 1-byte NAL header (as they appear in avcC).
    let sps_nal: &[u8] = &[H264_SPS, 0x42, 0x00, 0x1F, 0xAB, 0xCD];
    let pps_nal: &[u8] = &[H264_PPS, 0xCE, 0x3C, 0x80];
    let extradata = avcc_extradata(sps_nal, pps_nal);

    let snap = StreamParamProbe::from_extradata(CodecKind::H264, &extradata)
        .expect("avcC extradata carries an SPS + PPS");

    // An AU repeating the SAME SPS/PPS in-band (Annex-B) must show no drift
    // against the extradata-derived snapshot — proving the snapshot parsed the
    // parameter-set bytes out of the config record correctly.
    let inband = annexb_au(&[
        (&[H264_SPS], &sps_nal[1..]),
        (&[H264_PPS], &pps_nal[1..]),
        (&[H264_IDR], &[0x99]),
    ]);
    assert!(
        !diff(&snap, &inband, CodecKind::H264, NalFraming::AnnexB).changed,
        "an in-band AU repeating the extradata SPS/PPS must report no drift"
    );

    // A DIFFERENT in-band SPS must drift against the extradata snapshot.
    let changed = annexb_au(&[
        (&[H264_SPS], &[0x64, 0x00, 0x1F, 0xAB, 0xCD]),
        (&[H264_PPS], &pps_nal[1..]),
    ]);
    assert!(
        diff(&snap, &changed, CodecKind::H264, NalFraming::AnnexB).changed,
        "a changed in-band SPS must drift against the extradata-derived snapshot"
    );
}

#[test]
fn extradata_snapshot_reports_present_classes() {
    let sps_nal: &[u8] = &[H264_SPS, 0x42, 0x00, 0x1F];
    let pps_nal: &[u8] = &[H264_PPS, 0xCE, 0x3C, 0x80];
    let extradata = avcc_extradata(sps_nal, pps_nal);
    let snap = StreamParamProbe::from_extradata(CodecKind::H264, &extradata)
        .expect("avcC extradata carries an SPS + PPS");
    assert!(
        snap.has(ParamSetClass::Sps),
        "snapshot must contain the SPS"
    );
    assert!(
        snap.has(ParamSetClass::Pps),
        "snapshot must contain the PPS"
    );
}

// ---- robustness ----------------------------------------------------------

#[test]
fn no_parameter_sets_anywhere_yields_no_snapshot() {
    // An AU with only a slice (no SPS) has nothing to snapshot.
    let au = annexb_au(&[(&[H264_NONIDR], &[0x00, 0x01, 0x02])]);
    assert!(
        StreamParamProbe::snapshot_from_au(&au, CodecKind::H264, NalFraming::AnnexB).is_none(),
        "an AU with no parameter sets must yield no snapshot"
    );
}

#[test]
fn empty_snapshot_diffed_against_ps_au_is_drift() {
    // A snapshot with no parameter sets, diffed against an AU that DOES carry a
    // fresh SPS, is a drift (a parameter set appeared) — never a silent miss.
    let empty = ParamSnapshot::empty(CodecKind::H264);
    let au = annexb_au(&[(&[H264_SPS], &[0x42, 0x00, 0x1F]), (&[H264_PPS], &[0xCE])]);
    let drift = diff(&empty, &au, CodecKind::H264, NalFraming::AnnexB);
    assert!(
        drift.changed,
        "a parameter set appearing against an empty snapshot must report drift"
    );
}

#[test]
fn other_codec_never_drifts() {
    let au = annexb_au(&[(&[H264_SPS], &[0x42])]);
    let snap = ParamSnapshot::empty(CodecKind::Other);
    assert!(
        !diff(&snap, &au, CodecKind::Other, NalFraming::AnnexB).changed,
        "an unmodelled codec must conservatively report no drift"
    );
    assert!(
        StreamParamProbe::snapshot_from_au(&au, CodecKind::Other, NalFraming::AnnexB).is_none(),
        "an unmodelled codec yields no snapshot"
    );
}

#[test]
fn truncated_inputs_never_panic() {
    let snap = ParamSnapshot::empty(CodecKind::H264);
    // Truncated Annex-B (start code, no NAL).
    let _ = diff(
        &snap,
        &[0x00, 0x00, 0x00, 0x01],
        CodecKind::H264,
        NalFraming::AnnexB,
    );
    // Truncated length-prefix (claims 4 bytes, has 1).
    let _ = diff(
        &snap,
        &[0x00, 0x00, 0x00, 0x10, 0x67],
        CodecKind::H264,
        NalFraming::LengthPrefixed { nal_length_size: 4 },
    );
    // Truncated extradata.
    assert!(StreamParamProbe::from_extradata(CodecKind::H264, &[0x01, 0x42]).is_none());
    assert!(StreamParamProbe::from_extradata(CodecKind::H264, &[]).is_none());
}

// ---- properties ----------------------------------------------------------

mod properties {
    use super::{annexb_au, H264_IDR, H264_NONIDR, H264_PPS, H264_SPS};
    use multiview_ffmpeg::idr::{CodecKind, NalFraming};
    use multiview_input::param_probe::{diff, ParamSnapshot, StreamParamProbe};
    use proptest::prelude::*;

    /// Build an H.264 Annex-B AU carrying an SPS + PPS with the given bodies, then
    /// an IDR slice (so it is always a valid parameter-set-bearing AU).
    fn h264_ps_au(sps_body: &[u8], pps_body: &[u8]) -> Vec<u8> {
        annexb_au(&[
            (&[H264_SPS], sps_body),
            (&[H264_PPS], pps_body),
            (&[H264_IDR], &[0x80]),
        ])
    }

    proptest! {
        /// Reflexivity: an AU diffed against the snapshot taken FROM THAT SAME AU
        /// never drifts (a bit-identical parameter set is never a false positive).
        /// Bodies end in a non-zero byte so the Annex-B trailing-zero trim that
        /// strips start-code framing leaves the payload identical on both sides.
        #[test]
        fn snapshot_of_au_never_drifts_against_itself(
            mut sps in proptest::collection::vec(any::<u8>(), 1..16),
            mut pps in proptest::collection::vec(any::<u8>(), 1..16),
        ) {
            *sps.last_mut().unwrap() |= 0x01;
            *pps.last_mut().unwrap() |= 0x01;
            let au = h264_ps_au(&sps, &pps);
            let snap = StreamParamProbe::snapshot_from_au(&au, CodecKind::H264, NalFraming::AnnexB)
                .expect("the AU carries an SPS + PPS");
            let drift = diff(&snap, &au, CodecKind::H264, NalFraming::AnnexB);
            prop_assert!(!drift.changed, "an AU must not drift against its own snapshot");
        }

        /// Carry-forward: an AU carrying NO in-band parameter set (just an inter
        /// slice) never reports drift, for ANY snapshot — no false positives.
        #[test]
        fn inter_only_au_never_drifts(
            sps in proptest::collection::vec(any::<u8>(), 1..16),
            slice in proptest::collection::vec(any::<u8>(), 1..32),
        ) {
            let snap_au = h264_ps_au(&sps, &[0x80]);
            let snap = StreamParamProbe::snapshot_from_au(&snap_au, CodecKind::H264, NalFraming::AnnexB)
                .expect("the snapshot AU carries parameter sets");
            let inter = annexb_au(&[(&[H264_NONIDR], &slice)]);
            let drift = diff(&snap, &inter, CodecKind::H264, NalFraming::AnnexB);
            prop_assert!(
                !drift.changed,
                "an AU with no in-band parameter set must carry the snapshot forward"
            );
        }

        /// Robustness: arbitrary bytes, codec, and framing never panic, and the
        /// snapshot/diff are total. (`diff` against an empty snapshot may report a
        /// fresh-set appearance — that is fine; we only assert it returns.)
        #[test]
        fn arbitrary_bytes_never_panic(
            bytes in proptest::collection::vec(any::<u8>(), 0..256),
            codec_sel in 0_u8..4,
            length_size in 0_u8..6,
            annexb in any::<bool>(),
        ) {
            let codec = match codec_sel {
                0 => CodecKind::H264,
                1 => CodecKind::Hevc,
                2 => CodecKind::Av1,
                _ => CodecKind::Other,
            };
            let framing = if annexb {
                NalFraming::AnnexB
            } else {
                NalFraming::LengthPrefixed { nal_length_size: length_size }
            };
            let snap = ParamSnapshot::empty(codec);
            let _ = StreamParamProbe::snapshot_from_au(&bytes, codec, framing);
            let _ = StreamParamProbe::from_extradata(codec, &bytes);
            let _ = diff(&snap, &bytes, codec, framing);
            prop_assert!(true);
        }
    }
}
