//! Live integration proof for native HLS `WebVTT` caption ingest (#24).
//!
//! These tests hit the network, so they are `#[ignore]`d by default. They prove
//! the *real* decode path end-to-end against Apple's public bipbop test stream:
//! resolve the English `WebVTT` rendition from the master, demux + decode it on the
//! caption reader, and assert that a REAL decoded cue (`"English subtitle 1
//! -Unforced-"`, on screen 00:00:01–00:00:03) lands in the per-source cue store
//! at the matching media instant. A clean decoded cue is the proof — not a
//! metadata field.
//!
//! Run manually with the network available:
//!
//! ```text
//! cargo test -p multiview-cli --features ffmpeg,overlay \
//!     --test caption_ingest -- --ignored --nocapture
//! ```
#![cfg(all(feature = "ffmpeg", feature = "overlay"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use multiview_cli::captions::{caption_loop, caption_plan_for, CaptionPlan, CueStore};
use multiview_config::schema::CaptionSelector;
use multiview_config::{MultiviewConfig, Source, SourceKind};
use multiview_core::time::MediaTime;
use multiview_ffmpeg::caption::CaptionCue;
use multiview_ffmpeg::test_fixtures::{
    generate_hls_with_broken_webvtt, generate_hls_with_valid_webvtt, WEBVTT_CUE_START_S,
    WEBVTT_CUE_TEXT,
};

const BIPBOP_MASTER: &str = "https://devstreaming-cdn.apple.com/videos/streaming/examples/bipbop_16x9/bipbop_16x9_variant.m3u8";

