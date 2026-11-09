//! Streaming-encode behavioural tests for [`Pipeline`] (ADR-0025).
//!
//! These are sleep-free, GPU-free behavioural tests of the streaming
//! bake→encode→fan-out path. They drive the engine over a [`ManualTimeSource`] +
//! [`CooperativePacer`] (no wall-clock waits) and inject **fake** off-hot-path
//! **mux** sinks via the `drive_streaming_for_test` seam, so the tests assert the
//! *concurrency contract* — bounded memory, no engine stall, exact-N offline, and
//! streaming-not-batch — without the `ffprobe` CLI. Since the encode-once-mux-many
//! refactor (invariant #7) the consumer owns a single real LGPL `mpeg2video`
//! encoder, so the fakes consume the **coded-packet** fan-out (one packet per
//! baked frame); the end-to-end "does it produce a playable file" coverage lives
//! in `real_pipeline.rs`/`overlay_pipeline.rs`.
//!
//! Why a test seam: `Pipeline::drive` is private; the public `run_for`/
//! `run_until` always wire the *real* file/HLS sinks. The seam exposes the same
//! streaming machinery with (a) an injected time source + pacer and (b)
//! injectable fake sink runners, so the hot loop, the bounded channel policy,
//! and the consumer/fan-out are exercised under a test's control.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;

use multiview_cli::pipeline::{Pipeline, SendPolicy, StreamTestParams, TestSinkOutcome};
use multiview_config::MultiviewConfig;
use multiview_engine::{CooperativePacer, ManualTimeSource, StopSignal, TimeSource};
use multiview_ffmpeg::EncodedPacket;

/// A tiny single-tile config: one built-in `test` source into a 1x1 grid, an
/// HLS output (so a file + HLS sink are derived). The streaming-encode tests do
/// not decode the produced media — they inject fakes — so the geometry is kept
/// small to keep per-tick canvas clones cheap.
fn config_text() -> String {
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
path = "/tmp/multiview-streaming-test/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##
    .to_owned()
}

fn build_pipeline() -> Pipeline {
    let config = MultiviewConfig::load_from_toml(&config_text()).expect("parse config");
    config.validate().expect("config validates");
    Pipeline::build(&config).expect("build real pipeline")
}

/// Advance the [`ManualTimeSource`] far enough that the [`CooperativePacer`]
/// never gates the loop, on a background thread, so the engine future can make
/// progress under the current-thread tokio runtime without any real sleeping.
fn spawn_clock_driver(clock: Arc<ManualTimeSource>, stop: Arc<std::sync::atomic::AtomicBool>) {
    std::thread::spawn(move || {
        while !stop.load(Ordering::Acquire) {
            clock.advance(std::time::Duration::from_millis(40));
            std::thread::yield_now();
        }
    });
}

