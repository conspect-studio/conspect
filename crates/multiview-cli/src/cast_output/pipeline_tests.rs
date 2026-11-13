//! A cast output through the real streaming pipeline: the real output clock
//! (`ManualTimeSource` + `CooperativePacer`), the real bake consumer, a real
//! libx264 encoder and AAC program audio, the real cast sink (gate, ring,
//! packager, directory). The HLS output next to it is a recording fake.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use multiview_engine::StopSignal;
use multiview_output::hls::cast_check::{check_cast_dir, CastCheckReport};

use super::{CastOutputEntry, CastOutputsSlot, CAST_DIR_MARKER};
use crate::output_routing::{OutputRoutingHandle, OutputRoutingSlot};
use crate::pipeline::{Pipeline, SendPolicy, StreamTestParams, TestSinkOutcome, TestSinkRunner};

/// Output ticks per second of the test canvas.
const FPS: u64 = 25;

/// Two bars sources (both bound, so both run and carry a tone), one H.264
/// HLS output and one cast output. `@` is a per-test prefix (synthetic
/// sources are fenced per id process-wide); `%` is the test's directory.
const CONFIG: &str = r##"
schema_version = 1
[canvas]
width = 160
height = 90
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
kind = "bars"
[[sources]]
id = "@_b"
kind = "bars"
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
id = "hls0"
kind = "hls"
path = "%/hls/index.m3u8"
codec = "h264"
[[outputs]]
kind = "cast"
id = "cast-1"
dir = "%/cast"
"##;

/// Output time of `ticks` clock ticks, in seconds.
fn secs(ticks: u64) -> f64 {
    f64::from(u32::try_from(ticks).unwrap()) / f64::from(u32::try_from(FPS).unwrap())
}

/// What the controller thread of a run can reach.
struct Ctl {
    cast: CastOutputEntry,
    routing: Arc<OutputRoutingHandle>,
    /// Output ticks emitted so far (incremented on the hot loop).
    ticks: Arc<AtomicU64>,
    stop: StopSignal,
}

impl Ctl {
    fn dir(&self) -> &Path {
        &self.cast.dir
    }

    fn tick(&self) -> u64 {
        self.ticks.load(Ordering::Acquire)
    }

    /// Wait until the output clock has emitted `tick` ticks.
    fn wait_tick(&self, tick: u64) {
        wait_for(&format!("tick {tick}"), Duration::from_secs(120), || {
            (self.tick() >= tick).then_some(())
        });
    }

    /// Wait `seconds` of output time from now.
    fn run_seconds(&self, seconds: u64) {
        self.wait_tick(self.tick() + seconds * FPS);
    }

    /// Wait until the gate reports `ready`.
    fn wait_ready(&self, bound: Duration) {
        wait_for("the cast stream to be ready", bound, || {
            self.cast.gate.status().ready.then_some(())
        });
    }
}