/// Build an HLS `Source` with `captions = {mode="auto"}` pointing at the bipbop
/// master, by parsing a minimal full config (the `#[non_exhaustive]` `Source`
/// has no cross-crate struct literal) and taking its first source.
fn bipbop_source() -> Source {
    let toml = format!(
        r##"
schema_version = 1

[canvas]
width = 640
height = 360
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
id = "cam_bipbop"
kind = "hls"
url = "{BIPBOP_MASTER}"
[sources.captions]
mode = "auto"

[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "cam_bipbop"
"##,
    );
    let config = MultiviewConfig::load_from_toml(&toml).expect("parse config");
    config
        .sources
        .into_iter()
        .next()
        .expect("one source declared")
}

#[test]
#[ignore = "network: hits Apple's public bipbop stream; run with --ignored"]
fn bipbop_english_webvtt_cue_decodes_into_the_store() {
    let source = bipbop_source();
    assert_eq!(
        source.captions,
        Some(CaptionSelector::Auto),
        "the source carries an auto caption selector"
    );
    assert!(matches!(source.kind, SourceKind::Hls { .. }));

    // Resolve the WebVTT rendition from the master (fetch + parse + pick + join).
    let plan = caption_plan_for(&source)
        .expect("the bipbop master resolves an English WebVTT subtitle rendition");
    assert!(
        plan.rendition_url
            .ends_with("subtitles/eng/prog_index.m3u8"),
        "resolved rendition URL is the English WebVTT media playlist: {}",
        plan.rendition_url
    );

    let store = Arc::clone(&plan.store);

    // Run the reader on its own thread and SAMPLE the store concurrently — exactly
    // how the real pipeline works (the off-hot-path baker samples `active_at(pts)`
    // per tick while the reader publishes). This also respects the store's bounded
    // drop-oldest window: a VOD rendition carries ~900 cues, far more than the
    // store's retention, so we must catch the first cue while it is still live
    // rather than draining the whole VOD first (which would evict the early ones).
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let plan = plan;
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || caption_loop(&plan, &stop))
    };

    // The first real cue is on screen 00:00:01.000–00:00:03.000. Poll the store
    // mid-window (1.5s on the source-relative ns timeline) until it appears.
    let at = MediaTime::from_nanos(1_500_000_000);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let mut found: Option<Vec<String>> = None;
    while std::time::Instant::now() < deadline {
        if let Some(CaptionCue::Text { text, .. }) = store.active_at(at) {
            found = Some(text.lines);
            break;
        }
        if reader.is_finished() {
            // The reader drained the whole VOD; one last look (the early cue may
            // have been evicted by the bounded window — that is still a real
            // decode, just not catchable post-hoc).
            if let Some(CaptionCue::Text { text, .. }) = store.active_at(at) {
                found = Some(text.lines);
            }
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    stop.store(true, std::sync::atomic::Ordering::Release);
    let _ = reader.join();

    let lines = found.expect("a cue is active at 1.5s (the first bipbop English caption)");
    let joined = lines.join(" ");
    println!("DECODED CUE @1.5s: {joined:?}");
    assert!(
        joined.contains("English subtitle 1"),
        "the first decoded English cue must contain the real bipbop text \
         (got {joined:?})"
    );
}

/// Build a `CaptionPlan` for the rendition `m3u8` at `rendition_url`, with the
/// given `live` flag and a fresh cue store. The `#[non_exhaustive]`-free
/// `CaptionPlan` has public fields, so a test constructs it directly.
fn plan_for(rendition_url: String, live: bool) -> CaptionPlan {
    CaptionPlan {
        id: "cam".to_owned(),
        rendition_url,
        store: Arc::new(CueStore::new()),
        live,
    }
}

#[test]
fn valid_webvtt_cue_lands_via_isolated_reader() {
    // Fully offline: the isolated WebVTT reader decodes one known cue from a
    // `file://` rendition and publishes it into the per-source store. This is the
    // SOLE WebVTT path (the main demuxer discards the subtitle rendition), so it
    // must work on its own.
    let dir = tempfile::tempdir().expect("tempdir");
    generate_hls_with_valid_webvtt(dir.path()).expect("generate valid-webvtt fixture");
    let rendition = format!("file://{}/subs.m3u8", dir.path().display());

    // A finite VOD rendition: not live (plays out once). The reader publishes its
    // cue and returns at EOF.
    let plan = plan_for(rendition, false);
    let store = Arc::clone(&plan.store);

    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let plan = plan;
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || caption_loop(&plan, &stop))
    };

    // The cue is on screen WEBVTT_CUE_START_S..WEBVTT_CUE_END_S (1s..3s); sample
    // mid-window at 1.5s and poll until it appears (the reader paces by PTS).
    let at = MediaTime::from_nanos(i64::from(WEBVTT_CUE_START_S) * 1_000_000_000 + 500_000_000);
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut found: Option<Vec<String>> = None;
    while Instant::now() < deadline {
        if let Some(CaptionCue::Text { text, .. }) = store.active_at(at) {
            found = Some(text.lines);
            break;
        }
        if reader.is_finished() {
            if let Some(CaptionCue::Text { text, .. }) = store.active_at(at) {
                found = Some(text.lines);
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    stop.store(true, Ordering::Release);
    let _ = reader.join();

    let lines = found.expect("the known offline WebVTT cue must land in the store");
    let joined = lines.join(" ");
    assert!(
        joined.contains(WEBVTT_CUE_TEXT),
        "the decoded cue must contain the fixture's known text (got {joined:?})"
    );
}

#[test]
fn live_caption_reader_retries_after_transient_error() {
    // A LIVE caption reader pointed at a broken (corrupt-first-segment) rendition
    // must NOT return after the first error — it loops/backs-off (so a transient
    // .vtt 404/token-expiry never permanently kills captions) — and it must stop
    // PROMPTLY when the stop flag is raised.
    let dir = tempfile::tempdir().expect("tempdir");
    generate_hls_with_broken_webvtt(dir.path()).expect("generate broken-webvtt fixture");
    let rendition = format!("file://{}/subs.m3u8", dir.path().display());

    let plan = plan_for(rendition, true);
    let stop = Arc::new(AtomicBool::new(false));

    let reader = {
        let plan = plan;
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || caption_loop(&plan, &stop))
    };

    // Give the loop time to hit the open/read error at least once and enter its
    // backoff. A LIVE reader must still be running (it retries) — NOT finished.
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        !reader.is_finished(),
        "a live caption reader must keep retrying after a transient error, not return"
    );

    // Raising stop must tear it down promptly (the backoff sleep is stop-checked).
    stop.store(true, Ordering::Release);
    let joined_by = Instant::now() + Duration::from_secs(5);
    while Instant::now() < joined_by {
        if reader.is_finished() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        reader.is_finished(),
        "a live caption reader must stop promptly once the stop flag is raised"
    );
    let _ = reader.join();
}
