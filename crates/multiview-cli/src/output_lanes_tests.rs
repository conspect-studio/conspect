#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! Rate lanes end to end (#343, output-routing §12.6): outputs at a frame
//! rate other than Program's through the REAL streaming pipeline — the real
//! Program clock, the real bake consumer, real mpeg2video encoders and the
//! real lane clock thread, all on the run's `ManualTimeSource`.
//!
//! Time is driven by a **Program-gated** helper: it moves the manual clock to
//! Program tick `i`'s deadline only once Program emitted ticks `0..i` and at
//! most one Program period per wall period. So Program never has two ticks
//! due at once (no catch-up repeat on a loaded runner: `frames_repeated` is
//! exactly 0 in a healthy run) and manual time never runs ahead of wall time
//! (a wall-time wedge of a lane covers at most that much output time). Every
//! assertion is a count or a bound the design gives, never a timing
//! percentile.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use multiview_core::time::{MediaTime, Rational};
use multiview_engine::{LanePacing, ManualTimeSource};
use multiview_ffmpeg::{EncodedPacket, StreamKind};

use super::output_lanes::LaneHooks;
use super::{
    Pipeline, ProgramMetricsHandles, SendPolicy, SinkRunOutcome, SinkRunner, StreamTestParams,
    TestSinkOutcome, TestSinkRunner,
};
use crate::binding_delivery::LeasedPacket;

/// Program's cadence in every test here.
const PROGRAM: Rational = Rational::new(30_000, 1_001);
/// The rate outputs' cadence.
const RATE: Rational = Rational::new(25, 1);
/// The lane clock re-reads the manual clock after real sleeps of this.
const STEP: LanePacing = LanePacing::Stepped(Duration::from_millis(2));
/// A widened latch delay for the tests that check which Program frame a lane
/// tick latched: 500 ms (15 Program frames) of margin for a loaded runner's
/// bake consumer, inside the latch ring
/// (`ProgramLatch::capacity_for` sizes it from this delay).
const WIDE_LATCH: Duration = Duration::from_millis(500);

/// What to put in the document.
#[derive(Clone, Copy, Default)]
#[allow(clippy::struct_excessive_bools)] // Independent document switches.
struct Doc {
    /// A second Program-rate output (`out_sib`, same settings as `out_pgm`).
    sibling: bool,
    /// An M/E whose Program is `@_a` (bright) and Preview `@_b` (dark).
    switcher: bool,
    /// An `[audio]` section (program bus) for `enable_program_audio`.
    audio: bool,
    /// A second rate output `out_rate24` at 24/1 (a second output clock).
    second_rate: bool,
}

/// Program at 30000/1001, 64x64; `out_pgm` (and `out_sib`) at Program's
/// rate; `out_rate` with `[outputs.format] fps = "25/1"` at the same size.
/// Output order in the document: Program-rate outputs first, the rate output
/// last (the test runners follow config order on each side).
#[allow(clippy::too_many_lines)] // One TOML document, section by section.
fn document(prefix: &str, doc: Doc) -> String {
    let mut text = String::from(
        r##"
schema_version = 1
[canvas]
width = 64
height = 64
fps = "30000/1001"
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
id = "@_a"
kind = "solid"
color = "#f0f0f0"
[[sources]]
id = "@_b"
kind = "solid"
color = "#202020"
[[cells]]
id = "pgm"
area = "a"
fit = "fill"
[cells.source]
input_id = "@_a"
"##,
    );
    if doc.audio {
        text.push_str(
            r#"
[audio]
sample_rate_hz = 48000
[[audio.routes]]
input_id = "@_a"
channels = { kind = "stereo" }
include_in_program_bus = true
"#,
        );
    }
    text.push_str(
        r#"
[[outputs]]
id = "out_pgm"
kind = "hls"
path = "/tmp/multiview-lanes-e2e/@/pgm/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"#,
    );
    if doc.sibling {
        text.push_str(
            r#"
[[outputs]]
id = "out_sib"
kind = "hls"
path = "/tmp/multiview-lanes-e2e/@/sib/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"#,
        );
    }
    text.push_str(
        r#"
[[outputs]]
id = "out_rate"
kind = "hls"
path = "/tmp/multiview-lanes-e2e/@/rate/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
[outputs.format]
width = 64
height = 64
fps = "25/1"
"#,
    );
    if doc.second_rate {
        text.push_str(
            r#"
[[outputs]]
id = "out_rate24"
kind = "hls"
path = "/tmp/multiview-lanes-e2e/@/rate24/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
[outputs.format]
width = 64
height = 64
fps = "24/1"
"#,
        );
    }
    if doc.switcher {
        text.push_str(
            r#"
[switcher]
[[switcher.mix_effects]]
id = "me1"
program = "main"
program_source = "@_a"
preview_source = "@_b"
"#,
        );
    }
    text.replace('@', prefix)
}

/// One output's packets as its sink saw them.
#[derive(Default)]
struct Rec {
    video: Vec<EncodedPacket>,
    audio: Vec<EncodedPacket>,
}