fn wait_for<T>(what: &str, bound: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(start.elapsed() < bound, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// What one run produced.
struct Run {
    /// Output ticks emitted.
    frames: u64,
    /// Video PTS the HLS sink received, in order.
    hls_video_pts: Vec<i64>,
    /// Audio packets the HLS sink received.
    hls_audio_packets: usize,
    /// Packets the cast sink received.
    cast_packets: usize,
    /// Distinct encoder configurations the pipeline built.
    rendition_configs: usize,
    /// The run report's per-output lines.
    lines: Vec<String>,
    /// The cast directory (inside `_root`).
    cast_dir: PathBuf,
    _root: tempfile::TempDir,
}

/// How a run ends.
#[derive(Clone, Copy)]
enum Until {
    /// After exactly this many ticks.
    Ticks(u64),
    /// When the controller raises the stop signal (at most 6000 ticks).
    Stop,
}

/// Drive the real pipeline with program audio at real time (the synthetic
/// sources and the encoder's admitted lateness are wall-clock bound).
/// `control` runs on its own thread once the run has published its cast
/// output and routing handle.
fn drive<F>(prefix: &str, until: Until, control: F) -> Run
where
    F: FnOnce(&Ctl) + Send + 'static,
{
    drive_in(prefix, until, false, |_| {}, control)
}

/// As [`drive`]. With `default_dir` the cast output has no configured `dir`
/// and the pipeline is told its state directory (`<root>/.multiview`).
/// `prepare` sees the run's root directory before the pipeline is built.
#[allow(clippy::too_many_lines)] // One linear set-up of the real pipeline, its clock and its controller.
fn drive_in<F>(
    prefix: &str,
    until: Until,
    default_dir: bool,
    prepare: impl FnOnce(&Path),
    control: F,
) -> Run
where
    F: FnOnce(&Ctl) + Send + 'static,
{
    let root = tempfile::tempdir().unwrap();
    prepare(root.path());
    let template = if default_dir {
        CONFIG.replace("dir = \"%/cast\"\n", "")
    } else {
        CONFIG.to_owned()
    };
    let toml = template
        .replace('@', prefix)
        .replace('%', root.path().to_str().unwrap());
    let config = multiview_config::MultiviewConfig::load_from_toml(&toml).unwrap();
    config.validate().unwrap();
    let mut pipeline = Pipeline::build(&config).unwrap();
    let cast_dir = if default_dir {
        let state_dir = root.path().join(".multiview");
        pipeline.set_state_dir(&state_dir);
        state_dir.join("cast").join("cast-1")
    } else {
        root.path().join("cast")
    };
    pipeline.enable_program_audio();
    let rendition_configs = pipeline.rendition_config_count();
    let cast_slot: CastOutputsSlot = pipeline.cast_outputs();
    let routing_slot: OutputRoutingSlot = pipeline.output_routing();
    let video_pts = Arc::new(Mutex::new(Vec::new()));
    let audio_packets = Arc::new(AtomicU64::new(0));
    let hls: TestSinkRunner = {
        let (video_pts, audio_packets) = (Arc::clone(&video_pts), Arc::clone(&audio_packets));
        Box::new(move |rx| {
            let mut frames = 0;
            while let Ok(packet) = rx.recv() {
                frames += 1;
                match packet.kind() {
                    multiview_ffmpeg::StreamKind::Video => {
                        video_pts.lock().unwrap().push(packet.pts().unwrap());
                    }
                    _ => {
                        audio_packets.fetch_add(1, Ordering::AcqRel);
                    }
                }
            }
            TestSinkOutcome { frames }
        })
    };
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
    let ticks = Arc::new(AtomicU64::new(0));
    let stop = StopSignal::new();
    let controller = {
        let (ticks, stop) = (Arc::clone(&ticks), stop.clone());
        let (cast_slot, routing_slot) = (Arc::clone(&cast_slot), Arc::clone(&routing_slot));
        std::thread::spawn(move || {
            let cast = wait_for("the cast output", Duration::from_secs(30), || {
                cast_slot.load().first().cloned()
            });
            let routing = wait_for("the routing handle", Duration::from_secs(30), || {
                routing_slot.load()
            });
            let ctl = Ctl {
                cast,
                routing,
                ticks,
                stop,
            };
            control(&ctl);
        })
    };
    let max_ticks = match until {
        Until::Ticks(ticks) => ticks,
        Until::Stop => 6000,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = runtime
        .block_on(pipeline.drive_streaming_with_cast_for_test(
            StreamTestParams {
                time: clock,
                pacer: multiview_engine::CooperativePacer,
                max_ticks: Some(max_ticks),
                policy: SendPolicy::BlockForExact,
                runners: vec![hls],
                hot_tick_observer: Some(Arc::clone(&ticks)),
                consumer_fault_after_ticks: None,
                shed_at_tick: None,
            },
            &stop,
        ))
        .unwrap();
    stop_clock.store(true, Ordering::Release);
    controller.join().expect("controller thread");
    assert!(
        cast_slot.load().is_empty(),
        "the cast outputs are withdrawn when the run ends"
    );
    let hls_video_pts = std::mem::take(&mut *video_pts.lock().unwrap());
    Run {
        frames: result.report.frames,
        hls_video_pts,
        hls_audio_packets: usize::try_from(audio_packets.load(Ordering::Acquire)).unwrap(),
        cast_packets: result.sink_frames[1],
        rendition_configs,
        lines: result.report.outputs,
        cast_dir,
        _root: root,
    }
}

/// The stream file names in `dir`, sorted (the ownership marker excluded).
fn files(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name != CAST_DIR_MARKER)
        .collect();
    names.sort();
    names
}

/// Validate the directory between two publishes of the running packager: a
/// report is accepted only when no segment was published while (or just
/// after) it was read.
fn validated(ctl: &Ctl) -> CastCheckReport {
    for _ in 0..40 {
        let before = ctl.cast.gate.status().video_segments;
        let report = check_cast_dir(ctl.dir());
        std::thread::sleep(Duration::from_millis(30));
        if ctl.cast.gate.status().video_segments == before {
            assert_eq!(
                report.errors,
                Vec::<String>::new(),
                "files: {:?}",
                files(ctl.dir())
            );
            return report;
        }
    }
    panic!("the cast directory never stood still between two publishes");
}

/// The generation in the first segment name `video.m3u8` lists.
fn listed_generation(dir: &Path) -> u64 {
    let text = std::fs::read_to_string(dir.join("video.m3u8")).unwrap();
    let line = text.lines().find(|l| l.starts_with("seg-v-")).unwrap();
    line.split('-').nth(2).unwrap().parse().unwrap()
}

/// The NAL unit type of the first slice in the first listed video segment
/// (5 = IDR), read from the file itself.
fn first_slice_type(dir: &Path) -> u8 {
    let text = std::fs::read_to_string(dir.join("video.m3u8")).unwrap();
    let name = text.lines().find(|l| l.starts_with("seg-v-")).unwrap();
    let bytes = std::fs::read(dir.join(name)).unwrap();
    let mdat = multiview_output::hls::cmaf::find_box(&bytes, &[b"mdat"]).expect("mdat");
    let mut at = 0;
    while at + 5 <= mdat.len() {
        let len =
            usize::try_from(u32::from_be_bytes(mdat[at..at + 4].try_into().unwrap())).unwrap();
        let nal_type = mdat[at + 4] & 0x1F;
        if nal_type == 1 || nal_type == 5 {
            return nal_type;
        }
        at += 4 + len;
    }
    panic!("no slice in {name}");
}

/// (a) No cast stream: the cast output writes nothing at all, receives the
/// very packets the HLS output receives, and costs no encoder.
#[test]
fn a_inactive_cast_output_writes_no_file_and_shares_the_hls_encoder() {
    const TICKS: u64 = 200;
    let keys = Arc::new(AtomicU64::new(0));
    let seen = Arc::clone(&keys);
    let run = drive("cast_a", Until::Ticks(TICKS), move |ctl| {
        ctl.wait_tick(100);
        let snapshot = ctl.routing.snapshot();
        seen.store(
            u64::try_from(snapshot.video_keys).unwrap(),
            Ordering::Release,
        );
        let status = ctl.cast.gate.status();
        assert!(!status.active && !status.ready && status.last_error.is_none());
        assert_eq!((ctl.cast.width, ctl.cast.height), (160, 90));
        assert_eq!(ctl.cast.codec, "libx264");
    });
    assert_eq!(run.frames, TICKS);
    assert!(!run.cast_dir.exists(), "{:?}", files(&run.cast_dir));
    // One encoder configuration and one video key for both outputs.
    assert_eq!(run.rendition_configs, 1);
    assert_eq!(keys.load(Ordering::Acquire), 1);
    assert_eq!(
        run.hls_video_pts,
        (0..i64::try_from(TICKS).unwrap()).collect::<Vec<_>>()
    );
    assert!(run.hls_audio_packets > 0, "the run carries program audio");
    // The cast sink was handed exactly what the HLS sink was handed.
    assert_eq!(
        run.cast_packets,
        run.hls_video_pts.len() + run.hls_audio_packets,
        "{:?}",
        run.lines
    );
}

/// (b0) After 22 s of run a pre-roll activation is ready at once with a
/// full window; without pre-roll the stream starts after the activation.
#[test]
fn b0_preroll_publishes_a_full_window_at_once_and_no_preroll_starts_after_the_activation() {
    let run = drive("cast_b0", Until::Stop, |ctl| {
        ctl.wait_tick(22 * FPS + 10);
        assert!(!ctl.dir().exists(), "nothing on disk before the activation");
        let at = ctl.tick();
        let generation = ctl.cast.gate.activate(true).unwrap();
        // The pre-roll is published in one burst. The bound is the design's,
        // not this machine's speed: the clock emits at most 25 ticks per
        // wall second, so inside this 5 s deadline at most 3 new segments
        // (2 s each) can have been produced. 9 listed segments therefore
        // prove that at least 6 came from the buffered 20 s; without the
        // pre-roll 9 segments need 18 s of output.
        wait_for("the pre-roll window", Duration::from_secs(5), || {
            let status = ctl.cast.gate.status();
            (status.ready && status.video_segments >= 9).then_some(())
        });
        let report = validated(ctl);
        assert!(
            (9..=10).contains(&report.video_segments),
            "{} segments",
            report.video_segments
        );
        assert_eq!(report.audio_segments, report.video_segments);
        assert_eq!(report.discontinuities, 0);
        assert_eq!(listed_generation(ctl.dir()), generation);
        // It reaches back about 20 s before the activation.
        let behind = secs(at) - report.first_video_start;
        assert!((17.0..=21.5).contains(&behind), "{behind} s");

        // Deactivate, then activate without pre-roll.
        ctl.cast.gate.deactivate();
        wait_for("an empty directory", Duration::from_secs(5), || {
            files(ctl.dir()).is_empty().then_some(())
        });
        ctl.run_seconds(1);
        let at = ctl.tick();
        let second = ctl.cast.gate.activate(false).unwrap();
        assert!(second > generation);
        ctl.wait_ready(Duration::from_secs(30));
        let report = validated(ctl);
        let activated_at = secs(at);
        // Nothing of the buffered seconds is used: the stream starts at the
        // first keyframe the sink receives after the activation. The sink
        // runs a few ticks behind the clock (encode + delivery), so that
        // keyframe may carry a time up to that latency before the tick
        // count read above.
        assert!(
            (activated_at - 0.5..activated_at + 1.5).contains(&report.first_video_start),
            "first segment at {} s, activated at {activated_at} s",
            report.first_video_start
        );
        assert_eq!(listed_generation(ctl.dir()), second);
        ctl.stop.stop();
    });
    assert!(
        files(&run.cast_dir).is_empty(),
        "end of run purges the stream"
    );
}

/// (b) An activation at tick T: within the deadline the directory holds the
/// master, both playlists, both inits and the segments; the validator is
/// green, CODECS is the encoder's, the first segment starts at an IDR and
/// the gate is ready.
#[test]
fn b_an_activation_publishes_a_valid_stream_within_the_deadline() {
    drive("cast_b", Until::Stop, |ctl| {
        ctl.wait_tick(2 * FPS + 7);
        let at = ctl.tick();
        let generation = ctl.cast.gate.activate(false).unwrap();
        assert_eq!(
            ctl.cast.gate.activate(false).unwrap(),
            generation,
            "idempotent"
        );
        ctl.wait_ready(Duration::from_secs(60));
        // Ready = 5 segments of 2 s starting at the next keyframe: never
        // before 10 s of output after the activation (nothing buffered is
        // used), and not long after (the bound is in output ticks and leaves
        // 10 s for the keyframe wait and the sink's latency).
        let ready_after = ctl.tick() - at;
        assert!(
            (10 * FPS - FPS / 2..=20 * FPS).contains(&ready_after),
            "ready {ready_after} ticks after the activation"
        );
        let report = validated(ctl);
        assert!(report.video_segments >= 5 && report.has_audio, "{report:?}");
        assert_eq!(report.audio_segments, report.video_segments);
        assert_eq!(report.target_duration, 2);
        assert_eq!(report.discontinuities, 0);
        let names = files(ctl.dir());
        for name in [
            "master.m3u8".to_owned(),
            "video.m3u8".to_owned(),
            "audio.m3u8".to_owned(),
            format!("init-v-{generation}-0.mp4"),
            format!("init-a-{generation}-0.mp4"),
            format!("seg-v-{generation}-0.m4s"),
            format!("seg-a-{generation}-0.m4s"),
        ] {
            assert!(names.contains(&name), "{name} missing in {names:?}");
        }
        assert!(
            !names
                .iter()
                .any(|name| Path::new(name).extension().is_some_and(|ext| ext == "tmp")),
            "{names:?}"
        );
        // CODECS: H.264 High (libx264's default profile for 4:2:0 8-bit)
        // from the real SPS, AAC-LC; the master carries exactly that and the
        // encoder's real size.
        let master = std::fs::read_to_string(ctl.dir().join("master.m3u8")).unwrap();
        assert!(
            report.codecs.starts_with("avc1.64") && report.codecs.ends_with(",mp4a.40.2"),
            "{}",
            report.codecs
        );
        assert_eq!(report.codecs.len(), "avc1.640028,mp4a.40.2".len());
        assert!(
            master.contains(&format!("CODECS=\"{}\"", report.codecs)),
            "{master}"
        );
        assert!(master.contains("RESOLUTION=160x90"), "{master}");
        assert!(master.contains("FRAME-RATE=25.000"), "{master}");
        // The first segment starts at an IDR, at the first keyframe at or
        // after the activation.
        assert_eq!(first_slice_type(ctl.dir()), 5);
        let activated_at = secs(at);
        assert!(
            (activated_at - 0.5..activated_at + 1.5).contains(&report.first_video_start),
            "{} vs {activated_at}",
            report.first_video_start
        );
        let status = ctl.cast.gate.status();
        assert!(status.ready && status.active && status.last_error.is_none());
        assert_eq!((status.dropped_packets, status.restarts), (0, 0));
        assert!(status.last_publish_unix_ms > 0);
        ctl.run_seconds(12);
        keep_copy(ctl, "b-plain");
        ctl.stop.stop();
    });
}

/// (c) A deactivation empties the directory; a new activation is a new
/// generation whose stream validates.
#[test]
fn c_deactivation_empties_the_directory_and_reactivation_is_a_new_generation() {
    let run = drive("cast_c", Until::Stop, |ctl| {
        ctl.wait_tick(2 * FPS);
        let first = ctl.cast.gate.activate(false).unwrap();
        ctl.wait_ready(Duration::from_secs(60));
        validated(ctl);
        ctl.cast.gate.deactivate();
        wait_for("an empty directory", Duration::from_secs(5), || {
            files(ctl.dir()).is_empty().then_some(())
        });
        assert!(
            ctl.dir().join(CAST_DIR_MARKER).is_file(),
            "the marker stays"
        );
        let status = ctl.cast.gate.status();
        assert!(!status.active && !status.ready && status.generation == 0);
        // Stays empty while inactive.
        ctl.run_seconds(12);
        assert_eq!(files(ctl.dir()), Vec::<String>::new());
        // Re-activate with the 12 s buffered meanwhile: ready at once.
        let second = ctl.cast.gate.activate(true).unwrap();
        assert!(second > first);
        // Ready (5 segments = 10 s of media) inside a 5 s deadline: only
        // the buffered seconds can have provided them.
        ctl.wait_ready(Duration::from_secs(5));
        let report = validated(ctl);
        assert_eq!(listed_generation(ctl.dir()), second);
        assert_eq!(report.media_sequence, 0, "a new stream");
        assert_eq!(report.discontinuities, 0);
        assert_eq!(first_slice_type(ctl.dir()), 5);
        let names = files(ctl.dir());
        assert!(
            !names
                .iter()
                .any(|name| name.contains(&format!("-{first}-"))),
            "{names:?}"
        );
        ctl.stop.stop();
    });
    assert!(
        files(&run.cast_dir).is_empty(),
        "end of run purges the stream"
    );
    assert!(run.cast_dir.join(CAST_DIR_MARKER).is_file());
}

/// (d) A routing take of the active cast output to a source and back: the
/// same stream goes on (same generation, no restart), the validator stays
/// green, a discontinuity is declared only where the routed feed's
/// parameter sets or audio run differ, and the timeline stays the real one
/// (`tfdt` is the output tick time, never rebased).
#[test]
fn d_a_take_to_a_source_and_back_keeps_one_valid_stream_on_the_real_timeline() {
    use crate::output_routing::{OutputTake, RefuseReason, TakeResult};
    use crate::routing_aux::PictureBus;
    drive("cast_d", Until::Stop, |ctl| {
        let index = ctl
            .routing
            .index_of("cast-1")
            .expect("the cast output is routable");
        assert!(ctl.routing.outputs()[index].routable);
        let outcomes = ctl.routing.outcomes().expect("one outcome reader");
        // 12 s buffered, then a pre-roll activation: ready at once.
        ctl.wait_tick(12 * FPS);
        let generation = ctl.cast.gate.activate(true).unwrap();
        ctl.wait_ready(Duration::from_secs(5));
        let keys_before = ctl.routing.snapshot().video_keys;
        let take = |op: &str, bus: PictureBus| {
            // FeedNotFresh is the correct refusal when the synthetic producer
            // was starved on a loaded host: re-issue (at most 5 times).
            for attempt in 0..5 {
                let operation_id = format!("cast:{op}-{attempt}");
                ctl.routing
                    .try_take(OutputTake {
                        operation_id: operation_id.clone(),
                        output_index: index,
                        bus: Some(bus.clone()),
                        audio: None,
                        principal: "operator".to_owned(),
                    })
                    .unwrap();
                let outcome = wait_for(&operation_id, Duration::from_secs(10), || {
                    outcomes.try_recv().ok()
                });
                assert_eq!(outcome.operation_id, operation_id);
                match outcome.outcome {
                    TakeResult::Applied => return,
                    TakeResult::Refused(RefuseReason::FeedNotFresh) => {
                        std::thread::sleep(Duration::from_millis(300));
                    }
                    other => panic!("{operation_id}: {other:?}"),
                }
            }
            panic!("{op} never applied");
        };
        take("to-source", PictureBus::Source("cast_d_b".to_owned()));
        // The snapshot is published at the next routing tick after the
        // outcome: wait for it instead of reading it at once.
        let keys = wait_for(
            "the cast output on the source",
            Duration::from_secs(10),
            || {
                let snapshot = ctl.routing.snapshot();
                (snapshot.outputs[index].bus == "src:cast_d_b").then_some(snapshot.video_keys)
            },
        );
        assert_eq!(keys, keys_before + 1, "+1 encoder for the routed feed");
        ctl.run_seconds(6);
        let on_source = validated(ctl);
        take("to-program", PictureBus::Program);
        wait_for(
            "the cast output back on Program",
            Duration::from_secs(10),
            || (ctl.routing.snapshot().outputs[index].bus == "program").then_some(()),
        );
        ctl.run_seconds(6);
        let back = validated(ctl);
        keep_copy(ctl, "d-take-and-back");
        // One stream throughout: same generation, no restart, no drop.
        let status = ctl.cast.gate.status();
        assert_eq!(status.generation, generation);
        assert_eq!(
            (status.restarts, status.dropped_packets),
            (0, 0),
            "{status:?}"
        );
        assert_eq!(listed_generation(ctl.dir()), generation);
        // At most one declared discontinuity per move.
        assert!(on_source.discontinuities <= 1, "{on_source:?}");
        assert!(back.discontinuities <= 2, "{back:?}");
        assert!(back.media_sequence >= on_source.media_sequence);

        // The timeline is the real one: the last listed segment starts at
        // its tick time (tfdt in 1/25 s units, never rebased), one closed
        // segment plus the open one behind the clock.
        let last = last_video_tfdt_seconds(ctl.dir());
        let now = secs(ctl.tick());
        assert!(
            (1.9..=6.5).contains(&(now - last)),
            "last segment starts at {last} s, the clock is at {now} s"
        );
        // And nothing was skipped: the listed segments span from the first
        // start to the last start without a hole larger than a segment.
        let listed = back.video_segments;
        let mean = (last - back.first_video_start) / f64::from(u32::try_from(listed - 1).unwrap());
        assert!(
            (0.9..=2.3).contains(&mean),
            "{mean} s per segment over {listed}"
        );
        assert_eq!(first_slice_type(ctl.dir()), 5);
        ctl.stop.stop();
    });
}

/// The decode time, in seconds, of the last video segment `video.m3u8`
/// lists, read from the segment's `tfdt` (track timescale 25).
fn last_video_tfdt_seconds(dir: &Path) -> f64 {
    let text = std::fs::read_to_string(dir.join("video.m3u8")).unwrap();
    let name = text.lines().rfind(|l| l.starts_with("seg-v-")).unwrap();
    let bytes = std::fs::read(dir.join(name)).unwrap();
    let tfdt =
        multiview_output::hls::cmaf::find_box(&bytes, &[b"moof", b"traf", b"tfdt"]).expect("tfdt");
    assert_eq!(tfdt[0], 1, "tfdt version 1");
    let ticks = u64::from_be_bytes(tfdt[4..12].try_into().unwrap());
    secs(ticks)
}

/// Run `f` on its own thread and fail (instead of hanging the test binary)
/// when it does not return within `bound`: a run that never ends is a run
/// whose pipeline was paced by a sink.
fn bounded<T: Send + 'static>(bound: Duration, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(bound) {
        Ok(value) => {
            worker.join().expect("run thread");
            value
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            std::panic::resume_unwind(worker.join().expect_err("the run panicked"))
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            panic!("the run did not end within {bound:?}: something paced the pipeline")
        }
    }
}

