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
/// Program-audio packets as a sink saw them.
type SeenAudio = Arc<Mutex<Vec<multiview_ffmpeg::EncodedPacket>>>;
/// Video packets as a sink saw them (for decoding).
type SeenVideo = Arc<Mutex<Vec<multiview_ffmpeg::EncodedPacket>>>;

fn recorder(seen: Seen, audio: SeenAudio) -> TestSinkRunner {
    recorder_with_video(seen, audio, None)
}

fn recorder_with_video(seen: Seen, audio: SeenAudio, video: Option<SeenVideo>) -> TestSinkRunner {
    Box::new(move |rx| {
        let mut frames = 0;
        while let Ok(packet) = rx.recv() {
            frames += 1;
            if packet.kind() == multiview_ffmpeg::StreamKind::Audio {
                audio
                    .lock()
                    .unwrap()
                    .push(packet.clone_shared_ref().unwrap());
            }
            if packet.kind() == multiview_ffmpeg::StreamKind::Video {
                if let Some(video) = video.as_ref() {
                    video
                        .lock()
                        .unwrap()
                        .push(packet.clone_shared_ref().unwrap());
                }
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
    /// Ticks the output clock emitted (counted on the hot loop itself).
    // reason: only the overlay-gated layout-bake protection tests read it.
    #[cfg_attr(not(feature = "overlay"), allow(dead_code))]
    hot_ticks: u64,
    /// Frames the live policy shed before the consumer (0 offline).
    // reason: only the overlay-gated layout-bake protection tests read it.
    #[cfg_attr(not(feature = "overlay"), allow(dead_code))]
    dropped: u64,
    outputs: Vec<Vec<(i64, bool, bool)>>,
    audio: Vec<Vec<multiview_ffmpeg::EncodedPacket>>,
    rendition_configs: usize,
    final_snapshot: Arc<RoutingSnapshot>,
    video: Vec<Vec<multiview_ffmpeg::EncodedPacket>>,
}

/// Drive `ticks` real pipeline ticks; `control` runs on its own thread with
/// the run's routing slot while the clock runs.
fn drive<F>(prefix: &str, ticks: u64, control: F) -> Run
where
    F: FnOnce(OutputRoutingSlot) + Send + 'static,
{
    drive_config(CONFIG, false, prefix, ticks, control)
}

fn drive_config<F>(template: &str, audio: bool, prefix: &str, ticks: u64, control: F) -> Run
where
    F: FnOnce(OutputRoutingSlot) + Send + 'static,
{
    drive_full(template, audio, false, prefix, ticks, control)
}

/// As [`drive_config`]; `switcher` attaches the document's M/E (Preview and
/// Clean routable) and every output's video packets are kept for decoding.
fn drive_full<F>(
    template: &str,
    audio: bool,
    switcher: bool,
    prefix: &str,
    ticks: u64,
    control: F,
) -> Run
where
    F: FnOnce(OutputRoutingSlot) + Send + 'static,
{
    drive_clocked(template, audio, switcher, prefix, ticks, None, control)
}

/// As [`drive_full`], with an optional hook run on every `format-convert`
/// thread before each convert (#342 review I3 chaos).
fn drive_hooked<F>(
    template: &str,
    audio: bool,
    switcher: bool,
    prefix: &str,
    ticks: u64,
    control: F,
    convert_hook: Option<crate::format_convert::ConvertHook>,
) -> Run
where
    F: FnOnce(OutputRoutingSlot) + Send + 'static,
{
    drive_prepared(
        template,
        audio,
        switcher,
        prefix,
        ticks,
        move |pipeline| {
            if let Some(hook) = convert_hook {
                pipeline.set_convert_hook(hook);
            }
        },
        control,
    )
}

/// As [`drive_full`], with an optional boot-take retry policy (a test of the
/// give-up path shortens the 30 s window to stay within its wall bound).
fn drive_clocked<F>(
    template: &str,
    audio: bool,
    switcher: bool,
    prefix: &str,
    ticks: u64,
    boot_retry: Option<super::BootRetryPolicy>,
    control: F,
) -> Run
where
    F: FnOnce(OutputRoutingSlot) + Send + 'static,
{
    drive_prepared(
        template,
        audio,
        switcher,
        prefix,
        ticks,
        move |pipeline| {
            if let Some(policy) = boot_retry {
                pipeline.set_boot_retry(policy);
            }
        },
        control,
    )
}

/// As [`drive_full`]; `prep` sees the built pipeline before the run.
fn drive_prepared<F>(
    template: &str,
    audio: bool,
    switcher: bool,
    prefix: &str,
    ticks: u64,
    prep: impl FnOnce(&mut Pipeline),
    control: F,
) -> Run
where
    F: FnOnce(OutputRoutingSlot) + Send + 'static,
{
    drive_outputs(
        template,
        audio,
        switcher,
        prefix,
        ticks,
        2,
        (SendPolicy::BlockForExact, None),
        prep,
        control,
    )
}

/// As [`drive_config`] (no audio, no M/E) with `outputs` test sinks.
#[cfg(feature = "overlay")]
fn drive_runners<F>(
    template: &str,
    prefix: &str,
    ticks: u64,
    outputs: usize,
    policy: (SendPolicy, Option<u64>),
    control: F,
) -> Run
where
    F: FnOnce(OutputRoutingSlot) + Send + 'static,
{
    drive_outputs(
        template,
        false,
        false,
        prefix,
        ticks,
        outputs,
        policy,
        |_| {},
        control,
    )
}

/// As [`drive_prepared`] with `outputs` test sinks (one per config output).
#[allow(clippy::too_many_arguments)]
fn drive_outputs<F>(
    template: &str,
    audio: bool,
    switcher: bool,
    prefix: &str,
    ticks: u64,
    outputs: usize,
    (policy, shed_at): (SendPolicy, Option<u64>),
    prep: impl FnOnce(&mut Pipeline),
    control: F,
) -> Run
where
    F: FnOnce(OutputRoutingSlot) + Send + 'static,
{
    let config =
        multiview_config::MultiviewConfig::load_from_toml(&template.replace('@', prefix)).unwrap();
    let mut pipeline = Pipeline::build(&config).unwrap();
    pipeline.address_boot_feeds_to_test_sinks(&config);
    prep(&mut pipeline);
    if audio {
        pipeline.enable_program_audio();
    }
    // #342: a document without an M/E can still ask for decodable video.
    let keep_video = switcher || template.contains("[outputs.format]");
    if switcher {
        crate::switcher_runtime::attach_first_slice(&mut pipeline, None)
            .unwrap()
            .expect("the document declares an M/E");
    }
    let rendition_configs = pipeline.rendition_config_count();
    let slot = pipeline.output_routing();
    let seen: Vec<Seen> = (0..outputs)
        .map(|_| Arc::new(Mutex::new(Vec::new())))
        .collect();
    let seen_audio: Vec<SeenAudio> = (0..outputs)
        .map(|_| Arc::new(Mutex::new(Vec::new())))
        .collect();
    let seen_video: Vec<SeenVideo> = (0..outputs)
        .map(|_| Arc::new(Mutex::new(Vec::new())))
        .collect();
    let runners: Vec<TestSinkRunner> = seen
        .iter()
        .zip(&seen_audio)
        .zip(&seen_video)
        .map(|((s, a), v)| {
            if keep_video || template.contains("# keep-video") {
                recorder_with_video(Arc::clone(s), Arc::clone(a), Some(Arc::clone(v)))
            } else {
                recorder(Arc::clone(s), Arc::clone(a))
            }
        })
        .collect();
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
    let hot_ticks = Arc::new(std::sync::atomic::AtomicU64::new(0));
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
                policy,
                runners,
                hot_tick_observer: Some(Arc::clone(&hot_ticks)),
                consumer_fault_after_ticks: None,
                shed_at_tick: shed_at,
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
        hot_ticks: hot_ticks.load(Ordering::Acquire),
        dropped: result.report.dropped,
        outputs: seen.iter().map(|s| s.lock().unwrap().clone()).collect(),
        audio: seen_audio
            .iter()
            .map(|a| std::mem::take(&mut *a.lock().unwrap()))
            .collect(),
        rendition_configs,
        final_snapshot,
        video: seen_video
            .iter()
            .map(|v| std::mem::take(&mut *v.lock().unwrap()))
            .collect(),
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
        bus: Some(bus),
        audio: None,
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

/// Source `@_a` is a silent test card mixed into Program; `@_b` is `bars`
/// (the 1 kHz line-up tone, AUD-5) routed but EXCLUDED from the program mix.
/// So the tone can reach an output only through `audio = follow`.
const AUDIO_CONFIG: &str = r##"
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
kind = "solid"
color = "#202020"
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
[audio]
sample_rate_hz = 48000
[[audio.routes]]
input_id = "@_a"
channels = { kind = "stereo" }
include_in_program_bus = true
[[audio.routes]]
input_id = "@_b"
channels = { kind = "stereo" }
include_in_program_bus = false
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

/// Decode one output's program-audio packets; returns `(pts, left channel)`
/// per packet.
fn decode_audio(packets: &[multiview_ffmpeg::EncodedPacket]) -> Vec<(i64, Vec<f32>)> {
    // The same settings as the program encoder: the same AudioSpecificConfig.
    let reference = multiview_output::AudioRenditionEncoder::new(
        &multiview_output::AudioEncodeConfig::aac(48_000, 2, 128_000),
    )
    .unwrap();
    let mut decoder = multiview_ffmpeg::StreamAudioDecoder::from_codec_parameters(
        reference.codec_params(),
        reference.time_base(),
    )
    .unwrap();
    packets
        .iter()
        .map(|p| {
            let pcm = decoder.decode_interleaved(p).unwrap();
            (p.pts().unwrap(), pcm.chunks(2).map(|c| c[0]).collect())
        })
        .collect()
}

fn rms(v: &[f32]) -> f32 {
    let n = f32::from(u16::try_from(v.len().clamp(1, 60_000)).unwrap());
    (v.iter().map(|x| x * x).sum::<f32>() / n).sqrt()
}

fn crossings(v: &[f32]) -> usize {
    v.windows(2)
        .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
        .count()
}

/// ADR-0108 §5.1 `audio = follow` through the real pipeline: after a take of
/// output 1 to `src:<bars>`, output 1's audio after G decodes to the bars
/// 1 kHz line-up tone while output 0 keeps the (silent) program mix; after
/// the take back to Program it fades to the program mix again. Every output
/// keeps one audio frame per 1024 samples (PTS step constant), the private
/// encoder's `AudioSpecificConfig` matched (no audio move refused).
#[test]
fn audio_follows_a_take_to_a_source_with_a_crossfade_through_the_real_pipeline() {
    const TICKS: u64 = 400;
    let observed = Arc::new(Mutex::new(Observed::default()));
    let record = Arc::clone(&observed);
    let run = drive_config(AUDIO_CONFIG, true, "route_audio", TICKS, move |slot| {
        let handle = wait_for("the routing handle", Duration::from_secs(10), || {
            slot.load()
        });
        let outcomes = handle.outcomes().expect("one outcome reader");
        wait_for("the first routing tick", Duration::from_secs(10), || {
            (handle.snapshot().video_keys > 0).then_some(())
        });
        std::thread::sleep(Duration::from_millis(800));
        let applied = |make: &dyn Fn() -> OutputTake| {
            for _ in 0..5 {
                let op = make().operation_id;
                handle.try_take(make()).unwrap();
                let o = wait_for(&format!("{op} outcome"), Duration::from_secs(5), || {
                    outcomes.try_recv().ok()
                });
                match o.outcome {
                    TakeResult::Applied => return o,
                    TakeResult::Refused(super::RefuseReason::FeedNotFresh) => {}
                    other => panic!("{op}: {other:?}"),
                }
            }
            panic!("never applied");
        };
        let o = applied(&|| take("to-bars", 1, PictureBus::Source("route_audio_b".to_owned())));
        record.lock().unwrap().to_source = Some((o, 0));
        std::thread::sleep(Duration::from_millis(1600));
        let o = applied(&|| take("to-program", 1, PictureBus::Program));
        record.lock().unwrap().to_program = Some((o, 0));
    });
    let observed = observed.lock().unwrap();
    let (to_bars, _) = observed.to_source.clone().expect("take applied");
    let (to_program, _) = observed.to_program.clone().expect("back applied");
    assert_eq!(run.frames, TICKS);
    let snap = &run.final_snapshot;
    assert_eq!(snap.audio.opened, 1, "one private audio bus for output 1");
    assert_eq!(snap.audio.refused, 0, "AudioSpecificConfig byte-identical");
    assert_eq!(snap.audio.failed, 0, "{snap:?}");
    let pts = |o: usize| {
        run.audio[o]
            .iter()
            .map(|p| p.pts().unwrap())
            .collect::<Vec<_>>()
    };
    assert!(!run.audio[1].is_empty());
    for o in 0..2 {
        for pair in pts(o).windows(2) {
            assert_eq!(pair[1] - pair[0], 1024, "output {o}: audio PTS step");
        }
    }
    assert_eq!(pts(0), pts(1), "the same audio frames on both outputs");
    // 25 fps at 48 kHz: 1920 samples per tick; G = next 1024 boundary.
    let g = |tick: u64| -> i64 { (i64::try_from(tick).unwrap() * 1920 + 1023) / 1024 * 1024 };
    let decoded: Vec<_> = (0..2).map(|o| decode_audio(&run.audio[o])).collect();
    let span = |o: usize, from: i64, to: i64| -> Vec<f32> {
        decoded[o]
            .iter()
            .filter(|(p, _)| *p >= from && *p < to)
            .flat_map(|(_, s)| s.iter().copied())
            .collect()
    };
    // Settled after the fade (G + one tick + the decoder's frame of delay).
    let on_bars = (g(to_bars.tick) + 4096, g(to_program.tick) - 1024);
    assert!(
        on_bars.1 - on_bars.0 >= 8 * 1024,
        "window too short: {on_bars:?}"
    );
    let tone = span(1, on_bars.0, on_bars.1);
    let program = span(0, on_bars.0, on_bars.1);
    assert!(
        rms(&tone) > 0.02,
        "output 1 carries the bars tone: rms {}",
        rms(&tone)
    );
    // ~1 kHz: two zero crossings per millisecond.
    let ms = tone.len() / 48;
    let hz = crossings(&tone) * 500 / ms.max(1);
    assert!((900..=1100).contains(&hz), "output 1 tone at {hz} Hz");
    assert!(
        rms(&program) < 1e-3,
        "output 0 keeps the silent program mix: {}",
        rms(&program)
    );
    // Before the take, and after the return, output 1 carries the program mix.
    let before = span(1, 2048, g(to_bars.tick) - 1024);
    assert!(rms(&before) < 1e-3, "before the take: {}", rms(&before));
    let after = span(1, g(to_program.tick) + 4096, i64::MAX);
    assert!(after.len() > 1024, "audio after the return");
    assert!(rms(&after) < 1e-3, "after the return: {}", rms(&after));
    // The crossfade spans the samples from G: the tone rises inside the
    // tick after G, never before G.
    let pre_g = span(1, g(to_bars.tick) - 3072, g(to_bars.tick) - 1024);
    assert!(rms(&pre_g) < 1e-3, "no tone before G: {}", rms(&pre_g));
}

/// Rule 2 with the program loudness normaliser on (it always runs on the
/// program bus when program audio is enabled): the program mix carries the
/// bars tone (`@_b` included in the mix) and output 1 takes `src:<solid>`.
/// Up to G the private bus must be fed EXACTLY the PCM the shared program
/// encoder is fed for the same samples (the post-loudnorm program block),
/// sample for sample, so both encoders code one signal before the boundary.
#[test]
fn a_private_bus_is_fed_the_post_loudnorm_program_pcm_up_to_g() {
    const TICKS: u64 = 300;
    let config = AUDIO_CONFIG.replacen(
        "input_id = \"@_b\"\nchannels = { kind = \"stereo\" }\ninclude_in_program_bus = false",
        "input_id = \"@_b\"\nchannels = { kind = \"stereo\" }\ninclude_in_program_bus = true",
        1,
    );
    assert_ne!(config, AUDIO_CONFIG, "the tone is mixed into Program");
    let observed = Arc::new(Mutex::new(Observed::default()));
    let record = Arc::clone(&observed);
    let tap = Arc::new(Mutex::new(None));
    let tap_out = Arc::clone(&tap);
    let run = drive_config(&config, true, "route_ln", TICKS, move |slot| {
        let handle = wait_for("the routing handle", Duration::from_secs(10), || {
            slot.load()
        });
        *tap_out.lock().unwrap() = Some(Arc::clone(&handle.audio_tap));
        let outcomes = handle.outcomes().expect("one outcome reader");
        wait_for("the first routing tick", Duration::from_secs(10), || {
            (handle.snapshot().video_keys > 0).then_some(())
        });
        // After the normaliser's 3 s short-term window: a non-unity gain.
        std::thread::sleep(Duration::from_millis(4500));
        for _ in 0..5 {
            let t = take("to-solid", 1, PictureBus::Source("route_ln_a".to_owned()));
            handle.try_take(t).unwrap();
            let o = wait_for("outcome", Duration::from_secs(5), || {
                outcomes.try_recv().ok()
            });
            match o.outcome {
                TakeResult::Applied => {
                    record.lock().unwrap().to_source = Some((o, 0));
                    return;
                }
                TakeResult::Refused(super::RefuseReason::FeedNotFresh) => {}
                other => panic!("{other:?}"),
            }
        }
        panic!("never applied");
    });
    assert_eq!(run.frames, TICKS);
    assert_eq!(
        run.final_snapshot.audio.opened, 1,
        "{:?}",
        run.final_snapshot
    );
    let (applied, _) = observed.lock().unwrap().to_source.clone().expect("applied");
    let tap = tap.lock().unwrap().clone().expect("tap");
    let tap = tap.lock().unwrap();
    // The take's move is the first (a later fallback may add another).
    assert_ne!(tap.moves.len(), 0, "the take scheduled a crossfade");
    let (_, g) = tap.moves[0];
    assert!(
        g <= (applied.tick * 1920).div_ceil(1024) * 1024,
        "G {g} at or before the applied video"
    );
    let program: std::collections::BTreeMap<u64, f32> = tap
        .program
        .iter()
        .flat_map(|(start, pcm)| {
            pcm.iter()
                .enumerate()
                .map(move |(i, v)| (start * 2 + u64::try_from(i).unwrap(), *v))
        })
        .collect();
    let mut compared = 0_usize;
    let mut energy = 0.0_f64;
    for (output, start, pcm) in &tap.private {
        assert_eq!(*output, 1);
        for (i, v) in pcm.iter().enumerate() {
            let at = start * 2 + u64::try_from(i).unwrap();
            if at / 2 >= g {
                break;
            }
            let p = program
                .get(&at)
                .unwrap_or_else(|| panic!("sample {at} not encoded"));
            assert_eq!(
                v.to_bits(),
                p.to_bits(),
                "interleaved sample {at} before G {g}"
            );
            compared += 1;
            energy += f64::from(*v) * f64::from(*v);
        }
    }
    assert!(
        compared >= 2 * 2048,
        "the bus coded >= 2 frames before G: {compared}"
    );
    assert!(
        energy > 1.0,
        "the program mix is not silent before G: {energy}"
    );
}

/// One full-canvas cell on a bright solid source, an M/E (Clean routable),
/// two HLS outputs with identical settings. A canvas large enough for the
/// overlay bake to draw its per-tile chrome (state badge, label) in an
/// `overlay` build.
const CLEAN_CONFIG: &str = r##"
schema_version = 1
[canvas]
width = 320
height = 192
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
[switcher]
[[switcher.mix_effects]]
id = "me1"
program = "main"
program_source = "@_a"
preview_source = "@_b"
"##;

/// Decode one output's video packets (mpeg2video, no reordering): the luma
/// plane of every decoded picture, by packet PTS.
fn decode_luma(
    packets: &[multiview_ffmpeg::EncodedPacket],
) -> std::collections::BTreeMap<i64, Vec<u8>> {
    let codec = ffmpeg_next::decoder::find(ffmpeg_next::codec::Id::MPEG2VIDEO).unwrap();
    let mut decoder = ffmpeg_next::codec::context::Context::new_with_codec(codec)
        .decoder()
        .video()
        .unwrap();
    let mut out = std::collections::BTreeMap::new();
    let mut pending: std::collections::VecDeque<i64> = std::collections::VecDeque::new();
    let mut frame = ffmpeg_next::frame::Video::empty();
    let mut drain = |decoder: &mut ffmpeg_next::decoder::Video,
                     pending: &mut std::collections::VecDeque<i64>,
                     out: &mut std::collections::BTreeMap<i64, Vec<u8>>| {
        while decoder.receive_frame(&mut frame).is_ok() {
            let (w, h) = (
                usize::try_from(frame.width()).unwrap(),
                usize::try_from(frame.height()).unwrap(),
            );
            let stride = frame.stride(0);
            let data = frame.data(0);
            let mut luma = Vec::with_capacity(w * h);
            for row in 0..h {
                luma.extend_from_slice(&data[row * stride..row * stride + w]);
            }
            if let Some(pts) = pending.pop_front() {
                out.insert(pts, luma);
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

/// ADR-0108 slice 2 (§2.5): a take of output 1 to Clean through the real
/// pipeline. Output 1 then carries the Program canvas BEFORE the overlay
/// bake: in an `overlay` build its pictures differ from output 0's (Program,
/// baked: per-tile chrome) where the bake draws, and are the flat source
/// colour there; without `overlay` the bake is the identity and both match.
/// PTS step stays one tick on both outputs; output 0 is untouched; routing
/// only Clean never wakes the aux thread.
#[test]
fn a_take_to_clean_carries_the_pre_bake_canvas_through_the_real_pipeline() {
    const TICKS: u64 = 250;
    let observed = Arc::new(Mutex::new(None));
    let record = Arc::clone(&observed);
    let run = drive_full(
        CLEAN_CONFIG,
        false,
        true,
        "route_clean",
        TICKS,
        move |slot| {
            let handle = wait_for("the routing handle", Duration::from_secs(10), || {
                slot.load()
            });
            assert!(handle.has_preview(), "the M/E is attached");
            let outcomes = handle.outcomes().expect("one outcome reader");
            wait_for("the first routing tick", Duration::from_secs(10), || {
                (handle.snapshot().video_keys > 0).then_some(())
            });
            std::thread::sleep(Duration::from_millis(400));
            handle
                .try_take(take("to-clean", 1, PictureBus::Clean))
                .unwrap();
            let o = wait_for("to-clean outcome", Duration::from_secs(10), || {
                outcomes.try_recv().ok()
            });
            assert_eq!(o.outcome, TakeResult::Applied, "{o:?}");
            let s = wait_for("snapshot", Duration::from_secs(1), || {
                let s = handle.snapshot();
                (s.outputs[1].bus == "clean").then_some(s)
            });
            assert_eq!(s.aux_ticks_sent, 0, "only Clean routed: no aux tick");
            *record.lock().unwrap() = Some(o);
        },
    );
    let applied = observed.lock().unwrap().clone().expect("applied");
    assert_eq!(run.frames, TICKS);
    assert_eq!(run.final_snapshot.aux_ticks_sent, 0, "aux never woke");
    let all: Vec<i64> = (0..i64::try_from(TICKS).unwrap()).collect();
    assert_eq!(pts_of(&run.outputs[0]), all, "output 0 untouched");
    // Output 1 ends the run on a live-opened key: the router's end-of-run
    // shutdown retires it and drops its last in-flight windows (an existing
    // slice-1 end-of-run behaviour, not a Clean one), so its tail may be a
    // few ticks short. Every packet it did get is contiguous from tick 0.
    let o1 = pts_of(&run.outputs[1]);
    assert!(
        o1.len() + 4 >= all.len(),
        "output 1 tail: {} of {}",
        o1.len(),
        all.len()
    );
    assert_eq!(o1, all[..o1.len()], "output 1: PTS step one tick, no gap");
    let at = i64::try_from(applied.tick).unwrap();
    let first = run.outputs[1].iter().find(|p| p.0 == at).unwrap();
    assert!(
        first.1 && first.2,
        "the cut is a keyframe with a sequence header"
    );
    let program = decode_luma(&run.video[0]);
    let clean = decode_luma(&run.video[1]);
    // Compare settled pictures well after the cut (both encoders converged
    // on the flat field).
    let compared: Vec<i64> = (at + 20..at + 60)
        .filter(|t| clean.contains_key(t))
        .collect();
    assert!(compared.len() >= 30, "decoded {} pictures", compared.len());
    for t in &compared {
        let (p, c) = (&program[t], &clean[t]);
        assert_eq!(p.len(), c.len());
        // The Clean picture is the un-overlaid canvas: one flat colour.
        let (lo, hi) = c
            .iter()
            .fold((u8::MAX, u8::MIN), |(lo, hi), v| (lo.min(*v), hi.max(*v)));
        assert!(
            hi - lo <= 6,
            "tick {t}: Clean is the flat source picture ({lo}..{hi})"
        );
        let differing = p
            .iter()
            .zip(c)
            .filter(|(a, b)| a.abs_diff(**b) > 24)
            .count();
        if cfg!(feature = "overlay") {
            assert!(
                differing > 0,
                "tick {t}: Program carries the baked chrome, Clean does not"
            );
        } else {
            assert_eq!(differing, 0, "tick {t}: no bake in this build");
        }
    }
}

/// #342: two outputs at one non-canvas format (a vertical cover of the
/// 64x64 canvas) share ONE base encoder; its worker converts the baked
/// Program canvas, and the encoded pictures have that size.
const VERTICAL_CONFIG: &str = r##"
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
[outputs.format]
width = 32
height = 64
fit = { mode = "cover", anchor = "center" }
[[outputs]]
id = "out1"
kind = "hls"
path = "/tmp/multiview-routing-e2e/@/b/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
[outputs.format]
width = 32
height = 64
fit = { mode = "cover" }
"##;

fn decode_size(packets: &[multiview_ffmpeg::EncodedPacket]) -> Option<(u32, u32)> {
    let codec = ffmpeg_next::decoder::find(ffmpeg_next::codec::Id::MPEG2VIDEO).unwrap();
    let mut decoder = ffmpeg_next::codec::context::Context::new_with_codec(codec)
        .decoder()
        .video()
        .unwrap();
    let mut frame = ffmpeg_next::frame::Video::empty();
    for p in packets {
        decoder.send_packet(&p.to_owned_packet()).unwrap();
        if decoder.receive_frame(&mut frame).is_ok() {
            return Some((frame.width(), frame.height()));
        }
    }
    decoder.send_eof().unwrap();
    decoder
        .receive_frame(&mut frame)
        .is_ok()
        .then(|| (frame.width(), frame.height()))
}

#[test]
fn outputs_at_one_format_share_a_base_encoder_at_that_size() {
    const TICKS: u64 = 30;
    let run = drive_full(
        VERTICAL_CONFIG,
        false,
        false,
        "route_vert",
        TICKS,
        |_slot| {},
    );
    assert_eq!(run.frames, TICKS);
    assert_eq!(
        run.rendition_configs, 1,
        "one encoder for one (format, settings)"
    );
    assert_eq!(run.final_snapshot.video_keys, 1);
    let all: Vec<i64> = (0..i64::try_from(TICKS).unwrap()).collect();
    for (i, output) in run.outputs.iter().enumerate() {
        assert_eq!(pts_of(output), all, "output {i}: one frame per tick");
    }
    for (i, video) in run.video.iter().enumerate() {
        assert_eq!(
            decode_size(video),
            Some((32, 64)),
            "output {i} at its format"
        );
    }
}

/// #342: the base rendition key is (codec, width, height, bitrate, fit):
/// equal formats and settings share, a different format or bit rate does not,
/// and the canvas format declared explicitly is the canvas key.
#[test]
fn base_rendition_keys_follow_format_and_slot_settings() {
    let doc = VERTICAL_CONFIG.replace('@', "route_keys")
        + r#"
[[outputs]]
id = "out2"
kind = "hls"
path = "/tmp/multiview-routing-e2e/route_keys/c/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
[[outputs]]
id = "out3"
kind = "hls"
path = "/tmp/multiview-routing-e2e/route_keys/d/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
[outputs.format]
width = 64
height = 64
[[outputs]]
id = "out4"
kind = "hls"
path = "/tmp/multiview-routing-e2e/route_keys/e/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
[outputs.format]
width = 32
height = 64
fit = { mode = "cover" }
[[outputs.slots.video]]
bitrate_kbps = 1500
"#;
    let config = multiview_config::MultiviewConfig::load_from_toml(&doc).unwrap();
    config.validate().unwrap();
    let pipeline = Pipeline::build(&config).unwrap();
    let (keys, configs) = pipeline.rendition_keys_for_test();
    assert_eq!(keys[0], keys[1], "same format and settings share (P1)");
    assert_eq!(
        keys[2], keys[3],
        "an explicit canvas format is the canvas key"
    );
    assert_ne!(keys[0], keys[2], "another format: its own encoder");
    assert_ne!(keys[0], keys[4], "another bit rate: its own encoder");
    assert_eq!(configs.len(), 3);
    assert_eq!((configs[keys[0]].0, configs[keys[0]].1), (32, 64));
    assert_eq!((configs[keys[2]].0, configs[keys[2]].1), (64, 64));
    assert_eq!(configs[keys[2]].2, 4_000_000, "default bit rate unchanged");
    assert_eq!(configs[keys[4]].2, 1_500_000);
}

/// One canvas output and one 32x64 cover output.
const MIXED_FORMAT_CONFIG: &str = r##"
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
[outputs.format]
width = 32
height = 64
fit = { mode = "cover" }
"##;

/// #342 review I3 (§12.10 "a wedged convert thread"): the convert thread of
/// a non-canvas output wedges forever from tick 20. The canvas output keeps
/// exactly one frame per tick with a steady PTS step for 320 ticks, the run
/// is not fatal, and the wedged output keeps one frame per tick too (its
/// picture frozen at the last converted one).
#[test]
fn a_wedged_convert_thread_never_moves_the_clock_or_another_output() {
    const TICKS: u64 = 320;
    const WEDGE_AT: u64 = 20;
    let (release, wait) = std::sync::mpsc::channel::<()>();
    let wait = Mutex::new(wait);
    let wedged = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&wedged);
    let hook: crate::format_convert::ConvertHook = Arc::new(move |tick| {
        if tick >= WEDGE_AT {
            flag.store(true, Ordering::Release);
            // Blocks until the test ends (the sender is dropped then).
            let _ = wait.lock().unwrap().recv();
        }
    });
    let run = drive_hooked(
        MIXED_FORMAT_CONFIG,
        false,
        false,
        "route_wedge",
        TICKS,
        |_slot| {},
        Some(hook),
    );
    assert!(wedged.load(Ordering::Acquire), "the convert thread wedged");
    assert_eq!(run.frames, TICKS, "the clock emitted every tick");
    let all: Vec<i64> = (0..i64::try_from(TICKS).unwrap()).collect();
    assert_eq!(
        pts_of(&run.outputs[0]),
        all,
        "canvas output: one frame per tick, PTS step one tick"
    );
    assert_eq!(
        pts_of(&run.outputs[1]),
        all,
        "wedged output: still one (frozen) frame per tick"
    );
    assert_eq!(decode_size(&run.video[1]), Some((32, 64)));
    // Frozen: every keyframe well after the wedge decodes to one picture.
    let luma = decode_luma(&run.video[1]);
    let late_keys: Vec<&Vec<u8>> = run.outputs[1]
        .iter()
        .filter(|(pts, key, _)| *key && *pts > i64::try_from(WEDGE_AT).unwrap() + 30)
        .filter_map(|(pts, _, _)| luma.get(pts))
        .collect();
    assert!(late_keys.len() >= 5, "keyframes after the wedge");
    assert!(
        late_keys.windows(2).all(|w| w[0] == w[1]),
        "the wedged output's picture is frozen"
    );
    drop(release);
}

/// `CONFIG` plus a persisted feed for `out1`: `video = { source = "<id>" }`,
/// and a third source `@_never`: a file source whose file does not exist, so
/// it never produces a picture (never fresh).
fn boot_config(feed_source: &str) -> String {
    format!(
        "{CONFIG}\n[[sources]]\nid = \"@_never\"\nkind = \"file\"\npath = \"/nonexistent/multiview-routing-e2e/@/never.ts\"\n[[routing.output]]\noutput = \"out1\"\nvideo = {{ source = \"{feed_source}\" }}\n"
    )
}

/// #329 F1(b): a run whose config persists `out1 = { source = X }` comes up
/// with `out1` on `src:X` after the boot take (a live source; a refused boot
/// take on a starved host is re-issued within the window), `out0` on Program
/// throughout, one window per tick on both.
#[test]
fn a_persisted_source_feed_is_on_air_after_the_boot_take() {
    const TICKS: u64 = 300;
    let applied = Arc::new(Mutex::new(None::<TakeOutcome>));
    let record = Arc::clone(&applied);
    let template = boot_config("@_b");
    let run = drive_clocked(
        &template,
        false,
        false,
        "route_boot",
        TICKS,
        None,
        move |slot| {
            let handle = wait_for("the routing handle", Duration::from_secs(10), || {
                slot.load()
            });
            let outcomes = handle.outcomes().expect("one outcome reader");
            // Keyed on the output index (PR #352 round 2), not the config
            // id: `boot:1`, never `boot:test-sink-1`.
            let o = wait_for("the boot take applied", Duration::from_secs(10), || {
                outcomes
                    .try_iter()
                    .find(|o| o.operation_id == "boot:1" && o.outcome == TakeResult::Applied)
            });
            // The pipeline-minted id is a boot take to the control plane.
            assert!(multiview_control::output_routing::is_boot_take(
                &o.operation_id
            ));
            // The readback agrees with the outcome: out1 on air on the source.
            wait_for(
                "the readback shows the source on air",
                Duration::from_secs(1),
                || {
                    let s = handle.snapshot();
                    (s.outputs[1].bus == "src:route_boot_b"
                        && s.outputs[0].bus == "program"
                        && s.boot_takes_pending == 0)
                        .then_some(())
                },
            );
            *record.lock().unwrap() = Some(o);
        },
    );
    let applied = applied.lock().unwrap().clone().expect("boot take applied");
    assert_eq!(applied.output_index, 1);
    assert_eq!(applied.bus, PictureBus::Source("route_boot_b".to_owned()));
    assert_eq!(run.final_snapshot.boot_takes_given_up, 0);
    assert_eq!(run.frames, TICKS);
    let all: Vec<i64> = (0..i64::try_from(TICKS).unwrap()).collect();
    assert_eq!(pts_of(&run.outputs[0]), all, "out0: every Program window");
    // Contiguous, one window per tick, from tick 0 through the cut and on to
    // within 5 ticks of the end. The last few windows of a live-opened
    // encoder race the run's shutdown (seen under a parallel test load: one
    // tail window missing); that end-of-run tail is outside this test.
    let steady: Vec<i64> = pts_of(&run.outputs[1])
        .into_iter()
        .filter(|&t| t < i64::try_from(TICKS).unwrap() - 5)
        .collect();
    assert_eq!(
        steady,
        all[..steady.len()],
        "out1: one window per tick across the cut"
    );
    assert_eq!(
        u64::try_from(steady.len()).unwrap(),
        TICKS - 5,
        "out1 carried to the end"
    );
    let first = run.outputs[1]
        .iter()
        .find(|p| p.0 == i64::try_from(applied.tick).unwrap())
        .unwrap();
    assert!(
        first.1 && first.2,
        "the cut lands on a keyframe with a sequence header: {first:?}"
    );
}

/// PR #352 round 2: a run output whose config id is 300 bytes long (an HLS
/// config id is its path, which has no length bound) — well over
/// `MAX_TAKE_ID_BYTES` (128). The boot take must still be minted, retried on
/// its own slot, given up correctly, and its audio re-issued for the right
/// output — all keyed on the output index, never on the (unbounded) id.
#[test]
fn a_300_byte_config_id_boot_take_is_retried_and_given_up_on_the_right_slot() {
    // 25 fps: the 8 s window = 200 ticks; run past it.
    const TICKS: u64 = 330;
    let long_id = "o".repeat(300);
    assert!(
        long_id.len() > super::MAX_TAKE_ID_BYTES,
        "the config id itself must exceed the take-id cap"
    );
    let template = boot_config("@_never")
        .replace("id = \"out1\"", &format!("id = \"{long_id}\""))
        .replacen("output = \"out1\"", &format!("output = \"{long_id}\""), 1);
    let policy = super::BootRetryPolicy {
        retry_secs: 2,
        window_secs: 8,
    };
    let refusals = Arc::new(Mutex::new(Vec::<String>::new()));
    let record = Arc::clone(&refusals);
    let run = drive_clocked(
        &template,
        false,
        false,
        "route_bootlong",
        TICKS,
        Some(policy),
        move |slot| {
            let handle = wait_for("the routing handle", Duration::from_secs(10), || {
                slot.load()
            });
            let outcomes = handle.outcomes().expect("one outcome reader");
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(30) {
                while let Ok(o) = outcomes.try_recv() {
                    // Always `boot:1`, never a truncated/garbled form of the
                    // 300-byte id (which would also risk colliding with
                    // another output's truncated id).
                    assert_eq!(o.operation_id, "boot:1", "{o:?}");
                    assert!(
                        o.operation_id.len() < super::MAX_TAKE_ID_BYTES,
                        "the minted id stays well under the cap: {o:?}"
                    );
                    record.lock().unwrap().push(o.operation_id.clone());
                }
                if handle.snapshot().boot_takes_given_up == 1 {
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            panic!("the boot take was never given up");
        },
    );
    let seen = refusals.lock().unwrap().clone();
    assert!(seen.len() >= 2, "retried on the same slot: {seen:?}");
    let s = &run.final_snapshot;
    assert_eq!(s.boot_takes_given_up, 1);
    assert_eq!(s.boot_takes_pending, 0);
    // The output stays on Program (never routed, since the long-id source
    // never became fresh): the slot match, keyed on the output index, never
    // let a stray outcome for a different output's index collide.
    assert!(s
        .outputs
        .iter()
        .all(|o| o.bus == "program" && o.pending.is_none()));
    assert_eq!(run.frames, TICKS);
}

/// #329 F1(b): a persisted feed whose source never becomes fresh: the boot
/// take is refused `feed-not-fresh`, re-issued (at most once per retry
/// period) through the window, then given up; the output stays on Program and
/// Program is untouched (one window per tick on both outputs). The policy is
/// shortened (retry 2 s, window 8 s) only to bound the wall time; production
/// uses 2 s / 30 s (`BootRetryPolicy::default`).
#[test]
fn a_never_fresh_boot_feed_is_retried_then_given_up_and_program_is_untouched() {
    // 25 fps: the 8 s window = 200 ticks; run past it.
    const TICKS: u64 = 330;
    const WINDOW_TICKS: u64 = 200;
    let refusals = Arc::new(Mutex::new(Vec::<u64>::new()));
    let record = Arc::clone(&refusals);
    let template = boot_config("@_never");
    let policy = super::BootRetryPolicy {
        retry_secs: 2,
        window_secs: 8,
    };
    let run = drive_clocked(
        &template,
        false,
        false,
        "route_bootnf",
        TICKS,
        Some(policy),
        move |slot| {
            let handle = wait_for("the routing handle", Duration::from_secs(10), || {
                slot.load()
            });
            let outcomes = handle.outcomes().expect("one outcome reader");
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(30) {
                while let Ok(o) = outcomes.try_recv() {
                    // Keyed on the output index (PR #352 round 2).
                    assert_eq!(o.operation_id, "boot:1", "{o:?}");
                    assert_eq!(
                        o.outcome,
                        TakeResult::Refused(super::RefuseReason::FeedNotFresh),
                        "{o:?}"
                    );
                    record.lock().unwrap().push(o.tick);
                }
                if handle.snapshot().boot_takes_given_up == 1 {
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            panic!("the boot take was never given up");
        },
    );
    let ticks = refusals.lock().unwrap().clone();
    // Each attempt waits out the 50-tick prime deadline; a re-issue comes no
    // sooner than 2 s (50 ticks) after the previous issue, and only inside
    // the window — so at most WINDOW/50 + 1 attempts.
    assert!(ticks.len() >= 2, "retried: {ticks:?}");
    assert!(ticks.len() <= 5, "bounded: {ticks:?}");
    for pair in ticks.windows(2) {
        assert!(
            pair[1] - pair[0] >= 50,
            "at most one attempt per 2 s: {ticks:?}"
        );
    }
    assert!(
        ticks
            .iter()
            .all(|&t| t <= WINDOW_TICKS + super::TAKE_PRIME_DEADLINE_TICKS + 2),
        "no attempt after the window: {ticks:?}"
    );
    let s = &run.final_snapshot;
    assert_eq!(s.boot_takes_given_up, 1);
    assert_eq!(s.boot_takes_pending, 0);
    assert!(s
        .outputs
        .iter()
        .all(|o| o.bus == "program" && o.pending.is_none()));
    assert_eq!(s.dynamic_encoders_live, 0, "no encoder was ever opened");
    assert_eq!(run.frames, TICKS);
    let all: Vec<i64> = (0..i64::try_from(TICKS).unwrap()).collect();
    for (i, output) in run.outputs.iter().enumerate() {
        assert_eq!(pts_of(output), all, "output {i}: Program untouched");
    }
}

/// ADR-0108 slice 2 (spec G1) through the real pipeline: an audio-only take
/// of output 1 to `Source(<bars>)` while its video stays on Program. Output
/// 1's video is output 0's, packet for packet (same encoder, no move); its
/// audio decodes to the 1 kHz tone after G while output 0 keeps the program
/// mix. An audio-only take to `Program` returns it to the program mix and,
/// within ~10 ticks, to the shared stream (`audio_buses_live == 0`). Audio
/// PTS steps 1024 (1/48000) = 1920 MPEG-TS (90 kHz) units on every packet.
#[test]
#[allow(clippy::too_many_lines)] // One end-to-end scenario with its assertions.
fn an_audio_only_take_routes_a_source_and_returns_to_the_shared_stream() {
    const TICKS: u64 = 400;
    let observed = Arc::new(Mutex::new(Observed::default()));
    let record = Arc::clone(&observed);
    let buses_after = Arc::new(Mutex::new(None));
    let buses_out = Arc::clone(&buses_after);
    let run = drive_config(AUDIO_CONFIG, true, "route_ao", TICKS, move |slot| {
        let handle = wait_for("the routing handle", Duration::from_secs(10), || {
            slot.load()
        });
        let outcomes = handle.outcomes().expect("one outcome reader");
        wait_for("the first routing tick", Duration::from_secs(10), || {
            (handle.snapshot().video_keys > 0).then_some(())
        });
        std::thread::sleep(Duration::from_millis(800));
        let audio_take = |op: &str, choice: super::AudioChoice| {
            handle
                .try_take(OutputTake {
                    operation_id: op.to_owned(),
                    output_index: 1,
                    bus: None,
                    audio: Some(choice),
                    principal: "operator".to_owned(),
                })
                .unwrap();
            let o = wait_for(&format!("{op} outcome"), Duration::from_secs(5), || {
                outcomes.try_recv().ok()
            });
            assert_eq!(o.operation_id, op);
            assert_eq!(o.outcome, TakeResult::Applied, "{o:?}");
            o
        };
        let o = audio_take(
            "to-bars",
            super::AudioChoice::Source("route_ao_b".to_owned()),
        );
        let s = handle.snapshot();
        assert_eq!(s.outputs[1].bus, "program", "the video stays");
        assert_eq!(s.outputs[1].audio, "src:route_ao_b");
        record.lock().unwrap().to_source = Some((o, s.audio_buses_live));
        std::thread::sleep(Duration::from_millis(1600));
        let o = audio_take("to-program", super::AudioChoice::Program);
        record.lock().unwrap().to_program = Some((o, 0));
        // Back on the shared stream within ~10 ticks (4 frames + a fade):
        // 10 ticks are 400 ms of the 40 ms-per-tick test clock.
        let started = std::time::Instant::now();
        wait_for(
            "return to the shared stream",
            Duration::from_secs(5),
            || (handle.snapshot().audio_buses_live == 0).then_some(()),
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "returned late: {:?}",
            started.elapsed()
        );
        *buses_out.lock().unwrap() = Some(handle.snapshot().audio);
    });
    let observed = observed.lock().unwrap();
    let (to_bars, live) = observed.to_source.clone().expect("take applied");
    let (to_program, _) = observed.to_program.clone().expect("back applied");
    assert_eq!(live, 1, "one private bus while on the tone");
    assert_eq!(run.frames, TICKS);
    let counters = buses_after.lock().unwrap().expect("returned");
    assert_eq!(counters.opened, 1, "{counters:?}");
    assert_eq!(counters.returned, 1, "moved back to the shared stream");
    assert_eq!(counters.failed, 0, "{counters:?}");
    assert_eq!(counters.refused, 0, "{counters:?}");
    assert_eq!(run.final_snapshot.audio_buses_live, 0);
    // The video: output 1 is output 0, packet for packet.
    assert_eq!(run.outputs[0], run.outputs[1], "the same video packets");
    let all: Vec<i64> = (0..i64::try_from(TICKS).unwrap()).collect();
    assert_eq!(pts_of(&run.outputs[0]), all);
    let pts = |o: usize| {
        run.audio[o]
            .iter()
            .map(|p| p.pts().unwrap())
            .collect::<Vec<_>>()
    };
    for o in 0..2 {
        assert!(pts(o).len() > 100);
        for pair in pts(o).windows(2) {
            assert_eq!(pair[1] - pair[0], 1024, "output {o}: audio PTS step");
            assert_eq!(
                (pair[1] - pair[0]) * 90_000 / 48_000,
                1920,
                "output {o}: 90 kHz step"
            );
        }
    }
    assert_eq!(pts(0), pts(1), "the same audio frames on both outputs");
    let g = |tick: u64| -> i64 { (i64::try_from(tick).unwrap() * 1920 + 1023) / 1024 * 1024 };
    let decoded: Vec<_> = (0..2).map(|o| decode_audio(&run.audio[o])).collect();
    let span = |o: usize, from: i64, to: i64| -> Vec<f32> {
        decoded[o]
            .iter()
            .filter(|(p, _)| *p >= from && *p < to)
            .flat_map(|(_, s)| s.iter().copied())
            .collect()
    };
    // The outcome tick is the tick whose block holds G: G lies within it.
    let on_bars = (g(to_bars.tick) + 4096, g(to_program.tick) - 2048);
    assert!(on_bars.1 - on_bars.0 >= 8 * 1024, "{on_bars:?}");
    let tone = span(1, on_bars.0, on_bars.1);
    assert!(
        rms(&tone) > 0.02,
        "output 1 carries the tone: {}",
        rms(&tone)
    );
    let ms = tone.len() / 48;
    let hz = crossings(&tone) * 500 / ms.max(1);
    assert!((900..=1100).contains(&hz), "output 1 tone at {hz} Hz");
    let program = span(0, 2048, i64::MAX);
    assert!(
        rms(&program) < 1e-3,
        "output 0 keeps the silent program mix"
    );
    let before = span(1, 2048, g(to_bars.tick) - 3072);
    assert!(rms(&before) < 1e-3, "before the take: {}", rms(&before));
    let after = span(1, g(to_program.tick) + 4096, i64::MAX);
    assert!(after.len() > 1024);
    assert!(rms(&after) < 1e-3, "after the return: {}", rms(&after));
}

/// [`CONFIG`] with two distinct solid sources (dark left, bright right), so
/// a swap of the halves is visible in the decoded pictures.
const LAYOUT_CONFIG: &str = r##"
# keep-video
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
kind = "solid"
color = "#202020"
[[sources]]
id = "@_b"
kind = "solid"
color = "#e0e0e0"
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

/// A routed layout for the 64x64 test canvas: the Program grid with its two
/// cells swapped (`b` left, `a` right).
fn swapped_layout(prefix: &str) -> PictureBus {
    two_cell_layout(prefix, ["b", "a"])
}

/// The `swapped` layout id with its cells `order` (left, right).
fn two_cell_layout(prefix: &str, order: [&str; 2]) -> PictureBus {
    use multiview_core::layout::{Canvas, Cell, FitMode, Layout};
    let cell = |x: f32, source: &str| Cell {
        x,
        y: 0.0,
        w: 0.5,
        h: 1.0,
        z: 0,
        fit: FitMode::Fill,
        source: Some(format!("{prefix}_{source}")),
        opacity: 1.0,
    };
    let layout = Layout {
        name: "swapped".to_owned(),
        canvas: Canvas {
            width: 64,
            height: 64,
            fps_num: 25,
            fps_den: 1,
        },
        cells: vec![cell(0.0, order[0]), cell(0.5, order[1])],
    };
    PictureBus::Layout(crate::routing_aux::LayoutRef::new(Arc::new(
        crate::aux_layout::RoutedLayout {
            id: "swapped".to_owned(),
            slates: vec![multiview_config::FailoverSlate::Black; 2],
            audio: multiview_config::LayoutAudio::default(),
            overlays: multiview_config::LayoutOverlays::none(),
            revision: crate::output_routing::next_layout_revision(),
            layout,
        },
    )))
}

/// ADR-0108 slice 3 (#306) through the real pipeline: output 1 is taken to a
/// stored layout (Program's two cells swapped), composed by the real
/// `routing-aux` thread from the stores Program reads. The cut is a keyframe
/// with a sequence header; output 1's PTS step stays one tick; output 0 keeps
/// every Program window; output 1's decoded picture is Program's with its
/// halves swapped (the same sources, no new decoder). Then a flood of 12
/// back-to-back takes alternating layout/Program: every take gets exactly
/// one terminal outcome and Program never misses a window. Finally output 1
/// returns to Program at +0 encoders.
#[test]
#[allow(clippy::too_many_lines)] // One end-to-end scenario with its assertions.
fn a_take_to_a_layout_composes_it_live_and_survives_a_take_flood() {
    const TICKS: u64 = 400;
    let observed = Arc::new(Mutex::new(None));
    let record = Arc::clone(&observed);
    let run = drive_config(LAYOUT_CONFIG, false, "route_layout", TICKS, move |slot| {
        let handle = wait_for("the routing handle", Duration::from_secs(10), || {
            slot.load()
        });
        let outcomes = handle.outcomes().expect("one outcome reader");
        wait_for("the first routing tick", Duration::from_secs(10), || {
            (handle.snapshot().video_keys > 0).then_some(())
        });
        std::thread::sleep(Duration::from_millis(800));
        let layout = swapped_layout("route_layout");
        handle
            .try_take(take("to-layout", 1, layout.clone()))
            .unwrap();
        let o = wait_for("to-layout outcome", Duration::from_secs(10), || {
            outcomes.try_recv().ok()
        });
        assert_eq!(o.outcome, TakeResult::Applied, "{o:?}");
        assert_eq!(o.bus, layout, "applied carries the layout bus");
        let s = wait_for("snapshot", Duration::from_secs(1), || {
            let s = handle.snapshot();
            (s.outputs[1].bus == "aux:swapped").then_some(s)
        });
        assert_eq!(s.aux_canvases, vec!["aux:swapped".to_owned()]);
        assert_eq!(s.video_keys, 2, "+1 encoder for the layout bus");
        std::thread::sleep(Duration::from_millis(1500));
        // The flood: 12 takes, no waiting between them.
        let flood: Vec<String> = (0..12).map(|i| format!("flood-{i}")).collect();
        for (i, op) in flood.iter().enumerate() {
            let bus = if i % 2 == 0 {
                PictureBus::Program
            } else {
                layout.clone()
            };
            handle.try_take(take(op, 1, bus)).unwrap();
        }
        let mut settled = Vec::new();
        wait_for("every flood outcome", Duration::from_secs(20), || {
            settled.extend(outcomes.try_iter().map(|o| (o.operation_id, o.outcome)));
            (settled.len() >= flood.len()).then_some(())
        });
        // Exactly one terminal outcome per take, in order; the last applies.
        let ids: Vec<&String> = settled.iter().map(|(id, _)| id).collect();
        assert_eq!(ids, flood.iter().collect::<Vec<_>>(), "{settled:?}");
        assert_eq!(
            settled.last().unwrap().1,
            TakeResult::Applied,
            "{settled:?}"
        );
        // The flood ended on the layout; go back to Program at +0.
        handle
            .try_take(take("to-program", 1, PictureBus::Program))
            .unwrap();
        let back = wait_for("to-program outcome", Duration::from_secs(10), || {
            outcomes.try_recv().ok()
        });
        assert_eq!(back.outcome, TakeResult::Applied, "{back:?}");
        let s = wait_for("snapshot", Duration::from_secs(1), || {
            let s = handle.snapshot();
            (s.outputs[1].bus == "program").then_some(s)
        });
        assert_eq!(s.video_keys, 2, "+0 encoders back to Program");
        *record.lock().unwrap() = Some((o, back));
    });
    let (applied, back) = observed.lock().unwrap().clone().expect("applied");
    assert_eq!(run.frames, TICKS);
    let all: Vec<i64> = (0..i64::try_from(TICKS).unwrap()).collect();
    assert_eq!(
        pts_of(&run.outputs[0]),
        all,
        "output 0 gets every Program window"
    );
    assert_eq!(
        pts_of(&run.outputs[1]),
        all,
        "output 1: one packet per tick, PTS step one tick across every move"
    );
    for at in [applied.tick, back.tick] {
        let first = run.outputs[1]
            .iter()
            .find(|p| p.0 == i64::try_from(at).unwrap())
            .unwrap();
        assert!(
            first.1 && first.2,
            "cut at {at} is a keyframe with a sequence header"
        );
    }
    // Output 1 shows Program with its halves swapped while on the layout.
    let program = decode_luma(&run.video[0]);
    let routed = decode_luma(&run.video[1]);
    let at = i64::try_from(applied.tick).unwrap();
    let compared: Vec<i64> = (at + 10..at + 30)
        .filter(|t| routed.contains_key(t) && program.contains_key(t))
        .collect();
    assert!(compared.len() >= 15, "decoded {} pictures", compared.len());
    let mean = |plane: &[u8], x0: usize| -> i64 {
        let mut sum = 0i64;
        let mut n = 0i64;
        for row in 8..56 {
            for x in x0 + 8..x0 + 24 {
                sum += i64::from(plane[row * 64 + x]);
                n += 1;
            }
        }
        sum / n
    };
    for t in &compared {
        let (p, r) = (&program[t], &routed[t]);
        let (pl, pr, rl, rr) = (mean(p, 0), mean(p, 32), mean(r, 0), mean(r, 32));
        assert!(
            pr - pl > 100,
            "tick {t}: the halves really differ ({pl}, {pr})"
        );
        assert!(
            (rl - pr).abs() <= 6 && (rr - pl).abs() <= 6,
            "tick {t}: routed halves ({rl}, {rr}) are Program's swapped ({pl}, {pr})"
        );
    }
    assert_eq!(run.final_snapshot.dynamic_encoders_live, 0);
}

/// PR 2 (#306) through the real pipeline: a live edit of the routed layout
/// (its cells un-swapped) reaches output 1 with no take: from an IDR on, its
/// picture is Program's unswapped. Then the layout is deleted: output 1 falls
/// back to Program at a keyframe. Output 0 keeps every Program window and
/// output 1's PTS step stays one tick throughout.
#[test]
#[allow(clippy::too_many_lines)] // One end-to-end scenario with its assertions.
fn a_live_edit_reaches_the_routed_layout_and_a_delete_falls_back() {
    const TICKS: u64 = 300;
    let observed = Arc::new(Mutex::new(None));
    let record = Arc::clone(&observed);
    let run = drive_config(LAYOUT_CONFIG, false, "live_edit", TICKS, move |slot| {
        let handle = wait_for("the routing handle", Duration::from_secs(10), || {
            slot.load()
        });
        let outcomes = handle.outcomes().expect("one outcome reader");
        wait_for("the first routing tick", Duration::from_secs(10), || {
            (handle.snapshot().video_keys > 0).then_some(())
        });
        std::thread::sleep(Duration::from_millis(800));
        let swapped = swapped_layout("live_edit");
        handle.try_take(take("to-layout", 1, swapped)).unwrap();
        let applied = wait_for("to-layout outcome", Duration::from_secs(10), || {
            outcomes.try_recv().ok()
        });
        assert_eq!(applied.outcome, TakeResult::Applied, "{applied:?}");
        // Past the per-key IDR floor, then the live edit.
        std::thread::sleep(Duration::from_millis(1500));
        let PictureBus::Layout(edited) = two_cell_layout("live_edit", ["a", "b"]) else {
            panic!("a layout bus");
        };
        let edit_tick = handle.snapshot().outputs[1].since_tick;
        handle
            .try_layout_change(crate::output_routing::LayoutChange::Edited(edited))
            .unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        assert!(outcomes.try_recv().is_err(), "a live edit is not a take");
        assert_eq!(handle.snapshot().outputs[1].bus, "aux:swapped");
        handle
            .try_layout_change(crate::output_routing::LayoutChange::Removed {
                id: "swapped".to_owned(),
                revision: crate::output_routing::next_layout_revision(),
            })
            .unwrap();
        let fell = wait_for("fallback outcome", Duration::from_secs(10), || {
            outcomes.try_recv().ok()
        });
        assert_eq!(
            fell.outcome,
            TakeResult::Fallback(crate::output_routing::FallbackReason::AuxFailed),
            "{fell:?}"
        );
        wait_for("back on Program", Duration::from_secs(2), || {
            (handle.snapshot().outputs[1].bus == "program").then_some(())
        });
        *record.lock().unwrap() = Some((applied, edit_tick, fell));
    });
    let (applied, edit_tick, fell) = observed.lock().unwrap().clone().expect("observed");
    assert_eq!(edit_tick, applied.tick, "the edit did not re-take");
    assert_eq!(run.frames, TICKS);
    let all: Vec<i64> = (0..i64::try_from(TICKS).unwrap()).collect();
    assert_eq!(pts_of(&run.outputs[0]), all, "output 0 gets every window");
    assert_eq!(
        pts_of(&run.outputs[1]),
        all,
        "output 1: one packet per tick"
    );
    let fell_at = run.outputs[1]
        .iter()
        .find(|p| p.0 == i64::try_from(fell.tick).unwrap())
        .unwrap();
    assert!(fell_at.1, "the fallback cut is a keyframe");
    // Before the edit the halves are swapped; after it they match Program.
    let program = decode_luma(&run.video[0]);
    let routed = decode_luma(&run.video[1]);
    let mean = |plane: &[u8], x0: usize| -> i64 {
        let mut sum = 0i64;
        let mut n = 0i64;
        for row in 8..56 {
            for x in x0 + 8..x0 + 24 {
                sum += i64::from(plane[row * 64 + x]);
                n += 1;
            }
        }
        sum / n
    };
    let at = i64::try_from(applied.tick).unwrap();
    let fell_tick = i64::try_from(fell.tick).unwrap();
    let mut swapped_seen = 0;
    let mut unswapped_seen = 0;
    let mut first_unswapped = None;
    for t in at + 5..fell_tick {
        let (Some(p), Some(r)) = (program.get(&t), routed.get(&t)) else {
            continue;
        };
        let (pl, pr, rl, rr) = (mean(p, 0), mean(p, 32), mean(r, 0), mean(r, 32));
        if (rl - pr).abs() <= 6 && (rr - pl).abs() <= 6 {
            assert!(
                unswapped_seen == 0,
                "tick {t}: never back to swapped after the edit"
            );
            swapped_seen += 1;
        } else if (rl - pl).abs() <= 6 && (rr - pr).abs() <= 6 {
            first_unswapped.get_or_insert(t);
            unswapped_seen += 1;
        }
    }
    assert!(
        swapped_seen >= 10,
        "swapped before the edit ({swapped_seen})"
    );
    assert!(unswapped_seen >= 10, "the edit is live ({unswapped_seen})");
    let first = first_unswapped.unwrap();
    // The edited picture starts within a GOP of a forced keyframe.
    assert!(
        run.outputs[1]
            .iter()
            .any(|p| p.1 && (first - 3..=first + 1).contains(&p.0)),
        "an IDR near the edit at {first}"
    );
}

/// ADR-0108 slice 2 §6.1: a run whose config persists `audio = { source =
/// "<b>" }` on output 1 starts it on follow and re-applies the persisted feed
/// as an audio-only boot take: within a few ticks of the first routing tick
/// the router snapshot shows output 1 carrying `src:<b>` (video unchanged),
/// and output 0 keeps the program mix.
#[test]
fn a_persisted_audio_feed_is_re_applied_by_a_boot_take() {
    const TICKS: u64 = 120;
    let template = format!(
        "{AUDIO_CONFIG}\n[[routing.output]]\noutput = \"out1\"\naudio = {{ source = \"@_b\" }}\n"
    );
    let seen = Arc::new(Mutex::new(None));
    let record = Arc::clone(&seen);
    let prep = |pipeline: &mut Pipeline| {
        let takes = pipeline.boot_audio_takes_mut();
        assert_eq!(
            *takes,
            vec![(
                "out1".to_owned(),
                super::AudioChoice::Source("route_boot_b".to_owned())
            )],
            "the persisted feed is parsed into one boot take"
        );
        // The harness binds its sinks as `test-sink-N` (positional).
        takes[0].0 = "test-sink-1".to_owned();
    };
    let run = drive_prepared(
        &template,
        true,
        false,
        "route_boot",
        TICKS,
        prep,
        move |slot| {
            let handle = wait_for("the routing handle", Duration::from_secs(10), || {
                slot.load()
            });
            let outcomes = handle.outcomes().expect("one outcome reader");
            let o = wait_for("the boot take outcome", Duration::from_secs(10), || {
                outcomes.try_recv().ok()
            });
            // Keyed on the output index (PR #352 round 2), not `test-sink-1`.
            assert_eq!(o.operation_id, "boot:audio:1");
            assert!(multiview_control::output_routing::is_boot_take(
                &o.operation_id
            ));
            assert_eq!(o.outcome, TakeResult::Applied, "{o:?}");
            let s = wait_for("the boot audio on air", Duration::from_secs(5), || {
                let s = handle.snapshot();
                (s.outputs
                    .get(1)
                    .is_some_and(|v| v.audio == "src:route_boot_b"))
                .then_some(s)
            });
            *record.lock().unwrap() = Some((o.tick, s));
        },
    );
    let (tick, s) = seen.lock().unwrap().take().expect("observed");
    assert!(
        tick < 30,
        "the boot take applies within a few ticks: {tick}"
    );
    assert_eq!(s.outputs[1].bus, "program", "the video is untouched");
    assert_eq!(s.outputs[1].audio_choice, "src:route_boot_b");
    assert_eq!(s.outputs[0].audio, "program");
    assert_eq!(run.frames, TICKS);
}

/// PR #352 round 2: an audio-only boot take (no combined video) whose
/// output's config id is 300 bytes long is still enqueued — not rejected by
/// `try_take`'s length check, which the OLD `boot:audio:<config id>` id
/// would hit for an id this long. `boot_audio_choices` parses the long id
/// faithfully (it is not truncated at parse time); only the later mint, now
/// keyed on the output index, must stay short.
#[test]
fn an_audio_only_boot_take_with_a_300_byte_config_id_is_enqueued() {
    const TICKS: u64 = 120;
    let long_id = "b".repeat(300);
    let template = AUDIO_CONFIG.replace("id = \"out1\"", &format!("id = \"{long_id}\""))
        + &format!("[[routing.output]]\noutput = \"{long_id}\"\naudio = {{ source = \"@_b\" }}\n");
    let seen = Arc::new(Mutex::new(None));
    let record = Arc::clone(&seen);
    let prep = |pipeline: &mut Pipeline| {
        let takes = pipeline.boot_audio_takes_mut();
        assert_eq!(takes.len(), 1, "one persisted audio feed");
        assert_eq!(
            takes[0].0.len(),
            300,
            "boot_audio_choices preserves the long id, unparsed/untruncated"
        );
        // The harness binds its sinks as `test-sink-N` (positional); the
        // real run would resolve the 300-byte id to its own output index the
        // same way.
        takes[0].0 = "test-sink-1".to_owned();
    };
    let run = drive_prepared(
        &template,
        true,
        false,
        "route_bootlongaudio",
        TICKS,
        prep,
        move |slot| {
            let handle = wait_for("the routing handle", Duration::from_secs(10), || {
                slot.load()
            });
            let outcomes = handle.outcomes().expect("one outcome reader");
            let o = wait_for("the boot take outcome", Duration::from_secs(10), || {
                outcomes.try_recv().ok()
            });
            // Short and fixed, keyed on the output index: never a truncated
            // 300-byte id, and never refused for being too long.
            assert_eq!(o.operation_id, "boot:audio:1");
            assert!(o.operation_id.len() < super::MAX_TAKE_ID_BYTES, "{o:?}");
            assert_eq!(o.outcome, TakeResult::Applied, "{o:?}");
            *record.lock().unwrap() = Some(o);
        },
    );
    let o = seen.lock().unwrap().take().expect("observed");
    assert_eq!(
        o.audio,
        Some(super::AudioChoice::Source(
            "route_bootlongaudio_b".to_owned()
        ))
    );
    assert_eq!(run.frames, TICKS);
}

/// #342 B3 end to end: two HLS outputs on the 64x64 canvas; out1 is reformatted
/// live to 32x64 cover. out0 keeps every tick on the base key (PTS 0..T, one
/// per tick); out1's PTS stays one per tick across the move (make-before-
/// break); its decoded size is 64x64 before and 32x64 after; the move is one
/// flagged decoder-config change (a parameter-set change counted by the
/// router) on out1 only.
#[test]
fn a_live_reformat_resets_one_output_and_leaves_the_other_untouched() {
    const TICKS: u64 = 300;
    let applied_at = Arc::new(Mutex::new(None::<u64>));
    let record = Arc::clone(&applied_at);
    // CONFIG with out1 declaring the canvas format explicitly (identity:
    // the same base key; the `[outputs.format]` marker also makes the
    // harness record every output's video packets for decoding).
    let config = CONFIG.replace(
        "path = \"/tmp/multiview-routing-e2e/@/b/index.m3u8\"\ncodec = \"mpeg2video\"\nsegment_ms = 1000\n",
        "path = \"/tmp/multiview-routing-e2e/@/b/index.m3u8\"\ncodec = \"mpeg2video\"\nsegment_ms = 1000\n[outputs.format]\nwidth = 64\nheight = 64\n",
    );
    assert!(config.contains("[outputs.format]"));
    let run = drive_full(
        &config,
        false,
        false,
        "route_reformat",
        TICKS,
        move |slot| {
            let handle = wait_for("the routing handle", Duration::from_secs(10), || {
                slot.load()
            });
            let outcomes = handle.outcomes().expect("one outcome reader");
            wait_for("the first routing tick", Duration::from_secs(10), || {
                (handle.snapshot().video_keys > 0).then_some(())
            });
            std::thread::sleep(Duration::from_millis(600));
            let cover = crate::routing_aux::AuxFormat::new(
                32,
                64,
                multiview_config::OutputFit::new(
                    multiview_config::OutputFitMode::Cover,
                    multiview_config::Anchor::Center,
                ),
            );
            handle
                .try_reformat(super::Reformat {
                    operation_id: "rf".to_owned(),
                    output_index: 1,
                    settings: super::ReformatSettings {
                        codec_name: "mpeg2video".to_owned(),
                        width: 32,
                        height: 64,
                        bit_rate: 1_000_000,
                        base_format: Some(cover),
                    },
                    feed_format: Some(cover),
                    principal: "operator".to_owned(),
                })
                .unwrap();
            let o = wait_for("the reformat outcome", Duration::from_secs(10), || {
                outcomes.try_recv().ok()
            });
            assert_eq!(o.operation_id, "rf");
            assert_eq!(o.outcome, TakeResult::ResetApplied, "{o:?}");
            *record.lock().unwrap() = Some(o.tick);
        },
    );
    let at = applied_at.lock().unwrap().expect("applied");
    assert_eq!(run.frames, TICKS);
    let all: Vec<i64> = (0..i64::try_from(TICKS).unwrap()).collect();
    assert_eq!(
        pts_of(&run.outputs[0]),
        all,
        "out0: one frame per tick, untouched"
    );
    // out1 ends the run on its live-opened key: the router retires live
    // keys at end of run before their last in-flight windows close (the
    // same for any routed output still on a live key), so its stream is a
    // contiguous prefix of the ticks, reaching past the move by far.
    let out1 = pts_of(&run.outputs[1]);
    assert_eq!(
        out1[..],
        all[..out1.len()],
        "out1: one frame per tick across the reset"
    );
    assert!(
        out1.len() + 3 >= all.len() && out1.len() > usize::try_from(at).unwrap() + 100,
        "out1 kept emitting after the move: {} of {}",
        out1.len(),
        all.len()
    );
    assert_eq!(
        run.final_snapshot.parameter_set_changes, 1,
        "one flagged change"
    );
    // out1's first packet at the move is a keyframe with a sequence header.
    let at_pkt = run.outputs[1]
        .iter()
        .find(|(pts, _, _)| *pts == i64::try_from(at).unwrap())
        .unwrap();
    assert!(
        at_pkt.1 && at_pkt.2,
        "the move lands on a self-contained IDR"
    );
    // Decoded sizes: before the move the canvas, after it the new format.
    let at_index = run.video[1]
        .iter()
        .position(|p| p.pts() == Some(i64::try_from(at).unwrap()))
        .unwrap();
    assert_eq!(decode_size(&run.video[1][..at_index]), Some((64, 64)));
    assert_eq!(decode_size(&run.video[1][at_index..]), Some((32, 64)));
    assert_eq!(decode_size(&run.video[0]), Some((64, 64)));
}

/// Review INV-1 through the real pipeline: while the run is live its
/// subtitle routing is live; when the run returns, its published handle's
/// subtitle routing is ended (a stale take is refused, no live readback),
/// although the handle itself stays in the slot.
#[test]
fn a_run_ends_its_subtitle_routing_when_it_returns() {
    use multiview_control::output_routing::{
        OutputRouting, SubtitleTakeError, SubtitleTakeRequest,
    };
    let seen_live = Arc::new(AtomicBool::new(false));
    let record = Arc::clone(&seen_live);
    let kept: Arc<Mutex<Option<OutputRoutingSlot>>> = Arc::new(Mutex::new(None));
    let keep = Arc::clone(&kept);
    let run = drive("sub_run_end", 30, move |slot| {
        let handle = wait_for("the routing handle", Duration::from_secs(10), || {
            slot.load()
        });
        record.store(handle.subtitles_live(), Ordering::Release);
        *keep.lock().unwrap() = Some(slot);
    });
    assert_eq!(run.frames, 30);
    assert!(seen_live.load(Ordering::Acquire), "live during the run");
    let slot = kept
        .lock()
        .unwrap()
        .take()
        .expect("the controller kept the slot");
    let handle = slot.load().expect("the handle stays published");
    assert!(!handle.subtitles_live(), "ended with the run");
    assert_eq!(
        handle.repoint_subtitles("any", &[(0, None)]),
        Err(super::SubtitleRepoint::Ended)
    );
    let adapter = crate::output_routing_control::OutputRoutingAdapter::new(slot);
    assert_eq!(
        adapter.try_take_subtitles(SubtitleTakeRequest {
            operation_id: "late".to_owned(),
            output: "any".to_owned(),
            slots: vec![(0, None)],
            principal: "p".to_owned(),
        }),
        Err(SubtitleTakeError::Busy)
    );
}

/// What the teardown hook observed: (live, table unchanged, take, readback, table).
type TeardownSeen = (
    bool,
    bool,
    Result<u64, multiview_control::output_routing::SubtitleTakeError>,
    Option<Vec<Option<String>>>,
    Vec<Option<String>>,
);

/// Review INV2-1 through the real pipeline: teardown is paused after the
/// bake consumer exited (egress joined) and before ingest teardown and the
/// run's return. There, a subtitle take is `Busy`, the run's table is
/// unchanged and there is no live readback (GET /patch: `live: false`).
#[test]
fn a_subtitle_take_during_teardown_after_the_consumer_exited_is_busy() {
    use multiview_control::output_routing::{
        OutputRouting, SubtitleTakeError, SubtitleTakeRequest,
    };
    let template = CONFIG.replacen(
        "segment_ms = 1000\n[[outputs]]",
        "segment_ms = 1000\n[[outputs.slots.subtitle]]\nlanguage = \"en\"\nsource = \"@_a\"\n[[outputs]]",
        1,
    );
    assert_ne!(template, CONFIG, "out0 declares a subtitle slot");
    let seen: Arc<Mutex<Option<TeardownSeen>>> = Arc::new(Mutex::new(None));
    let record = Arc::clone(&seen);
    let live_during_run = Arc::new(AtomicBool::new(false));
    let live_flag = Arc::clone(&live_during_run);
    let run = drive_prepared(
        &template,
        false,
        false,
        "sub_teardown",
        30,
        move |pipeline| {
            let slot = pipeline.output_routing();
            pipeline.set_teardown_hook(Box::new(move || {
                let handle = slot.load().expect("the run published its handle");
                let routes = handle
                    .subtitle_routes_for_test("out0")
                    .expect("out0 has a route table");
                let before = routes.load().to_vec();
                let adapter =
                    crate::output_routing_control::OutputRoutingAdapter::new(Arc::clone(&slot));
                let take = adapter.try_take_subtitles(SubtitleTakeRequest {
                    operation_id: "late".to_owned(),
                    output: "out0".to_owned(),
                    slots: vec![(0, None)],
                    principal: "p".to_owned(),
                });
                let after = routes.load().to_vec();
                *record.lock().unwrap() = Some((
                    handle.subtitles_live(),
                    before == after,
                    take,
                    adapter.subtitle_routes("out0"),
                    after,
                ));
            }));
        },
        move |slot| {
            let handle = wait_for("the routing handle", Duration::from_secs(10), || {
                slot.load()
            });
            live_flag.store(
                handle.subtitles_live() && handle.subtitle_table("out0").is_some(),
                Ordering::Release,
            );
        },
    );
    assert_eq!(run.frames, 30);
    assert!(
        live_during_run.load(Ordering::Acquire),
        "live during the run"
    );
    let (live, unchanged, take, readback, table) =
        seen.lock().unwrap().take().expect("the teardown hook ran");
    assert!(!live, "admission ended before the run returned");
    assert_eq!(take, Err(SubtitleTakeError::Busy));
    assert!(unchanged, "the ended run's table is untouched: {table:?}");
    assert_eq!(table, vec![Some("sub_teardown_a".to_owned())]);
    assert_eq!(readback, None, "no live readback (GET /patch live:false)");
}

/// [`LAYOUT_CONFIG`] at 128x96 so a routed layout's overlay chrome fits
/// (64x96 cells; cells under 48 px draw none) while an unoptimised test
/// build still bakes at cadence and three outputs: output 0 stays on Program.
#[cfg(feature = "overlay")]
fn overlay_layout_config() -> String {
    let three = LAYOUT_CONFIG
        .replace("width = 64\nheight = 64", "width = 128\nheight = 96")
        .replace("# keep-video\n", "");
    format!(
        "{three}[[outputs]]\nid = \"out2\"\nkind = \"hls\"\npath = \"/tmp/multiview-routing-e2e/@/c/index.m3u8\"\ncodec = \"mpeg2video\"\nsegment_ms = 1000\n"
    )
}

/// A 128x96 routed layout `id` with overlays on: both sources side by side.
#[cfg(feature = "overlay")]
fn overlay_layout(prefix: &str, id: &str) -> PictureBus {
    use multiview_core::layout::{Canvas, Cell, Layout};
    let cell = |x: f32, source: &str| Cell {
        x,
        w: 0.5,
        h: 1.0,
        source: Some(format!("{prefix}_{source}")),
        ..Cell::default()
    };
    PictureBus::Layout(crate::routing_aux::LayoutRef::new(Arc::new(
        crate::aux_layout::RoutedLayout {
            id: id.to_owned(),
            slates: vec![multiview_config::FailoverSlate::Black; 2],
            audio: multiview_config::LayoutAudio::default(),
            overlays: multiview_config::LayoutOverlays::default(),
            revision: crate::output_routing::next_layout_revision(),
            layout: Layout {
                name: id.to_owned(),
                canvas: Canvas {
                    width: 128,
                    height: 96,
                    fps_num: 25,
                    fps_den: 1,
                },
                cells: vec![cell(0.0, "b"), cell(0.5, "a")],
            },
        },
    )))
}

/// Program protection (R3, invariants #1/#10) through the real pipeline: two
/// routed layouts with every default overlay layer on, each on its own
/// output, while output 0 stays on Program. Output 0 gets every window with a
/// one-tick PTS step; both routed outputs receive frames; the aux thread
/// never fails. Count-based only (no timing assertions).
#[cfg(feature = "overlay")]
#[test]
fn two_routed_layouts_with_overlays_never_cost_program_a_window() {
    const TICKS: u64 = 250;
    let baked = Arc::new(Mutex::new(Vec::new()));
    let run = drive_runners(
        &overlay_layout_config(),
        "route_overlays",
        TICKS,
        3,
        (SendPolicy::BlockForExact, None),
        take_two_overlay_layouts("route_overlays", Arc::clone(&baked)),
    );
    assert_eq!(run.frames, TICKS);
    assert_eq!(run.hot_ticks, TICKS);
    assert_eq!(run.dropped, 0, "offline: nothing shed");
    let all: Vec<i64> = (0..i64::try_from(TICKS).unwrap()).collect();
    assert_eq!(
        pts_of(&run.outputs[0]),
        all,
        "output 0 (Program) gets every window, one tick apart"
    );
    for output in [1, 2] {
        let pts = pts_of(&run.outputs[output]);
        // One packet per tick with a one-tick PTS step across the move onto
        // the layout; the routed encoder's last frames may still be in
        // flight when the bounded run ends (count-based, no timing).
        assert_eq!(pts.first(), Some(&0), "routed output {output}");
        assert!(
            pts.windows(2).all(|w| w[1] == w[0] + 1),
            "routed output {output}: a one-tick PTS step"
        );
        assert!(
            pts.len() + 4 >= all.len(),
            "routed output {output} received frames to the end: {}",
            pts.len()
        );
    }
    assert_layouts_baked_and_aux_alive(&run, &baked.lock().unwrap());
}

/// Program protection under the LIVE policy (I1): the same two baking
/// layouts with [`SendPolicy::DropOnOverload`], the policy `run_until` uses,
/// so the hot loop never waits for the consumer. The clock emits every tick
/// (counted on the hot loop itself) whatever the consumer manages; every
/// tick is either delivered to Program's sink or shed and counted (never
/// lost, never blocked); both layouts demonstrably bake; aux never fails.
/// Count-based only: how many ticks a loaded runner sheds is not asserted,
/// only that each one is accounted for.
#[cfg(feature = "overlay")]
#[test]
fn two_baking_layouts_never_hold_the_live_clock() {
    const TICKS: u64 = 250;
    const SHED_AT: u64 = 200;
    let baked = Arc::new(Mutex::new(Vec::new()));
    let run = drive_runners(
        &overlay_layout_config(),
        "route_overlays_live",
        TICKS,
        3,
        // A deterministic shed of one tick while both layouts bake (the
        // seam drops the hot item before the consumer), on top of whatever a
        // loaded runner sheds by itself.
        (SendPolicy::DropOnOverload, Some(SHED_AT)),
        take_two_overlay_layouts("route_overlays_live", Arc::clone(&baked)),
    );
    assert_eq!(run.frames, TICKS, "the clock ran every tick");
    assert_eq!(
        run.hot_ticks, TICKS,
        "every tick was emitted on the hot loop, independent of the consumer"
    );
    let program = pts_of(&run.outputs[0]);
    let delivered = u64::try_from(program.len()).unwrap();
    assert!(
        program.windows(2).all(|w| w[1] > w[0]),
        "Program's PTS stay monotonic"
    );
    assert!(
        program
            .iter()
            .all(|p| (0..i64::try_from(TICKS).unwrap()).contains(p)),
        "every Program PTS is a tick of this run"
    );
    // Every tick reached Program's encoder, substituted by the consumer's
    // gap fill, or was shed and counted: none is unaccounted for.
    assert!(
        delivered + run.dropped >= TICKS,
        "delivered {delivered} + shed {} covers {TICKS} ticks",
        run.dropped
    );
    assert!(run.dropped <= TICKS, "drops are bounded by the ticks");
    // The seam-shed tick (while both layouts bake) is substituted on
    // Program, not lost: the consumer restamps its last good canvas.
    assert!(
        program.contains(&i64::try_from(SHED_AT).unwrap()),
        "the shed tick {SHED_AT} is substituted on Program"
    );
    assert_layouts_baked_and_aux_alive(&run, &baked.lock().unwrap());
}

/// Both layouts carried baked frames and neither output fell back (an aux
/// failure moves every routed output to Program, §4.4).
#[cfg(feature = "overlay")]
fn assert_layouts_baked_and_aux_alive(run: &Run, baked: &[(String, u64, u64)]) {
    assert_eq!(baked.len(), 2, "{baked:?}");
    for (bus, composed, baked_frames) in baked {
        assert!(*composed > 0, "{bus} composed");
        assert!(
            *baked_frames > 0,
            "{bus} baked overlays ({baked_frames} of {composed})"
        );
    }
    let end = &run.final_snapshot.outputs;
    assert_eq!(end[1].bus, "aux:news-a", "aux never failed");
    assert_eq!(end[2].bus, "aux:news-b", "aux never failed");
}

/// The controller: take output 1 to `news-a` and output 2 to `news-b` (both
/// with overlays), then, once both have composed for a while, record each
/// layout feed's (bus, composed, baked) counters into `baked`.
#[cfg(feature = "overlay")]
fn take_two_overlay_layouts(
    prefix: &'static str,
    baked: Arc<Mutex<Vec<(String, u64, u64)>>>,
) -> impl FnOnce(OutputRoutingSlot) + Send + 'static {
    move |slot| {
        let handle = wait_for("the routing handle", Duration::from_secs(10), || {
            slot.load()
        });
        let outcomes = handle.outcomes().expect("one outcome reader");
        wait_for("the first routing tick", Duration::from_secs(10), || {
            (handle.snapshot().video_keys > 0).then_some(())
        });
        std::thread::sleep(Duration::from_millis(400));
        for (op, output, id) in [("l1", 1, "news-a"), ("l2", 2, "news-b")] {
            handle
                .try_take(take(op, output, overlay_layout(prefix, id)))
                .unwrap();
            let o = wait_for("layout outcome", Duration::from_secs(10), || {
                outcomes.try_recv().ok()
            });
            assert_eq!(o.outcome, TakeResult::Applied, "{o:?}");
        }
        let s = wait_for("both layouts routed", Duration::from_secs(2), || {
            let s = handle.snapshot();
            (s.outputs[1].bus == "aux:news-a" && s.outputs[2].bus == "aux:news-b").then_some(s)
        });
        assert_eq!(s.aux_canvases.len(), 2, "{:?}", s.aux_canvases);
        // Count-based: wait (bounded) until both layouts have baked frames.
        let counters = wait_for("both layouts bake", Duration::from_secs(20), || {
            let feeds = handle.aux_feeds.load();
            let rows: Vec<(String, u64, u64)> = feeds
                .iter()
                .filter(|f| matches!(f.bus, PictureBus::Layout(_)))
                .map(|f| {
                    (
                        f.key.to_string(),
                        f.slot.composed.load(Ordering::Relaxed),
                        f.slot.baked.load(Ordering::Relaxed),
                    )
                })
                .collect();
            (rows.len() == 2 && rows.iter().all(|r| r.2 > 0)).then_some(rows)
        });
        *baked.lock().unwrap() = counters;
    }
}