type Shared = Arc<Mutex<Rec>>;

fn record(rec: &Shared, packet: &EncodedPacket) {
    let copy = packet.clone_shared_ref().unwrap();
    let mut rec = rec.lock().unwrap();
    match packet.kind() {
        StreamKind::Video => rec.video.push(copy),
        StreamKind::Audio => rec.audio.push(copy),
        _ => {}
    }
}

/// A Program-rate output's recorder (through `StreamTestParams.runners`);
/// `finalise` is how long it takes to finalise after its last packet.
fn program_recorder(rec: Shared, finalise: Duration) -> TestSinkRunner {
    Box::new(move |rx: Receiver<EncodedPacket>| {
        let mut frames = 0;
        while let Ok(packet) = rx.recv() {
            frames += 1;
            record(&rec, &packet);
        }
        std::thread::sleep(finalise);
        TestSinkOutcome { frames }
    })
}

/// A rate output's recorder (through `LaneHooks.runners`).
fn rate_recorder(id: &'static str, rec: Shared) -> SinkRunner {
    Box::new(
        move |rx: Receiver<LeasedPacket>, _params, _tb, _audio, _tracks| {
            let mut frames = 0;
            while let Ok(leased) = rx.recv() {
                frames += 1;
                record(&rec, &leased.packet);
            }
            Ok(SinkRunOutcome {
                line: format!("{id}: test rate sink: {frames} packet(s)"),
                playlist: None,
                frames,
            })
        },
    )
}

/// The lane hooks a test sets (the runners are added by [`drive`]).
#[derive(Default)]
struct Hooks {
    on_tick: Option<Arc<dyn Fn(u64) + Send + Sync>>,
    on_encode: Option<Arc<dyn Fn(u64) + Send + Sync>>,
    latch_delay: Option<Duration>,
    /// How long each Program-rate sink takes to finalise.
    program_finalise: Duration,
}

/// What a test's controller thread gets while the run is live.
struct Ctl {
    /// Program ticks emitted so far.
    observer: Arc<AtomicU64>,
    /// The M/E, when the document declares one.
    switcher: Option<multiview_engine::switcher_control::SwitcherControl>,
    /// The Program-rate outputs' recorders (config order).
    program: Vec<Arc<Mutex<Rec>>>,
    /// While set, the manual clock waits for Program at most
    /// [`LOOSE_PATIENCE`] per tick, then advances anyway: a Program that
    /// waited on something else would then fall behind (and not progress).
    loose: Arc<AtomicBool>,
}

/// The gated clock's patience with Program while [`Ctl::loose`] is set.
const LOOSE_PATIENCE: Duration = Duration::from_millis(250);

struct LaneRun {
    /// Program ticks emitted (`report.frames`).
    frames: u64,
    /// The report's output lines (Program's egress, then the lanes').
    lines: Vec<String>,
    /// Program-rate outputs, in config order.
    program: Vec<Rec>,
    /// The rate output.
    rate: Rec,
    /// The 24/1 rate output (empty unless declared).
    rate24: Rec,
    /// Every lane tick's latch: (output tick, latched Program tick).
    latches: Vec<(u64, Option<u64>)>,
    /// The run's metric handles (Program repeats, the lane clock counters).
    metrics: Arc<ProgramMetricsHandles>,
}