/// Ticks of the chaos runs (12 s of output).
const CHAOS_TICKS: u64 = 300;
/// The tick at which the chaos runs activate the cast output.
const CHAOS_AT: u64 = 60;

/// What a chaos run observed besides its [`Run`].
#[derive(Default)]
struct ChaosSeen {
    /// Wall time from the fault to the last tick, and the ticks in between.
    after_fault: Option<(Duration, u64)>,
    status: Option<super::CastGateStatus>,
}

/// Drive `CHAOS_TICKS` ticks; at `CHAOS_AT` install `hook` on the cast sink
/// and activate the output; `entered` fires when the sink first reaches the
/// hook. Returns the run and what the controller saw.
fn chaos_run(
    prefix: &'static str,
    hook: Option<super::CastTestHook>,
    entered: Option<std::sync::mpsc::Receiver<()>>,
) -> (Run, ChaosSeen) {
    let seen = Arc::new(Mutex::new(ChaosSeen::default()));
    let record = Arc::clone(&seen);
    let run = bounded(Duration::from_secs(90), move || {
        drive(prefix, Until::Ticks(CHAOS_TICKS), move |ctl| {
            ctl.wait_tick(CHAOS_AT);
            // The control run (no hook) leaves the cast output inactive and
            // measures the same tick span.
            if let Some(hook) = hook {
                ctl.cast.gate.set_test_hook(Some(hook));
                ctl.cast.gate.activate(true).unwrap();
            }
            if let Some(entered) = entered {
                entered
                    .recv_timeout(Duration::from_secs(30))
                    .expect("the cast sink reached its disk write");
            }
            let (from, at) = (Instant::now(), ctl.tick());
            ctl.wait_tick(CHAOS_TICKS);
            let mut seen = record.lock().unwrap();
            seen.after_fault = Some((from.elapsed(), CHAOS_TICKS - at));
            seen.status = Some(ctl.cast.gate.status());
        })
    });
    let seen = std::mem::take(&mut *seen.lock().unwrap());
    (run, seen)
}

