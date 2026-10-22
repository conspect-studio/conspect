//! ALSA xrun-recovery state machine (DEV-B4 / bad-inputs-are-the-purpose).
//!
//! An ALSA underrun (`-EPIPE`) or suspend (`-ESTRPIPE`) must never crash the
//! sink: the recovery machine asks the PCM to prepare/resume and re-prime,
//! holding audio rather than faltering — the display/audio never goes black.
//! This is the **pure** state machine: it consumes scripted [`PcmOutcome`]s and
//! emits [`RecoverAction`]s, so the loop's behaviour is CI-proven hardware-free.
//! The [`super::alsa`] backend (feature `display-kms`) maps real libasound
//! return codes onto these outcomes and performs the recover.

use std::time::Duration;

/// What a single PCM write/recover attempt produced — the alphabet the
/// [`XrunRecovery`] machine consumes (mapped from libasound by [`super::alsa`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PcmOutcome {
    /// A write completed, delivering this many frames to the device.
    Wrote(usize),
    /// The device underran (`-EPIPE`): the ring drained before the next write.
    Underrun,
    /// The device was suspended (`-ESTRPIPE`): needs resume then prepare.
    Suspended,
    /// A recover (`snd_pcm_prepare`/`snd_pcm_resume`) succeeded.
    Recovered,
    /// A recover attempt itself failed (device gone, persistent fault).
    RecoverFailed,
}

/// The recovery lifecycle of the PCM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum XrunState {
    /// Opened but not yet confirmed running (the first write primes it).
    Priming,
    /// Steady state: writes are completing.
    Running,
    /// An xrun/suspend was seen; a recover has been requested and is pending.
    Recovering,
    /// Recovery has repeatedly failed: the sink stays alive but **silent** and
    /// backs off before re-trying, never spinning or crashing.
    Degraded,
}

/// What the loop should do as a result of feeding an outcome to the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct RecoverAction {
    /// Whether the loop should issue a PCM recover (prepare/resume) now.
    pub recover: bool,
}

/// The xrun-recovery state machine.
///
/// Feed every PCM attempt's [`PcmOutcome`] to [`on_outcome`](Self::on_outcome);
/// it advances [`state`](Self::state), returns the [`RecoverAction`] to take, and
/// tracks a recovery count + exponential backoff for the [`Degraded`] state.
#[derive(Debug, Clone)]
pub struct XrunRecovery {
    state: XrunState,
    recoveries: u64,
    consecutive_failures: u32,
}

impl Default for XrunRecovery {
    fn default() -> Self {
        Self::new()
    }
}

impl XrunRecovery {
    /// A fresh machine in [`XrunState::Priming`].
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: XrunState::Priming,
            recoveries: 0,
            consecutive_failures: 0,
        }
    }

    /// After how many consecutive recover failures the machine declares the PCM
    /// [`Degraded`](XrunState::Degraded) (silent + backoff) rather than retrying
    /// tightly.
    const DEGRADE_AFTER: u32 = 3;

    /// The current lifecycle state.
    #[must_use]
    pub const fn state(&self) -> XrunState {
        self.state
    }

    /// Total successful recoveries since construction (telemetry).
    #[must_use]
    pub const fn recoveries(&self) -> u64 {
        self.recoveries
    }

    /// Advance the machine with the latest PCM attempt outcome and return the
    /// action the loop should take.
    pub fn on_outcome(&mut self, outcome: PcmOutcome) -> RecoverAction {
        match outcome {
            PcmOutcome::Wrote(_) => {
                // A clean write confirms the PCM is running again.
                self.state = XrunState::Running;
                self.consecutive_failures = 0;
                RecoverAction { recover: false }
            }
            PcmOutcome::Underrun | PcmOutcome::Suspended => {
                self.state = XrunState::Recovering;
                // Always ask for a recover — even from Degraded, where the
                // backoff (see `backoff`) paces the retry instead of spinning.
                RecoverAction { recover: true }
            }
            PcmOutcome::Recovered => {
                self.recoveries = self.recoveries.saturating_add(1);
                self.consecutive_failures = 0;
                // Recovery prepared the PCM; the next write re-primes it. Lifting
                // straight to Priming (not Running) means a Degraded sink only
                // reports healthy once audio actually flows again.
                self.state = XrunState::Priming;
                RecoverAction { recover: false }
            }
            PcmOutcome::RecoverFailed => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                if self.consecutive_failures >= Self::DEGRADE_AFTER {
                    self.state = XrunState::Degraded;
                } else {
                    self.state = XrunState::Recovering;
                }
                // Keep asking to recover; the loop paces by `backoff()`.
                RecoverAction { recover: true }
            }
        }
    }

    /// How long the loop should wait before the next recover attempt. Zero while
    /// healthy; an exponential backoff (capped) once [`Degraded`](XrunState::Degraded),
    /// so a dead device is retried slowly rather than in a hot loop.
    #[must_use]
    pub fn backoff(&self) -> Duration {
        if self.state != XrunState::Degraded {
            return Duration::ZERO;
        }
        // 2^(failures) × base, capped — failures is small (>= DEGRADE_AFTER).
        let shift = self.consecutive_failures.min(8);
        let millis = 50u64.saturating_mul(1u64 << shift);
        Duration::from_millis(millis.min(2_000))
    }
}
