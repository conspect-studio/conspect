//! Channel-fold tests for the display-audio sink (DEV-B4 / display-out §5).
//!
//! ELD `negotiate` clamps the channel ask DOWN to the sink's ceiling ("never
//! assumed" — brief §5: multichannel is whatever the ELD declares). When the
//! program bus is stereo but the monitor declares mono-only LPCM, the sink must
//! open a 1-channel PCM and **fold** the stereo program into it (equal-gain
//! average via multiview-audio's `ChannelMatrix`) — not write stereo-interleaved
//! samples into a mono device (which would play at half speed as garbage).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::cast_possible_truncation
)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use multiview_audio::{AudioFormat, ChannelLayout};
use multiview_output::display::audio::{
    AlsaSink, DisplayAudioConfig, DisplayAudioSink, EldCapability, EldSource, PcmOutcome, PcmParams,
};

#[derive(Clone)]
struct FixedEld(EldCapability);
impl EldSource for FixedEld {
    fn read_capability(&mut self) -> Option<EldCapability> {
        Some(self.0.clone())
    }
}

/// Records every sample written plus the channel counts the loop used.
#[derive(Clone, Default)]
struct RecordingAlsa {
    samples: Arc<Mutex<Vec<f32>>>,
    open_params: Arc<Mutex<Option<PcmParams>>>,
    write_channels: Arc<AtomicUsize>,
}
impl AlsaSink for RecordingAlsa {
    fn open(&mut self, params: PcmParams) -> Result<(), String> {
        *self.open_params.lock().expect("poisoned") = Some(params);
        Ok(())
    }
    fn write(&mut self, interleaved: &[f32], channels: usize) -> PcmOutcome {
        self.write_channels.store(channels, Ordering::SeqCst);
        self.samples
            .lock()
            .expect("poisoned")
            .extend_from_slice(interleaved);
        PcmOutcome::Wrote(interleaved.len() / channels.max(1))
    }
    fn recover(&mut self) -> PcmOutcome {
        PcmOutcome::Recovered
    }
    fn close(&mut self) {}
}

#[test]
fn stereo_program_folds_to_a_mono_only_eld() {
    // Monitor declares 1-channel LPCM; program is stereo L=0.4 / R=0.0. The PCM
    // must open mono and every real written sample must be the equal-gain fold
    // 0.5·L + 0.5·R = 0.2 (zero-filled FIFO underrun stretches are silence).
    let eld = FixedEld(EldCapability::lpcm(1, &[48_000], "MONO"));
    let alsa = RecordingAlsa::default();
    let cfg = DisplayAudioConfig {
        output_id: "out-display".to_owned(),
        format: AudioFormat::new(48_000, ChannelLayout::Stereo),
        fifo_capacity_frames: 8_192,
        poll_interval: Duration::from_millis(2),
    };
    let (sink, publisher) = DisplayAudioSink::start(cfg, eld, alsa.clone());

    let mut block = Vec::with_capacity(480 * 2);
    for _ in 0..480 {
        block.push(0.4f32); // L
        block.push(0.0f32); // R
    }
    for _ in 0..50 {
        publisher.push_block(&block);
        std::thread::sleep(Duration::from_millis(1));
    }
    std::thread::sleep(Duration::from_millis(60));
    sink.stop();

    let params = alsa
        .open_params
        .lock()
        .expect("poisoned")
        .expect("PCM must have been opened");
    assert_eq!(params.channels, 1, "mono ELD => the PCM opens 1-channel");
    assert_eq!(
        alsa.write_channels.load(Ordering::SeqCst),
        1,
        "writes must be mono-interleaved"
    );
    let samples = alsa.samples.lock().expect("poisoned");
    let real: Vec<f32> = samples.iter().copied().filter(|s| s.abs() > 1e-6).collect();
    assert!(
        !real.is_empty(),
        "some real (non-silence) audio must have been written"
    );
    // The adaptive resampler linearly interpolates across the silence↔content
    // seams, so a bounded few samples lie BETWEEN 0 and the fold value; every
    // real sample must stay inside that hull (an unfolded stereo write would
    // show L=0.4 samples, outside it) and the bulk must be the exact fold.
    assert!(
        real.iter().all(|s| (-1e-3..=0.2 + 1e-3).contains(s)),
        "every real mono sample must lie within the silence..fold hull [0, 0.2]"
    );
    let exact = real.iter().filter(|s| (*s - 0.2).abs() < 1e-3).count();
    assert!(
        exact * 10 >= real.len() * 9,
        "at least 90% of real mono samples must be the exact 0.5/0.5 fold of \
         L=0.4/R=0.0 (=0.2); got {exact}/{}",
        real.len()
    );
}

#[test]
fn matching_channel_counts_pass_through_unfolded() {
    // Stereo ELD + stereo program: samples reach the PCM unmodified (constant
    // per-channel content survives the unity-ratio resample exactly).
    let eld = FixedEld(EldCapability::lpcm(2, &[48_000], "ST"));
    let alsa = RecordingAlsa::default();
    let cfg = DisplayAudioConfig {
        output_id: "out-display".to_owned(),
        format: AudioFormat::new(48_000, ChannelLayout::Stereo),
        fifo_capacity_frames: 8_192,
        poll_interval: Duration::from_millis(2),
    };
    let (sink, publisher) = DisplayAudioSink::start(cfg, eld, alsa.clone());
    let mut block = Vec::with_capacity(480 * 2);
    for _ in 0..480 {
        block.push(0.4f32);
        block.push(-0.3f32);
    }
    for _ in 0..50 {
        publisher.push_block(&block);
        std::thread::sleep(Duration::from_millis(1));
    }
    std::thread::sleep(Duration::from_millis(60));
    sink.stop();

    assert_eq!(alsa.write_channels.load(Ordering::SeqCst), 2);
    let samples = alsa.samples.lock().expect("poisoned");
    // Real (non-silence) frames carry the stereo pattern; the resampler's
    // silence↔content seam interpolation may scale a bounded few frames toward
    // zero, so every real frame must lie on the 0..pattern hull per channel (a
    // fold or channel swap would leave it) and the bulk must be exact.
    let mut real = 0usize;
    let mut exact = 0usize;
    for frame in samples.chunks_exact(2) {
        if frame[0].abs() > 1e-6 || frame[1].abs() > 1e-6 {
            real += 1;
            assert!(
                (-1e-3..=0.4 + 1e-3).contains(&frame[0])
                    && (-0.3 - 1e-3..=1e-3).contains(&frame[1]),
                "stereo frames must stay on the silence..content hull, got {frame:?}"
            );
            if (frame[0] - 0.4).abs() < 1e-3 && (frame[1] + 0.3).abs() < 1e-3 {
                exact += 1;
            }
        }
    }
    assert!(real > 0, "some real audio must have been written");
    assert!(
        exact * 10 >= real * 9,
        "at least 90% of real stereo frames must pass through exactly; got {exact}/{real}"
    );
}