/// The HLS output of a chaos run must be the control run's, packet for
/// packet, and the clock must have emitted every tick. These counts are the
/// proof; the wall-clock check after them only rules out a paced pipeline.
fn assert_unaffected(control: &(Run, ChaosSeen), run: &Run, seen: &ChaosSeen) {
    let (control, control_seen) = control;
    let ticks = i64::try_from(CHAOS_TICKS).unwrap();
    assert_eq!(control.frames, CHAOS_TICKS);
    assert_eq!(control.hls_video_pts, (0..ticks).collect::<Vec<_>>());
    assert_eq!(run.frames, CHAOS_TICKS, "every output tick emitted");
    assert_eq!(
        run.hls_video_pts,
        (0..ticks).collect::<Vec<_>>(),
        "contiguous PTS at the HLS output, no gap"
    );
    assert_eq!(
        run.hls_audio_packets, control.hls_audio_packets,
        "the HLS output lost no audio packet"
    );
    // Not paced: compared with the control run's time for the same ticks on
    // this machine (twice that, plus two delivery graces), never with a
    // fixed speed. A bake consumer that waited on the faulty sink would pay
    // one delivery grace (2 s) for every window it offered: more than 200
    // windows in this span, so several hundred seconds against a bound of
    // tens.
    let (elapsed, ticks_left) = seen.after_fault.expect("the controller saw the fault");
    let (control_elapsed, control_ticks) = control_seen.after_fault.expect("the control span");
    assert!(
        ticks_left > 200 && control_ticks > 200,
        "{ticks_left} / {control_ticks}"
    );
    let bound = control_elapsed * 2 + crate::binding_delivery::PUMP_WEDGE_GRACE * 2;
    assert!(
        elapsed < bound,
        "{ticks_left} ticks after the fault took {elapsed:?}; the control run took \
         {control_elapsed:?} for {control_ticks} (bound {bound:?})"
    );
}

