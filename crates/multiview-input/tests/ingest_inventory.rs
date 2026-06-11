//! RT-1 headline test: the libav ingest path surfaces the FULL `StreamInventory`
//! (ADR-0034 §3), no longer collapsing an input to its single best video stream.
//!
//! A multi-stream clip (1 video + 2 languaged audio + 1 languaged subtitle) is
//! generated with the **LGPL** `ffv1`/`flac`/`subrip` CLI codecs (never x264/x265,
//! LGPL-clean, no checked-in media), opened through [`FileSource`], and its
//! inventory asserted to carry **every** elementary stream with the right
//! languages — proving the previously-discarded audio/subtitle rows now survive.
//! The video-selection behaviour is asserted UNCHANGED (additive surface).
//!
//! Gated behind `ffmpeg`; the default pure-Rust build never compiles this.
//! Integration tests do not inherit `clippy.toml`'s test relaxations.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use multiview_core::stream::StreamKind;
use multiview_input::libav::FileSource;
use tempfile::TempDir;

const W: u32 = 160;
const H: u32 = 120;
const RATE: u32 = 25;
const SECONDS: u32 = 1;

/// Generate a clip with 1 video (`ffv1`), 2 languaged audio (`flac`, eng+spa),
/// and 1 languaged subtitle (`subrip`, fra) into `dir`.
fn generate_multi_stream_clip(dir: &Path) -> PathBuf {
    let srt = dir.join("sub.srt");
    std::fs::write(&srt, "1\n00:00:00,000 --> 00:00:01,000\nHello\n\n").expect("write srt");

    let out = dir.join("multi.mkv");
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=size={W}x{H}:rate={RATE}"),
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=660:sample_rate=48000",
        ])
        .arg("-i")
        .arg(&srt)
        .args([
            "-t",
            &SECONDS.to_string(),
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-map",
            "2:a",
            "-map",
            "3:s",
            "-c:v",
            "ffv1",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "flac",
            "-c:s",
            "srt",
            "-metadata:s:a:0",
            "language=eng",
            "-metadata:s:a:1",
            "language=spa",
            "-metadata:s:s:0",
            "language=fra",
        ])
        .arg(&out)
        .status()
        .expect("failed to spawn the `ffmpeg` CLI (is FFmpeg installed?)");
    assert!(status.success(), "ffmpeg CLI failed to generate the clip");
    assert!(out.exists(), "ffmpeg CLI produced no output file");
    out
}

#[test]
fn file_source_inventory_surfaces_all_elementary_streams() {
    let dir = TempDir::new().expect("tempdir");
    let clip = generate_multi_stream_clip(dir.path());

    let source = FileSource::open(&clip).expect("open + decode-first-frame the multi-stream clip");
    let inv = source.inventory();

    // The previously-discarded audio + subtitle rows now SURVIVE: the input no
    // longer collapses to one video.
    assert_eq!(
        inv.streams.len(),
        4,
        "1 video + 2 audio + 1 subtitle all survive ingest discovery"
    );
    assert_eq!(inv.video().count(), 1, "≥1 video");
    assert_eq!(inv.audio_tracks().count(), 2, "≥1 (here 2) audio survive");
    assert_eq!(inv.subtitle_tracks().count(), 1, "≥1 subtitle survives");
}

#[test]
fn file_source_inventory_carries_the_audio_languages() {
    let dir = TempDir::new().expect("tempdir");
    let clip = generate_multi_stream_clip(dir.path());
    let source = FileSource::open(&clip).expect("open file source");
    let inv = source.inventory();

    let mut langs: Vec<String> = inv
        .audio_tracks()
        .filter_map(|s| s.language.as_ref().map(|l| l.as_str().to_owned()))
        .collect();
    langs.sort();
    assert_eq!(
        langs,
        vec!["eng".to_owned(), "spa".to_owned()],
        "both audio languages survive onto the inventory"
    );
}

#[test]
fn inventory_is_additive_video_selection_behaviour_is_unchanged() {
    // The inventory is ADDITIVE: the video decode path still resolves the geometry
    // of the best video stream exactly as before RT-1, regardless of the extra
    // audio/subtitle streams now present.
    let dir = TempDir::new().expect("tempdir");
    let clip = generate_multi_stream_clip(dir.path());
    let source = FileSource::open(&clip).expect("open file source");

    // Unchanged: best-video geometry is what the CLI rendered.
    assert_eq!(source.width(), W, "best-video width unchanged");
    assert_eq!(source.height(), H, "best-video height unchanged");

    // And the inventory's default video matches that selection's geometry.
    let inv = source.inventory();
    let v = inv
        .default_for(StreamKind::is_video)
        .expect("a default video in the inventory");
    assert_eq!(
        v.detail.video_geometry(),
        Some((W, H)),
        "the inventory's default video matches the decode-path selection"
    );
}
