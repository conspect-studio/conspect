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
    plan.displayed_time.store(Some(Arc::new(
        multiview_framestore::tile::SourceFrameTime {
            pts: MediaTime::from_nanos(1_500_000_000),
            generation: 1,
            event_clock: None,
        },
    )));

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
        http_headers: multiview_ffmpeg::HttpHeaders::default(),
        id: "cam".to_owned(),
        rendition_url,
        store: Arc::new(CueStore::new()),
        live,
        generation: 1,
        displayed_time: Arc::new(multiview_framestore::SwapCell::empty()),
        video_anchor: Arc::new(multiview_framestore::SwapCell::empty()),
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
    plan.displayed_time.store(Some(Arc::new(
        multiview_framestore::tile::SourceFrameTime {
            pts: MediaTime::from_nanos(1_500_000_000),
            generation: 1,
            event_clock: None,
        },
    )));

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
fn rendition_waits_for_displayed_video_not_arrival_clock() {
    let dir = tempfile::tempdir().expect("tempdir");
    generate_hls_with_valid_webvtt(dir.path()).expect("offline WebVTT fixture");
    let rendition = format!("file://{}/subs.m3u8", dir.path().display());
    let plan = plan_for(rendition, false);
    let store = Arc::clone(&plan.store);
    let cursor = Arc::clone(&plan.displayed_time);
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || caption_loop(&plan, &stop))
    };
    std::thread::sleep(Duration::from_millis(120));
    assert!(
        store
            .active_at(MediaTime::from_nanos(1_500_000_000))
            .is_none(),
        "no video picture means no cue release"
    );
    cursor.store(Some(Arc::new(
        multiview_framestore::tile::SourceFrameTime {
            pts: MediaTime::from_nanos(1_500_000_000),
            generation: 1,
            event_clock: None,
        },
    )));
    let deadline = Instant::now() + Duration::from_secs(5);
    while store
        .active_at(MediaTime::from_nanos(1_500_000_000))
        .is_none()
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        store
            .active_at(MediaTime::from_nanos(1_500_000_000))
            .is_some(),
        "cue follows displayed video"
    );
    stop.store(true, Ordering::Release);
    reader.join().expect("reader terminates");
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

/// Publish a picture of source time `pts_ns` (generation 1) as the reader's
/// display cursor.
fn show(plan: &CaptionPlan, pts_ns: i64) {
    plan.displayed_time.store(Some(Arc::new(
        multiview_framestore::tile::SourceFrameTime {
            pts: MediaTime::from_nanos(pts_ns),
            generation: 1,
            event_clock: None,
        },
    )));
}

/// Set the plan's live video anchor: the picture with raw container PTS
/// `raw0_ns` has normalized PTS `pts0_ns`.
fn anchor(plan: &CaptionPlan, raw0_ns: i64, pts0_ns: i64) {
    plan.video_anchor
        .store(Some(Arc::new(multiview_cli::pipeline::LiveAnchor {
            generation: 1,
            out0: MediaTime::ZERO,
            pts0: MediaTime::from_nanos(pts0_ns),
            raw_pts0_ns: Some(raw0_ns),
        })));
}

/// Run the reader until `store` has a cue active at `at_ns` (or 10 s), and
/// return its joined text.
fn cue_text_at(plan: CaptionPlan, at_ns: i64) -> Option<String> {
    let store = Arc::clone(&plan.store);
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || caption_loop(&plan, &stop))
    };
    let at = MediaTime::from_nanos(at_ns);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut found = None;
    while Instant::now() < deadline {
        if let Some(CaptionCue::Text { text, .. }) = store.active_at(at) {
            found = Some(text.lines.join(" "));
            break;
        }
        if reader.is_finished() {
            // A finished playlist published everything it will.
            if let Some(CaptionCue::Text { text, .. }) = store.active_at(at) {
                found = Some(text.lines.join(" "));
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    stop.store(true, Ordering::Release);
    reader.join().expect("reader stops");
    found
}

/// #526: the recorded pluto.tv rendition slice. Its cues carry no
/// `X-TIMESTAMP-MAP` and sit on the video's raw MPEG-TS clock (13:26:15.143 =
/// 48375.143 s), while the selected picture's time is the normalized one. With
/// the video anchored (raw 48375.143 s = normalized 2.0 s), the cue written at
/// 13:26:16.180-13:26:17.849 shows at normalized 3.037-4.706 s. Before the fix
/// every cue was dropped as two minutes in the future.
#[test]
fn recorded_pluto_rendition_publishes_its_cues_on_the_video_timeline() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pluto-526");
    let plan = plan_for(format!("file://{}/playlist.m3u8", dir.display()), false);
    let raw0 = 48_375_143_000_000;
    anchor(&plan, raw0, 2_000_000_000);
    show(&plan, 3_500_000_000);
    let text = cue_text_at(plan, 3_500_000_000).expect("a recorded cue lands in the store");
    assert_eq!(text, "AND LASTS UNTIL THE PLAYER BEATS THE STORY.");
}

/// #526: a segment with a NONZERO `X-TIMESTAMP-MAP` (MPEGTS 900000 = 10 s at
/// LOCAL 0): the cue written at 00:00:01-00:00:03 is at MPEG-TS 11-13 s; the
/// video anchored raw 10 s = normalized 0.5 s puts it at 1.5-3.5 s. Ignoring
/// the map (what libav does) would put it at 1-3 s relative to the raw clock,
/// i.e. -8.5 s on the video's timeline: never shown.
#[test]
fn a_nonzero_timestamp_map_places_the_cue_against_the_video_anchor() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("s0.vtt"),
        "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:900000,LOCAL:00:00:00.000\n\n\
         00:00:01.000 --> 00:00:03.000\nmapped cue\n",
    )
    .expect("segment");
    std::fs::write(
        dir.path().join("subs.m3u8"),
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:0\n\
         #EXTINF:4,\ns0.vtt\n#EXT-X-ENDLIST\n",
    )
    .expect("playlist");
    let plan = plan_for(format!("file://{}/subs.m3u8", dir.path().display()), false);
    anchor(&plan, 10_000_000_000, 500_000_000);
    show(&plan, 3_000_000_000);
    assert_eq!(
        cue_text_at(plan, 3_000_000_000).as_deref(),
        Some("mapped cue"),
        "at normalized 3.0 s (MPEG-TS 12.5 s) the mapped cue is on screen"
    );
    // And not where an unmapped reading would put it.
    let plan = plan_for(format!("file://{}/subs.m3u8", dir.path().display()), false);
    anchor(&plan, 10_000_000_000, 500_000_000);
    show(&plan, 1_200_000_000);
    assert_eq!(
        cue_text_at(plan, 1_200_000_000),
        None,
        "1.2 s is before the mapped cue (1.5 s)"
    );
}

/// #526 review: a bare local rendition path (a local master) with RELATIVE
/// segment URIs resolves and reads its segments.
#[test]
fn a_bare_path_rendition_reads_its_relative_segments() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("s0.vtt"),
        "WEBVTT\n\n00:00:01.000 --> 00:00:03.000\nbare path cue\n",
    )
    .expect("segment");
    std::fs::write(
        dir.path().join("subs.m3u8"),
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4,\ns0.vtt\n#EXT-X-ENDLIST\n",
    )
    .expect("playlist");
    let plan = plan_for(format!("{}/subs.m3u8", dir.path().display()), false);
    show(&plan, 1_500_000_000);
    assert_eq!(
        cue_text_at(plan, 1_500_000_000).as_deref(),
        Some("bare path cue")
    );
}
