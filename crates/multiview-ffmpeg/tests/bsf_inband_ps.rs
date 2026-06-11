//! GP-3 in-band parameter-set / Annex-B framing BSF stage (ADR-0030 §4
//! "Framing prerequisite") — `ffmpeg`-feature-gated integration test.
//!
//! A guarded passthrough splices a pre-baked slate into a *copied* elementary
//! stream. For a continuous-ES splice the active SPS/PPS **must** be repeated
//! in-band immediately before **both** the slate IDR and the recovery IDR, so a
//! decoder that re-acquires across the seam needs nothing out-of-band. An mp4
//! (avcC, length-prefixed) H.264 source carries its SPS/PPS **only** in the
//! container's extradata — its keyframe access units have **no** in-band
//! parameter sets — which is exactly the case GP-3 must fix.
//!
//! This suite builds such a source with the `FFmpeg` CLI, demuxes it with the
//! safe [`Demuxer`], pushes the length-prefixed access units through the GP-3
//! [`BsfChain`], and asserts via the **GP-1** `idr` NAL parser (no decode) that
//! every output access unit is **Annex-B framed** and carries **SPS (7) + PPS
//! (8) in-band immediately before the IDR slice (5)**. "Verify, don't assert":
//! it counts the actual NAL types per access unit at the seam.
//!
//! Gated behind the `ffmpeg` feature (needs libavcodec's bitstream filters).
//! `FFmpeg` ships no LGPL H.264 *encoder*, so the clip is produced with
//! `libx264` **only if present**; when the linked `FFmpeg` has no H.264 encoder
//! each test is a no-op (it asserts nothing it cannot first produce — never a
//! weakened assertion).
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use multiview_core::time::Rational;
use multiview_ffmpeg::bsf_select::{BsfFraming, InputFraming};
use multiview_ffmpeg::convert::MediaKind;
use multiview_ffmpeg::idr::{CodecKind, NalFraming};
use multiview_ffmpeg::{BsfChain, Demuxer};

/// Whether the linked `FFmpeg` CLI exposes an H.264 encoder we can use to bake
/// the avcC source clip. Without one this whole suite cannot construct its
/// fixture, so each test early-returns (skips) rather than fail on a missing
/// encoder.
fn has_h264_encoder() -> bool {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-h", "encoder=libx264"])
        .output()
        .is_ok_and(|o| {
            let text = String::from_utf8_lossy(&o.stdout);
            o.status.success() && text.contains("Encoder libx264")
        })
}

/// Generate a short avcC (mp4, length-prefixed) H.264 clip whose keyframe access
/// units carry **no** in-band SPS/PPS — the parameter sets live only in the mp4
/// `avcC` extradata. A 2-second clip at 10 fps with a 10-frame GOP yields two
/// IDRs, so the test exercises the "before EVERY keyframe" guarantee.
fn generate_avcc_h264(dir: &Path) -> PathBuf {
    let out = dir.join("src.mp4");
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=160x120:rate=10",
            "-t",
            "2",
            "-c:v",
            "libx264",
            "-g",
            "10",
            "-keyint_min",
            "10",
            "-sc_threshold",
            "0",
            "-x264-params",
            // repeat-headers=0 keeps SPS/PPS OUT of the elementary stream so the
            // avcC source genuinely has no in-band parameter sets at keyframes —
            // the precondition GP-3 must repair.
            "repeat-headers=0",
            "-pix_fmt",
            "yuv420p",
            // Default mp4 muxer → avcC (length-prefixed) framing.
            "-movflags",
            "+faststart",
        ])
        .arg(&out)
        .status()
        .expect("spawn ffmpeg CLI");
    assert!(status.success(), "ffmpeg CLI failed to generate avcC clip");
    out
}

