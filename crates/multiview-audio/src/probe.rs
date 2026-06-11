//! Content-aware **audio fault probes**: silence, over-level, clip,
//! phase-invert and channel-imbalance detection emitting `multiview_core::alarm`
//! signals.
//!
//! Each probe is a per-sample/per-frame analyser feeding a small
//! **dwell + hysteresis** state machine: a condition must persist for a
//! configured *dwell-up* time before the alarm is raised, and the input must be
//! healthy for a *dwell-down* time before it clears. This matches the broadcast
//! monitoring brief (§4) and the X.733 severity vocabulary in
//! [`multiview_core::alarm`].
//!
//! The probes are pure DSP and read-only (ADR-R006): they observe the same PCM
//! the meters do, on a thread that never back-pressures the engine. They emit
//! value-type [`AlarmRecord`]s; the engine's X.733 state machine
//! (`multiview_engine::alarm::state::AlarmStateMachine`) is the lifecycle that
//! raises/clears and rolls these up.
use multiview_core::alarm::{AlarmId, AlarmKind, AlarmRecord, AlarmScope, PerceivedSeverity};
use multiview_core::time::MediaTime;

use crate::ballistics::{Ballistics, MeterScale, PeakMode};
use crate::error::{AudioError, Result};

/// The X.733 severity assigned to each audio fault class when it is active.
///
/// Defaults follow the broadcast brief's guidance: silence/clip are
/// service-affecting (Major), over-level/phase/imbalance are Warnings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeSeverityProfile {
    /// Severity for an active [`AlarmKind::Silence`].
    pub silence: PerceivedSeverity,
    /// Severity for an active [`AlarmKind::OverLevel`].
    pub over_level: PerceivedSeverity,
    /// Severity for an active [`AlarmKind::Clip`].
    pub clip: PerceivedSeverity,
    /// Severity for an active [`AlarmKind::PhaseInvert`].
    pub phase_invert: PerceivedSeverity,
    /// Severity for an active channel-imbalance condition (mapped to
    /// [`AlarmKind::OverLevel`], distinguished by probe id).
    pub imbalance: PerceivedSeverity,
}

impl Default for ProbeSeverityProfile {
    fn default() -> Self {
        Self {
            silence: PerceivedSeverity::Major,
            over_level: PerceivedSeverity::Warning,
            clip: PerceivedSeverity::Major,
            phase_invert: PerceivedSeverity::Warning,
            imbalance: PerceivedSeverity::Warning,
        }
    }
}

/// Thresholds and dwell windows for the audio probe bank.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioProbeConfig {
    /// Silence is declared when the sample-peak stays below this dBFS level.
    pub silence_dbfs: f64,
    /// Over-level is declared when the sample-peak exceeds this dBFS ceiling.
    pub over_level_dbfs: f64,
    /// Clipping is declared after this many consecutive full-scale samples.
    pub clip_run: usize,
    /// Phase-invert is declared when the L/R correlation stays below this value.
    pub phase_threshold: f64,
    /// Channel imbalance is declared when two channels' levels differ by more
    /// than this many dB.
    pub imbalance_db: f64,
    /// How long a condition must persist before its alarm is raised (seconds).
    pub dwell_up_secs: f64,
    /// How long the input must be healthy before the alarm clears (seconds).
    pub dwell_down_secs: f64,
    /// Per-class severity mapping.
    pub severity: ProbeSeverityProfile,
}

impl Default for AudioProbeConfig {
    fn default() -> Self {
        Self {
            silence_dbfs: -60.0,
            over_level_dbfs: -3.0,
            clip_run: 3,
            phase_threshold: -0.5,
            imbalance_db: 12.0,
            dwell_up_secs: 0.2,
            dwell_down_secs: 0.2,
            severity: ProbeSeverityProfile::default(),
        }
    }
}

/// A dwell + hysteresis latch over a boolean condition.
#[derive(Debug, Clone)]
struct DwellLatch {
    up_samples: u64,
    down_samples: u64,
    /// Consecutive samples the condition has held (true) — counts toward raise.
    held: u64,
    /// Consecutive samples the condition has been clear — counts toward clear.
    cleared: u64,
    active: bool,
}

