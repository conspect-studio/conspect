//! Teardown-bound + short-write-tail tests for the display-audio sink
//! (DEV-B4 adversarial review MAJOR-2: teardown must be BOUNDED).
//!
//! `stop()`/`Drop` must never be held hostage by a wedged device: a PCM write
//! that blocks inside the driver (dead HDMI encoder, kernel stall) keeps the
//! drain thread inside the device call indefinitely, and an unbounded `join`
//! would hang the caller — the run-teardown path, possibly on an async runtime
//! thread. `stop()` must return within a hard bound, detaching (and logging)
//! the wedged thread instead of waiting on it forever.
//!
//! And a SHORT write — the device took only part of the offered quantum, which
//! is routine for a nonblocking PCM once its ring fills — must have its tail
//! re-written within the quantum, not silently discarded: dropping the tail of
//! every quantum throws away most of the program audio.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

fn cfg() -> DisplayAudioConfig {
    DisplayAudioConfig {
        output_id: "out-display".to_owned(),
        format: AudioFormat::new(48_000, ChannelLayout::Stereo),
        fifo_capacity_frames: 8_192,
        poll_interval: Duration::from_millis(2),
    }
}

#[test]
fn stop_returns_bounded_even_when_a_pcm_write_wedges() {
    // A PCM whose write call WEDGES (blocks inside the device for tens of
    // seconds — the failure nonblocking mode + snd_pcm_wait avoid on real
    // hardware, but a hostile/buggy driver can always stall a kernel call).
    // The drain thread is stuck inside `write`; `stop()` must still return
    // within a hard bound (bounded join + detach-and-log), never block the
    // caller for the duration of the wedge.
    #[derive(Clone, Default)]
    struct WedgedWriteAlsa {
        entered: Arc<AtomicBool>,
    }
    impl AlsaSink for WedgedWriteAlsa {
        fn open(&mut self, _p: PcmParams) -> Result<(), String> {
            Ok(())
        }
        fn write(&mut self, interleaved: &[f32], channels: usize) -> PcmOutcome {
            self.entered.store(true, Ordering::SeqCst);
            // Wedge: the device call does not come back for a long time.
            std::thread::sleep(Duration::from_secs(10));
            PcmOutcome::Wrote(interleaved.len() / channels.max(1))
        }
        fn recover(&mut self) -> PcmOutcome {
            PcmOutcome::Recovered
        }
        fn close(&mut self) {}
    }

    let eld = FixedEld(EldCapability::lpcm(2, &[48_000], "MON"));
    let alsa = WedgedWriteAlsa::default();
    let entered = Arc::clone(&alsa.entered);
    let (sink, _publisher) = DisplayAudioSink::start(cfg(), eld, alsa);

    // Wait until the drain thread is provably inside the wedged write.
    let deadline = Instant::now() + Duration::from_secs(2);
    while !entered.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        entered.load(Ordering::SeqCst),
        "the drain loop must have entered the PCM write"
    );

    let start = Instant::now();
    sink.stop();
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "stop() must be bounded even with a wedged PCM write (detach-and-log), \
         took {elapsed:?}"
    );
}

#[test]
fn short_write_tails_are_carried_not_discarded() {
    // A PCM that accepts at most 120 frames per write call (a nonblocking
    // device whose ring is nearly full — the routine steady state). The loop
    // drains 480-frame quanta from the FIFO; if it writes each quantum once
    // and ignores the short count, 3 of every 4 frames of program audio are
    // silently discarded. The tail must be carried: every content frame popped
    // from the FIFO reaches the device.
    #[derive(Clone, Default)]
    struct ShortWriteAlsa {
        samples: Arc<Mutex<Vec<f32>>>,
    }
    impl AlsaSink for ShortWriteAlsa {
        fn open(&mut self, _p: PcmParams) -> Result<(), String> {
            Ok(())
        }
        fn write(&mut self, interleaved: &[f32], channels: usize) -> PcmOutcome {
            let channels = channels.max(1);
            let offered = interleaved.len() / channels;
            let taken = offered.min(120);
            self.samples
                .lock()
                .expect("poisoned")
                .extend_from_slice(&interleaved[..taken * channels]);
            PcmOutcome::Wrote(taken)
        }
        fn recover(&mut self) -> PcmOutcome {
            PcmOutcome::Recovered
        }
        fn close(&mut self) {}
    }

    let eld = FixedEld(EldCapability::lpcm(2, &[48_000], "MON"));
    let alsa = ShortWriteAlsa::default();
    let samples = Arc::clone(&alsa.samples);
    let (sink, publisher) = DisplayAudioSink::start(cfg(), eld, alsa);

    // 4800 frames of distinctive content, pushed up-front (the 8192-frame FIFO
    // holds it without dropping); everything after it drains as silence.
    let block = vec![0.7f32; 480 * 2];
    for _ in 0..10 {
        publisher.push_block(&block);
    }
    let total_content_samples = 4_800 * 2;

    // Poll until the content has reached the device (or the deadline proves it
    // never will — the pre-fix behaviour plateaus at ~1/4 of it).
    let target = total_content_samples * 9 / 10;
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut delivered = 0usize;
    while Instant::now() < deadline {
        delivered = samples
            .lock()
            .expect("poisoned")
            .iter()
            .filter(|s| **s > 0.5)
            .count();
        if delivered >= target {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    sink.stop();
    assert!(
        delivered >= target,
        "short-write tails must be re-written, not discarded: only {delivered} \
         of {total_content_samples} content samples reached the device"
    );
}
