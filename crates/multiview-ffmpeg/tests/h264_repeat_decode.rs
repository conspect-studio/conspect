//! The synthesised H.264 repeat AU decodes with libav (#344 D1, §12.5).
//!
//! Fixtures come from the `ffmpeg` CLI + libx264 (the established crate test
//! pattern, as in `h264_packet_decode.rs`): Annex-B elementary streams with
//! AUDs, no B-frames, closed GOPs. A repeat AU from
//! [`RepeatAuBuilder::repeat_after`] is inserted (a) right before an IDR and
//! (b) mid-GOP right after a reference P. The whole sequence must decode with
//! no send error, one frame per AU, and:
//! * the repeat frame's pixels equal the previous (reference) frame's exactly;
//! * the ffmpeg CLI decodes the sequence with `-err_detect explode` and logs
//!   no error;
//! * every other frame equals the same frame decoded without the insertion,
//!   so the repeat neither corrupted the reference state nor was concealed.
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
use multiview_ffmpeg::h264_repeat::RepeatAuBuilder;
use multiview_ffmpeg::{H264PacketDecoder, NalFraming};
use tempfile::TempDir;

const TB: Rational = Rational::new(1, 90_000);
const TICKS_PER_FRAME: i64 = 3_000;

fn generate(
    dir: &Path,
    name: &str,
    size: &str,
    profile: &str,
    preset: &str,
    bframes: u32,
) -> PathBuf {
    let out = dir.join(name);
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc2=size={size}:rate=30"),
            "-frames:v",
            "24",
            "-c:v",
            "libx264",
            "-profile:v",
            profile,
            "-preset",
            preset,
            "-pix_fmt",
            "yuv420p",
            "-x264-params",
            &format!(
                "aud=1:keyint=8:min-keyint=8:bframes={bframes}:b-adapt=0:b-pyramid=none:scenecut=0:open-gop=0:repeat-headers=1"
            ),
            "-f",
            "h264",
        ])
        .arg(&out)
        .status()
        .expect("spawn ffmpeg CLI");
    assert!(status.success(), "ffmpeg CLI failed to generate {name}");
    out
}

