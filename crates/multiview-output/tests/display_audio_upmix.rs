//! Mono-program → HDMI upmix tests (DEV-B4 adversarial review MEDIUM-4).
//!
//! HDMI/DP LPCM rides IEC 60958 subframe PAIRS: a sink's PCM never takes fewer
//! than two channels, even when the ELD's channel ceiling is higher and the
//! program is mono. The pre-fix negotiation only ever clamped DOWN, so a mono
//! program asked the device for a 1-channel PCM, the device refused, and the
//! head ran silent. Negotiation must raise a below-minimum ask to the stereo
//! minimum (never past the ELD ceiling) and the sink must UPMIX — mono
//! duplicated onto both channels — at the write boundary.
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

#[test]
fn negotiate_raises_mono_to_the_hdmi_stereo_minimum() {
    // A mono ask against a multichannel ELD must come back as 2 channels (the
    // HDMI LPCM minimum), not 1 (which the device would refuse → silence).
    let cap = EldCapability::lpcm(8, &[48_000], "MON");
    assert_eq!(
        cap.negotiate(48_000, 1),
        Some((48_000, 2)),
        "mono must be raised to the HDMI two-channel minimum"
    );
    // The minimum never exceeds the ELD ceiling: a (degenerate) mono-only
    // capability still negotiates 1 — we never ask a sink for more than it
    // declared.
    let mono_only = EldCapability::lpcm(1, &[48_000], "MONO");
    assert_eq!(mono_only.negotiate(48_000, 1), Some((48_000, 1)));
    // At-or-above-minimum asks are unchanged (clamping down still applies).
    assert_eq!(cap.negotiate(48_000, 2), Some((48_000, 2)));
    assert_eq!(
        EldCapability::lpcm(2, &[48_000], "ST").negotiate(48_000, 8),
        Some((48_000, 2))
    );
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

#[derive(Clone)]
struct FixedEld(EldCapability);
impl EldSource for FixedEld {
    fn read_capability(&mut self) -> Option<EldCapability> {
        Some(self.0.clone())
    }
}

#[test]
fn mono_program_upmixes_duplicated_into_a_two_channel_pcm() {
    // Mono program bus, stereo-capable ELD: the PCM must open with 2 channels
    // and every written frame must carry the mono sample duplicated L==R.
    let eld = FixedEld(EldCapability::lpcm(2, &[48_000], "ST"));
    let alsa = RecordingAlsa::default();
    let cfg = DisplayAudioConfig {
        output_id: "out-display".to_owned(),
        format: AudioFormat::new(48_000, ChannelLayout::Mono),
        fifo_capacity_frames: 8_192,
        poll_interval: Duration::from_millis(2),
    };
    let (sink, publisher) = DisplayAudioSink::start(cfg, eld, alsa.clone());

    let block = vec![0.25f32; 480];
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
    assert_eq!(
        params.channels, 2,
        "a mono program against a stereo-capable ELD must open a 2-channel PCM \
         (a 1-channel ask is refused by real HDMI devices)"
    );
    assert_eq!(
        alsa.write_channels.load(Ordering::SeqCst),
        2,
        "writes must be stereo-interleaved"
    );
    let samples = alsa.samples.lock().expect("poisoned");
    assert_eq!(samples.len() % 2, 0, "whole stereo frames only");
    let mut real = 0usize;
    let mut exact = 0usize;
    for frame in samples.chunks_exact(2) {
        if frame[0].abs() > 1e-6 || frame[1].abs() > 1e-6 {
            real += 1;
            assert!(
                (frame[0] - frame[1]).abs() < 1e-6,
                "upmixed mono must be DUPLICATED (L == R), got {frame:?}"
            );
            if (frame[0] - 0.25).abs() < 1e-3 {
                exact += 1;
            }
        }
    }
    assert!(real > 0, "some real audio must have been written");
    assert!(
        exact * 10 >= real * 9,
        "at least 90% of real frames must be the exact duplicated mono sample; \
         got {exact}/{real}"
    );
}
