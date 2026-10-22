//! Packet-fed H.264 access-unit decoder tests (ADR-T014, WHIP ingest).
//!
//! The WHIP path has no `AVFormatContext`: the RTP depacketizer hands the
//! decoder reassembled access-unit / NAL bytes. `H264PacketDecoder` must
//! decode those to NV12 with **true geometry from the SPS** and `ColorInfo`
//! from the VUI (raw tags; the BT.709-by-geometry defaulting stays downstream
//! in `resolve_defaults`, the same policy as every other compressed ingest).
//!
//! Fixtures are self-generated with the `ffmpeg` CLI (the established crate
//! test pattern): a tiny Annex-B elementary stream with access-unit delimiters
//! (`aud=1`) so the test can split real AUs without a demuxer.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use ffmpeg::format::Pixel;
use ffmpeg_next as ffmpeg;
use multiview_core::color::MatrixCoefficients;
use multiview_core::pixel::PixelFormat;
use multiview_core::time::Rational;
use multiview_ffmpeg::H264PacketDecoder;
use tempfile::TempDir;

/// RTP video clock (RFC 6184): 90 kHz.
const RTP_VIDEO_TB: Rational = Rational::new(1, 90_000);
/// 90 kHz ticks per frame at 25 fps.
const TICKS_PER_FRAME: i64 = 3_600;

/// Generate `frames` frames of `testsrc2` as a raw Annex-B H.264 elementary
/// stream with AUDs (so AUs are splittable) and no B-frames (the conforming
/// WHIP publisher shape: decode order == presentation order).
fn generate_h264(dir: &Path, name: &str, w: u32, h: u32, frames: u32, tag_bt709: bool) -> PathBuf {
    let out = dir.join(name);
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-f",
        "lavfi",
        "-i",
        &format!("testsrc2=size={w}x{h}:rate=25"),
        "-frames:v",
        &frames.to_string(),
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-pix_fmt",
        "yuv420p",
        "-x264-params",
        "aud=1:keyint=4:min-keyint=4:bframes=0:scenecut=0:repeat-headers=1",
    ]);
    if tag_bt709 {
        cmd.args([
            "-colorspace",
            "bt709",
            "-color_primaries",
            "bt709",
            "-color_trc",
            "bt709",
            "-color_range",
            "tv",
        ]);
    }
    cmd.args(["-f", "h264"]).arg(&out);
    let status = cmd.status().expect("spawn ffmpeg CLI");
    assert!(status.success(), "ffmpeg CLI failed to generate {name}");
    out
}

/// Every Annex-B start-code position in `data` as `(start_code_pos, nal_type)`.
fn scan_nals(data: &[u8]) -> Vec<(usize, u8)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            // Fold a 4-byte start code into the position of its leading zero.
            let sc_pos = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
            if let Some(&header) = data.get(i + 3) {
                out.push((sc_pos, header & 0x1F));
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    out
}

/// Split a raw Annex-B elementary stream into access units on the AUD NALs
/// (type 9) the fixture was encoded with.
fn split_aus(data: &[u8]) -> Vec<Vec<u8>> {
    let boundaries: Vec<usize> = scan_nals(data)
        .into_iter()
        .filter_map(|(pos, ty)| (ty == 9).then_some(pos))
        .collect();
    assert!(!boundaries.is_empty(), "fixture must carry AUDs");
    boundaries
        .iter()
        .enumerate()
        .map(|(n, &start)| {
            let end = boundaries.get(n + 1).copied().unwrap_or(data.len());
            data[start..end].to_vec()
        })
        .collect()
}

/// Split one Annex-B access unit into its bare NAL payloads (no start codes) —
/// the exact byte shape the RTP depacketizer emits per NAL (ADR-T014).
fn split_bare_nals(au: &[u8]) -> Vec<Vec<u8>> {
    let positions = scan_nals(au);
    positions
        .iter()
        .enumerate()
        .map(|(n, &(pos, _))| {
            // Skip the start code itself (3 or 4 bytes).
            let body = if au[pos..].starts_with(&[0, 0, 0, 1]) {
                pos + 4
            } else {
                pos + 3
            };
            let end = positions.get(n + 1).map_or(au.len(), |&(p, _)| p);
            au[body..end].to_vec()
        })
        .collect()
}

/// Re-frame an Annex-B access unit as AVCC (4-byte big-endian NAL lengths).
fn to_avcc(au: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(au.len() + 16);
    for nal in split_bare_nals(au) {
        let len = u32::try_from(nal.len()).expect("NAL fits u32");
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(&nal);
    }
    out
}

