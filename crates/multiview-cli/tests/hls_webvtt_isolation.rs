//! Flagship regression guard (ADR-T011): an HLS source whose master playlist
//! carries a **corrupt `WebVTT` subtitle rendition** must NOT kill the video tile.
//!
//! libav folds an `EXT-X-MEDIA:TYPE=SUBTITLES` rendition into the one shared
//! `AVFormatContext` it opens for the video, so a corrupt/404/expired `.vtt`
//! either aborts the open or makes `av_read_frame` return that rendition's error
//! for the whole context — blacking out the tile. The fix discards every unrouted
//! subtitle stream in the main demuxer (the isolated reader is the sole `WebVTT`
//! path), so the video keeps flowing.
//!
//! This drives the REAL [`Pipeline`] against a fully-offline fixture
//! ([`generate_hls_with_broken_webvtt`]) referenced by `file://` URLs, runs a
//! bounded tick budget, and asserts:
//!
//! 1. exactly `TICKS` composited frames were produced (video kept flowing,
//!    invariant #1), and
//! 2. the source's video tile store advanced past its initial slate — a real
//!    decoded frame landed (`is_primed()`), which is the picture-on-screen proof,
//!    not a metadata field.
//!
//! Before the fix BOTH fail: the broken rendition aborts ingest, so the tile
//! never primes and (with a wedged open) the run produces only slate.
#![cfg(all(feature = "ffmpeg", feature = "overlay"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_cli::pipeline::Pipeline;
use multiview_config::MultiviewConfig;
use multiview_ffmpeg::test_fixtures::generate_hls_with_broken_webvtt;

/// A 1x1 config whose single HLS source points at the broken-`WebVTT` master, with
/// an `auto` caption selector (so the isolated `WebVTT` reader path is exercised
/// too), a small canvas (so the CPU reference compositor keeps up in CI), and an
/// HLS output writing under `out_playlist`.
fn config_text(master: &str, out_playlist: &std::path::Path) -> String {
    format!(
        r##"
schema_version = 1

[canvas]
width = 320
height = 240
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"

[layout]
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]

[[sources]]
id = "cam_hls"
kind = "hls"
url = "{master}"
[sources.captions]
mode = "auto"

[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "cam_hls"

[[outputs]]
kind = "hls"
path = "{playlist}"
codec = "mpeg2video"
segment_ms = 1000
"##,
        playlist = out_playlist.display(),
    )
}

#[tokio::test]
async fn broken_webvtt_rendition_does_not_kill_the_video_tile() {
    // 25 ticks = 1 s of output. The fixture's video segment is 2 s, so a healthy
    // ingest primes the tile and the run produces exactly 25 frames; a broken
    // rendition that aborts ingest would leave the tile cold (no decoded frame).
    const TICKS: u64 = 25;

    let dir = tempfile::tempdir().expect("tempdir");
    generate_hls_with_broken_webvtt(dir.path()).expect("generate broken-webvtt HLS fixture");
    let master = format!("file://{}/master.m3u8", dir.path().display());

    let out_playlist = dir.path().join("out").join("index.m3u8");
    let toml = config_text(&master, &out_playlist);
    let config = MultiviewConfig::load_from_toml(&toml).expect("parse config");
    config.validate().expect("config validates");

    let mut pipeline = Pipeline::build(&config).expect("build real pipeline");
    assert_eq!(pipeline.source_count(), 1, "config wires one source");

    // The per-source tile stores the ingest threads publish into (the same Arcs).
    let stores = pipeline.preview_stores();
    let video_store = stores
        .get("cam_hls")
        .expect("the HLS source has a tile store")
        .clone();

    let report = pipeline.run_for(TICKS).await.expect("bounded real run");

    assert_eq!(
        report.frames, TICKS,
        "the output clock must emit exactly N frames despite the broken WebVTT rendition \
         (invariant #1) — got {}",
        report.frames
    );
    assert!(
        !report.faltered,
        "output must never falter (invariant #1) even with a broken subtitle rendition"
    );

    // The picture-on-screen proof: a REAL decoded frame landed in the video tile
    // (the tile advanced past its initial slate). Before the fix the broken
    // rendition aborts `open_and_stream`, so the tile never primes.
    assert!(
        video_store.is_primed(),
        "the video tile must have received at least one real decoded frame — the broken \
         WebVTT rendition must not black out the video"
    );
}
