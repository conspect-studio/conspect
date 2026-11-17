#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_cli::pipeline::Pipeline;
use multiview_cli::preview::{program_slot, ProgramAudioSlot};
use multiview_config::MultiviewConfig;
use multiview_control::EngineStateSnapshot;
use multiview_core::switcher::SourceRef;
use multiview_engine::switcher::SwitcherState;
use multiview_engine::switcher_control::{
    ScenePair, SwitcherCommand, SwitcherControl, SwitcherVerb,
};
use multiview_engine::{EnginePublisher, StopSignal};
use multiview_events::Event;
use std::sync::Arc;
use std::time::Duration;

struct StopGuard(StopSignal);
impl Drop for StopGuard {
    fn drop(&mut self) {
        self.0.stop();
    }
}

// Coherent Fourier energy: 40ms blocks contain integer cycles of both tones.
fn energy(samples: &[f32], hz: f64) -> f64 {
    let mut re = 0.0;
    let mut im = 0.0;
    for (i, frame) in samples.chunks_exact(2).enumerate() {
        let phase = std::f64::consts::TAU
            * hz
            * f64::from(u16::try_from(i).expect("40ms fixture block fits u16"))
            / 48_000.0;
        re += f64::from(frame[0]) * phase.cos();
        im += f64::from(frame[0]) * phase.sin();
    }
    re * re + im * im
}

async fn assert_tone(tap: &ProgramAudioSlot, wanted: f64, absent: f64) {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut consecutive = 0;
        loop {
            while let Some(block) = tap.pop() {
                let yes = energy(block.interleaved(), wanted);
                let no = energy(block.interleaved(), absent);
                if yes > 1.0 && yes > 100.0 * no {
                    consecutive += 1;
                } else {
                    consecutive = 0;
                }
                if consecutive == 3 {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("three consecutive actual PCM blocks contain only selected tone");
}

// Keep the real A/V fixture alive for the entire pipeline run.
fn real_av_pipeline() -> (tempfile::TempDir, Pipeline) {
    let dir = tempfile::tempdir().unwrap();
    let clip = dir.path().join("preview.ts");
    let generated = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=c=white:s=64x64:r=25:d=30",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=400:sample_rate=48000:duration=30",
            "-c:v",
            "mpeg2video",
            "-c:a",
            "aac",
            "-shortest",
            "-f",
            "mpegts",
        ])
        .arg(&clip)
        .status()
        .expect("fixture ffmpeg runs");
    assert!(generated.success(), "real A/V fixture generation succeeds");
    let config = format!(
        r##"schema_version=1
[canvas]
width=64
height=64
fps="25/1"
pixel_format="nv12"
background="#101014"
[canvas.color]
profile="sdr-bt709-limited"
[layout]
kind="grid"
columns=["1fr"]
rows=["1fr"]
areas=["a"]
[[sources]]
id="bars"
kind="bars"
[[sources]]
id="clip"
kind="file"
path="{}"
activation="on_demand"
[[cells]]
id="pgm"
area="a"
fit="fill"
[cells.source]
input_id="bars"
[[outputs]]
kind="hls"
path="{}"
codec="mpeg2video"
segment_ms=1000
[switcher]
[[switcher.mix_effects]]
id="me1"
program="main"
program_source="bars"
preview_source="clip"
"##,
        clip.display(),
        dir.path().join("index.m3u8").display()
    );
    let cfg = MultiviewConfig::load_from_toml(&config).unwrap();
    cfg.validate().unwrap();
    let pipeline = Pipeline::build(&cfg).unwrap();
    (dir, pipeline)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn real_cut_follows_program_audio_and_never_mixes_preview() {
    let (_fixture, mut pipeline) = real_av_pipeline();
    pipeline.enable_program_audio();
    let tap = ProgramAudioSlot::new();
    pipeline.set_program_audio_preview(tap.clone());
    let preview = program_slot();
    let pvw = program_slot();
    let (handle, driver) = SwitcherControl::new(SwitcherState::new(
        SourceRef::new("bars"),
        SourceRef::new("clip"),
        vec![],
    ));
    handle
        .set_scenes(ScenePair {
            version: 1,
            program: pipeline.switcher_program_layout(),
            preview: pipeline.switcher_preview_layout("clip"),
        })
        .unwrap();
    pipeline
        .attach_switcher(handle.clone(), driver, Arc::clone(&pvw))
        .unwrap();
    let stop = StopSignal::new();
    let watchdog_stop = stop.clone();
    let watchdog = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(25)).await;
        watchdog_stop.stop();
    });
    let relay_stop = stop.clone();
    let pgm = Arc::clone(&preview);
    let relay = tokio::spawn(async move {
        let _stop = StopGuard(relay_stop);
        tokio::time::timeout(Duration::from_secs(8), async {
            while !handle.is_ready() || pvw.load().is_none() || pgm.load().is_none() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("real pair primes");
        assert_tone(&tap, 1000.0, 400.0).await;
        let before = pgm.load().unwrap().sample(8, 8).unwrap().0;
        let mut outcomes = handle.outcomes();
        for (index, wanted, absent) in [(0, 400.0, 1000.0), (1, 1000.0, 400.0), (2, 400.0, 1000.0)]
        {
            while tap.pop().is_some() {}
            handle
                .try_send(SwitcherCommand {
                    operation_id: format!("audio-cut-{index}"),
                    scene_version: 1,
                    verb: SwitcherVerb::Cut,
                    expect: None,
                })
                .unwrap();
            let done = tokio::time::timeout(Duration::from_secs(4), outcomes.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(done.event.completed, "CUT reaches an actual composed frame");
            assert_tone(&tap, wanted, absent).await;
            let after = pgm.load().unwrap().sample(8, 8).unwrap().0;
            if index % 2 == 0 {
                assert_ne!(before, after, "video follows same CUT");
            } else {
                assert_eq!(before, after, "flip-flop restores original picture");
            }
        }
    });
    let publisher = EnginePublisher::<EngineStateSnapshot, Event>::new(64);
    let result = pipeline
        .run_until_serving(&stop, &publisher, &preview, |_| {})
        .await;
    watchdog.abort();
    relay.await.expect("audio assertions pass");
    assert!(!result.unwrap().faltered);
}
