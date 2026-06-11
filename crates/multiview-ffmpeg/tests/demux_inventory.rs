//! RT-1: `Demuxer::inventory()` full `StreamInventory` discovery (ADR-0034 §3).
//!
//! Proves the libav demux path no longer discards non-video streams: a clip with
//! a video, two languaged audio tracks, and a subtitle yields a typed
//! `StreamInventory` carrying **every** elementary stream (not just best-video).
//!
//! Gated behind the `ffmpeg` feature. The clip is generated at test time with the
//! **LGPL** `ffv1` video + `flac` audio + `subrip` subtitle codecs (all in-tree,
//! never x264/x265), so the suite carries no media and stays LGPL-clean.
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

use multiview_core::stream::{StreamDetail, StreamKind};
use multiview_ffmpeg::Demuxer;
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
        .expect("spawn ffmpeg CLI");
    assert!(status.success(), "ffmpeg CLI failed to generate clip");
    assert!(out.exists());
    out
}

#[test]
fn inventory_surfaces_every_elementary_stream_not_just_best_video() {
    let dir = TempDir::new().unwrap();
    let clip = generate_multi_stream_clip(dir.path());

    let demux = Demuxer::open(&clip).expect("open container");

    // The legacy path keeps only best-video; the inventory keeps ALL of them.
    let inv = demux.inventory();
    assert_eq!(
        inv.streams.len(),
        4,
        "one video + two audio + one subtitle survive (not just best-video)"
    );

    // One of each routing kind survives discovery.
    assert_eq!(inv.video().count(), 1, "the video stream survives");
    assert_eq!(
        inv.audio_tracks().count(),
        2,
        "BOTH audio tracks survive (previously discarded)"
    );
    assert_eq!(
        inv.subtitle_tracks().count(),
        1,
        "the subtitle track survives (previously discarded)"
    );
}

#[test]
fn inventory_carries_the_right_languages_on_the_audio_tracks() {
    let dir = TempDir::new().unwrap();
    let clip = generate_multi_stream_clip(dir.path());
    let demux = Demuxer::open(&clip).expect("open container");
    let inv = demux.inventory();

    let mut langs: Vec<String> = inv
        .audio_tracks()
        .filter_map(|s| s.language.as_ref().map(|l| l.as_str().to_owned()))
        .collect();
    langs.sort();
    assert_eq!(
        langs,
        vec!["eng".to_owned(), "spa".to_owned()],
        "both audio languages are parsed onto Bcp47 and survive"
    );

    // The subtitle language is carried too.
    let sub_lang = inv
        .subtitle_tracks()
        .next()
        .expect("a subtitle track")
        .language
        .as_ref()
        .map(|l| l.as_str().to_owned());
    assert_eq!(
        sub_lang,
        Some("fra".to_owned()),
        "subtitle language survives"
    );
}

#[test]
fn inventory_video_descriptor_carries_geometry() {
    let dir = TempDir::new().unwrap();
    let clip = generate_multi_stream_clip(dir.path());
    let demux = Demuxer::open(&clip).expect("open container");
    let inv = demux.inventory();

    let v = inv.video().next().expect("a video stream");
    assert_eq!(v.kind, StreamKind::Video);
    assert_eq!(
        v.detail.video_geometry(),
        Some((W, H)),
        "video geometry probed into the descriptor detail"
    );
}

#[test]
fn inventory_audio_descriptor_carries_layout() {
    let dir = TempDir::new().unwrap();
    let clip = generate_multi_stream_clip(dir.path());
    let demux = Demuxer::open(&clip).expect("open container");
    let inv = demux.inventory();

    for a in inv.audio_tracks() {
        let (channels, sample_rate) = a
            .detail
            .audio_layout()
            .expect("audio detail carries a layout");
        assert!(channels >= 1, "at least one channel");
        assert_eq!(sample_rate, 48_000, "sample rate probed into detail");
    }
}

#[test]
fn inventory_ids_are_distinct_per_stream() {
    let dir = TempDir::new().unwrap();
    let clip = generate_multi_stream_clip(dir.path());
    let demux = Demuxer::open(&clip).expect("open container");
    let inv = demux.inventory();

    // Every elementary stream gets a distinct stable id (the two same-codec
    // same-format audio tracks are disambiguated by ordinal + language).
    let mut ids: Vec<String> = inv.streams.iter().map(|s| s.id.to_string()).collect();
    let total = ids.len();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), total, "all stream ids are distinct");
}

#[test]
fn inventory_detail_matches_passthrough_for_non_av_kinds_in_a_pure_check() {
    // A pure check on the StreamDetail shape: a passthrough kind has no AV detail.
    assert!(StreamDetail::Passthrough.video_geometry().is_none());
    assert!(StreamDetail::Passthrough.audio_layout().is_none());
}
