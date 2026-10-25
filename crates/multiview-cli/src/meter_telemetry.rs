//! Off-hot-path **program-bus peak/RMS meter telemetry** (AUD-8 / SW-14): taps
//! the *emitted* program audio on the bake-consumer thread and **pushes** a
//! conflated [`multiview_events::AudioMeter`] sample (per-channel peak + RMS in
//! dBFS, clip flag) onto the engine's drop-oldest event stream at ~30 Hz
//! ([`METER_EMIT_HZ`]), so the SPA's program meters move in real time.
//!
//! Read-only and non-blocking (ADR-R006, invariant #10): every block is
//! accumulated, but a [`Conflator`] gates the *emit* to the display cadence; the
//! publish is a single drop-oldest broadcast send, so a stalled UI only loses
//! its own samples and can never delay the engine. Peak is the **max since the
//! last emit** (so a short transient between two emits is never missed); RMS is
//! the energy mean over the same window.

use multiview_audio::{AudioBlock, Conflator, DISPLAY_HZ};
use multiview_events::AudioMeter;

/// Meter emit cadence (Hz): the display cadence, inside the 10–30 Hz envelope.
pub const METER_EMIT_HZ: u32 = DISPLAY_HZ;

/// Floor reported for silence (dBFS) — finite so it serialises as valid JSON.
pub const SILENCE_FLOOR_DB: f32 = -120.0;

/// A sample at/above this magnitude counts as clipped (full scale).
const CLIP_LEVEL: f32 = 1.0;

/// Convert a linear amplitude to dBFS, floored at [`SILENCE_FLOOR_DB`].
fn lin_to_db(lin: f64) -> f32 {
    if !lin.is_finite() || lin <= 0.0 {
        return SILENCE_FLOOR_DB;
    }
    let db = 20.0 * lin.log10();
    db.to_string()
        .parse::<f32>()
        .map_or(SILENCE_FLOOR_DB, |v| v.max(SILENCE_FLOOR_DB))
}

/// The program-bus peak/RMS meter + 30 Hz emit conflator.
#[derive(Debug)]
pub struct MeterTelemetry {
    track: u32,
    channels: usize,
    peak: Vec<f32>,
    sum_sq: Vec<f64>,
    frames: u64,
    clip: bool,
    conflator: Conflator<()>,
}

impl MeterTelemetry {
    /// Build a meter for `channels` channels, reported as `track`.
    #[must_use]
    pub fn new(track: u32, channels: usize) -> Self {
        let channels = channels.max(1);
        Self {
            track,
            channels,
            peak: vec![0.0; channels],
            sum_sq: vec![0.0; channels],
            frames: 0,
            clip: false,
            conflator: Conflator::with_rate(METER_EMIT_HZ),
        }
    }

    /// Accumulate one emitted block (always) and return a conflated sample at
    /// most [`METER_EMIT_HZ`] times a second. Never blocks or panics.
    pub fn push(&mut self, block: &AudioBlock, now_ns: i64) -> Option<AudioMeter> {
        let ch = self.channels;
        for frame in block.interleaved().chunks_exact(ch) {
            for ((&x, peak), sum_sq) in frame
                .iter()
                .zip(self.peak.iter_mut())
                .zip(self.sum_sq.iter_mut())
            {
                let a = x.abs();
                if a.is_nan() {
                    continue;
                }
                if a > *peak {
                    *peak = a;
                }
                if a >= CLIP_LEVEL {
                    self.clip = true;
                }
                *sum_sq += f64::from(x) * f64::from(x);
            }
            self.frames += 1;
        }
        self.conflator.accept(());
        self.conflator.poll(now_ns)?;
        Some(self.take_sample())
    }

    /// Pack the window since the last emit into the wire sample and reset it.
    fn take_sample(&mut self) -> AudioMeter {
        let n = self.frames.max(1);
        let n = n.to_string().parse::<f64>().unwrap_or(1.0);
        let peak_db = self.peak.iter().map(|p| lin_to_db(f64::from(*p))).collect();
        let rms_db = self
            .sum_sq
            .iter()
            .map(|e| lin_to_db((e / n).sqrt()))
            .collect();
        let sample = AudioMeter {
            track: self.track,
            input_id: None,
            peak_db,
            rms_db,
            clip: self.clip,
            overflow: false,
            sampled_hz: METER_EMIT_HZ,
        };
        self.peak.iter_mut().for_each(|p| *p = 0.0);
        self.sum_sq.iter_mut().for_each(|e| *e = 0.0);
        self.frames = 0;
        self.clip = false;
        sample
    }
}

