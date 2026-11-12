#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! End-to-end output routing through the real streaming pipeline: the real
//! output clock (`ManualTimeSource` + `CooperativePacer`), the real bake
//! consumer, real mpeg2video encoders, the real `routing-aux` thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{OutputRoutingSlot, OutputTake, RoutingSnapshot, TakeOutcome, TakeResult};
use crate::pipeline::{Pipeline, SendPolicy, StreamTestParams, TestSinkOutcome, TestSinkRunner};
use crate::routing_aux::PictureBus;

/// Two sources (both bound so both run), two HLS outputs with identical
/// settings: ONE Program encoder key serves both. `@` is replaced by a
/// per-test prefix: synthetic-source lifecycles are fenced per source id
/// process-wide, so concurrent tests must not share ids.
const CONFIG: &str = r##"
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
columns = ["1fr", "1fr"]
rows = ["1fr"]
areas = ["a b"]
[[sources]]
id = "@_a"
kind = "test"
[[sources]]
id = "@_b"
kind = "test"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "@_a"
[[cells]]
id = "cell_b"
area = "b"
[cells.source]
input_id = "@_b"
[[outputs]]
id = "out0"
kind = "hls"
path = "/tmp/multiview-routing-e2e/@/a/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
[[outputs]]
id = "out1"
kind = "hls"
path = "/tmp/multiview-routing-e2e/@/b/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;

/// One video packet as a sink saw it: (pts, keyframe, MPEG-2 sequence header).
type Seen = Arc<Mutex<Vec<(i64, bool, bool)>>>;

fn recorder(seen: Seen) -> TestSinkRunner {
    Box::new(move |rx| {
        let mut frames = 0;
        while let Ok(packet) = rx.recv() {
            frames += 1;
            if packet.kind() == multiview_ffmpeg::StreamKind::Video {
                let header = packet
                    .payload()
                    .is_some_and(|p| p.windows(4).any(|w| w == [0, 0, 1, 0xB3]));
                seen.lock()
                    .unwrap()
                    .push((packet.pts().unwrap(), packet.is_keyframe(), header));
            }
        }
        TestSinkOutcome { frames }
    })
}

struct Run {
    frames: u64,
    outputs: Vec<Vec<(i64, bool, bool)>>,
    rendition_configs: usize,
    final_snapshot: Arc<RoutingSnapshot>,
}

/// Drive `ticks` real pipeline ticks; `control` runs on its own thread with
/// the run's routing slot while the clock runs.
fn drive<F>(prefix: &str, ticks: u64, control: F) -> Run
where
    F: FnOnce(OutputRoutingSlot) + Send + 'static,
{
    let config =
        multiview_config::MultiviewConfig::load_from_toml(&CONFIG.replace('@', prefix)).unwrap();
    let mut pipeline = Pipeline::build(&config).unwrap();
    let rendition_configs = pipeline.rendition_config_count();
    let slot = pipeline.output_routing();
    let seen: Vec<Seen> = (0..2).map(|_| Arc::new(Mutex::new(Vec::new()))).collect();
    let runners: Vec<TestSinkRunner> = seen.iter().map(|s| recorder(Arc::clone(s))).collect();
    let clock = Arc::new(multiview_engine::ManualTimeSource::new());
    let stop_clock = Arc::new(AtomicBool::new(false));
    {
        let (clock, stop_clock) = (Arc::clone(&clock), Arc::clone(&stop_clock));
        std::thread::spawn(move || {
            while !stop_clock.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(40));
                clock.advance(Duration::from_millis(40));
            }
        });
    }
    let control_slot = Arc::clone(&slot);
    let controller = std::thread::spawn(move || control(control_slot));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = runtime
        .block_on(pipeline.drive_streaming_for_test(
            StreamTestParams {
                time: clock,
                pacer: multiview_engine::CooperativePacer,
                max_ticks: Some(ticks),
                policy: SendPolicy::BlockForExact,
                runners,
                hot_tick_observer: None,
                consumer_fault_after_ticks: None,
                shed_at_tick: None,
            },
            &multiview_engine::StopSignal::new(),
        ))
        .unwrap();
    stop_clock.store(true, Ordering::Release);
    controller.join().expect("controller thread");
    let final_snapshot = slot
        .load()
        .expect("the run published its handle")
        .snapshot();
    Run {
        frames: result.report.frames,
        outputs: seen.iter().map(|s| s.lock().unwrap().clone()).collect(),
        rendition_configs,
        final_snapshot,
    }
}