/// Push every AU through a fresh decoder, draining after each push and at EOF;
/// returns `(width, height, format, matrix, raw_pts)` per decoded frame.
fn decode_all(aus: &[Vec<u8>]) -> Vec<(u32, u32, PixelFormat, MatrixCoefficients, Option<i64>)> {
    let mut dec = H264PacketDecoder::new(RTP_VIDEO_TB).expect("open packet decoder");
    let mut frames = Vec::new();
    let drain = |dec: &mut H264PacketDecoder, frames: &mut Vec<_>| {
        while let Some(f) = dec.receive_frame().expect("receive") {
            assert_eq!(
                f.frame.format(),
                Pixel::NV12,
                "decoded pixels must be NV12 (invariant #5)"
            );
            frames.push((
                f.meta.width,
                f.meta.height,
                f.meta.format,
                f.meta.color.matrix,
                f.raw_pts,
            ));
        }
    };
    for (i, au) in aus.iter().enumerate() {
        let pts = i64::try_from(i).unwrap() * TICKS_PER_FRAME;
        dec.push(au, Some(pts)).expect("push AU");
        drain(&mut dec, &mut frames);
    }
    dec.send_eof().expect("eof");
    drain(&mut dec, &mut frames);
    frames
}

#[test]
fn decodes_split_annexb_aus_with_sps_geometry_and_vui_color() {
    let dir = TempDir::new().unwrap();
    let clip = generate_h264(dir.path(), "a.h264", 320, 240, 8, true);
    let data = std::fs::read(&clip).unwrap();
    let aus = split_aus(&data);
    assert_eq!(aus.len(), 8, "one AU per encoded frame");

    let frames = decode_all(&aus);
    assert_eq!(frames.len(), 8, "every pushed AU decodes to one frame");
    for (i, &(w, h, fmt, matrix, raw_pts)) in frames.iter().enumerate() {
        // Geometry comes from the SPS — never from a declared constructor arg.
        assert_eq!((w, h), (320, 240), "SPS geometry (frame {i})");
        assert_eq!(fmt, PixelFormat::Nv12, "NV12 metadata (frame {i})");
        // The fixture's VUI tags BT.709 — the decoder must surface it.
        assert_eq!(matrix, MatrixCoefficients::Bt709, "VUI matrix (frame {i})");
        // bf=0: presentation order == push order, raw PTS carried verbatim.
        assert_eq!(
            raw_pts,
            Some(i64::try_from(i).unwrap() * TICKS_PER_FRAME),
            "raw 90 kHz PTS rides through (frame {i})"
        );
    }
}

#[test]
fn mid_stream_resolution_change_yields_updated_geometry() {
    // Concatenate two elementary streams (legal in Annex-B): 5 frames of
    // 320x240 then 5 frames of 160x120. The in-band SPS change must surface as
    // new geometry on the decoded frames — a WHIP publisher changing its
    // capture resolution mid-session (ADR-T014 consequence).
    let dir = TempDir::new().unwrap();
    let a = generate_h264(dir.path(), "a.h264", 320, 240, 5, true);
    let b = generate_h264(dir.path(), "b.h264", 160, 120, 5, false);
    let mut data = std::fs::read(&a).unwrap();
    data.extend_from_slice(&std::fs::read(&b).unwrap());
    let aus = split_aus(&data);
    assert_eq!(aus.len(), 10);

    let frames = decode_all(&aus);
    assert_eq!(frames.len(), 10, "all frames across the SPS change decode");
    for &(w, h, _, matrix, _) in &frames[..5] {
        assert_eq!((w, h), (320, 240), "pre-change geometry");
        assert_eq!(matrix, MatrixCoefficients::Bt709, "pre-change VUI");
    }
    for &(w, h, _, matrix, _) in &frames[5..] {
        assert_eq!((w, h), (160, 120), "post-change geometry from new SPS");
        // The second segment carries no VUI colour description. libav's
        // per-frame colour tags are STICKY across an in-band SPS change on a
        // live decoder (verified against libavcodec 61): the frames keep the
        // previous stream's BT.709 rather than reverting to Unspecified. The
        // decoder surfaces libav's tags honestly — asserting the retention
        // pins that behaviour (the fresh-decoder untagged case is covered by
        // `untagged_stream_on_a_fresh_decoder_stays_unspecified`).
        assert_eq!(matrix, MatrixCoefficients::Bt709, "sticky VUI tags");
    }
}