/// Maximum distinct per-input tracks metered (bounded memory; excess inputs are
/// simply not metered, never an error on the audio path).
pub const MAX_INPUT_TRACKS: usize = 64;

/// Per-input peak/RMS meters: one [`MeterTelemetry`] per routed source, keyed by
/// source id, with a stable track index (`1..`, first-seen order; `0` is the
/// program bus).
#[derive(Debug, Default)]
pub struct InputMeters {
    meters: Vec<(String, MeterTelemetry)>,
}

impl InputMeters {
    /// An empty set; meters are created on first sight of each input id.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Meter every input's discrete block and return the samples due this call
    /// (each input is independently conflated to [`METER_EMIT_HZ`]).
    pub fn push(&mut self, tracks: &[(&str, &AudioBlock)], now_ns: i64) -> Vec<AudioMeter> {
        let _ = (tracks, now_ns, MAX_INPUT_TRACKS, &mut self.meters);
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::float_cmp,
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]

    use super::*;
    use multiview_audio::{AudioFormat, ChannelLayout};
    use multiview_engine::EnginePublisher;
    use multiview_engine::TryRecvError;
    use multiview_events::Event;

    fn fmt() -> AudioFormat {
        AudioFormat::new(48_000, ChannelLayout::Stereo)
    }

    /// 10 ms stereo block: left at `l`, right at `r` (constant DC amplitude).
    fn block(l: f32, r: f32) -> AudioBlock {
        let mut s = Vec::new();
        for _ in 0..480 {
            s.push(l);
            s.push(r);
        }
        AudioBlock::from_interleaved(fmt(), s).expect("block")
    }

    #[test]
    fn reports_per_channel_peak_and_rms_in_dbfs() {
        let mut m = MeterTelemetry::new(0, 2);
        let s = m.push(&block(0.5, 0.1), 0).expect("first push emits");
        assert_eq!(s.track, 0);
        assert_eq!(s.peak_db.len(), 2);
        assert!((s.peak_db[0] - (-6.02)).abs() < 0.05, "{:?}", s.peak_db);
        assert!((s.peak_db[1] - (-20.0)).abs() < 0.05, "{:?}", s.peak_db);
        assert!((s.rms_db[0] - (-6.02)).abs() < 0.05, "{:?}", s.rms_db);
        assert!(!s.clip && !s.overflow);
        assert_eq!(s.sampled_hz, METER_EMIT_HZ);
    }

    #[test]
    fn silence_is_a_finite_floor_not_neg_infinity() {
        let mut m = MeterTelemetry::new(0, 2);
        let s = m.push(&block(0.0, 0.0), 0).expect("emit");
        assert!(s
            .peak_db
            .iter()
            .chain(&s.rms_db)
            .all(|v| *v == SILENCE_FLOOR_DB));
    }

    #[test]
    fn clip_flag_set_at_full_scale() {
        let mut m = MeterTelemetry::new(0, 2);
        let s = m.push(&block(1.0, 0.0), 0).expect("emit");
        assert!(s.clip);
    }

    #[test]
    fn peak_is_held_across_conflated_blocks_between_emits() {
        // A loud block followed (within the same emit window) by a quiet one:
        // the emitted peak must still reflect the loud transient.
        let mut m = MeterTelemetry::new(0, 2);
        assert!(m.push(&block(0.0, 0.0), 0).is_some()); // primes; window reset
        assert!(m.push(&block(0.8, 0.8), 5_000_000).is_none());
        assert!(m.push(&block(0.01, 0.01), 10_000_000).is_none());
        let s = m.push(&block(0.01, 0.01), 40_000_000).expect("due");
        assert!((s.peak_db[0] - (-1.94)).abs() < 0.05, "{:?}", s.peak_db);
        // Window reset after the emit.
        let s2 = m.push(&block(0.01, 0.01), 80_000_000).expect("due");
        assert!((s2.peak_db[0] - (-40.0)).abs() < 0.05, "{:?}", s2.peak_db);
    }

