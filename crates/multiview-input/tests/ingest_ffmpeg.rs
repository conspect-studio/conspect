//! Real-ingest integration tests (the `ffmpeg` feature).
//!
//! These generate a tiny **self-contained** clip at runtime with the `ffmpeg`
//! CLI (an LGPL `ffv1` software codec — never x264/x265, so the build stays
//! LGPL-clean), ingest it through the real `multiview-ffmpeg` safe demux/decode
//! wrappers via [`FileSource`] / [`TestPatternSource`], and assert that decoded
//! frames land in a [`TileStore`] with **strictly-monotonic, normalized
//! nanosecond PTS** (invariants #1/#2/#3). No media is checked in.
//!
//! Gated behind `ffmpeg`: the default pure-Rust build never compiles or runs
//! these, and never pulls a native dependency.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use multiview_core::time::MediaTime;
use multiview_framestore::TileStore;
use multiview_input::libav::{FileSource, TestPatternSource, TestPatternSpec};
use multiview_input::source::{FrameProducer, IngestConfig, IngestPump, StoredFrame};
use tempfile::TempDir;

const W: u32 = 320;
const H: u32 = 240;
const RATE: u32 = 25;
const SECONDS: u32 = 1;

/// Generate a 1-second `testsrc` clip with the LGPL `ffv1` codec, returning its
/// path. Mirrors the multiview-ffmpeg test fixtures so this suite is independent.
fn generate_clip(dir: &Path) -> PathBuf {
    let out = dir.join("ingest-src.mkv");
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
            "-t",
            &SECONDS.to_string(),
            // LGPL, in-tree software codec — NOT x264/x265 (GPL).
            "-c:v",
            "ffv1",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&out)
        .status()
        .expect("failed to spawn the `ffmpeg` CLI (is FFmpeg installed?)");
    assert!(status.success(), "ffmpeg CLI failed to generate the clip");
    assert!(out.exists(), "ffmpeg CLI produced no output file");
    out
}

/// Collect every normalized PTS (ns) published into the store by running the
/// producer to end, sampling the store's slot after each pump step.
fn ingest_collect_pts<P: FrameProducer>(mut producer: P, config: IngestConfig) -> (u64, Vec<i64>) {
    let store: TileStore<StoredFrame> = TileStore::with_defaults("ffmpeg-src");
    let mut pump = IngestPump::new(&producer, config);
    let anchor = MediaTime::ZERO;

    let mut pts: Vec<i64> = Vec::new();
    let mut last: Option<i64> = None;
    while pump
        .pump_one(&mut producer, &store, anchor)
        .expect("pump must not fault on a clean local clip")
    {
        if let Some(frame) = store.slot().load() {
            let p = frame.meta.pts.as_nanos();
            if last != Some(p) {
                pts.push(p);
                last = Some(p);
            }
        }
    }
    // Capture whatever the final (EOS) step flushed.
    if let Some(frame) = store.slot().load() {
        let p = frame.meta.pts.as_nanos();
        if last != Some(p) {
            pts.push(p);
        }
    }
    (pump.published(), pts)
}

#[test]
fn file_source_decodes_and_resolves_geometry() {
    let dir = TempDir::new().expect("tempdir");
    let clip = generate_clip(dir.path());

    let source = FileSource::open(&clip).expect("open + decode-first-frame the generated clip");
    // Geometry must match exactly what the CLI rendered (resolved by a REAL
    // decode of the first frame, not guessed).
    assert_eq!(source.width(), W, "decoded width");
    assert_eq!(source.height(), H, "decoded height");
}