/// `(start_code_pos, nal_type, body_start)` for every Annex-B NAL.
fn scan_nals(data: &[u8]) -> Vec<(usize, u8, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let sc = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
            if let Some(&h) = data.get(i + 3) {
                out.push((sc, h & 0x1F, i + 3));
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    out
}

fn split_aus(data: &[u8]) -> Vec<Vec<u8>> {
    let b: Vec<usize> = scan_nals(data)
        .into_iter()
        .filter_map(|(p, t, _)| (t == 9).then_some(p))
        .collect();
    b.iter()
        .enumerate()
        .map(|(n, &s)| data[s..b.get(n + 1).copied().unwrap_or(data.len())].to_vec())
        .collect()
}

/// The bare body of the first NAL of `ty` in `au`.
fn nal_of(au: &[u8], ty: u8) -> Option<Vec<u8>> {
    let nals = scan_nals(au);
    nals.iter()
        .enumerate()
        .find(|(_, n)| n.1 == ty)
        .map(|(k, n)| {
            let end = nals.get(k + 1).map_or(au.len(), |next| next.0);
            let mut body = au[n.2..end].to_vec();
            while body.last() == Some(&0) {
                body.pop();
            }
            body
        })
}

fn is_idr(au: &[u8]) -> bool {
    scan_nals(au).iter().any(|n| n.1 == 5)
}

fn is_ref_p(au: &[u8]) -> bool {
    scan_nals(au)
        .iter()
        .any(|n| n.1 == 1 && au.get(n.2).is_some_and(|h| (h >> 5) & 3 != 0))
}

/// Decode every AU; returns (Y plane, UV plane) per frame, tightly packed.
fn decode(aus: &[Vec<u8>]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut dec = H264PacketDecoder::new(TB).expect("open decoder");
    let mut frames = Vec::new();
    let drain = |dec: &mut H264PacketDecoder, frames: &mut Vec<(Vec<u8>, Vec<u8>)>| {
        while let Some(f) = dec.receive_frame().expect("receive (no decode error)") {
            let w = usize::try_from(f.meta.width).unwrap();
            let h = usize::try_from(f.meta.height).unwrap();
            let mut y = Vec::with_capacity(w * h);
            let mut uv = Vec::with_capacity(w * h / 2);
            let (ys, uvs) = (f.frame.stride(0), f.frame.stride(1));
            for row in 0..h {
                y.extend_from_slice(&f.frame.data(0)[row * ys..row * ys + w]);
            }
            for row in 0..h / 2 {
                uv.extend_from_slice(&f.frame.data(1)[row * uvs..row * uvs + w]);
            }
            frames.push((y, uv));
        }
    };
    for (i, au) in aus.iter().enumerate() {
        dec.push(au, Some(i64::try_from(i).unwrap() * TICKS_PER_FRAME))
            .expect("push AU (no send error)");
        drain(&mut dec, &mut frames);
    }
    dec.send_eof().expect("eof");
    drain(&mut dec, &mut frames);
    frames
}

/// `bframes > 0`: only the before-IDR insertion (a repeat is valid only where
/// the previous picture has the highest POC so far; mid-GOP in a B-frame
/// stream it would collide with a later B picture's POC, which is why the
/// passthrough clock never repeats for a reordering source).
fn check(name: &str, size: &str, profile: &str, preset: &str, bframes: u32, expect_cabac: bool) {
    let dir = TempDir::new().unwrap();
    let clip = generate(dir.path(), name, size, profile, preset, bframes);
    let data = std::fs::read(&clip).unwrap();
    let aus = split_aus(&data);
    assert_eq!(aus.len(), 24);
    let sps = nal_of(&aus[0], 7).expect("SPS in band");
    let pps = nal_of(&aus[0], 8).expect("PPS in band");
    let builder = RepeatAuBuilder::new(&sps, &pps).expect("supported stream");
    assert_eq!(
        builder.pps().entropy_coding_mode,
        expect_cabac,
        "{name}: entropy mode"
    );

    // (a) before the second IDR (index 8), after the last P of GOP 1;
    // (b) mid-GOP after the reference P at index 10 (no B-frames only).
    let mid_gop = bframes == 0;
    assert!(is_idr(&aus[8]) && is_idr(&aus[16]));
    assert!(
        is_ref_p(&aus[7]),
        "{name}: the GOP's last AU in decode order is a reference P"
    );
    let rep_a = builder
        .repeat_after(&aus[7], NalFraming::AnnexB)
        .expect("repeat before IDR");
    let rep_b = if mid_gop {
        assert!(is_ref_p(&aus[10]));
        Some(
            builder
                .repeat_after(&aus[10], NalFraming::AnnexB)
                .expect("repeat mid-GOP"),
        )
    } else {
        None
    };

    let mut seq = Vec::new();
    let mut repeat_positions = Vec::new();
    for (i, au) in aus.iter().enumerate() {
        seq.push(au.clone());
        if i == 7 {
            repeat_positions.push(seq.len());
            seq.push(rep_a.clone());
        }
        if let (10, Some(rep)) = (i, &rep_b) {
            repeat_positions.push(seq.len());
            seq.push(rep.clone());
        }
    }

    // Independent strict check: the ffmpeg CLI decoder with error detection
    // set to explode logs nothing and succeeds, so the repeat slice is not
    // being silently concealed.
    let seq_path = dir.path().join(format!("{name}.repeat.h264"));
    std::fs::write(&seq_path, seq.concat()).unwrap();
    let out = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-v",
            "error",
            "-err_detect",
            "+crccheck+bitstream+buffer+explode",
            "-i",
        ])
        .arg(&seq_path)
        .args(["-f", "null", "-"])
        .output()
        .expect("spawn ffmpeg CLI");
    assert!(out.status.success(), "{name}: strict decode failed");
    assert!(
        out.stderr.is_empty(),
        "{name}: decoder reported: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let baseline = decode(&aus);
    assert_eq!(baseline.len(), aus.len());
    let frames = decode(&seq);
    assert_eq!(frames.len(), seq.len(), "{name}: one frame per AU");

    let mut orig = 0;
    for (k, frame) in frames.iter().enumerate() {
        if repeat_positions.contains(&k) {
            assert!(
                frame == &frames[k - 1],
                "{name}: repeat frame {k} must equal the previous reference frame"
            );
        } else {
            assert!(
                frame == &baseline[orig],
                "{name}: frame {k} (source AU {orig}) differs from the clean decode"
            );
            orig += 1;
        }
    }
}

#[test]
fn cavlc_baseline_repeat_decodes_bit_exact() {
    check("base.h264", "320x240", "baseline", "ultrafast", 0, false);
}

#[test]
fn cabac_high_repeat_decodes_bit_exact() {
    // preset medium: CABAC, 3 refs, weighted P prediction (weightp=2).
    check("high.h264", "1280x720", "high", "medium", 0, true);
}

#[test]
fn cabac_main_odd_size_repeat_decodes_bit_exact() {
    // A non-multiple-of-16 size (cropping) and Main profile.
    check("main.h264", "638x358", "main", "fast", 0, true);
}

#[test]
fn cabac_poc_type_0_repeat_before_idr_decodes_bit_exact() {
    // x264 uses POC type 0 once B-frames are on: IPBPBPBP closed GOPs, the
    // last AU of each GOP in decode order is the reference P with the
    // highest POC, so a repeat there is valid (lsb + 2).
    check("hb.h264", "1280x720", "high", "medium", 1, true);
}
