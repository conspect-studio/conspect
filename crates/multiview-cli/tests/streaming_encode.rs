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

/// (1) Live + a blocked sink: the engine still emits every tick, the channel
/// occupancy never exceeds the cap, and the overload is shed and counted (a hard,
/// encoder-independent floor of dropped frames, not merely `dropped > 0`).
///
/// **Determinism across encoder speeds (ENG-1c).** The drop is forced by the
/// *engine* outpacing a *frozen* consumer, NOT by the real encoder being fast.
/// The earlier shape used a fixed tick budget (`max_ticks = 400`) and hoped the
/// real `mpeg2video` encoder emitted enough packets *within that budget* to fill
/// the sink channel (cap `SINK_QUEUE_CAP`), wedge the bake consumer, back up the
/// hot queue (cap `LIVE_QUEUE_CAP`), and make the hot loop `try_send` see `Full`.
/// On a slower encoder (e.g. ffmpeg 5.1) the no-sleep engine reached tick 400 and
/// the run *ended* before the consumer had wedged long enough to back up the hot
/// queue — so `dropped == 0` and the assertion failed, even though the
/// drop-oldest policy is correct. The bound here was on real encode latency, not
/// on the policy under test.
///
/// This version removes that race. It drives the engine **unbounded**
/// (`max_ticks = None`) and lets a watcher stop it only once the consumer is
/// provably wedged AND the engine has driven a generous margin of *additional*
/// ticks past that point. The sink freezes after its first packet, so the bake
/// consumer wedges on the full sink channel and cannot pull another hot item;
/// from then on the engine (manual clock, no real sleep) produces ticks at memory
/// speed while the consumer is frozen, so *every* tick after the hot queue fills
/// is dropped. The encoder speed only changes *how soon* that first packet lands
/// — not whether drops then accumulate — so `dropped` is large and deterministic
/// regardless of ffmpeg version. The never-stall / bounded-memory assertions are
/// unchanged and strengthened (we now also pin the exact post-saturation drop
/// floor, not just `> 0`).
#[tokio::test(flavor = "current_thread")]
async fn live_blocked_sink_stays_bounded_and_never_stalls() {
    // Ticks the engine must drive *past* the moment the consumer wedges before we
    // stop it. Once wedged, the consumer cannot pull another hot item, so the hot
    // queue (cap `LIVE_QUEUE_CAP`) fills within a few ticks and every subsequent
    // tick is shed. This margin therefore lower-bounds (minus the in-flight buffer
    // depth below) how many frames *must* be dropped before we stop — an encoder-
    // independent count.
    const POST_WEDGE_TICKS: u64 = 200;
    // The number of ticks that can still *succeed* in the window after the watcher
    // first observes the wedge (`received >= 1`) before the pipeline saturates and
    // drops begin. It bounds the total buffering between the hot-loop sender and
    // the frozen sink: the hot queue (`LIVE_QUEUE_CAP` = 4) + one frame in transit
    // at the consumer's `recv` + one being baked/encoded + the sink fan-out channel
    // (`SINK_QUEUE_CAP` = 4) + the one packet the sink already pulled, plus a couple
    // of ticks of scheduling slack between the watcher's `received`-read and its
    // `hot_tick` base-read. Measured at 9 here (ffmpeg 7.1); 24 is a generous
    // ceiling that holds across encoders/schedulers while still pinning a hard
    // floor of `POST_WEDGE_TICKS - 24` = 176 dropped — far stronger than `> 0`.
    const IN_FLIGHT_SLACK: u64 = 24;
    // Generous bound on how long the engine takes to drive `POST_WEDGE_TICKS`
    // memory-speed ticks once wedged; far above the microseconds it actually
    // needs, so the watcher never gives up early on a slow CI box.
    const WATCHDOG: std::time::Duration = std::time::Duration::from_secs(30);

    let mut pipeline = build_pipeline();

    // A fake sink that pulls exactly one packet and then freezes forever: a wedged
    // muxer/encoder that cannot keep up. It must never be able to stall the engine
    // — under the drop-on-overload policy the hot loop sheds frames instead.
    let received = Arc::new(AtomicUsize::new(0));
    let received_in_sink = Arc::clone(&received);
    let blocked_runner = move |rx: Receiver<EncodedPacket>| -> TestSinkOutcome {
        // Pull exactly one packet (records that the fan-out reached the sink), then
        // stop draining entirely. The sink channel fills behind us, wedging the
        // bake consumer, which can no longer pull from the hot queue.
        if rx.recv().is_ok() {
            received_in_sink.fetch_add(1, Ordering::Release);
        }
        // Now freeze HARD: never drain another packet. The sink channel
        // (cap `SINK_QUEUE_CAP`) fills behind us and stays full, so the bake
        // consumer wedges on its bounded fan-out send and can no longer pull from
        // the hot queue — which then fills and forces the hot loop to shed every
        // subsequent tick. We hold `rx` (so the channel is not Disconnected) and
        // park; teardown detaches this thread after the bounded `SINK_WEDGE_GRACE`
        // (exactly the bounded-teardown path test (5) exercises), so it never hangs
        // `stop`. Under DropOnOverload the hot loop sheds, never blocks, so the
        // output clock keeps ticking despite this permanent wedge.
        let _hold = rx;
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    };

    let clock = Arc::new(ManualTimeSource::new());
    let stop = StopSignal::new();
    let driver_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    spawn_clock_driver(Arc::clone(&clock), Arc::clone(&driver_stop));

    // The hot-loop tick observer: incremented once per emitted tick on the hot
    // loop (before the send), so the watcher can count engine ticks and prove the
    // engine drove well past the consumer's wedge — independent of encoder speed.
    let hot_tick = Arc::new(AtomicU64::new(0));

    // The watcher decides when the engine has provably outpaced the frozen
    // consumer, then raises `stop`. It does NOT touch the hot path — it only
    // *observes* (the sink's first-packet flag + the hot-tick gauge) and signals
    // stop, exactly like a controller would. This is what makes the drop count
    // deterministic across encoder speeds: the engine keeps ticking (and shedding)
    // until we say stop, rather than racing a fixed budget against the encoder.
    let watch_received = Arc::clone(&received);
    let watch_hot_tick = Arc::clone(&hot_tick);
    let watch_stop = stop.clone();
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + WATCHDOG;
        // 1) Wait for the consumer to wedge: the sink got its first packet, so the
        //    sink channel is now backing up behind the frozen sink.
        while watch_received.load(Ordering::Acquire) == 0 {
            if std::time::Instant::now() >= deadline {
                watch_stop.stop();
                return;
            }
            std::thread::yield_now();
        }
        // 2) From the tick the wedge was first observable, drive POST_WEDGE_TICKS
        //    further ticks. Every tick past the point the hot queue fills is shed,
        //    so this lower-bounds the drop count regardless of the encoder.
        let base = watch_hot_tick.load(Ordering::Acquire);
        let target = base.saturating_add(POST_WEDGE_TICKS);
        while watch_hot_tick.load(Ordering::Acquire) < target {
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::yield_now();
        }
        watch_stop.stop();
    });

    let clock_concrete: Arc<ManualTimeSource> = Arc::clone(&clock);
    let ts: Arc<dyn TimeSource> = clock_concrete;
    let result = pipeline
        .drive_streaming_for_test(
            StreamTestParams {
                time: ts,
                pacer: CooperativePacer,
                // Unbounded: the engine runs until the watcher stops it, so drops
                // are forced by saturation, not by a budget racing the encoder.
                max_ticks: None,
                policy: SendPolicy::DropOnOverload,
                runners: vec![Box::new(blocked_runner)],
                hot_tick_observer: Some(Arc::clone(&hot_tick)),
            },
            &stop,
        )
        .await
        .expect("drive streaming");
    driver_stop.store(true, Ordering::Release);

    // Invariant #1: the engine kept emitting ticks throughout despite the wedged
    // sink — it drove past the wedge by at least the post-wedge margin.
    assert!(
        result.report.frames >= POST_WEDGE_TICKS,
        "the output clock must keep ticking past the wedge (inv #1): emitted {} ticks, \
         expected at least the post-wedge margin {POST_WEDGE_TICKS}",
        result.report.frames
    );
    // Invariant #10 / bounded memory: occupancy is O(cap) — bounded by the fixed
    // cap, INDEPENDENT of the run length (it stays tiny next to the hundreds of
    // ticks driven, never growing with the number of frames; that is the OOM the
    // ADR fixes).
    //
    // The bound is cap+1, not cap, and that ceiling is exact and race-free — NOT
    // a fudge. `peak_occupancy` is a gauge: the single engine sender does
    // `in_flight.fetch_add(1)` AFTER a successful send, and the single bake
    // consumer does `in_flight.fetch_sub(1)` AFTER `recv()`. The channel itself
    // (`sync_channel(cap)`) never buffers more than cap by construction. The gauge
    // exceeds the true buffered count by at most one, and only at the full
    // boundary: the consumer's `recv()` frees a slot, the sender refills it and
    // increments before the consumer's matching `fetch_sub` lands — exactly one
    // in-transit frame double-counted. One sender + one consumer ⇒ at most one
    // such pending decrement ⇒ the gauge's high-watermark is at most cap+1.
    assert!(
        result.peak_occupancy <= result.capacity + 1,
        "peak occupancy {} must stay within cap+1 = {} — O(cap) bounded memory, \
         not O(ticks); the +1 is the single-consumer in-transit transient (inv #10)",
        result.peak_occupancy,
        result.capacity + 1
    );
    // Overload was shed and COUNTED (visible, not hidden), so the run faltered.
    // Deterministic floor: once the consumer wedges, every tick after the whole
    // pipeline buffer (hot queue + in-transit + sink channel, bounded by
    // `IN_FLIGHT_SLACK`) fills is shed. The watcher drove POST_WEDGE_TICKS ticks
    // past the wedge, so at least `POST_WEDGE_TICKS - IN_FLIGHT_SLACK` of them MUST
    // have been dropped — a hard, encoder-independent lower bound, not just `> 0`.
    let min_drops = POST_WEDGE_TICKS.saturating_sub(IN_FLIGHT_SLACK);
    assert!(
        result.report.dropped >= min_drops,
        "a wedged sink under live policy must shed frames and count them: got {} dropped, \
         expected at least {min_drops} (POST_WEDGE_TICKS {POST_WEDGE_TICKS} minus the \
         in-flight buffer depth {IN_FLIGHT_SLACK} that can still drain after the wedge)",
        result.report.dropped
    );
    assert!(
        result.report.faltered,
        "dropped > 0 means the live output faltered (honest reporting)"
    );
    assert!(
        received.load(Ordering::Acquire) >= 1,
        "the sink should have received at least one coded packet before wedging"
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
                runners: vec![Box::new(runner)],
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
    spawn_clock_driver(Arc::clone(&clock), Arc::clone(&driver_stop));

    // Raise the stop signal shortly after launching so the unbounded `run_until`
    // path stops promptly (it would otherwise run to MAX).
    let stop_for_thread = stop.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(50));
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
            let mut pipeline = build_pipeline();

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
            spawn_clock_driver(Arc::clone(&clock), Arc::clone(&driver_stop));
            let ts: Arc<dyn TimeSource> = clock;

            let out = pipeline
                .drive_streaming_for_test(
                    StreamTestParams {
                        time: ts,
                        pacer: CooperativePacer,
                        max_ticks: Some(TICKS),
                        policy: SendPolicy::BlockForExact,
                        runners: vec![Box::new(healthy), Box::new(wedged)],
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

/// (6) RT-8b held-out lip-sync-under-overload, END TO END through the real cli
/// bake consumer (not the extracted helper): with program audio enabled and the
/// engine driven UNBOUNDED while a slow-but-alive sink paces the bake consumer
/// below the engine, the hot queue overflows and `DropOnOverload` sheds VIDEO
/// frames. The decisive property — audio must NOT drift behind video under that
/// shed — is observed at the muxer boundary: the cumulative AUDIO samples the bake
/// consumer fed the encoder must track the **tick count** (every emitted output
/// tick), NOT the count of surviving video frames.
///
/// The audio packet `pts` is a pure sample counter (`audio_pts = Σ samples`), so
/// the last audio packet's `pts + its frame size` is the cumulative samples fed.
/// At 48 kHz / 25 fps that is 1920 samples per OUTPUT TICK. The pre-RT-8b driver
/// called `bus.tick()` once per SURVIVING frame, so under a heavy shed its
/// cumulative samples would be `survivors * 1920` — far short of `ticks * 1920` —
/// i.e. audio trailing video by the dropped ticks' worth of samples. This test
/// fails on that regression: it asserts the cumulative audio is close to the tick
/// ideal and strictly far above the survivor-count ceiling.
#[tokio::test(flavor = "current_thread")]
async fn rt8b_audio_stays_lip_synced_to_the_tick_index_under_video_drops() {
    // 48 kHz / 25 fps = exactly 1920 audio samples per output tick.
    const SAMPLES_PER_TICK: u64 = 1_920;
    // Drive a generous margin of ticks past the first observed drop so the shed is
    // substantial and the tick-vs-survivor gap is unmistakable.
    const POST_DROP_TICKS: u64 = 400;
    const WATCHDOG: std::time::Duration = std::time::Duration::from_secs(30);

    let mut pipeline = build_pipeline();
    // Opt into program audio: the bake consumer now drives the bus per tick index
    // and fans AAC packets to the SAME sink as video.
    pipeline.enable_program_audio();

    // The sink is SLOW (sleeps per packet) but never wedges: it keeps draining, so
    // the bake consumer keeps producing audio while running BELOW the engine — the
    // hot queue overflows and the engine sheds video frames. The sink tracks the
    // cumulative audio samples (the max `pts + frame_size` over audio packets) and
    // a flag once any drop is observable (it has drained a packet).
    let max_audio_end = Arc::new(AtomicU64::new(0));
    let audio_packets = Arc::new(AtomicU64::new(0));
    let drained_one = Arc::new(AtomicUsize::new(0));
    let max_audio_end_in = Arc::clone(&max_audio_end);
    let audio_packets_in = Arc::clone(&audio_packets);
    let drained_one_in = Arc::clone(&drained_one);
    let slow_runner = move |rx: Receiver<EncodedPacket>| -> TestSinkOutcome {
        let mut frames = 0_usize;
        while let Ok(packet) = rx.recv() {
            // Pace the consumer below the (no-sleep, memory-speed) engine so the
            // hot queue saturates and the engine sheds — but never wedge.
            std::thread::sleep(std::time::Duration::from_millis(1));
            if packet.kind() == multiview_ffmpeg::StreamKind::Audio {
                // `pts` is the cumulative sample position at the START of this
                // packet's frame; `len()` is bytes, not samples, so track the max
                // observed start `pts` and add a single AAC frame (1024) for the
                // tail. The exact tail size is immaterial — the tick-vs-survivor gap
                // is hundreds of thousands of samples.
                if let Some(pts) = packet.pts() {
                    let pts = u64::try_from(pts).unwrap_or(0);
                    let prev = max_audio_end_in.load(Ordering::Acquire);
                    if pts > prev {
                        max_audio_end_in.store(pts, Ordering::Release);
                    }
                }
                audio_packets_in.fetch_add(1, Ordering::Release);
            } else {
                frames += 1;
                drained_one_in.store(1, Ordering::Release);
            }
        }
        TestSinkOutcome { frames }
    };

    let clock = Arc::new(ManualTimeSource::new());
    let stop = StopSignal::new();
    let driver_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    spawn_clock_driver(Arc::clone(&clock), Arc::clone(&driver_stop));

    let hot_tick = Arc::new(AtomicU64::new(0));

    // Watcher: once the sink has drained at least one packet (the consumer is
    // running but paced below the engine), drive POST_DROP_TICKS further engine
    // ticks — every tick past the hot-queue fill is shed — then stop.
    let watch_drained = Arc::clone(&drained_one);
    let watch_hot_tick = Arc::clone(&hot_tick);
    let watch_stop = stop.clone();
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + WATCHDOG;
        while watch_drained.load(Ordering::Acquire) == 0 {
            if std::time::Instant::now() >= deadline {
                watch_stop.stop();
                return;
            }
            std::thread::yield_now();
        }
        let base = watch_hot_tick.load(Ordering::Acquire);
        let target = base.saturating_add(POST_DROP_TICKS);
        while watch_hot_tick.load(Ordering::Acquire) < target {
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::yield_now();
        }
        watch_stop.stop();
    });

    let clock_concrete: Arc<ManualTimeSource> = Arc::clone(&clock);
    let ts: Arc<dyn TimeSource> = clock_concrete;
    let result = pipeline
        .drive_streaming_for_test(
            StreamTestParams {
                time: ts,
                pacer: CooperativePacer,
                max_ticks: None,
                policy: SendPolicy::DropOnOverload,
                runners: vec![Box::new(slow_runner)],
                hot_tick_observer: Some(Arc::clone(&hot_tick)),
            },
            &stop,
        )
        .await
        .expect("drive streaming");
    driver_stop.store(true, Ordering::Release);

    // Preconditions: the shed actually happened, and audio flowed.
    assert!(
        result.report.dropped > 0,
        "the slow sink must have forced the engine to shed video frames (got {} dropped)",
        result.report.dropped
    );
    assert!(
        audio_packets.load(Ordering::Acquire) > 0,
        "program audio must have reached the sink"
    );

    assert_audio_tracks_tick_index(
        result.report.frames,
        result.report.dropped,
        max_audio_end.load(Ordering::Acquire),
        SAMPLES_PER_TICK,
    );
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