#[test]
fn ingest_lands_frames_in_store_with_monotonic_ns_pts() {
    let dir = TempDir::new().expect("tempdir");
    let clip = generate_clip(dir.path());
    let source = FileSource::open(&clip).expect("open file source");

    let (published, pts) = ingest_collect_pts(source, IngestConfig::default());

    // testsrc at 25 fps for 1 s yields exactly 25 video frames/packets.
    assert_eq!(
        published,
        u64::from(RATE * SECONDS),
        "all video frames published"
    );
    assert_eq!(
        u32::try_from(pts.len()).expect("frame count fits u32"),
        RATE * SECONDS,
        "one normalized PTS per frame"
    );

    // The first frame anchors to master_now = 0.
    assert_eq!(pts[0], 0, "first frame anchors at master_now");

    // Strictly monotonic, normalized nanosecond PTS — never a backwards or
    // duplicate timestamp (a muxer would abort on a non-monotonic DTS).
    for w in pts.windows(2) {
        assert!(
            w[1] > w[0],
            "normalized PTS must strictly increase across real decoded frames: {pts:?}"
        );
    }

    // The cadence is ~40 ms/frame at 25 fps; the last frame lands near 24*40 ms.
    // Assert a sane span rather than an exact value (the demuxer's packet PTS may
    // carry a small container offset; what matters is monotonic ~40 ms steps).
    let span = pts[pts.len() - 1] - pts[0];
    assert!(
        (900_000_000..=1_100_000_000).contains(&span),
        "≈1 s of 25 fps frames spans ≈960 ms, got {span} ns"
    );
}

#[test]
fn test_pattern_source_generates_and_ingests_self_contained() {
    let dir = TempDir::new().expect("tempdir");
    let spec = TestPatternSpec {
        width: 160,
        height: 120,
        rate: 30,
        seconds: 1,
    };
    let source = TestPatternSource::generate(dir.path(), spec)
        .expect("generate + open a self-contained test pattern");
    assert_eq!(source.width(), 160, "test pattern width");
    assert_eq!(source.height(), 120, "test pattern height");

    let (published, pts) = ingest_collect_pts(source, IngestConfig::default());
    assert_eq!(published, 30, "30 fps × 1 s = 30 frames");
    assert_eq!(pts.len(), 30, "one normalized PTS per frame");
    for w in pts.windows(2) {
        assert!(w[1] > w[0], "monotonic test-pattern PTS: {pts:?}");
    }
}

#[test]
fn ingest_with_jitter_window_stays_monotonic() {
    // Even with a reorder window enabled, an already-in-order file must publish
    // the same strictly-monotonic sequence (the window is a no-op on ordered
    // input but must never corrupt order).
    let dir = TempDir::new().expect("tempdir");
    let clip = generate_clip(dir.path());
    let source = FileSource::open(&clip).expect("open file source");

    let config = IngestConfig {
        jitter_depth: 3,
        ..IngestConfig::default()
    };
    let (published, pts) = ingest_collect_pts(source, config);
    assert_eq!(
        published,
        u64::from(RATE * SECONDS),
        "all frames published through jitter"
    );
    for w in pts.windows(2) {
        assert!(w[1] > w[0], "jittered ingest stays monotonic: {pts:?}");
    }
}

#[test]
fn open_missing_file_is_typed_error_not_panic() {
    // A non-existent path must surface a typed Ingest error, never panic.
    let missing = Path::new("/nonexistent/multiview-input/does-not-exist.mkv");
    match FileSource::open(missing) {
        Ok(_) => panic!("opening a missing file must fail"),
        Err(multiview_input::Error::Ingest(msg)) => {
            assert!(
                msg.contains("does-not-exist.mkv") || msg.to_lowercase().contains("open"),
                "error should describe the open failure, got: {msg}"
            );
        }
        Err(other) => panic!("expected Error::Ingest, got {other:?}"),
    }
}

#[test]
fn file_source_is_send() {
    // The producer must be able to move to a decode task. (It is intentionally
    // NOT asserted `Sync`: it owns a libav demux context.)
    fn assert_send<T: Send>() {}
    assert_send::<FileSource>();
    assert_send::<TestPatternSource>();
}