/// Drive `ticks` Program ticks of `doc`. `control` runs on its own thread
/// with the Program tick observer (and the switcher, when declared).
#[allow(clippy::too_many_lines)] // One linear harness: build, drive, collect.
fn drive(
    prefix: &str,
    doc: Doc,
    ticks: u64,
    hooks: Hooks,
    control: impl FnOnce(Ctl) + Send + 'static,
) -> LaneRun {
    let config = multiview_config::MultiviewConfig::load_from_toml(&document(prefix, doc)).unwrap();
    let mut pipeline = Pipeline::build(&config).unwrap();
    pipeline.address_boot_feeds_to_test_sinks(&config);
    let program_outputs = if doc.sibling { 2 } else { 1 };
    let program: Vec<Shared> = (0..program_outputs)
        .map(|_| Arc::new(Mutex::new(Rec::default())))
        .collect();
    let rate: Shared = Arc::new(Mutex::new(Rec::default()));
    let rate24: Shared = Arc::new(Mutex::new(Rec::default()));
    let mut lane_runners = vec![rate_recorder("out_rate", Arc::clone(&rate))];
    if doc.second_rate {
        lane_runners.push(rate_recorder("out_rate24", Arc::clone(&rate24)));
    }
    let latches = Arc::new(Mutex::new(Vec::new()));
    let observe = Arc::clone(&latches);
    pipeline.set_lane_hooks(LaneHooks {
        pacing: Some(STEP),
        on_tick: hooks.on_tick,
        on_encode: hooks.on_encode,
        on_latch: Some(Arc::new(move |tick, latched| {
            observe.lock().unwrap().push((tick, latched));
        })),
        latch_delay: hooks.latch_delay,
        runners: lane_runners,
    });
    if doc.audio {
        pipeline.enable_program_audio();
    }
    let switcher = doc.switcher.then(|| {
        crate::switcher_runtime::attach_first_slice(&mut pipeline, None)
            .unwrap()
            .expect("the document declares an M/E")
            .control
    });
    let runners: Vec<TestSinkRunner> = program
        .iter()
        .map(|rec| program_recorder(Arc::clone(rec), hooks.program_finalise))
        .collect();
    let clock = Arc::new(ManualTimeSource::new());
    let observer = Arc::new(AtomicU64::new(0));
    let stop_clock = Arc::new(AtomicBool::new(false));
    let loose = Arc::new(AtomicBool::new(false));
    let metrics_out = Arc::new(Mutex::new(None::<Arc<ProgramMetricsHandles>>));
    let pacer = {
        let clock = Arc::clone(&clock);
        let observer = Arc::clone(&observer);
        let stop = Arc::clone(&stop_clock);
        let loose = Arc::clone(&loose);
        let anchor = pipeline.epoch_anchor_slot();
        let metrics_slot = pipeline.program_metrics();
        let metrics_out = Arc::clone(&metrics_out);
        std::thread::spawn(move || {
            gated_clock(&clock, &observer, &stop, &loose, ticks, || {
                let seed = anchor.load()?.seed_nanos();
                *metrics_out.lock().unwrap() = metrics_slot.load();
                Some(seed)
            });
        })
    };
    let controller = {
        let ctl = Ctl {
            observer: Arc::clone(&observer),
            switcher,
            program: program.iter().map(Arc::clone).collect(),
            loose,
        };
        std::thread::spawn(move || control(ctl))
    };
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
                hot_tick_observer: Some(observer),
                consumer_fault_after_ticks: None,
                shed_at_tick: None,
            },
            &multiview_engine::StopSignal::new(),
        ))
        .unwrap();
    stop_clock.store(true, Ordering::Release);
    pacer.join().expect("clock thread");
    controller.join().expect("controller thread");
    let take = |rec: &Shared| std::mem::take(&mut *rec.lock().unwrap());
    let metrics = metrics_out
        .lock()
        .unwrap()
        .clone()
        .expect("the run published its metric handles");
    let latches = std::mem::take(&mut *latches.lock().unwrap());
    LaneRun {
        frames: result.report.frames,
        lines: result.report.outputs,
        program: program.iter().map(take).collect(),
        rate: take(&rate),
        rate24: take(&rate24),
        latches,
        metrics,
    }
}