impl DwellLatch {
    fn new(fs: f64, up_secs: f64, down_secs: f64) -> Self {
        Self {
            up_samples: secs_to_samples(fs, up_secs),
            down_samples: secs_to_samples(fs, down_secs),
            held: 0,
            cleared: 0,
            active: false,
        }
    }

    /// Advance the latch by one observation; returns the active state.
    fn update(&mut self, condition: bool) -> bool {
        if condition {
            self.held = self.held.saturating_add(1);
            self.cleared = 0;
            if self.held >= self.up_samples {
                self.active = true;
            }
        } else {
            self.cleared = self.cleared.saturating_add(1);
            self.held = 0;
            if self.cleared >= self.down_samples {
                self.active = false;
            }
        }
        self.active
    }
}

/// Round a duration in seconds to a non-zero sample count.
#[allow(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)] // reason: dwell windows are small, non-negative, finite seconds; round()+max keeps it in range.
fn secs_to_samples(fs: f64, secs: f64) -> u64 {
    (fs * secs.max(0.0)).round().max(1.0) as u64
}

/// A bank of audio fault probes over an `N`-channel stream.
///
/// Feed it interleaved frames with [`AudioProbeBank::push_frame`]; query the
/// current alarms with [`AudioProbeBank::active_alarms`].
#[derive(Debug)]
pub struct AudioProbeBank {
    channels: usize,
    cfg: AudioProbeConfig,
    /// Sample-peak meters per channel (for silence/over-level/imbalance).
    peaks: Vec<Ballistics>,
    /// Per-channel running clip run-length.
    clip_runs: Vec<usize>,
    /// Stereo correlation (channels 0/1) for phase detection.
    correlation: crate::correlation::CorrelationMeter,
    silence: DwellLatch,
    over_level: DwellLatch,
    clip: DwellLatch,
    phase: DwellLatch,
    imbalance: DwellLatch,
    /// Media time advanced one sample-period per frame, for `raised_at`.
    now: MediaTime,
    samples_seen: u64,
    fs: u32,
}

impl AudioProbeBank {
    /// Construct a probe bank for `channels` channels at `sample_rate` Hz.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::InvalidFormat`] if `sample_rate` or `channels` is
    /// zero.
    pub fn new(sample_rate: u32, channels: usize, cfg: AudioProbeConfig) -> Result<Self> {
        if sample_rate == 0 || channels == 0 {
            return Err(AudioError::InvalidFormat(
                "probe sample rate and channels must be non-zero",
            ));
        }
        let fs = f64::from(sample_rate);
        let peaks = (0..channels)
            .map(|_| Ballistics::new(sample_rate, MeterScale::SamplePeak(PeakMode::Sample)))
            .collect();
        let latch = |up: f64, down: f64| DwellLatch::new(fs, up, down);
        Ok(Self {
            channels,
            cfg,
            peaks,
            clip_runs: vec![0; channels],
            correlation: crate::correlation::CorrelationMeter::new(),
            silence: latch(cfg.dwell_up_secs, cfg.dwell_down_secs),
            over_level: latch(cfg.dwell_up_secs, cfg.dwell_down_secs),
            // Clipping is a sample-domain fault: the `clip_run` consecutive
            // full-scale samples are its debounce, so the time-latch raises
            // immediately on a confirmed run and only the *clear* side dwells.
            clip: latch(0.0, cfg.dwell_down_secs),
            phase: latch(cfg.dwell_up_secs, cfg.dwell_down_secs),
            imbalance: latch(cfg.dwell_up_secs, cfg.dwell_down_secs),
            now: MediaTime::ZERO,
            samples_seen: 0,
            fs: sample_rate,
        })
    }

