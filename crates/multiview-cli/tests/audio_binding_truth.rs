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