/// (e) R3 chaos test, invariants #1 and #10: a cast sink whose disk write
/// blocks forever costs the output clock zero ticks and the HLS output zero
/// packets.
#[test]
fn e_a_blocked_cast_sink_costs_the_clock_zero_ticks_and_the_hls_output_zero_packets() {
    let control = chaos_run("cast_e0", None, None);
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let release_rx = Mutex::new(release_rx);
    let hook: super::CastTestHook = Arc::new(move || {
        let _ = entered_tx.try_send(());
        // Wedge until the test drops the sender (after the run returned).
        let _ = release_rx.lock().unwrap().recv();
    });
    let (run, seen) = chaos_run("cast_e1", Some(hook), Some(entered_rx));
    assert_unaffected(&control, &run, &seen);
    let status = seen.status.unwrap();
    assert!(status.active && !status.ready, "{status:?}");
    assert_eq!(status.video_segments, 0, "nothing was published");
    // The delivery dropped (and counted) what the wedged sink did not take.
    assert!(status.dropped_packets > 0, "{status:?}");
    assert!(
        run.lines.iter().any(|line| line.contains("WEDGED")),
        "the wedged sink is reported, not hidden: {:?}",
        run.lines
    );
    drop(release_tx);
}

/// (e) A panicking cast sink disables only the cast output: the run and
/// the HLS output are untouched, and the output says why it is unusable.
#[test]
fn e_a_panicking_cast_sink_disables_only_the_cast_output() {
    let control = chaos_run("cast_e2", None, None);
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let hook: super::CastTestHook = Arc::new(move || {
        let _ = entered_tx.try_send(());
        panic!("injected cast sink panic");
    });
    let (run, seen) = chaos_run("cast_e3", Some(hook), Some(entered_rx));
    assert_unaffected(&control, &run, &seen);
    let status = seen.status.unwrap();
    assert!(!status.active && !status.ready, "{status:?}");
    let reason = status.last_error.unwrap();
    assert!(reason.contains("cast output \"cast-1\""), "{reason}");
    assert_eq!(status.dropped_packets, 0, "the dead sink keeps draining");
    // The cast sink still took every packet (it discards them), reported
    // itself disabled, and left no stream file behind.
    assert_eq!(
        run.cast_packets,
        run.hls_video_pts.len() + run.hls_audio_packets
    );
    assert!(
        run.lines
            .iter()
            .any(|line| line.contains("cast cast-1") && line.contains("(disabled)")),
        "{:?}",
        run.lines
    );
    assert!(
        files(&run.cast_dir).is_empty(),
        "{:?}",
        files(&run.cast_dir)
    );
}