/// The Program-gated manual clock (module docs). `seed` answers Program's
/// seed once the run published it.
fn gated_clock(
    clock: &ManualTimeSource,
    observer: &AtomicU64,
    stop: &AtomicBool,
    loose: &AtomicBool,
    ticks: u64,
    mut seed: impl FnMut() -> Option<i64>,
) {
    let seed = loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        if let Some(seed) = seed() {
            break seed;
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    let period = Duration::from_nanos(u64::try_from(PROGRAM.period_nanos()).unwrap());
    let mut last = Instant::now();
    let mut i: u64 = 1;
    while !stop.load(Ordering::Acquire) {
        let due = last + period;
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        }
        // Program tick `i` becomes due only once ticks 0..i were emitted:
        // never two Program ticks due at once. While `loose`, only for
        // LOOSE_PATIENCE: time then advances whether Program follows or not.
        let waited = Instant::now();
        while i < ticks && observer.load(Ordering::Acquire) < i {
            if stop.load(Ordering::Acquire) {
                return;
            }
            if loose.load(Ordering::Acquire) && waited.elapsed() >= LOOSE_PATIENCE {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        last = Instant::now();
        // +1 µs: past the deadline whatever its nanosecond rounding.
        clock.set(seed + pts_ns(i, PROGRAM) + 1_000);
        i += 1;
    }
}

fn pts_ns(tick: u64, cadence: Rational) -> i64 {
    MediaTime::from_tick(i64::try_from(tick).unwrap(), cadence).as_nanos()
}

/// The rate output's video tick count the design gives for `ticks` Program
/// ticks: every output tick whose PTS is not after the last Program frame's
/// (`RunEnd`: a later lane tick offers nothing).
fn expected_rate_frames(ticks: u64) -> u64 {
    let last = pts_ns(ticks - 1, PROGRAM);
    u64::try_from(
        (0..u64::MAX)
            .take_while(|&k| pts_ns(k, RATE) <= last)
            .count(),
    )
    .unwrap()
}

fn video_pts(packets: &[EncodedPacket]) -> Vec<i64> {
    packets.iter().map(|p| p.pts().unwrap()).collect()
}

fn consecutive(n: u64) -> Vec<i64> {
    (0..i64::try_from(n).unwrap()).collect()
}

/// Rate output video: PTS strictly consecutive from 0 (its own 1/25 time
/// base); the count within the design's bound of the elapsed Program time:
/// exactly [`expected_rate_frames`], less at most one when the run-end drain
/// (`LANE_DRAIN_GRACE`, 2 s wall) or one dropped last offer cut the tail.
fn assert_rate_video(run: &LaneRun, ticks: u64) {
    let pts = video_pts(&run.rate.video);
    let n = u64::try_from(pts.len()).unwrap();
    assert_eq!(pts, consecutive(n), "rate output: PTS consecutive from 0");
    let expected = expected_rate_frames(ticks);
    assert!(
        n <= expected && n + 1 >= expected,
        "rate output: {n} frames, design gives {expected} (-1) for {ticks} Program ticks"
    );
    for p in &run.rate.video {
        assert_eq!(p.dts(), p.pts(), "rate output: DTS = PTS = the output tick");
    }
}

/// Program outputs: one frame per tick, PTS 0..N; Program never repeated.
fn assert_program(run: &LaneRun, ticks: u64) {
    assert_eq!(run.frames, ticks, "Program emitted every tick");
    for (i, rec) in run.program.iter().enumerate() {
        assert_eq!(
            video_pts(&rec.video),
            consecutive(ticks),
            "Program-rate output {i}: one frame per tick, PTS consecutive"
        );
    }
    assert_eq!(
        run.metrics.frames_repeated.load(Ordering::Acquire),
        0,
        "Program never repeated a frame (gated clock: one tick due at a time)"
    );
}

/// The latched Program frame is never after the output tick:
/// `pts_prog(latched) <= pts_out(k)`.
fn assert_latch_not_after(run: &LaneRun) {
    assert!(!run.latches.is_empty(), "the lane ticked");
    for &(k, latched) in &run.latches {
        if let Some(p) = latched {
            assert!(
                pts_ns(p, PROGRAM) <= pts_ns(k, RATE),
                "output tick {k} latched Program tick {p}: after the output tick"
            );
        }
    }
}

fn no_lane_failure(lines: &[String]) {
    assert!(
        lines
            .iter()
            .all(|l| !l.contains("FAILED") && !l.contains("WEDGED") && !l.contains("panicked")),
        "no failure line in the report: {lines:#?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("output clock 25/1 (out_rate)")),
        "the lane clock reported: {lines:#?}"
    );
}

/// Decode mpeg2video packets (no reordering): mean luma per packet PTS.
fn mean_luma(packets: &[EncodedPacket]) -> std::collections::BTreeMap<i64, u32> {
    let codec = ffmpeg_next::decoder::find(ffmpeg_next::codec::Id::MPEG2VIDEO).unwrap();
    let mut decoder = ffmpeg_next::codec::context::Context::new_with_codec(codec)
        .decoder()
        .video()
        .unwrap();
    let mut out = std::collections::BTreeMap::new();
    let mut pending = std::collections::VecDeque::new();
    let mut frame = ffmpeg_next::frame::Video::empty();
    let mut drain = |decoder: &mut ffmpeg_next::decoder::Video,
                     pending: &mut std::collections::VecDeque<i64>,
                     out: &mut std::collections::BTreeMap<i64, u32>| {
        while decoder.receive_frame(&mut frame).is_ok() {
            let (w, h) = (
                usize::try_from(frame.width()).unwrap(),
                usize::try_from(frame.height()).unwrap(),
            );
            let stride = frame.stride(0);
            let data = frame.data(0);
            let mut sum: u64 = 0;
            for row in 0..h {
                sum += data[row * stride..row * stride + w]
                    .iter()
                    .map(|&v| u64::from(v))
                    .sum::<u64>();
            }
            if let Some(pts) = pending.pop_front() {
                out.insert(
                    pts,
                    u32::try_from(sum / u64::try_from(w * h).unwrap()).unwrap(),
                );
            }
        }
    };
    for p in packets {
        pending.push_back(p.pts().unwrap());
        decoder.send_packet(&p.to_owned_packet()).unwrap();
        drain(&mut decoder, &mut pending, &mut out);
    }
    decoder.send_eof().unwrap();
    drain(&mut decoder, &mut pending, &mut out);
    out
}

/// Tests owed 1: Program 30000/1001 with one HLS output at Program's rate and
/// one at 25/1. Program's output: one frame per tick, PTS 0..N; the rate
/// output: PTS consecutive from 0 at 1/25, as many frames as the design
/// gives for the elapsed Program time; no lane failure.
#[test]
fn a_25_fps_output_beside_a_29_97_program_keeps_both_cadences() {
    const TICKS: u64 = 240;
    let run = drive("lanes_cad", Doc::default(), TICKS, Hooks::default(), |_| {});
    assert_program(&run, TICKS);
    assert_rate_video(&run, TICKS);
    assert_latch_not_after(&run);
    no_lane_failure(&run.lines);
    let clock = &run.metrics.output_clocks;
    assert_eq!(clock.len(), 1, "one output clock for the one distinct rate");
    assert_eq!(clock[0].cadence, RATE);
    assert_eq!(clock[0].outputs, vec!["out_rate".to_owned()]);
    assert_eq!(
        clock[0].stats.skipped_ticks(),
        0,
        "a healthy lane skips nothing"
    );
}

/// Tests owed 2: a switcher CUT at a known Program tick `c` (read from the
/// decoded Program output) lands on exactly one rate-output frame: the first
/// output tick `k` with `pts_out(k) >= pts_prog(c)` shows the new picture,
/// every earlier one the old picture, every later one the new. The latch
/// observer agrees (each tick latched the nearest Program frame not after
/// it). Widened latch delay: the bake consumer of a loaded runner has 500 ms.
#[test]
#[allow(clippy::too_many_lines)] // One scenario: cut, Program frames, rate frames, latches.
fn a_program_cut_lands_on_exactly_one_rate_output_frame() {
    const TICKS: u64 = 240;
    const CUT_AFTER: u64 = 90;
    let cut_tick = Arc::new(Mutex::new(None::<u64>));
    let record = Arc::clone(&cut_tick);
    let doc = Doc {
        switcher: true,
        ..Doc::default()
    };
    let hooks = Hooks {
        latch_delay: Some(WIDE_LATCH),
        ..Hooks::default()
    };
    let run = drive("lanes_cut", doc, TICKS, hooks, move |ctl| {
        let Ctl {
            observer, switcher, ..
        } = ctl;
        let control = switcher.expect("an M/E");
        let start = Instant::now();
        while !(control.is_ready() && observer.load(Ordering::Acquire) >= CUT_AFTER) {
            assert!(start.elapsed() < Duration::from_secs(20), "switcher ready");
            std::thread::sleep(Duration::from_millis(5));
        }
        // A CUT refused for a not-yet-primed Preview is re-issued.
        for attempt in 0..5 {
            let mut outcomes = control.outcomes();
            let op = format!("cut-{attempt}");
            control
                .try_send(multiview_engine::switcher_control::SwitcherCommand {
                    operation_id: op.clone(),
                    scene_version: crate::switcher_runtime::FIRST_SLICE_SCENE_VERSION,
                    verb: multiview_engine::switcher_control::SwitcherVerb::Cut,
                })
                .unwrap();
            let start = Instant::now();
            let outcome = loop {
                if let Ok(e) = outcomes.try_recv() {
                    if e.event.operation_id == op {
                        break e.event;
                    }
                }
                assert!(start.elapsed() < Duration::from_secs(5), "{op} outcome");
                std::thread::sleep(Duration::from_millis(2));
            };
            if outcome.completed {
                *record.lock().unwrap() = Some(outcome.tick);
                return;
            }
        }
        panic!("the CUT never completed");
    });
    let applied = cut_tick.lock().unwrap().expect("the CUT completed");
    assert_program(&run, TICKS);
    assert_rate_video(&run, TICKS);
    assert_latch_not_after(&run);
    no_lane_failure(&run.lines);

    // Bright (#f0f0f0) before the CUT, dark (#202020) after it.
    let bright = |l: u32| l > 128;
    let program = mean_luma(&run.program[0].video);
    assert_eq!(
        program.len(),
        usize::try_from(TICKS).unwrap(),
        "every Program frame decodes"
    );
    let c = program
        .iter()
        .find(|(_, &l)| !bright(l))
        .map(|(&pts, _)| u64::try_from(pts).unwrap())
        .expect("the CUT reached Program");
    assert!(
        c >= CUT_AFTER && c.abs_diff(applied) <= 1,
        "Program cut at {c} (switcher outcome at tick {applied})"
    );
    for (&pts, &l) in &program {
        assert_eq!(
            bright(l),
            u64::try_from(pts).unwrap() < c,
            "Program frame {pts}: luma {l} (cut at {c})"
        );
    }

    let rate = mean_luma(&run.rate.video);
    assert_eq!(rate.len(), run.rate.video.len(), "every rate frame decodes");
    let first_new = (0..u64::MAX)
        .find(|&k| pts_ns(k, RATE) >= pts_ns(c, PROGRAM))
        .unwrap();
    let latched: std::collections::BTreeMap<u64, Option<u64>> =
        run.latches.iter().copied().collect();
    for k in [first_new - 1, first_new] {
        let p = latched[&k].expect("a Program frame was latched around the cut");
        assert_eq!(
            p >= c,
            k == first_new,
            "output tick {k} latched Program tick {p} (cut at {c})"
        );
    }
    let mut new_frames = 0;
    for (&pts, &l) in &rate {
        let k = u64::try_from(pts).unwrap();
        // A tick before Program's first published frame shows black.
        if latched.get(&k).copied().flatten().is_none() {
            assert!(k < first_new, "only the run's start can latch nothing");
            continue;
        }
        let new = k >= first_new;
        assert_eq!(
            !bright(l),
            new,
            "rate frame {k}: luma {l} (first new frame {first_new}, cut at Program {c})"
        );
        new_frames += u64::from(new);
    }
    assert!(new_frames > 0, "the cut reached the rate output");
    // Exactly one frame changes picture.
    let changes = rate
        .values()
        .zip(rate.values().skip(1))
        .zip(rate.keys().skip(1))
        .filter(|((a, b), &k)| bright(**a) != bright(**b) && u64::try_from(k).unwrap() > 0)
        .filter(|(_, &k)| {
            latched
                .get(&u64::try_from(k).unwrap())
                .copied()
                .flatten()
                .is_some()
        })
        .count();
    assert_eq!(
        changes, 1,
        "the picture changes on exactly one output frame"
    );
}

/// Tests owed 3: the rate output carries the Program AAC packets themselves
/// (same PTS on the continuous 1024 grid, same bytes) over the common prefix;
/// per output frame the shown Program picture is at most one Program frame
/// behind the frame's PTS (the latch, whole frames, nearest not after), so
/// the A/V offset is within one Program frame plus one output tick (§12.6).
#[test]
fn a_rate_output_carries_the_program_audio_packets_with_a_bounded_av_offset() {
    const TICKS: u64 = 240;
    let doc = Doc {
        audio: true,
        ..Doc::default()
    };
    let hooks = Hooks {
        latch_delay: Some(WIDE_LATCH),
        ..Hooks::default()
    };
    let run = drive("lanes_av", doc, TICKS, hooks, |_| {});
    assert_program(&run, TICKS);
    assert_rate_video(&run, TICKS);
    no_lane_failure(&run.lines);

    let program: Vec<i64> = run.program[0]
        .audio
        .iter()
        .map(|p| p.pts().unwrap())
        .collect();
    let rate: Vec<i64> = run.rate.audio.iter().map(|p| p.pts().unwrap()).collect();
    // About 8 s at 48 kHz / 1024: well over 300 frames on Program's output.
    assert!(
        program.len() > 300,
        "Program audio: {} packets",
        program.len()
    );
    assert!(
        program.windows(2).all(|w| w[1] - w[0] == 1024),
        "Program audio on the continuous 1024 grid"
    );
    let common = program.len().min(rate.len());
    // The lane ends at the last Program frame, Program's egress at its last
    // window: the common prefix is all but at most a lane tail.
    assert!(
        common * 10 >= program.len() * 9,
        "rate output audio: {} of {} Program packets",
        rate.len(),
        program.len()
    );
    assert_eq!(
        rate[..common],
        program[..common],
        "the same audio PTS sequence on both outputs"
    );
    assert!(
        rate.windows(2).all(|w| w[1] - w[0] == 1024),
        "rate output audio continuous on the 1024 grid"
    );
    for (a, b) in run
        .rate
        .audio
        .iter()
        .zip(&run.program[0].audio)
        .take(common)
    {
        assert_eq!(a.payload(), b.payload(), "the very Program AAC packets");
    }

    // A/V: video at output PTS k shows Program frame p with
    // pts_prog(p) <= pts_out(k) < pts_prog(p) + one Program frame (+ one
    // output tick, the §12.6 bound) — on the shared seed-based timeline the
    // audio PTS are on (samples since the seed).
    let program_period = PROGRAM.period_nanos();
    let output_period = RATE.period_nanos();
    let mut checked: u64 = 0;
    for &(k, latched) in &run.latches {
        let Some(p) = latched else { continue };
        let offset = pts_ns(k, RATE) - pts_ns(p, PROGRAM);
        assert!(
            (0..program_period + output_period).contains(&offset),
            "output tick {k} shows Program tick {p}: A/V offset {offset} ns"
        );
        // Nearest not after: the next Program frame would be after the tick.
        assert!(
            offset < program_period,
            "output tick {k} latched Program tick {p}, not the newest not after it"
        );
        checked += 1;
    }
    assert!(
        checked + 3 >= expected_rate_frames(TICKS),
        "latches observed"
    );
}

/// Chaos (R3, invariants #1/#10): the lane clock wedges (blocking, in its
/// tick body) mid-run and is held wedged by a handshake until Program has
/// visibly progressed: while it is held, the test clock no longer waits on
/// Program beyond [`LOOSE_PATIENCE`] (#343 review I3), and Program plus its
/// Program-rate sibling must emit at least 20 new frames within a generous
/// wall bound — a Program that waited on the lane would stall here and fail.
/// After the release: Program keeps exactly one frame per tick with
/// consecutive PTS and never repeats; the sibling is untouched; the lane
/// skips the ticks it missed (catch-up bound 8 ticks) and its worker
/// gap-fills them, so the rate output's PTS stay consecutive. 20 Program
/// frames are ~0.7 s of output time (~17 lane ticks): past the catch-up bound
/// and inside the 2 s gap a lane fills.
#[test]
#[allow(clippy::too_many_lines)] // One scenario: the handshake, then the counts.
fn a_wedged_lane_clock_never_moves_program_or_its_sibling() {
    const TICKS: u64 = 300;
    const WEDGE_AT: u64 = 60;
    const PROGRESS: u64 = 20;
    const BOUND: Duration = Duration::from_secs(15);
    let wedged = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&wedged);
    let (wedged_tx, wedged_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let hook_io = Mutex::new((wedged_tx, release_rx));
    let progress = Arc::new(Mutex::new(None::<(u64, usize)>));
    let record = Arc::clone(&progress);
    let doc = Doc {
        sibling: true,
        ..Doc::default()
    };
    let hooks = Hooks {
        on_tick: Some(Arc::new(move |tick| {
            if tick >= WEDGE_AT && !flag.swap(true, Ordering::AcqRel) {
                let io = hook_io.lock().unwrap();
                let _ = io.0.send(());
                // Held until the test observed Program progress (bounded).
                let _ = io.1.recv_timeout(Duration::from_secs(30));
            }
        })),
        ..Hooks::default()
    };
    let run = drive("lanes_wedge", doc, TICKS, hooks, move |ctl| {
        wedged_rx
            .recv_timeout(BOUND)
            .expect("the lane clock reached the wedge");
        ctl.loose.store(true, Ordering::Release);
        let program_at = ctl.observer.load(Ordering::Acquire);
        let sibling_at = ctl.program[1].lock().unwrap().video.len();
        let start = Instant::now();
        let progressed = loop {
            let program = ctl.observer.load(Ordering::Acquire) - program_at;
            let sibling = ctl.program[1].lock().unwrap().video.len() - sibling_at;
            if (program >= PROGRESS && u64::try_from(sibling).unwrap() >= PROGRESS)
                || start.elapsed() >= BOUND
            {
                break (program, sibling);
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        *record.lock().unwrap() = Some(progressed);
        ctl.loose.store(false, Ordering::Release);
        let _ = release_tx.send(());
    });
    let (program_progress, sibling_progress) = progress
        .lock()
        .unwrap()
        .expect("the controller observed the wedge");
    assert!(
        program_progress >= PROGRESS,
        "Program emitted {program_progress} frames while the lane clock was wedged"
    );
    assert!(
        u64::try_from(sibling_progress).unwrap() >= PROGRESS,
        "the sibling got {sibling_progress} frames while the lane clock was wedged"
    );
    assert!(wedged.load(Ordering::Acquire), "the lane clock wedged");
    assert_program(&run, TICKS);
    assert_eq!(run.program.len(), 2);
    assert_eq!(
        video_pts(&run.program[0].video),
        video_pts(&run.program[1].video),
        "the sibling carries the same Program windows"
    );
    assert_rate_video(&run, TICKS);
    // The tick that ran out of the wedge is later than the latch ring: its
    // Program frame was dropped, so it latched the oldest retained frame
    // (design: "else the oldest retained", never blocking) and counted it as
    // a late latch. Every latch after a Program frame newer than its tick is
    // such an expired, counted latch.
    let after = run
        .latches
        .iter()
        .filter(|&&(k, p)| p.is_some_and(|p| pts_ns(p, PROGRAM) > pts_ns(k, RATE)))
        .count();
    let after = u64::try_from(after).unwrap();
    let late = run.metrics.output_clocks[0]
        .late_latches
        .load(Ordering::Acquire);
    assert!(
        after <= late && after <= 2,
        "{after} latches after their tick, {late} late latches counted"
    );
    no_lane_failure(&run.lines);
    let stats = &run.metrics.output_clocks[0].stats;
    assert!(
        stats.skipped_ticks() > 0,
        "the wedged lane skipped to the tick due (skipped {})",
        stats.skipped_ticks()
    );
    // Skipped ticks were never latched: the latch observer saw fewer ticks
    // than the rate output carries, and the worker filled the gap.
    let latched = u64::try_from(run.latches.len()).unwrap();
    let carried = u64::try_from(run.rate.video.len()).unwrap();
    assert!(
        latched < carried,
        "gap-filled: {carried} frames from {latched} lane ticks"
    );
}

/// #343 review M1: the lanes learn the last Program frame from the bake
/// consumer, not from Program's sink joins. A Program sink that takes 1.5 s
/// to finalise (far beyond the 100 ms latch delay; the test clock keeps
/// advancing meanwhile) must not let the rate output carry frames past the
/// last Program frame: the design's upper count bound still holds.
#[test]
fn a_slow_program_sink_finalisation_adds_no_rate_frame_past_program_end() {
    const TICKS: u64 = 150;
    let hooks = Hooks {
        program_finalise: Duration::from_millis(1_500),
        ..Hooks::default()
    };
    let run = drive("lanes_end", Doc::default(), TICKS, hooks, |_| {});
    assert_program(&run, TICKS);
    assert_rate_video(&run, TICKS);
    assert_latch_not_after(&run);
    no_lane_failure(&run.lines);
    let last = pts_ns(TICKS - 1, PROGRAM);
    assert!(
        run.latches.iter().all(|&(k, _)| pts_ns(k, RATE) <= last),
        "no lane tick past the last Program frame was latched"
    );
}

/// #343 review I1: one lane clock wedges for good (its outputs cannot end)
/// beside a healthy lane at another rate. The healthy lane is stopped on its
/// own once it passed the last Program frame and finalises normally (no
/// failure line, every frame up to the end); the wedged lane is reported and
/// detached; the whole run end stays inside one shared shutdown budget (3 s)
/// instead of a drain grace plus a grace per clock, consumer and sink.
#[test]
fn a_wedged_lane_never_holds_a_healthy_lane_or_the_run_end() {
    const TICKS: u64 = 150;
    const WEDGE_AT: u64 = 30;
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    let wedged = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&wedged);
    let doc = Doc {
        second_rate: true,
        ..Doc::default()
    };
    let hooks = Hooks {
        on_tick: Some(Arc::new(move |tick| {
            let lane = std::thread::current().name().map(str::to_owned);
            if tick >= WEDGE_AT && lane.as_deref() == Some("clock-out-24") {
                flag.store(true, Ordering::Release);
                // Wedged until the test is over (bounded for the test binary).
                let _ = release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(60));
            }
        })),
        ..Hooks::default()
    };
    let ended = Arc::new(Mutex::new(None::<Instant>));
    let mark = Arc::clone(&ended);
    let run = drive("lanes_shut", doc, TICKS, hooks, move |ctl| {
        let start = Instant::now();
        while ctl.observer.load(Ordering::Acquire) < TICKS {
            assert!(start.elapsed() < Duration::from_secs(40), "Program ends");
            std::thread::sleep(Duration::from_millis(1));
        }
        *mark.lock().unwrap() = Some(Instant::now());
    });
    let run_end = ended.lock().unwrap().expect("Program ended").elapsed();
    drop(release_tx);
    assert!(wedged.load(Ordering::Acquire), "the 24 fps lane wedged");
    assert_program(&run, TICKS);
    assert_rate_video(&run, TICKS);
    let lines = |id: &str| -> Vec<&String> {
        run.lines
            .iter()
            .filter(|l| l.starts_with(&format!("{id}:")))
            .collect()
    };
    assert!(
        lines("out_rate")
            .iter()
            .all(|l| !l.contains("FAILED") && !l.contains("WEDGED")),
        "the healthy lane finalised normally: {:#?}",
        run.lines
    );
    assert!(
        lines("out_rate")
            .iter()
            .any(|l| l.contains("test rate sink")),
        "the healthy lane's sink completed: {:#?}",
        run.lines
    );
    assert!(
        lines("out_rate24").iter().any(|l| l.contains("WEDGED")),
        "the wedged lane is reported: {:#?}",
        run.lines
    );
    assert!(
        run.lines
            .iter()
            .any(|l| l.starts_with("output clock 24/1") && l.contains("detached")),
        "the wedged clock is detached: {:#?}",
        run.lines
    );
    let wedged_frames = run.rate24.video.len();
    assert!(
        (1..=usize::try_from(WEDGE_AT).unwrap()).contains(&wedged_frames),
        "the wedged lane carried {wedged_frames} frames"
    );
    // The lanes' budget is 3 s; the rest of the run end (Program's egress,
    // the ingest teardown) is sub-second. Without one shared budget this was
    // at least 2 s drain + 2 s clock + 2 s consumer + 2 s sink.
    assert!(
        run_end < Duration::from_secs(6),
        "run end took {run_end:?} after Program's last tick"
    );
}

/// Chaos: the lane encoder panics at output tick 30. Only that output ends
/// (its report line says FAILED); Program and its Program-rate sibling
/// complete fully, one frame per tick.
#[test]
fn a_panicking_lane_encoder_ends_only_its_output() {
    const TICKS: u64 = 180;
    const PANIC_AT: u64 = 30;
    let doc = Doc {
        sibling: true,
        ..Doc::default()
    };
    let hooks = Hooks {
        on_encode: Some(Arc::new(|tick| {
            assert!(tick < PANIC_AT, "chaos: the lane encoder panics");
        })),
        ..Hooks::default()
    };
    let run = drive("lanes_panic", doc, TICKS, hooks, |_| {});
    assert_program(&run, TICKS);
    assert_eq!(
        video_pts(&run.program[1].video),
        consecutive(TICKS),
        "the sibling completes"
    );
    assert!(
        run.lines
            .iter()
            .any(|l| l.starts_with("out_rate: rate output FAILED")),
        "the rate output's line says FAILED: {:#?}",
        run.lines
    );
    let pts = video_pts(&run.rate.video);
    let n = u64::try_from(pts.len()).unwrap();
    assert_eq!(
        pts,
        consecutive(n),
        "what the rate output carried is consecutive"
    );
    assert!(n <= PANIC_AT, "nothing after the panic: {n} frames");
    assert!(
        run.metrics.output_clocks[0]
            .dropped_offers
            .load(Ordering::Acquire)
            > 0,
        "the lane clock kept ticking; the dead worker refused its offers"
    );
}
