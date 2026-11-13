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
    drive_hooked(template, audio, switcher, prefix, ticks, control, None)
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
    let config =
        multiview_config::MultiviewConfig::load_from_toml(&template.replace('@', prefix)).unwrap();
    let mut pipeline = Pipeline::build(&config).unwrap();
    if let Some(hook) = convert_hook {
        pipeline.set_convert_hook(hook);
    }
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
    let seen: Vec<Seen> = (0..2).map(|_| Arc::new(Mutex::new(Vec::new()))).collect();
    let seen_audio: Vec<SeenAudio> = (0..2).map(|_| Arc::new(Mutex::new(Vec::new()))).collect();
    let seen_video: Vec<SeenVideo> = (0..2).map(|_| Arc::new(Mutex::new(Vec::new()))).collect();
    let runners: Vec<TestSinkRunner> = seen
        .iter()
        .zip(&seen_audio)
        .zip(&seen_video)
        .map(|((s, a), v)| {
            if keep_video {
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
