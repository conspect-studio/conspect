//! Display-audio sink seam tests (DEV-B4): the whole ALSA HDMI audio sink run
//! loop over the `EldSource` + `AlsaSink` trait seams with scripted mocks —
//! proving, WITHOUT hardware or `/proc`: a valid ELD lights the audio path and
//! drains the FIFO into the PCM through the servo; an **EDID-less head (no ELD)
//! stays silent and never crashes**; a wedged/erroring PCM recovers and holds
//! rather than faltering; and the engine-side push never blocks on the sink
//! (invariants #1 + #10).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use multiview_audio::{AudioFormat, ChannelLayout};
use multiview_output::display::audio::{
    AlsaSink, DisplayAudioConfig, DisplayAudioSink, EldCapability, EldSource, PcmOutcome, PcmParams,
};

// --- A scripted ELD source: returns whatever capability the test sets. ---
#[derive(Clone, Default)]
struct MockEld {
    cap: Arc<Mutex<Option<EldCapability>>>,
}
impl EldSource for MockEld {
    fn read_capability(&mut self) -> Option<EldCapability> {
        self.cap.lock().expect("poisoned").clone()
    }
}

// --- A scripted ALSA PCM: records frames written, can be told to xrun once. ---
#[derive(Clone, Default)]
struct MockAlsa {
    frames_written: Arc<AtomicU64>,
    opened: Arc<AtomicBool>,
    xrun_once: Arc<AtomicBool>,
    recovered: Arc<AtomicU64>,
}
impl AlsaSink for MockAlsa {
    fn open(&mut self, _params: PcmParams) -> Result<(), String> {
        self.opened.store(true, Ordering::SeqCst);
        Ok(())
    }
    fn write(&mut self, interleaved: &[f32], channels: usize) -> PcmOutcome {
        if self.xrun_once.swap(false, Ordering::SeqCst) {
            return PcmOutcome::Underrun;
        }
        let frames = interleaved.len() / channels.max(1);
        self.frames_written
            .fetch_add(frames as u64, Ordering::SeqCst);
        PcmOutcome::Wrote(frames)
    }
    fn recover(&mut self) -> PcmOutcome {
        self.recovered.fetch_add(1, Ordering::SeqCst);
        PcmOutcome::Recovered
    }
    fn close(&mut self) {}
}

fn cfg() -> DisplayAudioConfig {
    DisplayAudioConfig {
        output_id: "out-display".to_owned(),
        format: AudioFormat::new(48_000, ChannelLayout::Stereo),
        fifo_capacity_frames: 8_192,
        poll_interval: Duration::from_millis(2),
    }
}

#[test]
fn no_eld_means_silent_not_crash() {
    // EDID-less head: the ELD source yields None. The sink must run (no panic),
    // never open the PCM, and report "no audio path" — the documented field
    // condition. The engine-side push still succeeds (never blocks).
    let eld = MockEld::default(); // cap = None
    let alsa = MockAlsa::default();
    let (sink, publisher) = DisplayAudioSink::start(cfg(), eld, alsa.clone());

    // The engine pushes a few blocks; they are accepted (dropped into the FIFO,
    // never blocking) even though no audio is flowing out.
    for _ in 0..10 {
        publisher.push_block(&vec![0.1f32; 480 * 2]);
    }
    std::thread::sleep(Duration::from_millis(40));
    let stats = sink.stats();
    assert!(!stats.audio_active, "no ELD => audio path inactive");
    assert!(
        !alsa.opened.load(Ordering::SeqCst),
        "no ELD => PCM is never opened"
    );
    sink.stop();
}

#[test]
fn valid_eld_drains_the_fifo_into_the_pcm() {
    let eld = MockEld::default();
    *eld.cap.lock().unwrap() = Some(EldCapability::lpcm(2, &[48_000], "MON"));
    let alsa = MockAlsa::default();
    let (sink, publisher) = DisplayAudioSink::start(cfg(), eld, alsa.clone());

    for _ in 0..50 {
        publisher.push_block(&vec![0.2f32; 480 * 2]);
        std::thread::sleep(Duration::from_millis(1));
    }
    std::thread::sleep(Duration::from_millis(60));
    assert!(
        alsa.opened.load(Ordering::SeqCst),
        "valid ELD opens the PCM"
    );
    assert!(
        alsa.frames_written.load(Ordering::SeqCst) > 0,
        "valid ELD must drain audio frames into the PCM"
    );
    assert!(sink.stats().audio_active, "valid ELD => audio active");
    sink.stop();
}

#[test]
fn xrun_recovers_and_keeps_running() {
    let eld = MockEld::default();
    *eld.cap.lock().unwrap() = Some(EldCapability::lpcm(2, &[48_000], "MON"));
    let alsa = MockAlsa::default();
    alsa.xrun_once.store(true, Ordering::SeqCst); // first write underruns
    let (sink, publisher) = DisplayAudioSink::start(cfg(), eld, alsa.clone());

    for _ in 0..50 {
        publisher.push_block(&vec![0.2f32; 480 * 2]);
        std::thread::sleep(Duration::from_millis(1));
    }
    std::thread::sleep(Duration::from_millis(60));
    assert!(
        alsa.recovered.load(Ordering::SeqCst) >= 1,
        "the underrun must have triggered a recover()"
    );
    assert!(
        alsa.frames_written.load(Ordering::SeqCst) > 0,
        "after recovery, audio resumes (never falters)"
    );
    sink.stop();
}

#[test]
fn engine_push_never_blocks_even_with_a_wedged_pcm() {
    // A PCM that never accepts a write (always underruns) must NOT back-pressure
    // the engine: the push path is wait-free and bounded, so a flood of blocks
    // returns immediately and the FIFO stays bounded (invariants #1 + #10).
    #[derive(Clone, Default)]
    struct WedgedAlsa;
    impl AlsaSink for WedgedAlsa {
        fn open(&mut self, _p: PcmParams) -> Result<(), String> {
            Ok(())
        }
        fn write(&mut self, _i: &[f32], _c: usize) -> PcmOutcome {
            PcmOutcome::Underrun
        }
        fn recover(&mut self) -> PcmOutcome {
            PcmOutcome::RecoverFailed
        }
        fn close(&mut self) {}
    }
    let eld = MockEld::default();
    *eld.cap.lock().unwrap() = Some(EldCapability::lpcm(2, &[48_000], "MON"));
    let (sink, publisher) = DisplayAudioSink::start(cfg(), eld, WedgedAlsa);

    let start = std::time::Instant::now();
    for _ in 0..10_000 {
        publisher.push_block(&vec![0.2f32; 480 * 2]); // 10k blocks, way past FIFO cap
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "engine-side push must be wait-free; flood took {elapsed:?}"
    );
    assert!(
        publisher.dropped_frames() > 0,
        "a wedged sink must DROP (bounded FIFO), never grow/block"
    );
    sink.stop();
}