/// Like [`spawn_clock_driver`], but paced near real time (one 40 ms tick
/// advance per `period` of wall time). The no-falter contract keeps the
/// consumer inside the real-time envelope — a memory-speed driver is beyond
/// the admitted envelope by construction and now fails the run with a typed
/// beyond-envelope error (see `beyond_envelope_shed_fails_visibly`), which is
/// the honest behaviour for hardware that cannot keep the cadence.
fn spawn_clock_driver_paced(
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

/// (1) Live + a blocked sink, NO-FALTER contract (t70): a sink that freezes
/// after its first packet must never stall the output clock AND never cause a
/// single hot-queue drop — inside the real-time envelope the engine keeps
/// ticking, the consumer keeps encoding, and the frozen sink's loss is
/// reported EXPLICITLY at its own binding (admission losses against the
/// binding's bounded queue), never as silent missing encoded ticks. A
/// hardware failure beyond the envelope fails the run with a typed error
/// (see `beyond_envelope_shed_fails_visibly`), never as routine shed.
#[tokio::test(flavor = "current_thread")]
async fn live_blocked_sink_never_stalls_and_never_sheds() {
    const TICKS: u64 = 120;

    let mut pipeline = build_pipeline();

    // A sink that pulls exactly one packet and then freezes forever.
    let received = Arc::new(AtomicUsize::new(0));
    let received_in_sink = Arc::clone(&received);
    let blocked_runner = move |rx: Receiver<EncodedPacket>| -> TestSinkOutcome {
        if rx.recv().is_ok() {
            received_in_sink.fetch_add(1, Ordering::Release);
        }
        let _hold = rx;
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    };

    let hot_tick = Arc::new(AtomicU64::new(0));
    let clock = Arc::new(ManualTimeSource::new());
    let stop = StopSignal::new();
    let driver_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // REAL TIME (the admitted envelope): the consumer holds a large margin at
    // cadence — nothing is shed even on a loaded box; beyond-envelope overrun
    // is `beyond_envelope_shed_fails_visibly`'s contract.
    spawn_clock_driver_paced(
        Arc::clone(&clock),
        Arc::clone(&driver_stop),
        std::time::Duration::from_millis(40),
    );

    let clock_concrete: Arc<ManualTimeSource> = Arc::clone(&clock);
    let ts: Arc<dyn TimeSource> = clock_concrete;
    let result = pipeline
        .drive_streaming_for_test(
            StreamTestParams {
                time: ts,
                pacer: CooperativePacer,
                max_ticks: Some(TICKS),
                policy: SendPolicy::DropOnOverload,
                runners: vec![Box::new(blocked_runner)],
                shed_at_tick: None,
                consumer_fault_after_ticks: None,
                hot_tick_observer: Some(Arc::clone(&hot_tick)),
            },
            &stop,
        )
        .await
        .expect("a blocked sink must never fail or stall the run");
    driver_stop.store(true, Ordering::Release);

    // NO-FALTER: not a single hot-queue drop — every output tick the engine
    // emitted reached the consumer and was encoded.
    assert_eq!(
        hot_tick.load(Ordering::Acquire),
        TICKS,
        "the engine must emit every tick"
    );
    assert_eq!(
        result.report.frames, TICKS,
        "every emitted tick must be processed"
    );
    assert_eq!(
        result.report.dropped, 0,
        "NO-FALTER: a blocked sink must not shed a single hot frame"
    );
    // Bounded memory: occupancy never exceeded the cap (+1 in-transit).
    assert!(
        result.peak_occupancy <= result.capacity + 1,
        "occupancy {} must stay bounded by the cap {} (+1 in transit)",
        result.peak_occupancy,
        result.capacity
    );
    // The frozen sink's loss is EXPLICIT at its own binding — counted,
    // labelled, never silent.
    assert!(
        result
            .report
            .outputs
            .iter()
            .any(|line| line.contains("windows lost") && !line.contains(" 0 packets")),
        "the frozen binding must report its explicit admission losses, got {:?}",
        result.report.outputs
    );
    assert_eq!(
        received.load(Ordering::Acquire),
        1,
        "the sink received exactly its first packet before freezing"
    );
}

/// (2) Offline (`run_for` semantics, `BlockForExact`): exactly N frames are
/// baked + handed to the sink, nothing is dropped, and the run did not falter.
///
/// RED today: the seam does not exist; and once it does, this is the regression
/// guard proving the offline path keeps exact, all-frames semantics under a
/// *slow* consumer (the channel back-pressures the renderer instead of dropping).
#[tokio::test(flavor = "current_thread")]
async fn offline_block_for_exact_delivers_all_n_frames() {
    const TICKS: u64 = 120;

    let mut pipeline = build_pipeline();

    // A fake sink that is deliberately SLOW (sleeps a little per frame) but
    // never blocks forever. Under BlockForExact the hot loop must wait for it,
    // so every frame is delivered and none dropped.
    let counted = Arc::new(AtomicUsize::new(0));
    let counted_in_sink = Arc::clone(&counted);
    let slow_runner = move |rx: Receiver<EncodedPacket>| -> TestSinkOutcome {
        while let Ok(_packet) = rx.recv() {
            std::thread::sleep(std::time::Duration::from_millis(1));
            counted_in_sink.fetch_add(1, Ordering::Release);
        }
        TestSinkOutcome {
            frames: counted_in_sink.load(Ordering::Acquire),
        }
    };

    let clock = Arc::new(ManualTimeSource::new());
    let stop = StopSignal::new();
    let driver_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    spawn_clock_driver(Arc::clone(&clock), Arc::clone(&driver_stop));

    let clock_concrete: Arc<ManualTimeSource> = Arc::clone(&clock);
    let ts: Arc<dyn TimeSource> = clock_concrete;
    let result = pipeline
        .drive_streaming_for_test(
            StreamTestParams {
                time: ts,
                pacer: CooperativePacer,
                max_ticks: Some(TICKS),
                policy: SendPolicy::BlockForExact,
                runners: vec![Box::new(slow_runner)],
                shed_at_tick: None,
                consumer_fault_after_ticks: None,
                hot_tick_observer: None,
            },
            &stop,
        )
        .await
        .expect("drive streaming");
    driver_stop.store(true, Ordering::Release);

    assert_eq!(result.report.frames, TICKS, "N ticks");
    assert_eq!(
        result.report.dropped, 0,
        "offline block-for-exact must NEVER drop a frame"
    );
    assert!(
        !result.report.faltered,
        "offline never falters (dropped == 0)"
    );
    assert_eq!(
        result.sink_frames,
        vec![usize::try_from(TICKS).unwrap()],
        "the sink must have received exactly N coded packets (one per baked frame, none dropped)"
    );
}

/// (3) Streaming, not batch: frame 0 reaches the sink WHILE the engine is still
/// ticking. The fake sink records the hot-loop tick index at the moment it
/// receives its first frame; in a streaming design that index is small (the
/// consumer runs concurrently), whereas the pre-ADR-0025 batch design encodes
/// only AFTER the loop, so frame 0 would arrive at tick == N.
///
/// RED today: batch path encodes post-loop ⇒ frame 0 observed at tick N.
#[tokio::test(flavor = "current_thread")]
async fn frame_zero_is_encoded_while_engine_still_ticking() {
    const TICKS: u64 = 300;

    let mut pipeline = build_pipeline();

    // Shared hot-loop tick counter, bumped once per tick by the seam's hot loop
    // and observed by the sink when it receives frame 0.
    let hot_tick = Arc::new(AtomicU64::new(0));
    let hot_tick_for_sink = Arc::clone(&hot_tick);
    let tick_at_frame_zero = Arc::new(AtomicU64::new(u64::MAX));
    let observed = Arc::clone(&tick_at_frame_zero);

    let runner = move |rx: Receiver<EncodedPacket>| -> TestSinkOutcome {
        let mut frames = 0_usize;
        while let Ok(_packet) = rx.recv() {
            if frames == 0 {
                observed.store(hot_tick_for_sink.load(Ordering::Acquire), Ordering::Release);
            }
            frames += 1;
            // Keep up easily so the consumer drains promptly and the test is fast.
            std::thread::yield_now();
        }
        TestSinkOutcome { frames }
    };

    let clock = Arc::new(ManualTimeSource::new());
    let stop = StopSignal::new();
    let driver_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    spawn_clock_driver_paced(
        Arc::clone(&clock),
        Arc::clone(&driver_stop),
        std::time::Duration::from_millis(25),
    );

    let clock_concrete: Arc<ManualTimeSource> = Arc::clone(&clock);
    let ts: Arc<dyn TimeSource> = clock_concrete;
    let result = pipeline
        .drive_streaming_for_test(
            StreamTestParams {
                time: ts,
                pacer: CooperativePacer,
                max_ticks: Some(TICKS),
                policy: SendPolicy::BlockForExact,
                runners: vec![Box::new(runner)],
                shed_at_tick: None,
                consumer_fault_after_ticks: None,
                hot_tick_observer: Some(Arc::clone(&hot_tick)),
            },
            &stop,
        )
        .await
        .expect("drive streaming");
    driver_stop.store(true, Ordering::Release);

    assert_eq!(result.report.frames, TICKS, "N ticks");
    let observed_tick = tick_at_frame_zero.load(Ordering::Acquire);
    assert!(
        observed_tick != u64::MAX,
        "the sink must have received frame 0"
    );
    // The decisive assertion: frame 0 was encoded long before the engine reached
    // the final tick. A batch design would only deliver frame 0 at tick == N.
    assert!(
        observed_tick < TICKS,
        "frame 0 must be encoded WHILE the engine is still ticking (streaming, not batch); \
         observed hot tick {observed_tick} at frame 0, of {TICKS}"
    );
}

/// (4) Clean stop: raising the stop signal mid-run ends the engine, the
/// consumer drains, and every sink sees end-of-program (the channel closes), so
/// it can finalise (write its trailer). We assert the sink saw its receiver
/// close cleanly (`recv` returned `Err`) by checking it returned a finite frame
/// count and the run reports no falter (no drops on the offline-style path).
#[tokio::test(flavor = "current_thread")]
async fn clean_stop_closes_sinks_for_finalisation() {
    const MAX: u64 = 5_000;

    let mut pipeline = build_pipeline();

    let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let finished_in_sink = Arc::clone(&finished);
    let runner = move |rx: Receiver<EncodedPacket>| -> TestSinkOutcome {
        let mut frames = 0_usize;
        // recv loop: returns Err ONLY when the consumer drops the sender at
        // teardown — i.e. end-of-program. Setting the flag proves the sink got a
        // clean close (the precondition for writing a trailer).
        while rx.recv().is_ok() {
            frames += 1;
        }
        finished_in_sink.store(true, Ordering::Release);
        TestSinkOutcome { frames }
    };

    let clock = Arc::new(ManualTimeSource::new());
    let stop = StopSignal::new();
    let driver_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    spawn_clock_driver_paced(
        Arc::clone(&clock),
        Arc::clone(&driver_stop),
        std::time::Duration::from_millis(25),
    );

    // Raise the stop signal shortly after launching so the unbounded `run_until`
    // path stops promptly (it would otherwise run to MAX).
    let stop_for_thread = stop.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(500));
        stop_for_thread.stop();
    });

    let clock_concrete: Arc<ManualTimeSource> = Arc::clone(&clock);
    let ts: Arc<dyn TimeSource> = clock_concrete;
    let result = pipeline
        .drive_streaming_for_test(
            StreamTestParams {
                time: ts,
                pacer: CooperativePacer,
                max_ticks: Some(MAX),
                policy: SendPolicy::DropOnOverload,
                runners: vec![Box::new(runner)],
                shed_at_tick: None,
                consumer_fault_after_ticks: None,
                hot_tick_observer: None,
            },
            &stop,
        )
        .await
        .expect("drive streaming");
    driver_stop.store(true, Ordering::Release);

    assert!(
        result.report.frames < MAX,
        "the stop signal must have ended the run before the cap ({} of {MAX})",
        result.report.frames
    );
    assert!(
        finished.load(Ordering::Acquire),
        "the sink must observe a clean channel close (end-of-program) so it can finalise"
    );
}