#[test]
fn accepts_avcc_length_prefixed_access_units() {
    // The depacketizer may hand AVCC-normalized AUs; the decoder must accept
    // both framings (ADR-T014 scope).
    let dir = TempDir::new().unwrap();
    let clip = generate_h264(dir.path(), "a.h264", 320, 240, 8, true);
    let data = std::fs::read(&clip).unwrap();
    let aus: Vec<Vec<u8>> = split_aus(&data).iter().map(|au| to_avcc(au)).collect();

    let frames = decode_all(&aus);
    assert_eq!(frames.len(), 8, "AVCC AUs decode identically");
    assert!(frames.iter().all(|&(w, h, ..)| (w, h) == (320, 240)));
}

#[test]
fn accepts_bare_nal_units_as_emitted_by_the_rtp_depacketizer() {
    // The pure H264Depacketizer emits each NAL as bare bytes (no start code,
    // no length prefix): SPS, PPS, AUD, and slices arrive as separate pushes.
    // Parameter-set pushes yield no frame; each slice yields one.
    let dir = TempDir::new().unwrap();
    let clip = generate_h264(dir.path(), "a.h264", 320, 240, 8, true);
    let data = std::fs::read(&clip).unwrap();

    let mut dec = H264PacketDecoder::new(RTP_VIDEO_TB).expect("open packet decoder");
    let mut decoded = 0_u32;
    for (frame_index, au) in split_aus(&data).into_iter().enumerate() {
        let pts = i64::try_from(frame_index).unwrap() * TICKS_PER_FRAME;
        for nal in split_bare_nals(&au) {
            dec.push(&nal, Some(pts)).expect("push bare NAL");
            while let Some(f) = dec.receive_frame().expect("receive") {
                assert_eq!((f.meta.width, f.meta.height), (320, 240));
                decoded += 1;
            }
        }
    }
    dec.send_eof().expect("eof");
    while let Some(_f) = dec.receive_frame().expect("drain") {
        decoded += 1;
    }
    assert_eq!(decoded, 8, "one frame per slice; parameter sets yield none");
}

#[test]
fn untagged_stream_on_a_fresh_decoder_stays_unspecified() {
    // The detect step never guesses (invariant #8): with no VUI colour
    // description and no prior stream, every axis stays Unspecified — the
    // BT.709-by-geometry defaulting belongs downstream in resolve_defaults.
    let dir = TempDir::new().unwrap();
    let clip = generate_h264(dir.path(), "u.h264", 160, 120, 5, false);
    let data = std::fs::read(&clip).unwrap();
    let frames = decode_all(&split_aus(&data));
    assert_eq!(frames.len(), 5);
    for &(w, h, _, matrix, _) in &frames {
        assert_eq!((w, h), (160, 120));
        assert_eq!(matrix, MatrixCoefficients::Unspecified, "honest detect");
    }
}

#[test]
fn empty_push_is_a_harmless_no_op() {
    let mut dec = H264PacketDecoder::new(RTP_VIDEO_TB).expect("open packet decoder");
    dec.push(&[], Some(0)).expect("empty push must not error");
    assert!(dec.receive_frame().expect("receive").is_none());
}

/// A stream of ONLY non-VCL NALs (SEI/parameter-set spam) must never grow the
/// pending access-unit buffer past [`MAX_PENDING_AU_BYTES`]: `submit_pending`
/// deliberately refuses to send a slice-less packet, so the cap branch itself
/// must DROP the over-cap non-VCL head (drop-never-grow, CLAUDE.md safety
/// rule 5). Found by adversarial review of PR #83.
#[test]
fn non_vcl_spam_keeps_the_pending_buffer_bounded() {
    use multiview_ffmpeg::packet_decode::MAX_PENDING_AU_BYTES;
    let mut dec = multiview_ffmpeg::H264PacketDecoder::new(RTP_VIDEO_TB).unwrap();
    // 64 KiB SEI NAL (type 6): header 0x06 + payload. 400 pushes ≈ 25 MiB —
    // far past the 8 MiB cap if nothing bounds the non-VCL path.
    let mut sei = vec![0x06u8];
    sei.extend(std::iter::repeat_n(0xAA, 64 * 1024));
    for i in 0..400 {
        dec.push(&sei, Some(i)).unwrap();
        assert!(
            dec.pending_len() <= MAX_PENDING_AU_BYTES,
            "pending grew past the cap on push {i}: {} bytes",
            dec.pending_len()
        );
    }
}