fn wait_for<T>(what: &str, bound: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(start.elapsed() < bound, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn pts_of(packets: &[(i64, bool, bool)]) -> Vec<i64> {
    packets.iter().map(|p| p.0).collect()
}

fn take(op: &str, output: usize, bus: PictureBus) -> OutputTake {
    OutputTake {
        operation_id: op.to_owned(),
        output_index: output,
        bus,
        principal: "operator".to_owned(),
    }
}

/// What the controller observed, per take.
#[derive(Default)]
struct Observed {
    to_source: Option<(TakeOutcome, usize)>,
    to_program: Option<(TakeOutcome, usize)>,
}

/// B3 test 1 end to end: a take of output 1 to `src:<second source>` opens ONE encoder
/// and applies; output 0 (same settings) keeps every Program window; output 1's
/// first packet after the move is a keyframe carrying a sequence header and
/// the PTS step stays one tick; a take back to Program applies with +0
/// encoders. Bounds: each attempt resolves within 5 s wall; 400 ticks.
#[test]
fn a_take_to_a_source_and_back_through_the_real_pipeline() {
    const TICKS: u64 = 400;
    let observed = Arc::new(Mutex::new(Observed::default()));
    let record = Arc::clone(&observed);
    let run = drive("route_take", TICKS, move |slot| {
        let handle = wait_for("the routing handle", Duration::from_secs(10), || {
            slot.load()
        });
        let outcomes = handle.outcomes().expect("one outcome reader");
        // Let the run settle (the consumer published, the sources go live).
        wait_for("the first routing tick", Duration::from_secs(10), || {
            (handle.snapshot().video_keys > 0).then_some(())
        });
        std::thread::sleep(Duration::from_millis(800));
        // A take applies, or is refused FeedNotFresh when the (wall-clock)
        // synthetic producer was starved by a loaded host — the correct
        // refusal; the take is then re-issued (at most 5 attempts).
        let applied = |make: &dyn Fn() -> OutputTake| {
            for _ in 0..5 {
                let op = make().operation_id;
                handle.try_take(make()).unwrap();
                let o = wait_for(&format!("{op} outcome"), Duration::from_secs(5), || {
                    outcomes.try_recv().ok()
                });
                assert_eq!(o.operation_id, op, "unexpected outcome {o:?}");
                match o.outcome {
                    TakeResult::Applied => return o,
                    TakeResult::Refused(super::RefuseReason::FeedNotFresh) => {}
                    other => panic!("{op}: {other:?}"),
                }
            }
            panic!("never applied");
        };
        let o = applied(&|| {
            take(
                "to-source",
                1,
                PictureBus::Source("route_take_b".to_owned()),
            )
        });
        let keys = wait_for("snapshot", Duration::from_secs(1), || {
            let s = handle.snapshot();
            (s.outputs[1].bus == "src:route_take_b").then_some(s.video_keys)
        });
        record.lock().unwrap().to_source = Some((o, keys));
        std::thread::sleep(Duration::from_millis(600));
        let o = applied(&|| take("to-program", 1, PictureBus::Program));
        let keys = wait_for("snapshot", Duration::from_secs(1), || {
            let s = handle.snapshot();
            (s.outputs[1].bus == "program").then_some(s.video_keys)
        });
        record.lock().unwrap().to_program = Some((o, keys));
    });
    let observed = observed.lock().unwrap();
    let (to_source, keys_after_source) = observed.to_source.clone().expect("take applied");
    let (to_program, keys_after_program) = observed.to_program.clone().expect("back applied");
    assert_eq!(run.rendition_configs, 1, "identical settings share one key");
    assert_eq!(keys_after_source, 2, "+1 encoder for the source bus");
    assert_eq!(keys_after_program, 2, "+0 encoders back to Program");
    assert_eq!(run.frames, TICKS, "every output tick emitted");

    let all: Vec<i64> = (0..i64::try_from(TICKS).unwrap()).collect();
    assert_eq!(
        pts_of(&run.outputs[0]),
        all,
        "output 0 gets every Program window"
    );
    assert_eq!(
        pts_of(&run.outputs[1]),
        all,
        "output 1: one packet per tick, PTS step one tick across both moves"
    );
    for (name, at) in [
        ("to source", to_source.tick),
        ("to program", to_program.tick),
    ] {
        let first = run.outputs[1]
            .iter()
            .find(|p| p.0 == i64::try_from(at).unwrap())
            .unwrap();
        assert!(
            first.1 && first.2,
            "{name}: first packet at tick {at} is a keyframe with a sequence header: {first:?}"
        );
    }
    assert!(to_source.tick < to_program.tick);
    assert_eq!(
        run.final_snapshot.parameter_set_changes, 0,
        "mpeg2video: the live-opened encoder's sequence header equals the running one's (plain splice both ways)"
    );
    assert_eq!(
        run.final_snapshot.dynamic_encoders_live, 0,
        "closed at the end of the run"
    );
}

/// B3 test 8 (A/B): a run with no take is today's topology exactly. Base
/// logic: one encoder per distinct rendition config and one window per output
/// per tick. With routing wired: `video_keys == rendition_configs.len()`, the
/// same contiguous windows on every output, and the clock's aux sender never
/// sent a tick (inactive; 0 sends, 0 drops).
#[test]
fn a_run_without_takes_keeps_the_base_topology_and_never_wakes_aux() {
    const TICKS: u64 = 40;
    let run = drive("route_ab", TICKS, |_slot| {});
    let all: Vec<i64> = (0..i64::try_from(TICKS).unwrap()).collect();
    assert_eq!(run.frames, TICKS);
    assert_eq!(run.final_snapshot.video_keys, run.rendition_configs);
    assert_eq!(run.final_snapshot.video_keys, 1);
    for (i, output) in run.outputs.iter().enumerate() {
        assert_eq!(pts_of(output), all, "output {i} windows");
    }
    assert_eq!(run.final_snapshot.aux_ticks_sent, 0, "aux sender inactive");
    assert_eq!(run.final_snapshot.dropped_aux_ticks, 0);
    assert_eq!(run.final_snapshot.dynamic_encoders_live, 0);
    assert!(run
        .final_snapshot
        .outputs
        .iter()
        .all(|o| o.bus == "program"));
}