/// (f) Drop when full: the cast sink stalls for longer than the delivery
/// waits, then comes back. The packets it missed are counted, the stream
/// declares the gap as a discontinuity, the directory validates, and the
/// HLS output never noticed.
#[test]
fn f_a_stalled_then_released_cast_sink_declares_the_gap_and_still_validates() {
    const TICKS: u64 = 700;
    let stall = Arc::new(AtomicBool::new(false));
    let stalled = Arc::new(AtomicBool::new(false));
    let hook: super::CastTestHook = {
        let (stall, stalled) = (Arc::clone(&stall), Arc::clone(&stalled));
        Arc::new(move || {
            if stall.swap(false, Ordering::AcqRel) {
                stalled.store(true, Ordering::Release);
                // One disk write that takes 4 s: twice the delivery's grace.
                std::thread::sleep(Duration::from_secs(4));
            }
        })
    };
    let seen = Arc::new(Mutex::new(None));
    let record = Arc::clone(&seen);
    let run = bounded(Duration::from_secs(120), move || {
        drive("cast_f", Until::Ticks(TICKS), move |ctl| {
            ctl.wait_tick(11 * FPS);
            ctl.cast.gate.set_test_hook(Some(hook));
            let generation = ctl.cast.gate.activate(true).unwrap();
            ctl.wait_ready(Duration::from_secs(5));
            let before = validated(ctl);
            assert_eq!(before.discontinuities, 0);
            stall.store(true, Ordering::Release);
            wait_for("the stall", Duration::from_secs(5), || {
                stalled.load(Ordering::Acquire).then_some(())
            });
            // Stalled for 4 s, then 8 s of healthy stream.
            ctl.run_seconds(12);
            let after = validated(ctl);
            let status = ctl.cast.gate.status();
            *record.lock().unwrap() = Some((generation, before, after, status));
        })
    });
    let (generation, before, after, status) = seen.lock().unwrap().take().unwrap();
    // The HLS output and the clock never noticed.
    assert_eq!(run.frames, TICKS);
    assert_eq!(
        run.hls_video_pts,
        (0..i64::try_from(TICKS).unwrap()).collect::<Vec<_>>()
    );
    // The gap is counted and declared; the same stream goes on.
    assert!(status.dropped_packets > 0, "{status:?}");
    assert!(status.active && status.ready, "{status:?}");
    assert_eq!(status.generation, generation);
    assert_eq!(
        status.restarts, 0,
        "a gap is not a packager error: {status:?}"
    );
    assert_eq!(after.discontinuities, 1, "{after:?}");
    assert!(
        after.video_segments > before.video_segments.min(9) - 4,
        "{after:?}"
    );
    // The cast sink received fewer packets than the HLS sink, by at least
    // what was counted as dropped... and the report shows the loss.
    let offered = run.hls_video_pts.len() + run.hls_audio_packets;
    assert!(
        run.cast_packets < offered,
        "{} of {offered}",
        run.cast_packets
    );
    assert!(
        run.lines
            .iter()
            .any(|line| line.starts_with("binding 1:") && line.contains("windows lost")),
        "{:?}",
        run.lines
    );
}