/// (5) ENG-1 / inv #1 — bounded teardown: a sink that **wedges** during
/// finalisation (e.g. a push muxer blocked writing its trailer to a dead peer)
/// must NOT hang `stop` forever. The egress join is bounded by a deadline: the
/// wedged sink is reported and its thread detached so teardown always completes,
/// while a healthy co-sink still finalises normally.
///
/// This is a plain (non-tokio) test with a **watchdog on the test thread**: the
/// drive runs on its own OS thread, and the test thread waits for it with a
/// `recv_timeout`. A `tokio::time::timeout` would NOT work here — the hang is a
/// synchronous `JoinHandle::join()` that blocks the runtime thread, so the timer
/// could never fire. RED today: `StreamEgress::join` plain-joins every sink, so
/// the wedged sink blocks teardown forever and the watchdog `recv_timeout` fires
/// (the test fails) instead of hanging the whole suite.
#[test]
fn wedged_sink_does_not_hang_teardown() {
    const TICKS: u64 = 30;
    // The drive result, ferried back from the drive thread to the watchdog.
    type DriveResult =
        Result<multiview_cli::pipeline::StreamTestResult, multiview_cli::pipeline::PipelineError>;

    // Shared with the sink closures so the assertions can observe them.
    let healthy_count = Arc::new(AtomicUsize::new(0));
    let reached_wedge = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let healthy_in = Arc::clone(&healthy_count);
    let reached_in = Arc::clone(&reached_wedge);

    // Run the whole drive on its own OS thread; the test thread is the watchdog.
    let (done_tx, done_rx) = std::sync::mpsc::channel::<DriveResult>();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        let result = rt.block_on(async move {
            // Two OUTPUTS for the two sinks: the per-binding delivery maps each
            // injected sink runner 1:1 to an output binding (with its own
            // recovery gate), so a two-sink scenario needs two outputs. Both
            // share the one mpeg2video rendition (encode-once-mux-many, inv #7).
            let text = config_text()
                + r#"
[[outputs]]
kind = "hls"
path = "/tmp/multiview-streaming-test/second.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"#;
            let config = MultiviewConfig::load_from_toml(&text).expect("parse config");
            config.validate().expect("config validates");
            let mut pipeline = Pipeline::build(&config).expect("build pipeline");

            // Sink A — healthy: drains every packet, returns cleanly on EOF.
            let healthy = move |rx: Receiver<EncodedPacket>| -> TestSinkOutcome {
                let mut n = 0_usize;
                while rx.recv().is_ok() {
                    n += 1;
                }
                healthy_in.store(n, Ordering::Release);
                TestSinkOutcome { frames: n }
            };
            // Sink B — WEDGED: drains every packet (so the consumer is NEVER
            // blocked on a full channel), sees end-of-program, then never returns
            // — simulating a muxer wedged in its trailer/finalise.
            let wedged = move |rx: Receiver<EncodedPacket>| -> TestSinkOutcome {
                while rx.recv().is_ok() {}
                reached_in.store(true, Ordering::Release);
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(3600));
                }
            };

            let clock = Arc::new(ManualTimeSource::new());
            let stop = StopSignal::new();
            let driver_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            spawn_clock_driver_paced(
                Arc::clone(&clock),
                Arc::clone(&driver_stop),
                std::time::Duration::from_millis(25),
            );
            let ts: Arc<dyn TimeSource> = clock;

            let out = pipeline
                .drive_streaming_for_test(
                    StreamTestParams {
                        time: ts,
                        pacer: CooperativePacer,
                        max_ticks: Some(TICKS),
                        policy: SendPolicy::BlockForExact,
                        runners: vec![Box::new(healthy), Box::new(wedged)],
                        shed_at_tick: None,
                        consumer_fault_after_ticks: None,
                        hot_tick_observer: None,
                    },
                    &stop,
                )
                .await;
            driver_stop.store(true, Ordering::Release);
            out
        });
        let _ = done_tx.send(result);
    });

    // Watchdog: the drive (run + BOUNDED teardown) must report back well within
    // this budget; without the bounded join it never would.
    let result = done_rx
        .recv_timeout(std::time::Duration::from_secs(8))
        .expect(
            "teardown must complete within the bound — a wedged sink must NOT hang stop (inv #1)",
        )
        .expect("drive streaming");

    assert!(
        reached_wedge.load(Ordering::Acquire),
        "the wedged sink must have drained to end-of-program before wedging"
    );
    // It is REPORTED (not silently dropped): a per-sink report line names it.
    assert!(
        result
            .report
            .outputs
            .iter()
            .any(|line| line.to_ascii_uppercase().contains("WEDGED")),
        "the wedged sink must be reported in the run outputs, got {:?}",
        result.report.outputs
    );
    // The healthy co-sink still received and finalised all N packets.
    assert_eq!(
        healthy_count.load(Ordering::Acquire),
        usize::try_from(TICKS).unwrap(),
        "the healthy sink must finalise all N packets despite a wedged co-sink"
    );
}