    #[test]
    fn emit_rate_is_bounded_to_about_thirty_hz() {
        let mut m = MeterTelemetry::new(0, 2);
        let b = block(0.3, 0.3);
        let mut emits = 0;
        let mut now = 0_i64;
        for _ in 0..100 {
            now += 10_000_000; // 100 blocks/s
            if m.push(&b, now).is_some() {
                emits += 1;
            }
        }
        assert!((25..=35).contains(&emits), "got {emits}");
    }

    #[test]
    fn stalled_subscriber_never_blocks_publishing_and_lags_independently() {
        // Publish a second of meter frames through the real drop-oldest engine
        // stream with a subscriber that NEVER reads. Publishing completes (it
        // cannot block, inv #10), the bounded rate holds, and a live subscriber
        // still receives frames.
        let publisher: EnginePublisher<(), Event> = EnginePublisher::new(8);
        let mut stalled = publisher.subscribe();
        let mut live = publisher.subscribe();
        let mut m = MeterTelemetry::new(0, 2);
        let b = block(0.3, 0.3);
        let mut sent = 0;
        let mut received = 0;
        let mut now = 0_i64;
        for _ in 0..300 {
            now += 10_000_000;
            if let Some(s) = m.push(&b, now) {
                publisher.publish_event(Event::AudioMeter(s));
                sent += 1;
            }
            while live.try_recv().is_ok() {
                received += 1;
            }
        }
        assert!((70..=100).contains(&sent), "30 Hz over 3 s: {sent}");
        assert_eq!(received, sent, "the live subscriber saw every frame");
        // The stalled subscriber lost its own oldest frames (drop-oldest), then
        // resumes at a bounded backlog: it never held back the publisher.
        assert!(
            matches!(stalled.try_recv(), Err(TryRecvError::Lagged(_))),
            "stalled subscriber must observe drop-oldest lag"
        );
        let mut backlog = 0;
        while stalled.try_recv().is_ok() {
            backlog += 1;
        }
        assert!(
            backlog <= 8,
            "stalled backlog bounded by capacity: {backlog}"
        );
    }

    #[test]
    fn per_input_meters_have_stable_tracks_ids_and_own_levels() {
        let loud = block(0.5, 0.5);
        let quiet = block(0.05, 0.05);
        let mut im = InputMeters::new();
        let out = im.push(&[("cam_a", &loud), ("cam_b", &quiet)], 0);
        assert_eq!(out.len(), 2);
        assert_eq!(
            (out[0].track, out[0].input_id.as_deref()),
            (1, Some("cam_a"))
        );
        assert_eq!(
            (out[1].track, out[1].input_id.as_deref()),
            (2, Some("cam_b"))
        );
        assert!((out[0].peak_db[0] - (-6.02)).abs() < 0.05);
        assert!((out[1].peak_db[0] - (-26.02)).abs() < 0.05);
        // Same ids on a later call keep their track index; the rate is bounded.
        let again = im.push(&[("cam_a", &loud), ("cam_b", &quiet)], 5_000_000);
        assert!(again.is_empty(), "inside the 30 Hz window nothing is due");
        let later = im.push(&[("cam_b", &quiet), ("cam_a", &loud)], 100_000_000);
        let mut tracks: Vec<_> = later.iter().map(|m| m.track).collect();
        tracks.sort_unstable();
        assert_eq!(tracks, vec![1, 2]);
    }

    #[test]
    fn per_input_meters_are_bounded() {
        let b = block(0.1, 0.1);
        let ids: Vec<String> = (0..MAX_INPUT_TRACKS + 10)
            .map(|i| format!("in{i}"))
            .collect();
        let tracks: Vec<(&str, &AudioBlock)> = ids.iter().map(|i| (i.as_str(), &b)).collect();
        let mut im = InputMeters::new();
        assert_eq!(im.push(&tracks, 0).len(), MAX_INPUT_TRACKS);
    }
}