/// Split an Annex-B access unit into the ordered list of its NAL `type` bytes
/// (`nal_unit_type`, the low 5 bits of each NAL header). Used to verify the exact
/// NAL ordering at each access unit — SPS(7), PPS(8) before the IDR(5).
fn annexb_nal_types(au: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut i = 0_usize;
    let mut nal_start: Option<usize> = None;
    while i + 3 <= au.len() {
        if au[i] == 0x00 && au[i + 1] == 0x00 && au[i + 2] == 0x01 {
            if let Some(start) = nal_start.take() {
                if let Some(&h) = au.get(start) {
                    types.push(h & 0x1F);
                }
            }
            nal_start = Some(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    if let Some(start) = nal_start {
        if let Some(&h) = au.get(start) {
            types.push(h & 0x1F);
        }
    }
    types
}

/// Whether `au` is Annex-B framed (begins with a `00 00 01` / `00 00 00 01`
/// start code).
fn is_annexb_framed(au: &[u8]) -> bool {
    au.starts_with(&[0x00, 0x00, 0x01]) || au.starts_with(&[0x00, 0x00, 0x00, 0x01])
}

#[test]
fn gp3_repeats_sps_pps_in_band_before_every_idr_and_emits_annexb() {
    if !has_h264_encoder() {
        eprintln!("skipping: linked FFmpeg has no libx264 encoder to build the avcC fixture");
        return;
    }

    let dir = tempfile::TempDir::new().unwrap();
    let clip = generate_avcc_h264(dir.path());

    // Demux the avcC source: its access units are length-prefixed and (by
    // construction) carry no in-band SPS/PPS.
    let mut demux = Demuxer::open(&clip).expect("open avcC source");
    let streams = demux.streams();
    let video = streams
        .iter()
        .find(|s| s.kind == MediaKind::Video)
        .expect("video stream");
    let vidx = video.index;
    assert_eq!(video.codec_name, "h264", "fixture must be H.264");

    // Confirm the precondition: the source is length-prefixed (avcC).
    let (codec, in_framing) = demux
        .stream_idr_framing(vidx)
        .expect("h264 framing resolves");
    assert_eq!(codec, CodecKind::H264);
    assert!(
        matches!(in_framing, NalFraming::LengthPrefixed { .. }),
        "fixture must be avcC length-prefixed, got {in_framing:?}"
    );

    // Build the GP-3 chain from the stream's codec parameters, asking for the
    // Annex-B in-band-PS framing a continuous-ES splice target needs.
    let params = demux
        .stream_parameters(vidx)
        .expect("clone stream parameters");
    let mut chain = BsfChain::new(
        CodecKind::H264,
        InputFraming::LengthPrefixed,
        BsfFraming::AnnexBInBand,
        &params,
        video.time_base,
    )
    .expect("build GP-3 BsfChain");

    // Push every video access unit through the chain and collect the filtered
    // output access units' raw bytes. Along the way capture the FIRST input
    // keyframe's raw bytes to prove non-vacuity: the avcC input is NOT what the
    // assertions below check for, so GP-3 is genuinely load-bearing.
    let mut filtered_aus: Vec<Vec<u8>> = Vec::new();
    let mut first_input_keyframe: Option<Vec<u8>> = None;
    while let Some(pkt) = demux.read_packet().expect("read") {
        if pkt.stream_index != vidx {
            continue;
        }
        if first_input_keyframe.is_none() && pkt.is_key() {
            if let Some(bytes) = pkt.packet.data() {
                first_input_keyframe = Some(bytes.to_vec());
            }
        }
        chain.send_packet(&pkt.packet).expect("send to bsf chain");
        while let Some(out) = chain.receive_packet().expect("receive from bsf chain") {
            if let Some(bytes) = out.data() {
                filtered_aus.push(bytes.to_vec());
            }
        }
    }

    // Non-vacuity: the raw avcC input keyframe is length-prefixed, so it is NOT
    // Annex-B framed and the Annex-B NAL walk finds no in-band SPS/PPS — exactly
    // the deficiency GP-3 repairs. (If this ever held on the input, the output
    // assertions would be trivially satisfiable.)
    let raw_idr = first_input_keyframe.expect("the source must contain a keyframe");
    assert!(
        !is_annexb_framed(&raw_idr),
        "precondition: the avcC input keyframe must NOT already be Annex-B framed"
    );
    let raw_types = annexb_nal_types(&raw_idr);
    assert!(
        !raw_types.contains(&7) && !raw_types.contains(&8),
        "precondition: the avcC input keyframe must carry NO in-band SPS/PPS under an Annex-B walk; saw {raw_types:?}"
    );
    // Drain trailing filtered packets.
    chain.send_eof().expect("eof to bsf chain");
    while let Some(out) = chain.receive_packet().expect("drain bsf chain") {
        if let Some(bytes) = out.data() {
            filtered_aus.push(bytes.to_vec());
        }
    }

    assert_annexb_with_ps_before_every_idr(&filtered_aus);
}

/// The GP-3 output contract: every access unit is Annex-B framed, and every IDR
/// (per the GP-1 strict classifier) carries SPS (7) then PPS (8) in-band before
/// its IDR slice (5). Requires at least two IDRs so the "before EVERY keyframe"
/// guarantee — not just at stream-start — is genuinely exercised. "Verify, don't
/// assert": this counts the actual per-AU NAL types at each seam.
fn assert_annexb_with_ps_before_every_idr(filtered_aus: &[Vec<u8>]) {
    assert!(
        !filtered_aus.is_empty(),
        "the GP-3 chain must emit filtered access units"
    );

    // Every output access unit must be Annex-B framed.
    for (n, au) in filtered_aus.iter().enumerate() {
        assert!(
            is_annexb_framed(au),
            "output AU #{n} must be Annex-B start-code framed"
        );
    }

    let mut idr_count = 0_usize;
    for au in filtered_aus {
        let is_idr = multiview_ffmpeg::is_idr(au, CodecKind::H264, NalFraming::AnnexB);
        let types = annexb_nal_types(au);
        if !is_idr {
            continue;
        }
        idr_count += 1;

        // Locate the first IDR slice (type 5) and require SPS (7) then PPS (8) to
        // appear among the NALs preceding it, in that order.
        let idr_pos = types
            .iter()
            .position(|&t| t == 5)
            .expect("an IDR AU must contain a type-5 slice");
        let before = &types[..idr_pos];
        let sps_pos = before.iter().position(|&t| t == 7).unwrap_or_else(|| {
            panic!("IDR AU is missing an in-band SPS (7) before the IDR; NALs = {types:?}")
        });
        let pps_pos = before.iter().position(|&t| t == 8).unwrap_or_else(|| {
            panic!("IDR AU is missing an in-band PPS (8) before the IDR; NALs = {types:?}")
        });
        assert!(
            sps_pos < pps_pos,
            "SPS (7) must precede PPS (8) before the IDR; NALs = {types:?}"
        );
    }

    assert!(
        idr_count >= 2,
        "the fixture must yield at least two IDRs so 'before EVERY keyframe' is exercised, saw {idr_count}"
    );
}

/// Generate a short **MPEG-TS** (Annex-B framed) H.264 clip. TS already carries
/// in-band SPS/PPS, so this exercises the GP-3 *Annex-B-input* branch
/// (`extract_extradata` → `dump_extra`) rather than the avcC-converter branch.
fn generate_annexb_ts_h264(dir: &Path) -> PathBuf {
    let out = dir.join("src.ts");
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=160x120:rate=10",
            "-t",
            "2",
            "-c:v",
            "libx264",
            "-g",
            "10",
            "-keyint_min",
            "10",
            "-sc_threshold",
            "0",
            "-pix_fmt",
            "yuv420p",
            "-f",
            "mpegts",
        ])
        .arg(&out)
        .status()
        .expect("spawn ffmpeg CLI");
    assert!(status.success(), "ffmpeg CLI failed to generate TS clip");
    out
}

#[test]
fn gp3_annexb_input_yields_annexb_with_ps_before_every_idr() {
    if !has_h264_encoder() {
        eprintln!("skipping: linked FFmpeg has no libx264 encoder to build the TS fixture");
        return;
    }

    let dir = tempfile::TempDir::new().unwrap();
    let clip = generate_annexb_ts_h264(dir.path());

    let mut demux = Demuxer::open(&clip).expect("open TS source");
    let streams = demux.streams();
    let video = streams
        .iter()
        .find(|s| s.kind == MediaKind::Video)
        .expect("video stream");
    let vidx = video.index;
    assert_eq!(video.codec_name, "h264", "fixture must be H.264");

    // Precondition: an MPEG-TS H.264 source is Annex-B framed.
    let (codec, in_framing) = demux
        .stream_idr_framing(vidx)
        .expect("h264 framing resolves");
    assert_eq!(codec, CodecKind::H264);
    assert!(
        matches!(in_framing, NalFraming::AnnexB),
        "an MPEG-TS H.264 fixture must be Annex-B, got {in_framing:?}"
    );

    let params = demux
        .stream_parameters(vidx)
        .expect("clone stream parameters");
    let mut chain = BsfChain::new(
        CodecKind::H264,
        InputFraming::AnnexB,
        BsfFraming::AnnexBInBand,
        &params,
        video.time_base,
    )
    .expect("build GP-3 Annex-B-input BsfChain");

    let mut filtered_aus: Vec<Vec<u8>> = Vec::new();
    while let Some(pkt) = demux.read_packet().expect("read") {
        if pkt.stream_index != vidx {
            continue;
        }
        chain.send_packet(&pkt.packet).expect("send to bsf chain");
        while let Some(out) = chain.receive_packet().expect("receive from bsf chain") {
            if let Some(bytes) = out.data() {
                filtered_aus.push(bytes.to_vec());
            }
        }
    }
    chain.send_eof().expect("eof to bsf chain");
    while let Some(out) = chain.receive_packet().expect("drain bsf chain") {
        if let Some(bytes) = out.data() {
            filtered_aus.push(bytes.to_vec());
        }
    }

    assert_annexb_with_ps_before_every_idr(&filtered_aus);
}

#[test]
fn gp3_empty_chain_passes_packets_through_byte_identical() {
    if !has_h264_encoder() {
        eprintln!("skipping: linked FFmpeg has no libx264 encoder to build the fixture");
        return;
    }

    // AV1 / unmodelled codecs select an EMPTY plan: the chain must pass every
    // packet through byte-for-byte (it carries its own headers per temporal
    // unit). Drive it with a real H.264 source's packets but ask for the
    // `CodecKind::Other` selection so the plan is empty; the bytes must be
    // unchanged regardless of the source codec.
    let dir = tempfile::TempDir::new().unwrap();
    let clip = generate_avcc_h264(dir.path());
    let mut demux = Demuxer::open(&clip).expect("open source");
    let streams = demux.streams();
    let video = streams
        .iter()
        .find(|s| s.kind == MediaKind::Video)
        .expect("video stream");
    let vidx = video.index;
    let params = demux
        .stream_parameters(vidx)
        .expect("clone stream parameters");

    let mut chain = BsfChain::new(
        CodecKind::Other,
        InputFraming::LengthPrefixed,
        BsfFraming::AnnexBInBand,
        &params,
        Rational::new(1, 10),
    )
    .expect("build empty (pass-through) BsfChain");

    let mut inputs: Vec<Vec<u8>> = Vec::new();
    let mut outputs: Vec<Vec<u8>> = Vec::new();
    while let Some(pkt) = demux.read_packet().expect("read") {
        if pkt.stream_index != vidx {
            continue;
        }
        if let Some(b) = pkt.packet.data() {
            inputs.push(b.to_vec());
        }
        chain.send_packet(&pkt.packet).expect("send");
        while let Some(out) = chain.receive_packet().expect("receive") {
            if let Some(b) = out.data() {
                outputs.push(b.to_vec());
            }
        }
    }
    chain.send_eof().expect("eof");
    while let Some(out) = chain.receive_packet().expect("drain") {
        if let Some(b) = out.data() {
            outputs.push(b.to_vec());
        }
    }

    assert!(!inputs.is_empty(), "the source must have video packets");
    assert_eq!(
        inputs, outputs,
        "an empty (AV1/Other) plan must pass every packet through byte-identical"
    );
}