    /// Push one interleaved frame (`channels` samples). Extra/short frames are
    /// tolerated: missing channels read as silence.
    pub fn push_frame(&mut self, frame: &[f64]) {
        // Update per-channel peak meters and clip run-lengths.
        let mut clip_now = false;
        for c in 0..self.channels {
            let x = frame.get(c).copied().unwrap_or(0.0);
            if let Some(p) = self.peaks.get_mut(c) {
                p.push(x);
            }
            // Clip: |x| at or above full scale, run of `clip_run` samples.
            if let Some(run) = self.clip_runs.get_mut(c) {
                if x.abs() >= 1.0 - 1e-6 {
                    *run = run.saturating_add(1);
                    if *run >= self.cfg.clip_run.max(1) {
                        clip_now = true;
                    }
                } else {
                    *run = 0;
                }
            }
        }

        // Stereo phase correlation on channels 0/1 (if present).
        if self.channels >= 2 {
            let l = frame.first().copied().unwrap_or(0.0);
            let r = frame.get(1).copied().unwrap_or(0.0);
            self.correlation.push(l, r);
        }

        // Evaluate the level-based conditions from the current peak readings.
        let peak_max = self.peak_max_db();
        let silence_now = peak_max <= self.cfg.silence_dbfs;
        let over_now = peak_max > self.cfg.over_level_dbfs;
        let phase_now =
            self.channels >= 2 && self.correlation.correlation() < self.cfg.phase_threshold;
        let imbalance_now = self.imbalance_now();

        self.silence.update(silence_now);
        self.over_level.update(over_now);
        self.clip.update(clip_now);
        self.phase.update(phase_now);
        self.imbalance.update(imbalance_now);

        // Advance the media clock by one sample period.
        self.samples_seen = self.samples_seen.saturating_add(1);
        self.now = MediaTime::from_nanos(self.sample_period_ns(self.samples_seen));
    }

    /// Nanoseconds elapsed for `samples` at the current sample rate.
    fn sample_period_ns(&self, samples: u64) -> i64 {
        let ns = samples
            .saturating_mul(1_000_000_000)
            .checked_div(u64::from(self.fs))
            .unwrap_or(0);
        i64::try_from(ns).unwrap_or(i64::MAX)
    }

    /// Maximum per-channel sample-peak reading, in dBFS.
    fn peak_max_db(&self) -> f64 {
        self.peaks
            .iter()
            .map(Ballistics::reading_db)
            .fold(Ballistics::FLOOR_DB, f64::max)
    }

    /// Whether any two channels' peak readings differ by more than the
    /// configured imbalance threshold.
    fn imbalance_now(&self) -> bool {
        if self.channels < 2 {
            return false;
        }
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        // Only consider channels carrying signal (above the silence floor); an
        // entirely silent stream is "balanced", not imbalanced.
        for p in &self.peaks {
            let db = p.reading_db();
            if db > self.cfg.silence_dbfs {
                min = min.min(db);
                max = max.max(db);
            }
        }
        max.is_finite() && min.is_finite() && (max - min) > self.cfg.imbalance_db
    }

    /// Iterate the currently-active alarm records.
    pub fn active_alarms(&self) -> impl Iterator<Item = AlarmRecord> + '_ {
        let sev = &self.cfg.severity;
        let mk = move |active: bool, id: &str, kind: AlarmKind, severity: PerceivedSeverity| {
            if active {
                Some(AlarmRecord::new(
                    AlarmId::new(id),
                    kind,
                    severity,
                    AlarmScope::Probe { id: id.to_owned() },
                    self.now,
                ))
            } else {
                None
            }
        };
        [
            mk(
                self.silence.active,
                "audio.silence",
                AlarmKind::Silence,
                sev.silence,
            ),
            mk(
                self.over_level.active,
                "audio.over_level",
                AlarmKind::OverLevel,
                sev.over_level,
            ),
            mk(self.clip.active, "audio.clip", AlarmKind::Clip, sev.clip),
            mk(
                self.phase.active,
                "audio.phase_invert",
                AlarmKind::PhaseInvert,
                sev.phase_invert,
            ),
            mk(
                self.imbalance.active,
                "audio.imbalance",
                AlarmKind::OverLevel,
                sev.imbalance,
            ),
        ]
        .into_iter()
        .flatten()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;

    #[test]
    fn dwell_latch_requires_persistence() {
        let mut l = DwellLatch::new(48_000.0, 0.001, 0.001); // ~48 samples
        assert!(!l.update(true));
        for _ in 0..47 {
            l.update(true);
        }
        assert!(l.active, "latch should raise after the dwell window");
        for _ in 0..48 {
            l.update(false);
        }
        assert!(!l.active, "latch should clear after the down window");
    }
}