/// For the manual ffprobe / ffmpeg decode check: when
/// `MULTIVIEW_CAST_KEEP_DIR` is set, copy the running stream's directory to
/// `<that dir>/<name>` (a copy taken between two publishes, validated).
fn keep_copy(ctl: &Ctl, name: &str) {
    let Some(root) = std::env::var_os("MULTIVIEW_CAST_KEEP_DIR") else {
        return;
    };
    let target = PathBuf::from(root).join(name);
    for _ in 0..40 {
        let _ = std::fs::remove_dir_all(&target);
        std::fs::create_dir_all(&target).unwrap();
        let before = ctl.cast.gate.status().video_segments;
        for file in files(ctl.dir()) {
            // A file evicted between the listing and the copy is not listed
            // any more; the publish check below then retries.
            let _ = std::fs::copy(ctl.dir().join(&file), target.join(&file));
        }
        if ctl.cast.gate.status().video_segments == before {
            let report = check_cast_dir(&target);
            assert_eq!(report.errors, Vec::<String>::new());
            return;
        }
    }
    panic!("no consistent copy of the cast directory");
}

/// A cast output without a configured `dir` writes to
/// `<state dir>/cast/<id>`, created `0700` on the first activation (and not
/// before).
#[test]
fn the_default_directory_is_under_the_state_dir_and_created_on_activation() {
    let run = drive_in(
        "cast_g",
        Until::Stop,
        true,
        |_| {},
        |ctl| {
            ctl.wait_tick(2 * FPS);
            assert!(
                ctl.dir().ends_with(".multiview/cast/cast-1"),
                "{:?}",
                ctl.dir()
            );
            assert!(
                !ctl.dir().exists(),
                "nothing is created before a cast starts"
            );
            ctl.cast.gate.activate(false).unwrap();
            wait_for("the first segment", Duration::from_secs(60), || {
                (ctl.cast.gate.status().video_segments >= 1).then_some(())
            });
            assert_eq!(
                std::fs::read_to_string(ctl.dir().join(CAST_DIR_MARKER)).unwrap(),
                "cast-1"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(ctl.dir()).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o700);
            }
            ctl.stop.stop();
        },
    );
    assert_eq!(files(&run.cast_dir), Vec::<String>::new());
}

