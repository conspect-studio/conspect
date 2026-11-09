//! Truthful program-audio binding regressions (t70 causal 1 / t62 finding).
//!
//! The dispatcher assembles each binding's whole window from the audio stream
//! indexes the binding DECLARES, so those indexes must be decided against the
//! program-audio encode the run actually OPENS. Keying them off the input
//! `[audio]` ROUTING section (or silently dropping declared audio) produced two
//! dead-air paths: a run with program audio enabled but no routing section
//! declared no audio track — every AAC packet was staged and admitted by the
//! dispatcher but silently never assembled into any binding window (the rt8b
//! audio RED root cause) — and a run with routing but no program-audio encode
//! declared a phantom track and killed the consumer at startup.

#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;

use multiview_cli::pipeline::{Pipeline, SendPolicy, StreamTestParams, TestSinkOutcome};
use multiview_config::MultiviewConfig;
use multiview_engine::{CooperativePacer, ManualTimeSource, StopSignal, TimeSource};
use multiview_ffmpeg::{EncodedPacket, StreamKind};

/// The tiny single-tile FFV1 config (LGPL, intra-only, zero-delay — real
/// packets provably flow on this base). `audio_routing` adds the INPUT-side
/// `[audio]` routing section; `explicit_output_audio` adds a DECLARED
/// `[[outputs.audio]]` program selection on the output.
fn config_text(audio_routing: bool, explicit_output_audio: bool) -> String {
    let routing = if audio_routing {
        "\n[audio]\nsample_rate_hz = 48000\n"
    } else {
        ""
    };
    let declared = if explicit_output_audio {
        "\n[outputs.audio]\nmode = \"program\"\n"
    } else {
        ""
    };
    format!(
        r##"
schema_version = 1

[canvas]
width = 64
height = 64
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
id = "in_a"
kind = "test"

[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"

[[outputs]]
kind = "hls"
path = "/tmp/mv-audio-binding/index.m3u8"
codec = "ffv1"
segment_ms = 1000{declared}{routing}
"##
    )
}

fn build_pipeline(audio_routing: bool, explicit_output_audio: bool) -> Pipeline {
    let config =
        MultiviewConfig::load_from_toml(&config_text(audio_routing, explicit_output_audio))
            .expect("parse config");
    config.validate().expect("config validates");
    Pipeline::build(&config).expect("build real pipeline")
}

fn spawn_clock_paced(
    clock: Arc<ManualTimeSource>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    period: std::time::Duration,
) {
    std::thread::spawn(move || {
        while !stop.load(Ordering::Acquire) {
            clock.advance(std::time::Duration::from_millis(40));
            std::thread::sleep(period);
        }
    });
}

/// Count video and audio packets separately at the sink.
async fn drive_counting(
    mut pipeline: Pipeline,
    ticks: u64,
) -> (
    Result<multiview_cli::pipeline::StreamTestResult, multiview_cli::pipeline::PipelineError>,
    usize,
    usize,
) {
    let video = Arc::new(AtomicUsize::new(0));
    let audio = Arc::new(AtomicUsize::new(0));
    let (v_in, a_in) = (Arc::clone(&video), Arc::clone(&audio));
    let runner = move |rx: Receiver<EncodedPacket>| -> TestSinkOutcome {
        let mut frames = 0;
        while let Ok(packet) = rx.recv() {
            match packet.kind() {
                StreamKind::Video => {
                    v_in.fetch_add(1, Ordering::Release);
                }
                _ => {
                    a_in.fetch_add(1, Ordering::Release);
                }
            }
            frames += 1;
        }
        TestSinkOutcome { frames }
    };

    let clock = Arc::new(ManualTimeSource::new());
    let stop = StopSignal::new();
    let driver_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    spawn_clock_paced(
        Arc::clone(&clock),
        Arc::clone(&driver_stop),
        std::time::Duration::from_millis(25),
    );
    let concrete: Arc<ManualTimeSource> = Arc::clone(&clock);
    let ts: Arc<dyn TimeSource> = concrete;
    let result = pipeline
        .drive_streaming_for_test(
            StreamTestParams {
                time: ts,
                pacer: CooperativePacer,
                max_ticks: Some(ticks),
                policy: SendPolicy::BlockForExact,
                runners: vec![Box::new(runner)],
                shed_at_tick: None,
                consumer_fault_after_ticks: None,
                hot_tick_observer: None,
            },
            &stop,
        )
        .await;
    driver_stop.store(true, Ordering::Release);
    (
        result,
        video.load(Ordering::Acquire),
        audio.load(Ordering::Acquire),
    )
}

/// Program audio enabled WITHOUT an input routing section: the run's OPENED
/// encoder is the ground truth — the binding declares the program track and
/// the sink must receive BOTH video and audio packets. (This is the exact
/// rt8b shape: audio was encoded, staged and admitted by the dispatcher, but
/// silently never assembled into any binding window.)
#[tokio::test(flavor = "current_thread")]
async fn program_audio_seam_reaches_the_sink() {
    let mut pipeline = build_pipeline(false, false);
    pipeline.enable_program_audio();

    let (result, video, audio) = drive_counting(pipeline, 40).await;
    let out = result.expect("program audio must flow through the real seam");
    assert_eq!(video, 40, "every FFV1 frame reaches the sink");
    assert!(
        audio >= 40,
        "program audio must be ASSEMBLED into binding windows and reach the sink \
         (the rt8b root cause: staged but silently never bound), got {audio} audio packets"
    );
    // The binding's truth line must count the audio packets too.
    assert!(
        out.report
            .outputs
            .iter()
            .any(|line| line.contains("packets admitted") && !line.contains(" 40 packets")),
        "the binding line must report more than the 40 video packets, got {:?}",
        out.report.outputs
    );
}

/// An input `[audio]` ROUTING section with program audio NOT enabled: routing
/// is an input-side fact, not a program-audio encode — the run must proceed
/// VIDEO-ONLY (no phantom program track, no dispatcher InvalidConfig death),
/// with the implicit default's track 0 stripped.
#[tokio::test(flavor = "current_thread")]
async fn audio_routing_without_encode_is_video_only() {
    let pipeline = build_pipeline(true, false);
    let (result, video, audio) = drive_counting(pipeline, 40).await;
    let out = result.expect("routing without program-audio encode must run video-only");
    assert_eq!(video, 40, "every FFV1 frame reaches the sink");
    assert_eq!(audio, 0, "no program-audio encode: no audio packets");
    assert!(
        out.report
            .outputs
            .iter()
            .any(|line| line.contains("40 windows / 40 packets admitted")),
        "video-only binding truth expected, got {:?}",
        out.report.outputs
    );
}

/// An EXPLICIT `[[outputs.audio]]` program declaration with program audio NOT
/// enabled: the operator asked for audio on this output; silently airing
/// video-only is dead air. The run must fail with a typed error naming the
/// output — the explicit reject.
#[tokio::test(flavor = "current_thread")]
async fn declared_program_audio_without_encode_rejects_explicitly() {
    let pipeline = build_pipeline(false, true);
    let (result, _video, _audio) = drive_counting(pipeline, 10).await;
    let err = result.expect_err("a declared audio selection must not silently air video-only");
    match &err {
        multiview_cli::pipeline::PipelineError::Output { kind, reason } => {
            assert_eq!(*kind, "encode-audio");
            assert!(
                reason.contains("explicitly declares an audio selection")
                    || reason.contains("no program-audio encoder"),
                "the reject must name the declared selection and the missing encode, got {reason:?}"
            );
        }
        other => panic!("expected a typed output error, got {other:?}"),
    }
}

/// Declared program audio WITH the encode enabled: the declaration is honored —
/// audio flows. (The reject must never fire when the run carries the encoder.)
#[tokio::test(flavor = "current_thread")]
async fn declared_program_audio_with_encode_flows() {
    let mut pipeline = build_pipeline(false, true);
    pipeline.enable_program_audio();
    let (result, video, audio) = drive_counting(pipeline, 40).await;
    result.expect("declared + enabled must run");
    assert_eq!(video, 40);
    assert!(audio >= 40, "declared program audio must flow, got {audio}");
}

/// FRAME-SUBSTITUTION REGRESSION (t73): a deterministically shed tick (the
/// seam drops the hot item at tick `SHED` — the exact replay of a transient
/// hot-queue shed) must be SUBSTITUTED by the consumer: the muxed output
/// still carries exactly one video packet for EVERY tick 0..TICKS-1 with
/// strictly contiguous PTS (the substituted tick's packet restamped at the
/// missing tick's index — never a missing encoded tick, never a timestamp
/// jump), and the program audio's cumulative samples still equal the tick
/// ideal exactly. Real FFV1 encoder, real dispatcher, real binding delivery;
/// the sink observes the actual delivered windows.
#[tokio::test(flavor = "current_thread")]
async fn substituted_shed_tick_keeps_pts_contiguous_and_audio_exact() {
    const TICKS: u64 = 40;
    const SHED: u64 = 20;
    const SAMPLES_PER_TICK: u64 = 1_920;

    let mut pipeline = build_pipeline(false, false);
    pipeline.enable_program_audio();

    let video_pts: Arc<std::sync::Mutex<Vec<i64>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let max_audio_pts = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let audio_packets = Arc::new(AtomicUsize::new(0));
    let (pts_in, max_in, aud_in) = (
        Arc::clone(&video_pts),
        Arc::clone(&max_audio_pts),
        Arc::clone(&audio_packets),
    );
    let runner = move |rx: Receiver<EncodedPacket>| -> TestSinkOutcome {
        let mut frames = 0;
        while let Ok(packet) = rx.recv() {
            match packet.kind() {
                StreamKind::Video => {
                    if let Some(pts) = packet.pts() {
                        pts_in.lock().unwrap().push(pts);
                    }
                    frames += 1;
                }
                _ => {
                    if let Some(pts) = packet.pts() {
                        let pts = u64::try_from(pts).unwrap_or(0);
                        let prev = max_in.load(Ordering::Acquire);
                        if pts > prev {
                            max_in.store(pts, Ordering::Release);
                        }
                    }
                    aud_in.fetch_add(1, Ordering::Release);
                }
            }
        }
        TestSinkOutcome { frames }
    };

    let clock = Arc::new(ManualTimeSource::new());
    let stop = StopSignal::new();
    let driver_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    spawn_clock_paced(
        Arc::clone(&clock),
        Arc::clone(&driver_stop),
        std::time::Duration::from_millis(25),
    );
    let concrete: Arc<ManualTimeSource> = Arc::clone(&clock);
    let ts: Arc<dyn TimeSource> = concrete;
    let result = pipeline
        .drive_streaming_for_test(
            StreamTestParams {
                time: ts,
                pacer: CooperativePacer,
                max_ticks: Some(TICKS),
                policy: SendPolicy::DropOnOverload,
                runners: vec![Box::new(runner)],
                shed_at_tick: Some(SHED),
                consumer_fault_after_ticks: None,
                hot_tick_observer: None,
            },
            &stop,
        )
        .await;
    driver_stop.store(true, Ordering::Release);
    let out = result.expect("the shed tick must be substituted, never fatal");

    // The engine emitted every tick; the run completed honestly.
    assert_eq!(out.report.frames, TICKS, "every engine tick processed");

    // DIRECT SUBSTITUTION PROOF: the delivered video PTS sequence is exactly
    // 0..TICKS — one frame per tick, CONTIGUOUS, including the shed tick 20
    // (its packet carries the substituted restamped frame at PTS 20).
    let mut pts = video_pts.lock().unwrap().clone();
    pts.sort_unstable();
    assert_eq!(
        pts.len(),
        usize::try_from(TICKS).unwrap(),
        "exactly one video packet per output tick, got {pts:?}"
    );
    for (index, p) in pts.iter().enumerate() {
        assert_eq!(
            *p,
            i64::try_from(index).unwrap(),
            "video PTS must be strictly contiguous tick stamps — tick {index} has PTS {p} \
             (a missing or jumped tick breaks the fixed cadence)"
        );
    }

    // Audio is unaffected by the video shed: cumulative samples equal the tick
    // ideal (the bus catch-up is a pure function of the tick index).
    let audio_end = max_audio_pts.load(Ordering::Acquire) + 1024;
    let tick_ideal = TICKS * SAMPLES_PER_TICK;
    assert!(
        tick_ideal.saturating_sub(audio_end) <= SAMPLES_PER_TICK,
        "audio cumulative {audio_end} must equal the tick ideal {tick_ideal} \
         (within one in-flight AAC frame), shed tick must not drift audio"
    );
    assert!(
        audio_packets.load(Ordering::Acquire) >= usize::try_from(TICKS).unwrap(),
        "audio packets flow for every tick"
    );
}