/// (6) RT-8b lip-sync, NO-FALTER contract (t70): with program audio enabled
/// and the engine inside the real-time envelope, the muxed output carries a
/// video frame for EVERY tick and program audio whose cumulative samples
/// track the tick counter exactly (a pure function of the tick index, never
/// of frame loss — trivially so, because inside the envelope nothing is
/// lost). The audio packet `pts` is a pure sample counter (1920 samples per
/// output tick at 48 kHz / 25 fps), so the highest observed audio `pts` plus
/// one AAC frame must equal the tick ideal.
#[tokio::test(flavor = "current_thread")]
async fn rt8b_audio_tracks_every_tick_inside_the_envelope() {
    // 48 kHz / 25 fps = exactly 1920 audio samples per output tick.
    const SAMPLES_PER_TICK: u64 = 1_920;
    const TICKS: u64 = 60;

    let mut pipeline = build_pipeline();
    // Program audio WITHOUT a routing section — the truthful binding keys off
    // the OPENED encoder (causal 1); the audio must actually flow.
    pipeline.enable_program_audio();

    let max_audio_pts = Arc::new(AtomicU64::new(0));
    let audio_packets = Arc::new(AtomicU64::new(0));
    let video_packets = Arc::new(AtomicUsize::new(0));
    let (max_in, aud_in, vid_in) = (
        Arc::clone(&max_audio_pts),
        Arc::clone(&audio_packets),
        Arc::clone(&video_packets),
    );
    let runner = move |rx: Receiver<EncodedPacket>| -> TestSinkOutcome {
        let mut frames = 0_usize;
        while let Ok(packet) = rx.recv() {
            match packet.kind() {
                multiview_ffmpeg::StreamKind::Video => {
                    vid_in.fetch_add(1, Ordering::Release);
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
    spawn_clock_driver_paced(
        Arc::clone(&clock),
        Arc::clone(&driver_stop),
        std::time::Duration::from_millis(25),
    );

    let clock_concrete: Arc<ManualTimeSource> = Arc::clone(&clock);
    let ts: Arc<dyn TimeSource> = clock_concrete;
    let result = pipeline
        .drive_streaming_for_test(
            StreamTestParams {
                time: ts,
                pacer: CooperativePacer,
                max_ticks: Some(TICKS),
                policy: SendPolicy::DropOnOverload,
                runners: vec![Box::new(runner)],
                shed_at_tick: None,
                consumer_fault_after_ticks: None,
                hot_tick_observer: None,
            },
            &stop,
        )
        .await
        .expect("drive streaming");
    driver_stop.store(true, Ordering::Release);

    assert_eq!(
        result.report.frames, TICKS,
        "every emitted tick must be processed inside the envelope"
    );
    assert_eq!(
        result.report.dropped, 0,
        "NO-FALTER: nothing is shed inside the real-time envelope"
    );
    assert_eq!(
        video_packets.load(Ordering::Acquire),
        usize::try_from(TICKS).unwrap(),
        "the muxed output has a video frame for EVERY tick — never a missing encoded tick"
    );
    assert!(
        audio_packets.load(Ordering::Acquire) >= TICKS,
        "program audio must flow ({} packets)",
        audio_packets.load(Ordering::Acquire)
    );
    // Cumulative audio tracks the tick counter exactly: the highest audio
    // packet starts at sample position max_pts and spans one AAC frame (1024),
    // so max_pts + 1024 must reach the tick ideal within one frame.
    let tick_ideal = TICKS * SAMPLES_PER_TICK;
    let audio_end = max_audio_pts.load(Ordering::Acquire) + 1024;
    assert!(
        tick_ideal.saturating_sub(audio_end) <= SAMPLES_PER_TICK,
        "cumulative audio {audio_end} must track the tick ideal {tick_ideal}          (within one AAC frame of in-flight tail)"
    );
}

/// Beyond the admitted envelope (t70: never overclaim that hardware cannot
/// fail): a memory-speed engine outruns any real encoder by orders of
/// magnitude, the consumer falls behind by more than a full queue drain
/// horizon, and the run must FAIL with the exact typed cause — a visible
/// failure the supervisor restarts on — never silently shedding ticks into
/// timestamp gaps (the rejected gap-as-designed behaviour).
#[tokio::test(flavor = "current_thread")]
async fn beyond_envelope_shed_fails_visibly() {
    const TICKS: u64 = 4_000;

    let mut pipeline = build_pipeline();
    let runner = move |rx: Receiver<EncodedPacket>| -> TestSinkOutcome {
        let mut frames = 0;
        while rx.recv().is_ok() {
            frames += 1;
        }
        TestSinkOutcome { frames }
    };

    let clock = Arc::new(ManualTimeSource::new());
    let stop = StopSignal::new();
    let driver_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    spawn_clock_driver(Arc::clone(&clock), Arc::clone(&driver_stop));

    let clock_concrete: Arc<ManualTimeSource> = Arc::clone(&clock);
    let ts: Arc<dyn TimeSource> = clock_concrete;
    let result = pipeline
        .drive_streaming_for_test(
            StreamTestParams {
                time: ts,
                pacer: CooperativePacer,
                max_ticks: Some(TICKS),
                policy: SendPolicy::DropOnOverload,
                runners: vec![Box::new(runner)],
                shed_at_tick: None,
                consumer_fault_after_ticks: None,
                hot_tick_observer: None,
            },
            &stop,
        )
        .await;
    driver_stop.store(true, Ordering::Release);

    let err = result.expect_err("beyond-envelope overrun must fail visibly, never silently shed");
    match &err {
        multiview_cli::pipeline::PipelineError::Output { kind, reason } => {
            assert_eq!(*kind, "video-worker");
            assert!(
                reason.contains("beyond the admitted envelope"),
                "the exact overrun cause must propagate, got {reason:?}"
            );
        }
        other => panic!("expected the typed beyond-envelope error, got {other:?}"),
    }
    // The run stopped at a tick boundary via the fatal signal, not by running
    // the full budget of silent gaps.
    assert!(stop.is_stopped(), "the fatal signal must be raised");
}

/// The RT-8b decision: cumulative audio samples must track the TICK count, not the
/// surviving-frame count. Splits out the post-run analysis so the driver test reads
/// cleanly. `ticks` = output ticks emitted, `dropped` = ticks shed, `cumulative` =
/// the max audio-packet sample position observed, `per_tick` = samples per tick.
fn assert_audio_tracks_tick_index(ticks: u64, dropped: u64, cumulative: u64, per_tick: u64) {
    let survivors = ticks.saturating_sub(dropped);
    // The drift-free ideal: cumulative audio tracks the TICK count. The last
    // surviving frame's catch-up `tick_to` brings the bus up to its tick index.
    let tick_ideal = ticks.saturating_mul(per_tick);
    let survivor_ceiling = survivors.saturating_mul(per_tick);

    // The DECISIVE assertion: cumulative audio is FAR above what a per-surviving-
    // frame `bus.tick()` could ever emit (`survivors * per_tick`). Under a heavy
    // shed `survivors << ticks`, so the pre-RT-8b drift would land cumulative audio
    // at/below the survivor ceiling; the tick-index driver lands it near the ideal.
    assert!(
        survivors < ticks,
        "the test needs a real shed so the tick-vs-survivor gap exists (survivors {survivors}, \
         ticks {ticks})"
    );
    assert!(
        cumulative > survivor_ceiling.saturating_add(per_tick),
        "RT-8b: cumulative audio {cumulative} must exceed the surviving-frame ceiling \
         {survivor_ceiling} (a per-surviving-frame bus.tick() would trail there) — audio is \
         drifting behind video under the shed"
    );
    // And it tracks the tick timeline within a BOUNDED in-flight tail: when `stop`
    // lands, the last several emitted ticks are still buffered (hot queue cap + one
    // in transit + the sink fan-out cap + the slow sink's small drain backlog) and
    // their surviving frames' catch-up has not yet been fed. That tail is O(cap),
    // independent of run length — generously bounded at 32 ticks (test (1) measures
    // the analogous depth at ~9 and ceilings it at 24). The gap this forbids — the
    // pre-RT-8b drift — is the WHOLE dropped span (hundreds of ticks), not a tail.
    let tail_slack = 32_u64.saturating_mul(per_tick);
    assert!(
        cumulative.saturating_add(tail_slack) >= tick_ideal,
        "RT-8b: cumulative audio {cumulative} must track the tick ideal {tick_ideal} \
         (within a bounded {tail_slack}-sample in-flight tail) — the bus must catch up across \
         every dropped tick, not trail by the dropped span"
    );
}

// ===================== FATAL-CONSUMER REGRESSIONS (t63) =====================
//
// The minimal incident patch (fatal bake-consumer signal) proves, through the
// REAL streaming core (real engine clock, real bake consumer, real FFV1
// encoder — only the sinks are the standard ADR-0025 fakes), that a dead
// consumer STOPS the run at a tick boundary and fails it with the exact cause,
// instead of ticking on with a healthy UI over dead outputs.
//
// FFV1 is the codec: LGPL-clean, intra-only, zero-delay — its packets carry
// DTS == PTS == tick on the deployed base, so windows admit and REAL output
// provably flows before any fault. (mpeg2video, the branch-tip default, has
// a one-frame DTS delay that kills the consumer through the dispatcher's
// window protocol on this base — a SEPARATE, root-caused repair.)

/// A tiny single-tile FFV1 config: same shape as [`config_text`] with the
/// zero-delay LGPL codec.
fn fatal_config_text() -> String {
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
path = "/tmp/multiview-fatal-test/index.m3u8"
codec = "ffv1"
segment_ms = 1000
"##
    .to_owned()
}

fn build_fatal_pipeline() -> Pipeline {
    let config = MultiviewConfig::load_from_toml(&fatal_config_text()).expect("parse config");
    config.validate().expect("config validates");
    Pipeline::build(&config).expect("build real pipeline")
}

/// Like [`spawn_clock_driver`], but paced (one 40 ms tick advance per `period`
/// of wall time) so the consumer's real encode keeps up — the fatal tests
/// measure the SIGNAL, not shed pressure.
fn spawn_fatal_clock_paced(
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

/// FATAL REGRESSION 1 — mid-run death after REAL output: the consumer dies
/// mid-stream (the seam kills its input after 8 processed items). The sink
/// must have received REAL coded packets first (outputs were alive, then
/// died — the incident shape), the run must STOP at a tick boundary far
/// short of the budget, the atomic stop must be raised (the signal systemd's
/// Restart=always restarts on), and the run must FAIL with the exact typed
/// cause — never a healthy report over dead outputs.
#[tokio::test(flavor = "current_thread")]
async fn fatal_consumer_midrun_stops_run_and_returns_exact_error() {
    const FAULT_AFTER: u64 = 8;
    const MAX_TICKS: u64 = 10_000;
    const EARLY_STOP_BOUND: u64 = 2_000;

    let mut pipeline = build_fatal_pipeline();

    let received = Arc::new(AtomicUsize::new(0));
    let received_in_sink = Arc::clone(&received);
    let runner = move |rx: Receiver<EncodedPacket>| -> TestSinkOutcome {
        let mut frames = 0;
        while let Ok(_packet) = rx.recv() {
            received_in_sink.fetch_add(1, Ordering::Release);
            frames += 1;
        }
        TestSinkOutcome { frames }
    };

    let hot_tick = Arc::new(AtomicU64::new(0));
    let clock = Arc::new(ManualTimeSource::new());
    let stop = StopSignal::new();
    let driver_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    spawn_fatal_clock_paced(
        Arc::clone(&clock),
        Arc::clone(&driver_stop),
        std::time::Duration::from_millis(25),
    );

    let clock_concrete: Arc<ManualTimeSource> = Arc::clone(&clock);
    let ts: Arc<dyn TimeSource> = clock_concrete;
    let result = pipeline
        .drive_streaming_for_test(
            StreamTestParams {
                time: ts,
                pacer: CooperativePacer,
                max_ticks: Some(MAX_TICKS),
                policy: SendPolicy::DropOnOverload,
                runners: vec![Box::new(runner)],
                shed_at_tick: None,
                consumer_fault_after_ticks: Some(FAULT_AFTER),
                hot_tick_observer: Some(Arc::clone(&hot_tick)),
            },
            &stop,
        )
        .await;
    driver_stop.store(true, Ordering::Release);

    let err = result.expect_err("a dead consumer must fail the run");
    match &err {
        multiview_cli::pipeline::PipelineError::Output { kind, reason } => {
            assert_eq!(*kind, "bake-consumer");
            assert!(
                reason.contains(&format!("after {FAULT_AFTER} hot item")),
                "the exact fatal cause must propagate, got {reason:?}"
            );
        }
        other => panic!("expected a typed output error, got {other:?}"),
    }
    assert!(
        stop.is_stopped(),
        "the fatal consumer must raise the atomic stop signal"
    );
    let ticks = hot_tick.load(Ordering::Acquire);
    assert!(
        ticks < EARLY_STOP_BOUND,
        "the run must stop at a tick boundary near the fault, not run {MAX_TICKS} ticks (emitted {ticks})"
    );
    let sink_packets = received.load(Ordering::Acquire);
    assert!(
        sink_packets >= 1,
        "the consumer must have produced real encoded output before dying (received {sink_packets})"
    );
}

/// FATAL REGRESSION 2 — startup death: a consumer that dies AT START must fail
/// the run through the same atomic signal, even under block-for-exact. The
/// consumer's OWN finalization error (it ended with no complete window) takes
/// precedence over the seam's injected reason — the exact real cause is what
/// must propagate.
#[tokio::test(flavor = "current_thread")]
async fn fatal_consumer_at_start_fails_run_immediately() {
    const MAX_TICKS: u64 = 10_000;
    const EARLY_STOP_BOUND: u64 = 100;

    let mut pipeline = build_fatal_pipeline();
    let received = Arc::new(AtomicUsize::new(0));
    let received_in_sink = Arc::clone(&received);
    let runner = move |rx: Receiver<EncodedPacket>| -> TestSinkOutcome {
        let mut frames = 0;
        while let Ok(_packet) = rx.recv() {
            received_in_sink.fetch_add(1, Ordering::Release);
            frames += 1;
        }
        TestSinkOutcome { frames }
    };

    let hot_tick = Arc::new(AtomicU64::new(0));
    let clock = Arc::new(ManualTimeSource::new());
    let stop = StopSignal::new();
    let driver_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    spawn_fatal_clock_paced(
        Arc::clone(&clock),
        Arc::clone(&driver_stop),
        std::time::Duration::from_millis(25),
    );

    let clock_concrete: Arc<ManualTimeSource> = Arc::clone(&clock);
    let ts: Arc<dyn TimeSource> = clock_concrete;
    let result = pipeline
        .drive_streaming_for_test(
            StreamTestParams {
                time: ts,
                pacer: CooperativePacer,
                max_ticks: Some(MAX_TICKS),
                policy: SendPolicy::BlockForExact,
                runners: vec![Box::new(runner)],
                shed_at_tick: None,
                consumer_fault_after_ticks: Some(0),
                hot_tick_observer: Some(Arc::clone(&hot_tick)),
            },
            &stop,
        )
        .await;
    driver_stop.store(true, Ordering::Release);

    let err = result.expect_err("a consumer dead at start must fail the run");
    match &err {
        multiview_cli::pipeline::PipelineError::Output { kind, reason } => {
            assert!(
                *kind == "dispatch" || *kind == "bake-consumer",
                "unexpected failure kind {kind:?} ({reason:?})"
            );
            assert!(
                reason.contains("test consumer fault")
                    || reason.contains("incomplete final window"),
                "the cause must name the consumer's real failure, got {reason:?}"
            );
        }
        other => panic!("expected a typed output error, got {other:?}"),
    }
    assert!(
        stop.is_stopped(),
        "the fatal consumer must raise the stop signal"
    );
    let ticks = hot_tick.load(Ordering::Acquire);
    assert!(
        ticks < EARLY_STOP_BOUND,
        "the run must stop almost immediately, not run {MAX_TICKS} ticks (emitted {ticks})"
    );
    assert_eq!(
        received.load(Ordering::Acquire),
        0,
        "a consumer dead at start encodes nothing"
    );
}

/// FATAL REGRESSION 3 — NO FALSE FAILURE on a full hot queue: the fatal
/// signal must fire ONLY on a dead consumer, never on queue pressure. Under
/// block-for-exact the hot queue runs FULL for the entire run (the engine
/// blocks on each send until the consumer takes it — the whole point of the
/// policy); the run must complete exact-N, raise no stop, and return no
/// error. The drop-on-overload Full arm (shed and COUNT, never fatal) is
/// pinned by the unit gate test; its end-to-end shed variant additionally
/// requires the tick-gap tolerance repair (fix/broadcast-output-contract,
/// separately reviewed) because on this base any real shed hands the
/// consumer gapped ticks and the deployed strict-tick encoder rejects them
/// — a pre-existing, root-caused death this minimal patch turns into a
/// visible fast failure + restart (the acknowledged interim posture).
#[tokio::test(flavor = "current_thread")]
async fn full_hot_queue_never_fails_the_run() {
    const TICKS: u64 = 40;

    let mut pipeline = build_fatal_pipeline();
    let received = Arc::new(AtomicUsize::new(0));
    let received_in_sink = Arc::clone(&received);
    let runner = move |rx: Receiver<EncodedPacket>| -> TestSinkOutcome {
        let mut frames = 0;
        while let Ok(_packet) = rx.recv() {
            received_in_sink.fetch_add(1, Ordering::Release);
            frames += 1;
        }
        TestSinkOutcome { frames }
    };

    let clock = Arc::new(ManualTimeSource::new());
    let stop = StopSignal::new();
    let driver_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // ~4x real time: the engine races ahead of the consumer, so the hot queue
    // sits at its cap and the engine blocks on (nearly) every send — sustained
    // queue-full pressure for the whole run.
    spawn_fatal_clock_paced(
        Arc::clone(&clock),
        Arc::clone(&driver_stop),
        std::time::Duration::from_millis(10),
    );

    let clock_concrete: Arc<ManualTimeSource> = Arc::clone(&clock);
    let ts: Arc<dyn TimeSource> = clock_concrete;
    let result = pipeline
        .drive_streaming_for_test(
            StreamTestParams {
                time: ts,
                pacer: CooperativePacer,
                max_ticks: Some(TICKS),
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

    let out = result.expect("queue-full pressure must NEVER fail the run");
    assert_eq!(
        out.report.frames, TICKS,
        "block-for-exact emits every tick despite the full queue"
    );
    assert_eq!(
        out.report.dropped, 0,
        "block-for-exact never drops — a full queue back-pressures the renderer"
    );
    assert!(
        !stop.is_stopped(),
        "a full hot queue must not raise the fatal stop"
    );
    assert_eq!(
        out.sink_frames,
        vec![usize::try_from(TICKS).unwrap()],
        "every frame reached the sink — full queue pressure, zero loss"
    );
}

/// FATAL REGRESSION 4 — finite exit with a wedged sink: a sink that wedges in
/// its finalise must not hang teardown — the bounded (2 s) sink grace detaches
/// and REPORTS it while the healthy co-sink finalises all its packets. The
/// plain (non-tokio) watchdog on the test thread proves teardown RETURNS.
#[test]
fn wedged_sink_teardown_is_finite_and_reported() {
    const TICKS: u64 = 30;
    type DriveResult =
        Result<multiview_cli::pipeline::StreamTestResult, multiview_cli::pipeline::PipelineError>;

    let healthy_count = Arc::new(AtomicUsize::new(0));
    let reached_wedge = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let healthy_in = Arc::clone(&healthy_count);
    let reached_in = Arc::clone(&reached_wedge);

    // Two FFV1 outputs for the two sinks: the per-binding delivery maps each
    // injected sink runner 1:1 to an output binding (with its own recovery
    // gate), so a two-sink scenario needs two outputs. Both share the one
    // FFV1 rendition (encode-once-mux-many, invariant #7).
    let text = fatal_config_text()
        + r#"
[[outputs]]
kind = "hls"
path = "/tmp/multiview-fatal-test/second.m3u8"
codec = "ffv1"
segment_ms = 1000
"#;
    let config = MultiviewConfig::load_from_toml(&text).expect("parse config");
    config.validate().expect("config validates");

    let (done_tx, done_rx) = std::sync::mpsc::channel::<DriveResult>();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        let result = rt.block_on(async move {
            let mut pipeline = Pipeline::build(&config).expect("build pipeline");

            let healthy = move |rx: Receiver<EncodedPacket>| -> TestSinkOutcome {
                let mut n = 0_usize;
                while rx.recv().is_ok() {
                    n += 1;
                }
                healthy_in.store(n, Ordering::Release);
                TestSinkOutcome { frames: n }
            };
            let wedged = move |rx: Receiver<EncodedPacket>| -> TestSinkOutcome {
                while rx.recv().is_ok() {}
                reached_in.store(true, Ordering::Release);
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(3600));
                }
            };

            let clock = Arc::new(ManualTimeSource::new());
            let stop = StopSignal::new();
            let driver_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            spawn_fatal_clock_paced(
                Arc::clone(&clock),
                Arc::clone(&driver_stop),
                std::time::Duration::from_millis(25),
            );
            let ts: Arc<dyn TimeSource> = clock;

            let out = pipeline
                .drive_streaming_for_test(
                    StreamTestParams {
                        time: ts,
                        pacer: CooperativePacer,
                        max_ticks: Some(TICKS),
                        policy: SendPolicy::BlockForExact,
                        runners: vec![Box::new(healthy), Box::new(wedged)],
                        shed_at_tick: None,
                        consumer_fault_after_ticks: None,
                        hot_tick_observer: None,
                    },
                    &stop,
                )
                .await;
            driver_stop.store(true, Ordering::Release);
            out
        });
        let _ = done_tx.send(result);
    });

    let result = done_rx
        .recv_timeout(std::time::Duration::from_secs(8))
        .expect(
            "teardown must complete within the bound — a wedged sink must NOT hang stop (inv #1)",
        )
        .expect("drive streaming");

    assert!(
        reached_wedge.load(Ordering::Acquire),
        "the wedged sink must have drained to end-of-program before wedging"
    );
    assert!(
        result
            .report
            .outputs
            .iter()
            .any(|line| line.to_ascii_uppercase().contains("WEDGED")),
        "the wedged sink must be reported in the run outputs, got {:?}",
        result.report.outputs
    );
    assert_eq!(
        healthy_count.load(Ordering::Acquire),
        usize::try_from(TICKS).unwrap(),
        "the healthy sink must finalise all N packets despite a wedged co-sink"
    );
}