/// A directory that holds files this engine did not write disables the
/// cast output for the run: the run starts, the HLS output is complete, the
/// control-plane handle refuses the activation with the fix in its text,
/// and the operator's files are untouched from start to end of run.
#[test]
fn a_foreign_directory_disables_the_cast_output_without_touching_it_or_the_run() {
    use multiview_control::cast_engine::{CastEngine, CastEngineError};
    const TICKS: u64 = 100;
    let foreign = ["holiday.mp4", "master.m3u8", "seg-v-1-0.m4s"];
    let populate = |root: &Path| {
        std::fs::create_dir(root.join("cast")).unwrap();
        for name in foreign {
            std::fs::write(root.join("cast").join(name), b"keep").unwrap();
        }
    };
    let adapter = Arc::new(Mutex::new(None));
    let keep = Arc::clone(&adapter);
    let run = drive_in("cast_h", Until::Ticks(TICKS), false, populate, move |ctl| {
        // The adapter the control plane uses, over a slot of this run.
        let slot = super::cast_outputs_slot();
        slot.store(Arc::new(vec![ctl.cast.clone()]));
        let engine = super::CastEngineAdapter(Arc::clone(&slot));
        let outputs = engine.outputs();
        assert_eq!(outputs.len(), 1);
        let status = &outputs[0];
        assert_eq!(
            (status.id.as_str(), status.active, status.ready),
            ("cast-1", false, false)
        );
        assert_eq!((status.width, status.height), (160, 90));
        assert_eq!((status.fps_num, status.fps_den), (25, 1));
        let reason = status
            .last_error
            .clone()
            .expect("the reason is in the status");
        assert!(
            reason.contains("cast output \"cast-1\"")
                && reason.contains(
                    "holds files this engine did not write; point `dir` at an \
                                    empty directory or remove `dir`"
                ),
            "{reason}"
        );
        assert_eq!(
            engine.activate("cast-1", true),
            Err(CastEngineError::Unavailable(reason))
        );
        assert_eq!(engine.deactivate("cast-1"), Ok(()));
        assert_eq!(
            engine.activate("nope", true),
            Err(CastEngineError::UnknownOutput("nope".to_owned()))
        );
        ctl.wait_tick(TICKS - 10);
        assert_eq!(files(ctl.dir()), foreign);
        *keep.lock().unwrap() = Some((engine, slot));
    });
    // The run was not affected.
    assert_eq!(run.frames, TICKS);
    assert_eq!(
        run.hls_video_pts,
        (0..i64::try_from(TICKS).unwrap()).collect::<Vec<_>>()
    );
    // Nothing was written, deleted or marked, also at the end of the run.
    assert_eq!(files(&run.cast_dir), foreign);
    for name in foreign {
        assert_eq!(std::fs::read(run.cast_dir.join(name)).unwrap(), b"keep");
    }
    assert!(!run.cast_dir.join(CAST_DIR_MARKER).exists());
    // After the run the handle answers "not running".
    let (engine, slot) = adapter.lock().unwrap().take().unwrap();
    assert_eq!(
        engine.activate("cast-1", true),
        Err(CastEngineError::NotRunning)
    );
    slot.store(Arc::new(Vec::new()));
    assert_eq!(engine.outputs(), Vec::new());
    assert_eq!(
        engine.deactivate("cast-1"),
        Err(CastEngineError::NotRunning)
    );
}
